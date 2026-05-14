use sea_orm::DatabaseConnection;
use std::sync::Arc;
use temps_backup_core::BackupRunner;
use temps_core::AuditLogger;
use temps_providers::postgres_upgrade_service::PostgresUpgradeService;

use crate::services::{BackupService, RestoreService};

/// Application state shared across all backup HTTP handlers.
///
/// The runner is always present (ADR-014 Phase 5: the legacy synchronous path
/// has been removed). Handlers always enqueue via the runner and return
/// `202 Accepted`. The optional `backup_runner` field of previous phases is
/// now a required `Arc<BackupRunner>`.
pub struct BackupAppState {
    pub backup_service: Arc<BackupService>,
    pub restore_service: Arc<RestoreService>,
    pub audit_service: Arc<dyn AuditLogger>,
    pub pg_upgrade_service: Arc<PostgresUpgradeService>,
    pub db: Arc<DatabaseConnection>,
    /// The runner instance used by handlers to enqueue jobs.
    pub backup_runner: Arc<BackupRunner>,
}

/// Construct `BackupAppState` with a required runner.
///
/// The runner must be fully constructed (engines registered) before calling
/// this function. There is no deferred-runner-injection path: the runner is
/// the only backup execution path.
pub fn create_backup_app_state(
    backup_service: Arc<BackupService>,
    restore_service: Arc<RestoreService>,
    audit_service: Arc<dyn AuditLogger>,
    pg_upgrade_service: Arc<PostgresUpgradeService>,
    db: Arc<DatabaseConnection>,
    backup_runner: Arc<BackupRunner>,
) -> Arc<BackupAppState> {
    Arc::new(BackupAppState {
        backup_service,
        restore_service,
        audit_service,
        pg_upgrade_service,
        db,
        backup_runner,
    })
}
