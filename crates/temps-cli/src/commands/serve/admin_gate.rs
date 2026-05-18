//! Defense-in-depth gate for the admin console listener.
//!
//! The admin listener is normally bound to a private interface (loopback,
//! VPN, etc.) so external traffic cannot reach it at the network layer. This
//! middleware adds a second check inside Axum: requests are rejected unless
//! their source IP matches `TEMPS_ADMIN_ALLOWED_IPS` and their Host header
//! matches `TEMPS_ADMIN_ALLOWED_HOSTS` (when either is configured).
//!
//! Denials return `404 Not Found` rather than `403 Forbidden` so that a
//! probing client cannot tell the admin surface exists at all.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ipnet::IpNet;
use tracing::{debug, warn};

/// Configuration parsed once at startup and shared across requests.
#[derive(Clone, Debug)]
pub struct AdminGateConfig {
    /// Allowed source networks. Empty = allow any source.
    pub allowed_nets: Arc<Vec<IpNet>>,
    /// Allowed `Host` header values (port stripped, lowercased).
    /// Empty = allow any host.
    pub allowed_hosts: Arc<Vec<String>>,
    /// When true, the gate honors an `X-Forwarded-For` header — but only
    /// when the immediate peer is loopback, so an external client cannot
    /// spoof their source IP by setting the header themselves.
    pub trust_forwarded_for: bool,
}

impl AdminGateConfig {
    pub fn from_env(
        allowed_ips: &[String],
        allowed_hosts: &[String],
        trust_forwarded_for: bool,
    ) -> Result<Self, AdminGateConfigError> {
        let allowed_nets = allowed_ips
            .iter()
            .map(|raw| parse_cidr(raw))
            .collect::<Result<Vec<_>, _>>()?;

        let allowed_hosts = allowed_hosts
            .iter()
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .collect::<Vec<_>>();

        Ok(Self {
            allowed_nets: Arc::new(allowed_nets),
            allowed_hosts: Arc::new(allowed_hosts),
            trust_forwarded_for,
        })
    }

    /// Returns true when no gate is configured. Callers can skip wiring the
    /// middleware in that case to avoid the per-request lookup cost.
    pub fn is_noop(&self) -> bool {
        self.allowed_nets.is_empty() && self.allowed_hosts.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AdminGateConfigError {
    #[error("Invalid admin allowlist entry '{raw}': {reason}")]
    InvalidCidr { raw: String, reason: String },
}

/// Parse a single allowlist entry. Bare IPs are upgraded to /32 (v4) or /128
/// (v6) so the rest of the code can treat everything as a network.
fn parse_cidr(raw: &str) -> Result<IpNet, AdminGateConfigError> {
    let trimmed = raw.trim();
    if let Ok(net) = trimmed.parse::<IpNet>() {
        return Ok(net);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        let net = match ip {
            IpAddr::V4(v4) => IpNet::V4(ipnet::Ipv4Net::new(v4, 32).unwrap()),
            IpAddr::V6(v6) => IpNet::V6(ipnet::Ipv6Net::new(v6, 128).unwrap()),
        };
        return Ok(net);
    }
    Err(AdminGateConfigError::InvalidCidr {
        raw: trimmed.to_string(),
        reason: "expected an IP address or CIDR (e.g. 10.0.0.0/8)".into(),
    })
}

/// Resolve the effective client IP for gating purposes. When
/// `trust_forwarded_for` is true and the immediate peer is loopback, the
/// leftmost address in `X-Forwarded-For` wins; otherwise the peer's address
/// is used directly.
fn effective_client_ip(req: &Request, peer: IpAddr, trust_forwarded_for: bool) -> IpAddr {
    if !trust_forwarded_for || !peer.is_loopback() {
        return peer;
    }
    let Some(value) = req.headers().get("x-forwarded-for") else {
        return peer;
    };
    let Ok(value) = value.to_str() else {
        return peer;
    };
    value
        .split(',')
        .next()
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

fn host_matches(req: &Request, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_lowercase());
    match host {
        Some(host) => allowed.iter().any(|allowed| allowed == &host),
        None => false,
    }
}

fn ip_matches(ip: IpAddr, allowed: &[IpNet]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed.iter().any(|net| net.contains(&ip))
}

/// Axum middleware that enforces the admin gate. Wire this onto the admin
/// router after `build_split_application` and before `axum::serve`.
pub async fn admin_gate(
    State(config): State<AdminGateConfig>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let client_ip = effective_client_ip(&req, peer.ip(), config.trust_forwarded_for);

    if !ip_matches(client_ip, &config.allowed_nets) {
        warn!(
            client_ip = %client_ip,
            peer = %peer,
            path = %req.uri().path(),
            "admin gate denied: source IP not in allowlist"
        );
        return StatusCode::NOT_FOUND.into_response();
    }

    if !host_matches(&req, &config.allowed_hosts) {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        warn!(
            client_ip = %client_ip,
            host = %host,
            path = %req.uri().path(),
            "admin gate denied: Host header not in allowlist"
        );
        return StatusCode::NOT_FOUND.into_response();
    }

    debug!(client_ip = %client_ip, path = %req.uri().path(), "admin gate allow");
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn cfg(ips: &[&str], hosts: &[&str], trust_xff: bool) -> AdminGateConfig {
        let ips: Vec<String> = ips.iter().map(|s| s.to_string()).collect();
        let hosts: Vec<String> = hosts.iter().map(|s| s.to_string()).collect();
        AdminGateConfig::from_env(&ips, &hosts, trust_xff).unwrap()
    }

    #[test]
    fn bare_ip_is_upgraded_to_host_route() {
        let net = parse_cidr("10.0.0.1").unwrap();
        assert!(net.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!net.contains(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))));
    }

    #[test]
    fn cidr_is_parsed() {
        let net = parse_cidr("10.0.0.0/8").unwrap();
        assert!(net.contains(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(!net.contains(&IpAddr::V4(Ipv4Addr::new(11, 0, 0, 0))));
    }

    #[test]
    fn ipv6_cidr_is_parsed() {
        let net = parse_cidr("2001:db8::/32").unwrap();
        assert!(net.contains(&IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap())));
        assert!(!net.contains(&IpAddr::V6("2001:dead::1".parse::<Ipv6Addr>().unwrap())));
    }

    #[test]
    fn invalid_cidr_is_rejected() {
        let err = parse_cidr("not-an-ip").unwrap_err();
        matches!(err, AdminGateConfigError::InvalidCidr { .. });
    }

    #[test]
    fn empty_config_is_noop() {
        let c = cfg(&[], &[], false);
        assert!(c.is_noop());
    }

    #[test]
    fn ip_matcher_allows_any_when_empty() {
        assert!(ip_matches(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            &Vec::new()
        ));
    }

    #[test]
    fn ip_matcher_denies_outside_cidr() {
        let nets = vec![parse_cidr("10.0.0.0/8").unwrap()];
        assert!(!ip_matches(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1)), &nets));
    }

    #[test]
    fn host_matcher_strips_port_and_lowercases() {
        let req = Request::builder()
            .uri("/")
            .header(header::HOST, "Admin.Example.COM:8443")
            .body(axum::body::Body::empty())
            .unwrap();
        let allowed = vec!["admin.example.com".to_string()];
        assert!(host_matches(&req, &allowed));
    }

    #[test]
    fn host_matcher_denies_unknown_host() {
        let req = Request::builder()
            .uri("/")
            .header(header::HOST, "evil.example.com")
            .body(axum::body::Body::empty())
            .unwrap();
        let allowed = vec!["admin.example.com".to_string()];
        assert!(!host_matches(&req, &allowed));
    }

    #[test]
    fn forwarded_for_only_trusted_from_loopback() {
        let req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", "203.0.113.5")
            .body(axum::body::Body::empty())
            .unwrap();

        let peer_loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let peer_external = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));

        // Loopback + trust → use header
        assert_eq!(
            effective_client_ip(&req, peer_loopback, true),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))
        );
        // External + trust → ignore header (anti-spoofing)
        assert_eq!(
            effective_client_ip(&req, peer_external, true),
            peer_external
        );
        // Loopback + no trust → ignore header
        assert_eq!(
            effective_client_ip(&req, peer_loopback, false),
            peer_loopback
        );
    }
}
