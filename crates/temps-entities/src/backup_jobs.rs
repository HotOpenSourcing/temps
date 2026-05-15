//! Sea-ORM entity for the `backup_jobs` table (ADR-014).
//!
//! One row per execution attempt. The `BackupRunner` (in `temps-backup-core`)
//! claims rows atomically via `FOR UPDATE SKIP LOCKED`, advances their state,
//! and writes final results back to the parent `backups` row on `Done` or
//! terminal failure.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use temps_core::DBDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "backup_jobs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// FK to the parent `backups` row. Cascade-delete keeps jobs clean.
    pub backup_id: i32,
    /// Machine-readable engine identifier, e.g. `"redis"`, `"postgres_walg"`.
    /// Must match the value returned by `BackupEngine::engine()`.
    pub engine: String,
    /// `"control_plane"` or `"external_service"`.
    pub target_kind: String,
    /// `None` for control-plane backups; FK to `external_services.id` otherwise.
    pub target_id: Option<i32>,
    /// Engine-specific parameters (S3 bucket, compression, max_concurrent, etc.).
    pub params: Json,
    /// Lifecycle state: `pending` | `running` | `completed` | `failed` | `cancelled`.
    pub state: String,
    /// Name of the last completed step. `None` on the first attempt.
    pub step: Option<String>,
    /// Durable cursor written by the engine at the last `StepCompleted` event.
    /// Passed back verbatim on resume.
    pub step_state: Json,
    /// Total number of times this job has been claimed and run.
    pub attempts: i32,
    /// Maximum attempts before the job is permanently failed.
    pub max_attempts: i32,
    /// Fencing token rotated on every claim. The runner includes this in all
    /// UPDATE … WHERE clauses to prevent a stale worker from overwriting a
    /// newer owner's progress.
    pub claim_token: Option<Uuid>,
    /// Hostname or instance-id of the process that currently holds this job.
    pub claimed_by: Option<String>,
    /// Hard expiry of the current lease. The engine must emit a `StepCompleted`
    /// or `Heartbeat` event before this timestamp, or a competing runner reclaims.
    pub leased_until: Option<DBDateTime>,
    /// Earliest time the job may be claimed. Backoff formula advances this on retry.
    pub next_attempt_at: DBDateTime,
    /// Error message from the last failed attempt, if any.
    pub error_message: Option<String>,
    /// Stamped on the first claim; not reset on retry.
    pub started_at: Option<DBDateTime>,
    /// Stamped by the runner at the exact moment of `Done` or terminal failure.
    pub finished_at: Option<DBDateTime>,
    pub created_at: DBDateTime,
    pub updated_at: DBDateTime,
    /// Wall-clock timeout baked in at enqueue time (seconds).
    ///
    /// Resolution order at enqueue: caller-supplied override →
    /// schedule-level `backup_schedules.max_runtime_secs` → engine default
    /// (see `temps_backup_core::timeouts::default_max_runtime_secs`).
    ///
    /// The runner reads this column directly so it never needs to infer
    /// the timeout from the engine key at dispatch time.
    ///
    /// DB default is 86 400 (24 h), matching the engine default for unknown
    /// engines and Postgres. Existing rows produced before this migration was
    /// applied receive 24 h automatically.
    pub max_runtime_secs: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::backups::Entity",
        from = "Column::BackupId",
        to = "super::backups::Column::Id"
    )]
    Backup,
    #[sea_orm(has_many = "super::backup_job_steps::Entity")]
    BackupJobSteps,
}

impl Related<super::backups::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Backup.def()
    }
}

impl Related<super::backup_job_steps::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::BackupJobSteps.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
