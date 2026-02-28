//! SEO Analyzer — an example external plugin for Temps.
//!
//! Crawls deployed sites, analyzes pages for technical SEO issues, and
//! generates actionable reports with scores. Demonstrates realistic use
//! of the plugin SDK: HTTP routes, SQLite persistence, background analysis,
//! and a React-based UI embedded at compile time.
//!
//! ## Features
//!
//! - **Site crawling**: Follows internal links with configurable depth
//! - **Technical SEO checks**: Title, meta description, headings, images,
//!   canonical URLs, Open Graph tags, robots directives, and more
//! - **Scoring**: Per-page and per-report aggregate scores (0–100)
//! - **Issue classification**: Critical / Warning / Info severity levels
//! - **SQLite persistence**: Reports survive plugin restarts
//! - **Configurable**: Max pages, crawl delay, user-agent, timeout — all
//!   adjustable via the settings API
//! - **React UI**: Full React + TypeScript frontend embedded at compile time
//!
//! ## API
//!
//! ```text
//! POST   /analyze              — Start a new analysis (body: { "url": "...", "max_pages": 100 })
//! GET    /reports               — List all reports
//! GET    /reports/{id}          — Get a single report with page-level details
//! GET    /reports/{id}/prompt   — Get report as LLM-friendly plain text (text/plain)
//! DELETE /reports/{id}          — Delete a report
//! GET    /settings              — Get plugin settings
//! PATCH  /settings              — Update plugin settings (partial)
//! GET    /ui/                   — Plugin UI (React SPA, served in Temps iframe)
//! GET    /ui/{*path}            — Plugin UI static assets
//! ```
//!
//! ## Development
//!
//! ```bash
//! # Run the React dev server (hot reload)
//! cd examples/example-plugin/web && bun install && bun run dev
//!
//! # Build the plugin binary (skips web build in debug mode)
//! cargo build -p temps-example-plugin
//!
//! # Build with embedded UI
//! FORCE_WEB_BUILD=1 cargo build -p temps-example-plugin
//! ```

mod crawl;
mod db;
mod handlers;
mod types;

use axum::routing::{get, post};
use include_dir::{include_dir, Dir};
use temps_plugin_sdk::prelude::*;

use crate::db::SeoStore;
use crate::handlers::AppState;

/// Embed the web/dist/ directory at compile time.
/// In debug mode without FORCE_WEB_BUILD, this contains a placeholder page.
static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

/// Access the embedded UI directory (used by handlers module).
pub fn ui_dist() -> &'static Dir<'static> {
    &UI_DIST
}

// ============================================================================
// Plugin Definition
// ============================================================================

#[derive(Default)]
struct SeoPlugin;

impl ExternalPlugin for SeoPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::builder("seo-analyzer", "0.1.0")
            .display_name("SEO Analyzer")
            .description(
                "Crawl deployed sites and generate technical SEO reports with actionable insights",
            )
            .requires_db(false)
            .nav(NavEntry {
                label: "SEO Reports".into(),
                icon: "search".into(),
                section: NavSection::Platform,
                path: "/seo-reports".into(),
                order: 42,
            })
            .build()
    }

    fn router(&self, ctx: PluginContext) -> axum::Router {
        // router() is sync but called inside the tokio runtime during startup.
        // Use block_in_place to run the async SQLite init without deadlocking.
        let store = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(SeoStore::open(ctx.data_dir()))
        })
        .expect("Failed to open SEO store");

        // Build HTTP client using plugin settings defaults.
        // Settings are loaded lazily per-request for the crawl itself,
        // but the client timeout is set here.
        let http_client = reqwest::Client::builder()
            .user_agent(types::PluginSettings::DEFAULT_USER_AGENT)
            .timeout(std::time::Duration::from_secs(
                types::PluginSettings::DEFAULT_REQUEST_TIMEOUT_SECS,
            ))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_default();

        let state = AppState { store, http_client };

        axum::Router::new()
            // API routes
            .route("/analyze", post(handlers::start_analysis))
            .route("/reports", get(handlers::list_reports))
            .route(
                "/reports/{id}",
                get(handlers::get_report).delete(handlers::delete_report),
            )
            .route("/reports/{id}/prompt", get(handlers::get_report_prompt))
            .route(
                "/settings",
                get(handlers::get_settings).patch(handlers::update_settings),
            )
            // UI routes — serve the embedded React SPA
            .route("/ui", get(handlers::redirect_to_ui))
            .route("/ui/", get(handlers::serve_ui_index))
            .route("/ui/{*path}", get(handlers::serve_ui_asset))
            .with_state(state)
    }

    fn on_start(&self, ctx: &PluginContext) -> Result<(), PluginSdkError> {
        tracing::info!(
            plugin = ctx.plugin_name(),
            data_dir = %ctx.data_dir().display(),
            "SEO Analyzer plugin started"
        );
        Ok(())
    }
}

temps_plugin_sdk::main!(SeoPlugin);
