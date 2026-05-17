//! Preview gateway authentication for Pingora.
//!
//! When a request hits a hostname matching
//! `ws-<sandbox_hex>-<port>.<preview_domain>`, the proxy looks up the sandbox,
//! checks for a valid preview cookie, and (on success) forwards the request
//! to the local preview gateway at `127.0.0.1:8090`.
//!
//! Unauthenticated requests are redirected to a form-based login page at
//! `/__temps/preview/login` (handled in [`crate::handler::preview_wall`] and
//! `proxy.rs`). HTTP Basic auth is **not** supported — browsers cache Basic
//! credentials unpredictably across subdomains and some clients refuse to
//! send them over plain HTTP. Cookie + form is the only supported flow.
//!
//! Design notes:
//! - The preview gateway itself is a dumb TCP-level reverse proxy bound to
//!   loopback. All authentication happens here in Pingora so the gateway never
//!   needs to talk to the database.
//! - Failures are rate-limited per (client_ip, sandbox_hex) using an in-memory
//!   sliding window. This is best-effort and resets on proxy restart.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::password_hash::PasswordHash;
use argon2::{Argon2, PasswordVerifier};
use dashmap::DashMap;
use sea_orm::EntityTrait;
use temps_core::CookieCrypto;
use temps_database::DbConnection;
use tracing::{debug, warn};

/// Cookie name template for sandbox previews (`temps_preview_sbx_<hex>`).
pub const PREVIEW_SANDBOX_COOKIE_PREFIX: &str = "temps_preview_sbx_";

/// How long a preview session cookie is valid before the user is asked to
/// re-enter the password. Rotating the password invalidates cookies
/// immediately regardless of this TTL.
pub const PREVIEW_COOKIE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The local TCP address where the preview gateway listens. Pingora forwards
/// authenticated preview requests to this peer.
pub const PREVIEW_GATEWAY_PEER: &str = "127.0.0.1:8090";

/// Maximum number of failed auth attempts allowed per (client_ip, sandbox_hex)
/// inside [`RATE_LIMIT_WINDOW`] before the proxy starts rejecting with 429.
const MAX_FAILURES: u32 = 10;

/// Sliding window for rate limiting failed auth attempts.
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Parsed preview hostname components.
///
/// `hex` is the 16-hex suffix of the sandbox `sbx_<hex>` public_id, stored
/// lowercase to avoid case-sensitivity bugs in cookie names and log output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewHost {
    pub hex: String,
    pub port: u16,
}

impl PreviewHost {
    /// Human-readable label for logs.
    pub fn label(&self) -> &str {
        &self.hex
    }
}

/// Parse a hostname against `ws-<hex>-<port>.<preview_domain>`.
///
/// `preview_domain` may start with `*.` (wildcard form) — the leading `*.` is
/// stripped before comparison. The label must be exactly 16 hex chars (the
/// suffix of a sandbox `sbx_<hex>` public_id). The port must be a non-zero
/// `u16`.
pub fn parse_preview_host(host: &str, preview_domain: &str) -> Option<PreviewHost> {
    let domain = preview_domain.trim_start_matches("*.");
    let host_no_port = host.split(':').next()?.to_ascii_lowercase();
    let suffix = format!(".{}", domain.to_ascii_lowercase());
    let label = host_no_port.strip_suffix(&suffix)?;

    // label must be `ws-<hex>-<port>`
    let rest = label.strip_prefix("ws-")?;
    let (sid_str, port_str) = rest.rsplit_once('-')?;

    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }

    if sid_str.len() != 16 || !sid_str.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    Some(PreviewHost {
        hex: sid_str.to_ascii_lowercase(),
        port,
    })
}

/// Outcome of preview auth processing.
#[derive(Debug)]
pub enum PreviewAuthOutcome {
    /// Auth succeeded — forward the request to [`PREVIEW_GATEWAY_PEER`].
    Allow { host: PreviewHost },
    /// No valid cookie — reply with 303 redirect to the login form.
    LoginRequired { host: PreviewHost },
    /// Too many failed attempts — reply with 429.
    RateLimited { host: PreviewHost },
    /// Target sandbox does not exist (or DB lookup failed).
    NotFound { host: PreviewHost },
}

/// SHA-256 of the full argon2 PHC hash, truncated to 16 hex chars. Folded
/// into the cookie payload so rotating the password (which changes the
/// argon2 hash) immediately invalidates every live cookie for that sandbox.
pub fn hash_fingerprint(hash: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(hash.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8]) // 16 hex chars
}

/// Encode a fresh preview cookie value: `subject|fingerprint|expires_unix`,
/// then encrypted+authenticated by `CookieCrypto` (AES-256-GCM). The subject
/// is the sandbox `sbx_<hex>` public_id and must not contain `|`.
pub fn encode_preview_cookie_subject(
    crypto: &CookieCrypto,
    subject: &str,
    password_hash: &str,
    now: SystemTime,
) -> Option<String> {
    if subject.contains('|') {
        return None;
    }
    let exp = now
        .checked_add(PREVIEW_COOKIE_TTL)?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    let payload = format!("{}|{}|{}", subject, hash_fingerprint(password_hash), exp);
    crypto.encrypt(&payload).ok()
}

/// Validate a previously issued preview cookie. Returns true iff the cookie
/// decrypts cleanly, names the expected `subject`, was minted against the
/// current password hash (so rotation revokes), and has not expired.
pub fn verify_preview_cookie_subject(
    crypto: &CookieCrypto,
    cookie_value: &str,
    subject: &str,
    password_hash: &str,
    now: SystemTime,
) -> bool {
    let Ok(plain) = crypto.decrypt(cookie_value) else {
        return false;
    };
    let parts: Vec<&str> = plain.splitn(3, '|').collect();
    if parts.len() != 3 {
        return false;
    }
    if parts[0] != subject {
        return false;
    }
    if parts[1] != hash_fingerprint(password_hash) {
        return false;
    }
    let Ok(exp) = parts[2].parse::<u64>() else {
        return false;
    };
    let Ok(now_secs) = now.duration_since(UNIX_EPOCH) else {
        return false;
    };
    now_secs.as_secs() <= exp
}

/// Pull a single cookie value out of a `Cookie:` header by name.
pub fn extract_cookie<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(v);
            }
        }
    }
    None
}

/// Build the `Set-Cookie` header for a sandbox preview cookie.
pub fn build_set_cookie_sandbox(
    public_id_suffix: &str,
    cookie_value: &str,
    preview_domain: &str,
    secure: bool,
) -> String {
    // `Secure` is only emitted when the request came in over TLS. Browsers
    // silently drop `Secure` cookies sent over plain HTTP, which would
    // completely break preview auth for self-hosted setups running without
    // TLS (e.g. `http://host.docker.internal:8080`).
    let domain = preview_domain.trim_start_matches("*.");
    let secure_attr = if secure { "; Secure" } else { "" };
    let ttl = PREVIEW_COOKIE_TTL.as_secs();
    format!(
        "{PREVIEW_SANDBOX_COOKIE_PREFIX}{public_id_suffix}={cookie_value}; Domain=.{domain}; Path=/; HttpOnly{secure_attr}; SameSite=Lax; Max-Age={ttl}"
    )
}

#[derive(Debug, Default)]
struct FailureState {
    count: u32,
    window_start: Option<Instant>,
}

/// Hard cap on distinct (ip, sandbox_hex) pairs tracked concurrently.
/// An attacker spraying unique IPs/hex labels can no longer grow this map
/// without bound — at the cap we sweep expired entries, and if that fails
/// to free space we drop the oldest entry.
const MAX_TRACKED_ENTRIES: usize = 65_536;

/// In-memory rate limiter for preview auth failures.
#[derive(Debug, Default)]
pub struct PreviewAuthLimiter {
    failures: DashMap<(IpAddr, String), FailureState>,
}

impl PreviewAuthLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the (ip, sandbox_hex) pair is currently rate-limited.
    pub fn is_blocked(&self, ip: IpAddr, hex: &str) -> bool {
        let entry = self.failures.get(&(ip, hex.to_string()));
        let Some(state) = entry else { return false };
        let Some(start) = state.window_start else {
            return false;
        };
        if start.elapsed() > RATE_LIMIT_WINDOW {
            return false;
        }
        state.count >= MAX_FAILURES
    }

    pub fn record_failure(&self, ip: IpAddr, hex: &str) {
        let key = (ip, hex.to_string());
        // Cap enforcement: opportunistically evict before insert so the map
        // cannot be weaponized as an unbounded memory sink.
        if !self.failures.contains_key(&key) && self.failures.len() >= MAX_TRACKED_ENTRIES {
            self.evict_expired();
            if self.failures.len() >= MAX_TRACKED_ENTRIES {
                if let Some(victim) = self
                    .failures
                    .iter()
                    .min_by_key(|e| e.value().window_start)
                    .map(|e| e.key().clone())
                {
                    self.failures.remove(&victim);
                }
            }
        }

        let mut entry = self.failures.entry(key).or_default();
        let now = Instant::now();
        match entry.window_start {
            Some(start) if start.elapsed() <= RATE_LIMIT_WINDOW => {
                entry.count = entry.count.saturating_add(1);
            }
            _ => {
                entry.window_start = Some(now);
                entry.count = 1;
            }
        }
    }

    pub fn record_success(&self, ip: IpAddr, hex: &str) {
        self.failures.remove(&(ip, hex.to_string()));
    }

    /// Drop all entries whose window has expired. O(n), but only called when
    /// we hit the cap — amortized cost is negligible under normal load.
    fn evict_expired(&self) {
        self.failures.retain(|_, state| match state.window_start {
            Some(start) => start.elapsed() <= RATE_LIMIT_WINDOW,
            None => false,
        });
    }
}

/// Verify a plaintext password against an argon2 PHC hash.
pub fn verify_argon2(plaintext: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        warn!("preview-auth: stored password hash is malformed");
        return false;
    };
    Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok()
}

/// Outcome of looking up a sandbox for preview auth. Distinguishes the
/// three states that drive routing: doesn't exist, exists with no
/// password (URL-only), exists with a configured password.
#[derive(Debug)]
pub enum PreviewSandboxLookup {
    /// Sandbox exists and is live, with a password configured. The
    /// gateway should require a valid cookie or redirect to login.
    Protected { password_hash: String },
    /// Sandbox exists and is live but has no password — the unguessable
    /// hex public_id is the only gate. Forward traffic directly.
    Open,
    /// Sandbox does not exist (or is destroyed).
    NotFound,
}

/// Load a sandbox's existence and preview password hash. The result drives
/// three-way routing in `check_preview_auth`: missing → 404, unprotected →
/// Allow, protected → require cookie/login.
///
/// `"stopped"` (paused) sandboxes still resolve so the gateway can surface
/// a 502 from the dev-server side rather than a 404 from the auth side.
pub async fn lookup_sandbox(
    db: &Arc<DbConnection>,
    public_id_suffix: &str,
) -> PreviewSandboxLookup {
    use sea_orm::{ColumnTrait, QueryFilter};
    use temps_entities::sandboxes;

    // The DB stores the full `sbx_<hex>` public_id; rebuild it here.
    let full_public_id = format!("sbx_{}", public_id_suffix);
    match sandboxes::Entity::find()
        .filter(sandboxes::Column::PublicId.eq(full_public_id.clone()))
        .one(db.as_ref())
        .await
    {
        Ok(Some(row)) if row.status == "destroyed" => PreviewSandboxLookup::NotFound,
        Ok(Some(row)) => match row.preview_password_hash {
            Some(hash) => PreviewSandboxLookup::Protected {
                password_hash: hash,
            },
            None => PreviewSandboxLookup::Open,
        },
        Ok(None) => {
            debug!(
                public_id = %full_public_id,
                "preview-auth: sandbox not found"
            );
            PreviewSandboxLookup::NotFound
        }
        Err(e) => {
            warn!(
                public_id = %full_public_id,
                error = %e,
                "preview-auth: failed to load sandbox"
            );
            PreviewSandboxLookup::NotFound
        }
    }
}

/// Run the preview auth check for a parsed preview host (cookie-only).
///
/// Order of operations:
/// 1. Look up the sandbox row.
/// 2. If missing → NotFound; if unprotected → Allow.
/// 3. Rate-limit gate.
/// 4. If a valid `temps_preview_sbx_<hex>` cookie is present → Allow.
/// 5. Otherwise → LoginRequired (caller issues a 303 to the login form).
///
/// Note: this does NOT record a rate-limit failure for missing cookies —
/// only the login POST records failures (via [`PreviewAuthLimiter::record_failure`])
/// so GETs without a cookie don't lock users out after a browser refresh.
pub async fn check_preview_auth(
    db: &Arc<DbConnection>,
    crypto: &CookieCrypto,
    limiter: &PreviewAuthLimiter,
    host: PreviewHost,
    client_ip: IpAddr,
    cookie_header: Option<&str>,
) -> PreviewAuthOutcome {
    let stored_hash = match lookup_sandbox(db, &host.hex).await {
        PreviewSandboxLookup::NotFound => {
            return PreviewAuthOutcome::NotFound { host };
        }
        PreviewSandboxLookup::Open => {
            return PreviewAuthOutcome::Allow { host };
        }
        PreviewSandboxLookup::Protected { password_hash } => {
            if limiter.is_blocked(client_ip, &host.hex) {
                return PreviewAuthOutcome::RateLimited { host };
            }
            password_hash
        }
    };

    let subject = format!("sbx_{}", host.hex);
    let cookie_name = format!("{}{}", PREVIEW_SANDBOX_COOKIE_PREFIX, host.hex);

    if let Some(header) = cookie_header {
        if let Some(value) = extract_cookie(header, &cookie_name) {
            if verify_preview_cookie_subject(
                crypto,
                value,
                &subject,
                &stored_hash,
                SystemTime::now(),
            ) {
                limiter.record_success(client_ip, &host.hex);
                return PreviewAuthOutcome::Allow { host };
            }
        }
    }

    PreviewAuthOutcome::LoginRequired { host }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_fingerprint_differs_for_different_hashes() {
        let hash_a = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let hash_b = "$argon2id$v=19$m=19456,t=2,p=1$BBBBBBBBBBBBBBBBBBBBBB$BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        assert_ne!(hash_fingerprint(hash_a), hash_fingerprint(hash_b));
        assert_eq!(hash_fingerprint(hash_a), hash_fingerprint(hash_a));
        assert_eq!(hash_fingerprint(hash_a).len(), 16);
    }

    #[test]
    fn parse_preview_host_accepts_sandbox_hex() {
        let h = parse_preview_host("ws-7702c56bfb804b49-3000.localho.st", "localho.st").unwrap();
        assert_eq!(h.hex, "7702c56bfb804b49");
        assert_eq!(h.port, 3000);
    }

    #[test]
    fn parse_preview_host_lowercases_sandbox_hex() {
        let h = parse_preview_host("ws-7702C56BFB804B49-3000.localho.st", "localho.st").unwrap();
        assert_eq!(h.hex, "7702c56bfb804b49");
    }

    #[test]
    fn parse_preview_host_strips_wildcard_prefix() {
        let h = parse_preview_host(
            "ws-7702c56bfb804b49-8080.preview.example.com",
            "*.preview.example.com",
        )
        .unwrap();
        assert_eq!(h.hex, "7702c56bfb804b49");
        assert_eq!(h.port, 8080);
    }

    #[test]
    fn parse_preview_host_strips_request_port() {
        let h =
            parse_preview_host("ws-7702c56bfb804b49-3000.localho.st:8443", "localho.st").unwrap();
        assert_eq!(h.hex, "7702c56bfb804b49");
        assert_eq!(h.port, 3000);
    }

    #[test]
    fn parse_preview_host_rejects_wrong_hex_length() {
        // 15 chars, 17 chars: must be exactly 16.
        assert!(parse_preview_host("ws-7702c56bfb804b4-3000.localho.st", "localho.st").is_none());
        assert!(parse_preview_host("ws-7702c56bfb804b49a-3000.localho.st", "localho.st").is_none());
    }

    #[test]
    fn parse_preview_host_rejects_non_hex_mixed_label() {
        assert!(parse_preview_host("ws-gggggggggggggggg-3000.localho.st", "localho.st").is_none());
    }

    #[test]
    fn parse_preview_host_rejects_digit_only_label() {
        // Legacy workspace URLs used pure-digit labels; the sandbox-only
        // parser must reject them. (16 digits would also fail the hex check
        // — digits are valid hex — so test with non-16-length to be explicit.)
        assert!(parse_preview_host("ws-14-3000.localho.st", "localho.st").is_none());
    }

    #[test]
    fn parse_preview_host_rejects_wrong_domain() {
        assert!(parse_preview_host("ws-7702c56bfb804b49-3000.example.org", "localho.st").is_none());
    }

    #[test]
    fn parse_preview_host_rejects_missing_prefix() {
        assert!(parse_preview_host("foo-7702c56bfb804b49-3000.localho.st", "localho.st").is_none());
    }

    #[test]
    fn parse_preview_host_rejects_zero_port() {
        assert!(parse_preview_host("ws-7702c56bfb804b49-0.localho.st", "localho.st").is_none());
    }

    #[test]
    fn rate_limiter_trips_after_max_failures() {
        let limiter = PreviewAuthLimiter::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(!limiter.is_blocked(ip, "abc"));
            limiter.record_failure(ip, "abc");
        }
        assert!(limiter.is_blocked(ip, "abc"));
    }

    #[test]
    fn rate_limiter_resets_on_success() {
        let limiter = PreviewAuthLimiter::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure(ip, "abc");
        }
        assert!(limiter.is_blocked(ip, "abc"));
        limiter.record_success(ip, "abc");
        assert!(!limiter.is_blocked(ip, "abc"));
    }

    #[test]
    fn rate_limiter_is_bounded_under_flood() {
        // Spray far more unique (ip, hex) pairs than the cap allows.
        let limiter = PreviewAuthLimiter::new();
        let attacker_count = MAX_TRACKED_ENTRIES + 5_000;
        for i in 0..attacker_count {
            let octet_a = ((i >> 16) & 0xff) as u8;
            let octet_b = ((i >> 8) & 0xff) as u8;
            let octet_c = (i & 0xff) as u8;
            let ip: IpAddr = format!("10.{}.{}.{}", octet_a, octet_b, octet_c)
                .parse()
                .unwrap();
            limiter.record_failure(ip, &format!("hex{:08x}", i % 1024));
        }
        assert!(
            limiter.failures.len() <= MAX_TRACKED_ENTRIES,
            "limiter grew beyond cap: {}",
            limiter.failures.len()
        );
    }
}
