//! Plugin runtime context providing access to platform services.

use crate::client::TempsClient;

/// Runtime context provided to external plugins.
///
/// This is the plugin's window into the Temps ecosystem.
/// Platform data is accessed exclusively through the [`TempsClient`]
/// returned by [`temps()`](Self::temps) — the plugin never has
/// direct database access.
#[derive(Clone)]
pub struct PluginContext {
    /// Typed client for querying the Temps platform
    temps_client: TempsClient,
    /// The plugin's name (from manifest)
    plugin_name: String,
    /// Directory for plugin-specific data files
    data_dir: std::path::PathBuf,
    /// HMAC secret for validating requests from Temps
    auth_secret: String,
}

impl PluginContext {
    /// Create a new plugin context.
    pub fn new(
        temps_client: TempsClient,
        plugin_name: String,
        data_dir: std::path::PathBuf,
        auth_secret: String,
    ) -> Self {
        Self {
            temps_client,
            plugin_name,
            data_dir,
            auth_secret,
        }
    }

    /// Get a client for querying the Temps platform.
    ///
    /// The client provides typed, read-only access to projects,
    /// environments, deployments, and other platform data.
    ///
    /// # Example
    /// ```rust,no_run
    /// use temps_plugin_sdk::prelude::*;
    ///
    /// async fn list_projects(ctx: &PluginContext) {
    ///     let projects = ctx.temps().list_projects().await.unwrap();
    ///     for p in projects {
    ///         println!("{}: {}", p.id, p.name);
    ///     }
    /// }
    /// ```
    pub fn temps(&self) -> &TempsClient {
        &self.temps_client
    }

    /// Get the plugin's name.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Get the plugin's data directory.
    ///
    /// Use this for storing plugin-specific files (caches, state, etc.).
    /// The directory is guaranteed to exist when the plugin starts.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Get the HMAC auth secret for request validation.
    ///
    /// Temps signs proxied requests with this secret.
    /// Use this to verify that incoming requests actually come from Temps.
    pub fn auth_secret(&self) -> &str {
        &self.auth_secret
    }
}
