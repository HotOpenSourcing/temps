//! Error types for the plugin SDK.

use thiserror::Error;

/// Errors that can occur during plugin operation.
#[derive(Error, Debug)]
pub enum PluginSdkError {
    #[error("Failed to parse CLI arguments: {message}")]
    Args { message: String },

    #[error("Failed to bind Unix socket at '{path}': {reason}")]
    SocketBind { path: String, reason: String },

    #[error("Handshake failed for plugin '{plugin_name}': {reason}")]
    Handshake { plugin_name: String, reason: String },

    #[error("Plugin initialization failed for '{plugin_name}': {reason}")]
    Initialization { plugin_name: String, reason: String },

    #[error("Server error: {0}")]
    Server(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Platform channel closed unexpectedly")]
    ChannelClosed,

    #[error("Platform returned error ({code}): {message}")]
    PlatformError { code: String, message: String },

    #[error("Failed to deserialize platform response: {reason}")]
    Deserialization { reason: String },
}
