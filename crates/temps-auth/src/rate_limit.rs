//! IP-based rate limiting middleware for authentication endpoints.
//!
//! Provides a simple sliding-window rate limiter to prevent brute force attacks
//! on login, password reset, magic link, and MFA verification endpoints.

use axum::{
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

/// Configuration for the auth rate limiter.
#[derive(Debug, Clone)]
pub struct AuthRateLimitConfig {
    /// Maximum requests allowed within the window.
    pub max_requests: u32,
    /// Time window for counting requests.
    pub window: Duration,
}

impl Default for AuthRateLimitConfig {
    fn default() -> Self {
        Self {
            // 10 auth attempts per minute per IP
            max_requests: 10,
            window: Duration::from_secs(60),
        }
    }
}

/// Entry tracking requests from a single IP.
#[derive(Debug)]
struct RateLimitEntry {
    /// Timestamps of recent requests within the window.
    timestamps: Vec<Instant>,
}

/// Shared state for the rate limiter.
#[derive(Debug, Clone)]
pub struct AuthRateLimiter {
    entries: Arc<Mutex<HashMap<String, RateLimitEntry>>>,
    config: AuthRateLimitConfig,
}

impl AuthRateLimiter {
    pub fn new(config: AuthRateLimitConfig) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Check if a request from the given IP should be allowed.
    /// Returns Ok(()) if allowed, Err(()) if rate limited.
    async fn check(&self, ip: &str) -> Result<(), ()> {
        let now = Instant::now();
        let window_start = now - self.config.window;

        let mut entries = self.entries.lock().await;

        let entry = entries.entry(ip.to_string()).or_insert(RateLimitEntry {
            timestamps: Vec::new(),
        });

        // Remove timestamps outside the window
        entry.timestamps.retain(|t| *t > window_start);

        if entry.timestamps.len() >= self.config.max_requests as usize {
            return Err(());
        }

        entry.timestamps.push(now);

        // Periodic cleanup: if map is getting large, remove stale entries
        if entries.len() > 10_000 {
            entries.retain(|_, v| v.timestamps.last().is_some_and(|t| *t > window_start));
        }

        Ok(())
    }
}

/// Axum middleware function for rate limiting auth endpoints.
///
/// Extracts the client IP from `X-Forwarded-For` header, `X-Real-IP` header,
/// or falls back to "unknown".
pub async fn auth_rate_limit_middleware(
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Extract rate limiter from request extensions
    let limiter = request.extensions().get::<AuthRateLimiter>().cloned();

    let limiter = match limiter {
        Some(l) => l,
        None => {
            // No rate limiter configured, pass through
            return next.run(request).await;
        }
    };

    // Extract client IP from headers (set by reverse proxy)
    let ip = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            request
                .headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    match limiter.check(&ip).await {
        Ok(()) => next.run(request).await,
        Err(()) => {
            warn!("Rate limit exceeded for IP {} on auth endpoint", ip);
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "60")],
                "Too many requests. Please try again later.",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limiter_allows_within_limit() {
        let limiter = AuthRateLimiter::new(AuthRateLimitConfig {
            max_requests: 5,
            window: Duration::from_secs(60),
        });

        for _ in 0..5 {
            assert!(limiter.check("1.2.3.4").await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_over_limit() {
        let limiter = AuthRateLimiter::new(AuthRateLimitConfig {
            max_requests: 3,
            window: Duration::from_secs(60),
        });

        // First 3 should succeed
        assert!(limiter.check("1.2.3.4").await.is_ok());
        assert!(limiter.check("1.2.3.4").await.is_ok());
        assert!(limiter.check("1.2.3.4").await.is_ok());

        // 4th should be blocked
        assert!(limiter.check("1.2.3.4").await.is_err());
    }

    #[tokio::test]
    async fn test_rate_limiter_different_ips_independent() {
        let limiter = AuthRateLimiter::new(AuthRateLimitConfig {
            max_requests: 2,
            window: Duration::from_secs(60),
        });

        // IP A fills its quota
        assert!(limiter.check("1.1.1.1").await.is_ok());
        assert!(limiter.check("1.1.1.1").await.is_ok());
        assert!(limiter.check("1.1.1.1").await.is_err());

        // IP B should still have its own quota
        assert!(limiter.check("2.2.2.2").await.is_ok());
        assert!(limiter.check("2.2.2.2").await.is_ok());
        assert!(limiter.check("2.2.2.2").await.is_err());
    }

    #[tokio::test]
    async fn test_rate_limiter_window_expiry() {
        let limiter = AuthRateLimiter::new(AuthRateLimitConfig {
            max_requests: 2,
            window: Duration::from_millis(50), // Very short window for testing
        });

        assert!(limiter.check("1.2.3.4").await.is_ok());
        assert!(limiter.check("1.2.3.4").await.is_ok());
        assert!(limiter.check("1.2.3.4").await.is_err());

        // Wait for window to expire
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Should be allowed again
        assert!(limiter.check("1.2.3.4").await.is_ok());
    }

    #[tokio::test]
    async fn test_rate_limiter_brute_force_simulation() {
        let limiter = AuthRateLimiter::new(AuthRateLimitConfig {
            max_requests: 10,
            window: Duration::from_secs(60),
        });

        let attacker_ip = "10.0.0.1";

        // Simulate 10 rapid login attempts (allowed)
        for i in 0..10 {
            assert!(
                limiter.check(attacker_ip).await.is_ok(),
                "Request {} should be allowed",
                i + 1
            );
        }

        // 11th attempt should be blocked
        assert!(
            limiter.check(attacker_ip).await.is_err(),
            "11th request must be blocked to prevent brute force"
        );

        // But a legitimate user from a different IP should not be affected
        assert!(
            limiter.check("8.8.8.8").await.is_ok(),
            "Different IP should not be rate limited"
        );
    }
}
