//! Typed client for querying the Temps platform over the channel.
//!
//! [`TempsClient`] is available on [`PluginContext`] and provides
//! structured, read-only access to platform data (projects, environments,
//! deployments).  All calls go through the WebSocket channel that Temps
//! opens to the plugin — the plugin never touches the database directly.
//!
//! # Example
//!
//! ```rust,no_run
//! use temps_plugin_sdk::prelude::*;
//!
//! async fn example(ctx: &PluginContext) {
//!     let client = ctx.temps();
//!
//!     let project = client.get_project(42).await.unwrap();
//!     println!("Project: {}", project.name);
//!
//!     let envs = client.list_environments(42).await.unwrap();
//!     for env in envs {
//!         println!("  env: {} ({})", env.name, env.slug);
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::stream::StreamExt;
use futures::SinkExt;
use temps_core::external_plugin::channel::*;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, warn};

use crate::error::PluginSdkError;

/// A typed, async client for querying the Temps platform.
///
/// Obtained via [`PluginContext::temps()`].  All methods are async and
/// return `Result<T, PluginSdkError>`.
#[derive(Clone)]
pub struct TempsClient {
    inner: Arc<TempsClientInner>,
}

struct TempsClientInner {
    /// Sender for outgoing channel messages (requests).
    tx: mpsc::UnboundedSender<ChannelMessage>,
    /// Pending request waiters, keyed by request id.
    pending: RwLock<HashMap<u64, oneshot::Sender<ChannelResponse>>>,
    /// Monotonic request ID counter.
    next_id: AtomicU64,
}

/// Sender half used by the runtime to forward events to the plugin.
pub type EventSender = mpsc::UnboundedSender<temps_core::external_plugin::PluginEvent>;
/// Receiver half consumed by the runtime to deliver events.
pub type EventReceiver = mpsc::UnboundedReceiver<temps_core::external_plugin::PluginEvent>;

impl TempsClient {
    /// Create a new client wired to a WebSocket connection.
    ///
    /// This is called by the plugin runtime after accepting the platform's
    /// incoming WebSocket on `/_temps/channel`.  It spawns a background task
    /// that reads responses/events and dispatches them.
    ///
    /// Returns `(client, event_receiver)`.  The event receiver yields
    /// platform events pushed over the channel.
    pub(crate) fn from_ws<S>(ws_stream: S) -> (Self, EventReceiver)
    where
        S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
            + Send
            + Unpin
            + 'static,
    {
        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Outgoing message sender (requests from client methods)
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<ChannelMessage>();

        let inner = Arc::new(TempsClientInner {
            tx: msg_tx.clone(),
            pending: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });

        // Event channel — events received from the platform are forwarded here
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Writer task: forwards queued messages to the WebSocket
        tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        error!("Failed to serialize channel request: {}", e);
                        continue;
                    }
                };
                if let Err(e) = ws_tx.send(Message::Text(json.into())).await {
                    debug!("Channel WebSocket write error: {}", e);
                    break;
                }
            }
        });

        // Reader task: dispatches incoming responses and events
        let reader_inner = inner.clone();
        tokio::spawn(async move {
            while let Some(frame) = ws_rx.next().await {
                let text = match frame {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) => {
                        debug!("Platform closed the channel");
                        break;
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                    Ok(_) => continue,
                    Err(e) => {
                        debug!("Channel read error: {}", e);
                        break;
                    }
                };

                let msg: ChannelMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Invalid channel message from platform: {}", e);
                        continue;
                    }
                };

                match msg {
                    ChannelMessage::Response(resp) => {
                        let mut pending = reader_inner.pending.write().await;
                        if let Some(waiter) = pending.remove(&resp.id) {
                            let _ = waiter.send(resp);
                        } else {
                            warn!("Received response for unknown request id {}", resp.id);
                        }
                    }
                    ChannelMessage::Event(evt) => {
                        let _ = event_tx.send(evt.event);
                    }
                    ChannelMessage::Request(_) => {
                        warn!("Unexpected Request message from platform (ignoring)");
                    }
                }
            }

            // Channel closed — wake all pending waiters with errors
            let mut pending = reader_inner.pending.write().await;
            for (id, waiter) in pending.drain() {
                let _ = waiter.send(ChannelResponse::err(
                    id,
                    ChannelErrorCode::Internal,
                    "Channel closed",
                ));
            }
        });

        let client = Self { inner };
        (client, event_rx)
    }

    // ── Low-level request/response ─────────────────────────────────────

    /// Send a request and wait for the response.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PluginSdkError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = oneshot::channel();
        self.inner.pending.write().await.insert(id, tx);

        let msg = ChannelMessage::Request(ChannelRequest {
            id,
            method: method.to_string(),
            params,
        });

        self.inner
            .tx
            .send(msg)
            .map_err(|_| PluginSdkError::ChannelClosed)?;

        let resp = rx.await.map_err(|_| PluginSdkError::ChannelClosed)?;

        if let Some(err) = resp.error {
            return Err(PluginSdkError::PlatformError {
                code: format!("{:?}", err.code),
                message: err.message,
            });
        }

        resp.result.ok_or(PluginSdkError::ChannelClosed)
    }

    // ── Typed query methods ────────────────────────────────────────────

    /// Get a project by ID.
    pub async fn get_project(&self, project_id: i32) -> Result<ProjectInfo, PluginSdkError> {
        let value = self
            .request(
                "get_project",
                serde_json::json!({ "project_id": project_id }),
            )
            .await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }

    /// List all (non-deleted) projects.
    pub async fn list_projects(&self) -> Result<Vec<ProjectInfo>, PluginSdkError> {
        let value = self.request("list_projects", serde_json::json!({})).await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }

    /// Get an environment by ID.
    pub async fn get_environment(
        &self,
        environment_id: i32,
    ) -> Result<EnvironmentInfo, PluginSdkError> {
        let value = self
            .request(
                "get_environment",
                serde_json::json!({ "environment_id": environment_id }),
            )
            .await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }

    /// List environments for a project.
    pub async fn list_environments(
        &self,
        project_id: i32,
    ) -> Result<Vec<EnvironmentInfo>, PluginSdkError> {
        let value = self
            .request(
                "list_environments",
                serde_json::json!({ "project_id": project_id }),
            )
            .await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }

    /// Get a deployment by ID.
    pub async fn get_deployment(
        &self,
        deployment_id: i32,
    ) -> Result<DeploymentInfo, PluginSdkError> {
        let value = self
            .request(
                "get_deployment",
                serde_json::json!({ "deployment_id": deployment_id }),
            )
            .await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }

    /// Get the most recent deployment for a project, optionally filtered
    /// by environment.
    pub async fn get_last_deployment(
        &self,
        project_id: i32,
        environment_id: Option<i32>,
    ) -> Result<DeploymentInfo, PluginSdkError> {
        let mut params = serde_json::json!({ "project_id": project_id });
        if let Some(env_id) = environment_id {
            params["environment_id"] = serde_json::json!(env_id);
        }
        let value = self.request("get_last_deployment", params).await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }

    /// List deployments for a project, optionally filtered by environment.
    pub async fn list_deployments(
        &self,
        project_id: i32,
        environment_id: Option<i32>,
        limit: Option<u64>,
    ) -> Result<Vec<DeploymentInfo>, PluginSdkError> {
        let mut params = serde_json::json!({ "project_id": project_id });
        if let Some(env_id) = environment_id {
            params["environment_id"] = serde_json::json!(env_id);
        }
        if let Some(limit) = limit {
            params["limit"] = serde_json::json!(limit);
        }
        let value = self.request("list_deployments", params).await?;
        serde_json::from_value(value).map_err(|e| PluginSdkError::Deserialization {
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temps_client_is_clone() {
        // Verify TempsClient is Clone (needed for PluginContext)
        fn assert_clone<T: Clone>() {}
        assert_clone::<TempsClient>();
    }
}
