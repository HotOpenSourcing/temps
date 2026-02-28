//! HTTP handlers for the Lighthouse Performance Audit plugin.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use include_dir::Dir;

use crate::db::AuditStore;
use crate::lighthouse;
use crate::types::*;

// ============================================================================
// State
// ============================================================================

#[derive(Clone)]
pub struct AppState {
    pub store: AuditStore,
}

// ============================================================================
// UI Handlers
// ============================================================================

pub async fn redirect_to_ui() -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, "ui/")
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn serve_ui_index() -> Response {
    serve_embedded_file(crate::ui_dist(), "index.html")
}

pub async fn serve_ui_asset(Path(path): Path<String>) -> Response {
    let dist = crate::ui_dist();
    if dist.get_file(&path).is_some() {
        return serve_embedded_file(dist, &path);
    }
    serve_embedded_file(dist, "index.html")
}

fn serve_embedded_file(dist: &Dir<'static>, path: &str) -> Response {
    match dist.get_file(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();

            let cache = if path == "index.html" {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, cache)
                .body(Body::from(file.contents()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found"))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

// ============================================================================
// API Handlers
// ============================================================================

/// Start a manual Lighthouse audit for a URL.
pub async fn start_audit(
    State(state): State<AppState>,
    Json(req): Json<AuditRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Validate URL
    if !req.url.starts_with("http://") && !req.url.starts_with("https://") {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "URL must use http or https scheme" })),
        ));
    }

    let settings = state.store.get_settings().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to load settings: {}", e) })),
        )
    })?;

    let device = req
        .device
        .clone()
        .unwrap_or_else(|| settings.device.clone());
    let audit_id = uuid::Uuid::new_v4().to_string();

    state
        .store
        .create_audit(
            &audit_id,
            &req.url,
            &AuditTrigger::Manual,
            None,
            None,
            &device,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Failed to create audit: {}", e) })),
            )
        })?;

    // Run audit in background
    let bg_store = state.store.clone();
    let bg_url = req.url.clone();
    let bg_id = audit_id.clone();
    let bg_categories = req.categories.clone();
    let bg_device = device.clone();

    tokio::spawn(async move {
        run_audit_background(
            &bg_store,
            &bg_id,
            &bg_url,
            &settings,
            Some(&bg_device),
            bg_categories.as_deref(),
        )
        .await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "id": audit_id,
            "status": "running",
            "message": format!("Lighthouse audit started for {} ({})", req.url, device),
        })),
    ))
}

/// Run an audit in the background and update the store.
pub async fn run_audit_background(
    store: &AuditStore,
    audit_id: &str,
    url: &str,
    settings: &PluginSettings,
    device_override: Option<&str>,
    categories_override: Option<&[String]>,
) {
    match lighthouse::run_audit(url, settings, device_override, categories_override).await {
        Ok(result) => {
            if let Err(e) = store.complete_audit(audit_id, &result).await {
                tracing::error!(
                    audit_id = %audit_id,
                    error = %e,
                    "Failed to save audit result"
                );
            }
        }
        Err(e) => {
            tracing::error!(
                audit_id = %audit_id,
                url = %url,
                error = %e,
                "Lighthouse audit failed"
            );
            if let Err(save_err) = store.mark_failed(audit_id, &e.to_string()).await {
                tracing::error!(
                    audit_id = %audit_id,
                    error = %save_err,
                    "Failed to mark audit as failed"
                );
            }
        }
    }
}

/// List all audits.
pub async fn list_audits(
    State(state): State<AppState>,
) -> Result<Json<Vec<AuditSummary>>, (StatusCode, Json<serde_json::Value>)> {
    let audits = state.store.list_audits().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to list audits: {}", e) })),
        )
    })?;
    Ok(Json(audits))
}

/// Get a full audit with details.
pub async fn get_audit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LighthouseAudit>, StatusCode> {
    match state.store.get_audit(&id).await {
        Ok(Some(audit)) => Ok(Json(audit)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(audit_id = %id, error = %e, "Failed to get audit");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Delete an audit.
pub async fn delete_audit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    match state.store.delete_audit(&id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Ok(StatusCode::NOT_FOUND),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to delete audit: {}", e) })),
        )),
    }
}

/// Get raw Lighthouse JSON for an audit.
pub async fn get_raw_json(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    match state.store.get_raw_json(&id).await {
        Ok(Some(json)) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(audit_id = %id, error = %e, "Failed to get raw JSON");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get score history for charts.
pub async fn get_score_history(
    State(state): State<AppState>,
) -> Result<Json<Vec<ScoreHistoryPoint>>, (StatusCode, Json<serde_json::Value>)> {
    let history = state.store.get_score_history(50).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to get score history: {}", e) })),
        )
    })?;
    Ok(Json(history))
}

/// Check if Lighthouse CLI is available.
pub async fn get_status() -> Json<serde_json::Value> {
    let available = lighthouse::is_lighthouse_available().await;
    Json(serde_json::json!({
        "lighthouse_available": available,
    }))
}

/// Get plugin settings.
pub async fn get_settings(
    State(state): State<AppState>,
) -> Result<Json<PluginSettings>, (StatusCode, Json<serde_json::Value>)> {
    let settings = state.store.get_settings().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to load settings: {}", e) })),
        )
    })?;
    Ok(Json(settings))
}

/// Update plugin settings (partial update).
pub async fn update_settings(
    State(state): State<AppState>,
    Json(update): Json<UpdateSettings>,
) -> Result<Json<PluginSettings>, (StatusCode, Json<serde_json::Value>)> {
    let settings = state.store.update_settings(&update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to update settings: {}", e) })),
        )
    })?;
    Ok(Json(settings))
}
