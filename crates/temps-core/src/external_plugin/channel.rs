//! Platform channel protocol types.
//!
//! Defines the bidirectional JSON message protocol used between Temps and
//! external plugins over a WebSocket connection.  Temps initiates the
//! connection to the plugin's `/_temps/channel` endpoint after the handshake
//! completes.
//!
//! ## Message flow
//!
//! ```text
//! Plugin (client role)                Temps (server role)
//!     │                                    │
//!     │─── Request { id, method, params } ─>│  plugin asks for data
//!     │<── Response { id, result/error } ───│  platform responds
//!     │                                    │
//!     │<── Event { event }  ───────────────│  platform pushes event
//!     │                                    │
//! ```
//!
//! Plugins send [`ChannelMessage::Request`] and receive
//! [`ChannelMessage::Response`] or [`ChannelMessage::Event`].

use serde::{Deserialize, Serialize};

// ── Wire messages ──────────────────────────────────────────────────────

/// Top-level envelope for all messages on the channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelMessage {
    /// Plugin → Platform: request data.
    Request(ChannelRequest),
    /// Platform → Plugin: response to a request.
    Response(ChannelResponse),
    /// Platform → Plugin: pushed event (fire-and-forget).
    Event(ChannelEvent),
}

/// A request from the plugin to the platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRequest {
    /// Caller-assigned correlation ID (echoed in the response).
    pub id: u64,
    /// Method name, e.g. `"get_project"`, `"list_environments"`.
    pub method: String,
    /// Method-specific parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// The platform's response to a [`ChannelRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelResponse {
    /// Correlation ID from the original request.
    pub id: u64,
    /// On success, the result payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// On failure, a structured error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ChannelError>,
}

/// A platform-pushed event (replaces POST to `/_events`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEvent {
    pub event: super::PluginEvent,
}

/// Structured error returned inside a [`ChannelResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelError {
    /// Machine-readable error code.
    pub code: ChannelErrorCode,
    /// Human-readable description.
    pub message: String,
}

/// Error codes for the channel protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelErrorCode {
    /// The requested method does not exist.
    MethodNotFound,
    /// The parameters are invalid or missing required fields.
    InvalidParams,
    /// The plugin does not have permission for this operation.
    PermissionDenied,
    /// The requested resource was not found.
    NotFound,
    /// An internal platform error occurred.
    Internal,
}

// ── Convenience constructors ───────────────────────────────────────────

impl ChannelResponse {
    /// Build a successful response.
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response.
    pub fn err(id: u64, code: ChannelErrorCode, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(ChannelError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ── Well-known path ────────────────────────────────────────────────────

/// WebSocket endpoint path that plugins must serve for the platform channel.
pub const PLUGIN_CHANNEL_PATH: &str = "/_temps/channel";

// ── Response DTOs ──────────────────────────────────────────────────────
//
// These are the canonical types returned by platform channel methods.
// They are intentionally decoupled from the internal entity models and
// from the HTTP API response types — plugins get a stable, minimal
// contract that can evolve independently.

/// A project as returned by the platform channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: i32,
    pub name: String,
    pub slug: String,
    pub repo_name: String,
    pub repo_owner: String,
    pub main_branch: String,
    pub preset: String,
    pub source_type: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_deployment: Option<String>,
    pub enable_preview_environments: bool,
}

/// An environment as returned by the platform channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub id: i32,
    pub project_id: i32,
    pub name: String,
    pub slug: String,
    pub branch: Option<String>,
    pub is_preview: bool,
    pub current_deployment_id: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

/// A deployment as returned by the platform channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub id: i32,
    pub project_id: i32,
    pub environment_id: i32,
    pub state: String,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
    pub commit_author: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization_roundtrip() {
        let msg = ChannelMessage::Request(ChannelRequest {
            id: 1,
            method: "get_project".into(),
            params: serde_json::json!({ "project_id": 42 }),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let deser: ChannelMessage = serde_json::from_str(&json).unwrap();
        match deser {
            ChannelMessage::Request(req) => {
                assert_eq!(req.id, 1);
                assert_eq!(req.method, "get_project");
                assert_eq!(req.params["project_id"], 42);
            }
            _ => panic!("Expected Request variant"),
        }
    }

    #[test]
    fn test_response_ok_serialization() {
        let resp = ChannelResponse::ok(1, serde_json::json!({ "name": "test" }));
        let msg = ChannelMessage::Response(resp);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_response_err_serialization() {
        let resp = ChannelResponse::err(2, ChannelErrorCode::NotFound, "Project 99 not found");
        let msg = ChannelMessage::Response(resp);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"error\""));
        assert!(!json.contains("\"result\""));
        assert!(json.contains("not_found"));
    }

    #[test]
    fn test_event_serialization() {
        let event = super::super::PluginEvent {
            id: "evt-1".into(),
            event_type: "deployment.succeeded".into(),
            timestamp: chrono::Utc::now(),
            project_id: Some(7),
            data: serde_json::json!({}),
        };
        let msg = ChannelMessage::Event(ChannelEvent { event });
        let json = serde_json::to_string(&msg).unwrap();
        let deser: ChannelMessage = serde_json::from_str(&json).unwrap();
        match deser {
            ChannelMessage::Event(evt) => {
                assert_eq!(evt.event.event_type, "deployment.succeeded");
            }
            _ => panic!("Expected Event variant"),
        }
    }

    #[test]
    fn test_all_error_codes_serialize() {
        let codes = [
            ChannelErrorCode::MethodNotFound,
            ChannelErrorCode::InvalidParams,
            ChannelErrorCode::PermissionDenied,
            ChannelErrorCode::NotFound,
            ChannelErrorCode::Internal,
        ];
        for code in codes {
            let resp = ChannelResponse::err(0, code, "test");
            let json = serde_json::to_string(&resp).unwrap();
            let deser: ChannelResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(deser.error.unwrap().code, code);
        }
    }
}
