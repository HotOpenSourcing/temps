#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub address: String,
    pub console_address: String,
    pub tls_address: Option<String>,
    pub preview_domain: Option<String>, // e.g., "preview.example.com"
    /// When true, HTTP requests are served directly without redirecting to HTTPS.
    /// Useful for local development without TLS certificates.
    pub disable_https_redirect: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:8080".to_string(),
            console_address: "127.0.0.1:3000".to_string(),
            tls_address: None,
            preview_domain: Some("localhost".to_string()), // Default for local development
            disable_https_redirect: false,
        }
    }
}
