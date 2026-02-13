use sea_orm::DatabaseConnection;
use std::sync::Arc;
use temps_core::UtcDateTime;
use tokio::sync::OnceCell;

use super::error_analytics_service::{ErrorAnalyticsService, ErrorDashboardStats};
use super::error_crud_service::ErrorCRUDService;
use super::error_ingestion_service::ErrorIngestionService;
use super::source_map_service::SourceMapService;
use super::types::*;

/// Facade service that coordinates all error tracking functionality
///
/// This is the main service that applications should use. It delegates
/// to specialized services for different concerns:
/// - Ingestion: Processing and fingerprinting errors
/// - CRUD: Reading and updating error data
/// - Analytics: Statistics and metrics
/// - Source maps: Symbolicating minified stack traces
pub struct ErrorTrackingService {
    pub ingestion: ErrorIngestionService,
    pub crud: ErrorCRUDService,
    pub analytics: ErrorAnalyticsService,
    source_map_service: OnceCell<Arc<SourceMapService>>,
}

impl ErrorTrackingService {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self {
            ingestion: ErrorIngestionService::new(db.clone()),
            crud: ErrorCRUDService::new(db.clone()),
            analytics: ErrorAnalyticsService::new(db),
            source_map_service: OnceCell::new(),
        }
    }

    /// Set the source map service for symbolication support.
    /// This is called after construction since SourceMapService and ErrorTrackingService
    /// are created independently in the plugin registration.
    pub fn set_source_map_service(&self, service: Arc<SourceMapService>) {
        let _ = self.source_map_service.set(service);
    }

    // Convenience methods that delegate to specialized services

    /// Process a new error event.
    /// If a source map service is configured and the event has a release version,
    /// stack traces will be symbolicated before storage.
    pub async fn process_error_event(
        &self,
        mut error_data: CreateErrorEventData,
    ) -> Result<i32, ErrorTrackingError> {
        // Symbolicate stack traces if source maps are available
        if let Some(sm_service) = self.source_map_service.get() {
            if let Err(e) = sm_service.symbolicate_error_event(&mut error_data).await {
                tracing::warn!(
                    "Source map symbolication failed (continuing without): {}",
                    e
                );
            }
        }

        self.ingestion.process_error_event(error_data).await
    }

    /// List error groups (delegates to CRUD service)
    #[allow(clippy::too_many_arguments)]
    pub async fn list_error_groups(
        &self,
        project_id: i32,
        page: Option<u64>,
        page_size: Option<u64>,
        status_filter: Option<String>,
        environment_id: Option<i32>,
        sort_by: Option<String>,
        sort_order: Option<String>,
    ) -> Result<(Vec<ErrorGroupDomain>, u64), ErrorTrackingError> {
        self.crud
            .list_error_groups(
                project_id,
                page,
                page_size,
                status_filter,
                environment_id,
                sort_by,
                sort_order,
            )
            .await
    }

    /// Get error group by ID (delegates to CRUD service)
    pub async fn get_error_group(
        &self,
        group_id: i32,
        project_id: i32,
    ) -> Result<ErrorGroupDomain, ErrorTrackingError> {
        self.crud.get_error_group(group_id, project_id).await
    }

    /// Update error group status (delegates to CRUD service)
    pub async fn update_error_group_status(
        &self,
        group_id: i32,
        project_id: i32,
        status: String,
        assigned_to: Option<String>,
    ) -> Result<(), ErrorTrackingError> {
        self.crud
            .update_error_group_status(group_id, project_id, status, assigned_to)
            .await
    }

    /// List error events (delegates to CRUD service)
    pub async fn list_error_events(
        &self,
        group_id: i32,
        project_id: i32,
        page: Option<u64>,
        page_size: Option<u64>,
    ) -> Result<(Vec<ErrorEventDomain>, u64), ErrorTrackingError> {
        self.crud
            .list_error_events(group_id, project_id, page, page_size)
            .await
    }

    /// Get error statistics (delegates to analytics service)
    pub async fn get_error_stats(
        &self,
        project_id: i32,
        environment_id: Option<i32>,
    ) -> Result<ErrorGroupStats, ErrorTrackingError> {
        self.analytics
            .get_error_stats(project_id, environment_id)
            .await
    }

    /// Get error time series (delegates to analytics service)
    pub async fn get_error_time_series(
        &self,
        project_id: i32,
        start_time: UtcDateTime,
        end_time: UtcDateTime,
        interval: &str,
    ) -> Result<Vec<ErrorTimeSeriesPoint>, ErrorTrackingError> {
        self.analytics
            .get_error_time_series(project_id, start_time, end_time, interval)
            .await
    }

    /// Get dashboard stats (delegates to analytics service)
    pub async fn get_dashboard_stats(
        &self,
        project_id: i32,
        start_time: UtcDateTime,
        end_time: UtcDateTime,
        environment_id: Option<i32>,
        compare_to_previous: bool,
    ) -> Result<ErrorDashboardStats, ErrorTrackingError> {
        self.analytics
            .get_dashboard_stats(
                project_id,
                start_time,
                end_time,
                environment_id,
                compare_to_previous,
            )
            .await
    }

    /// Check if project has error groups (delegates to CRUD service)
    pub async fn has_error_groups(&self, project_id: i32) -> Result<bool, ErrorTrackingError> {
        self.crud.has_error_groups(project_id).await
    }

    /// Get a specific error event by ID (delegates to CRUD service).
    ///
    /// Performs on-the-fly symbolication if:
    /// - A source map service is configured
    /// - The event has stored sentry data with a release version
    /// - The stack frames haven't been symbolicated yet
    pub async fn get_error_event(
        &self,
        event_id: i64,
        group_id: i32,
        project_id: i32,
    ) -> Result<ErrorEventDomain, ErrorTrackingError> {
        let mut event = self
            .crud
            .get_error_event_by_ids(event_id, group_id, project_id)
            .await?;

        // On-the-fly symbolication: resolve stack frames using stored source maps
        if let Some(sm_service) = self.source_map_service.get() {
            if let Some(data) = &mut event.data {
                sm_service.symbolicate_stored_event(project_id, data).await;
            }
        }

        Ok(event)
    }
}
