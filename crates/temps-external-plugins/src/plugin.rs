//! TempsPlugin implementation for external plugin management.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::Request;
use axum::response::IntoResponse;
use axum::Router;
use temps_core::plugin::{
    PluginContext, PluginError, PluginRoutes, ServiceRegistrationContext, TempsPlugin,
};
use temps_core::JobQueue;
use tokio::sync::RwLock;
use tower::ServiceExt;
use utoipa::openapi::OpenApi;
use utoipa::OpenApi as OpenApiTrait;

use crate::handler::{self, ExternalPluginsApiDoc, ExternalPluginsAppState};
use crate::manager::ExternalPluginConfig;
use crate::service::ExternalPluginsService;

/// Swappable proxy router holder.
///
/// The `Arc<RwLock<Router>>` is read on every proxied request and written to
/// during [`ExternalPluginsService::reload_plugins`]. This allows hot-swapping
/// plugin proxy routes without restarting the server.
pub(crate) struct DynamicPluginRouter {
    pub inner: Arc<RwLock<Router>>,
}

/// External plugins plugin — discovers, manages, and proxies standalone
/// binary plugins following the TempsPlugin lifecycle.
pub struct ExternalPluginsPlugin {
    config: ExternalPluginConfig,
}

impl ExternalPluginsPlugin {
    pub fn new(config: ExternalPluginConfig) -> Self {
        Self { config }
    }
}

impl TempsPlugin for ExternalPluginsPlugin {
    fn name(&self) -> &'static str {
        "external-plugins"
    }

    fn register_services<'a>(
        &'a self,
        context: &'a ServiceRegistrationContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>> {
        Box::pin(async move {
            // Try to get the JobQueue from the service registry (registered by queue plugin).
            // This is optional — event delivery is disabled if no queue is available.
            let queue: Option<Arc<dyn JobQueue>> = context.get_service::<dyn JobQueue>();

            // Get the database connection for the platform channel.
            let db = context.require_service::<sea_orm::DatabaseConnection>();

            // Create the service — this discovers and starts all external plugins,
            // and starts the event listener if plugins subscribe to events.
            let service =
                Arc::new(ExternalPluginsService::new(self.config.clone(), queue, db).await);

            // Get the swappable router reference. On reload, the service writes
            // a new Router into this Arc<RwLock<Router>> and subsequent requests
            // pick it up immediately.
            let dynamic_router = service.proxy_router();

            // Register the handler app state
            let app_state = Arc::new(ExternalPluginsAppState {
                service: service.clone(),
            });

            context.register_service(service);
            context.register_service(app_state);
            context.register_service(Arc::new(DynamicPluginRouter {
                inner: dynamic_router,
            }));

            tracing::debug!("External plugins services registered successfully");
            Ok(())
        })
    }

    fn configure_routes(&self, context: &PluginContext) -> Option<PluginRoutes> {
        let app_state = context.require_service::<ExternalPluginsAppState>();
        let dynamic = context.require_service::<DynamicPluginRouter>();

        // Build the listing + admin routes (/x/plugins, /x/plugins/reload)
        let listing_router = handler::configure_routes().with_state((*app_state).clone());

        // Create a dynamic routing layer that reads the swappable proxy router
        // on every request. When reload_plugins() swaps the inner Router, all
        // subsequent requests are routed to the new plugins.
        let router_ref = dynamic.inner.clone();
        let dynamic_proxy = Router::new().fallback(move |request: Request| {
            let router_ref = router_ref.clone();
            async move {
                let router = router_ref.read().await.clone();
                router.oneshot(request).await.into_response()
            }
        });

        let combined_router = listing_router.merge(dynamic_proxy);

        Some(PluginRoutes::new(combined_router))
    }

    fn openapi_schema(&self) -> Option<OpenApi> {
        Some(ExternalPluginsApiDoc::openapi())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_plugin_name() {
        let config = ExternalPluginConfig::new(
            PathBuf::from("/tmp/test"),
            "postgres://localhost/test".to_string(),
        );
        let plugin = ExternalPluginsPlugin::new(config);
        assert_eq!(plugin.name(), "external-plugins");
    }

    #[test]
    fn test_plugin_openapi_schema() {
        let config = ExternalPluginConfig::new(
            PathBuf::from("/tmp/test"),
            "postgres://localhost/test".to_string(),
        );
        let plugin = ExternalPluginsPlugin::new(config);
        let schema = plugin.openapi_schema();
        assert!(schema.is_some());
        let spec = schema.unwrap();
        assert!(spec.paths.paths.contains_key("/x/plugins"));
    }
}
