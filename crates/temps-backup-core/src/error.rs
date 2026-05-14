//! Typed error enum for the backup runner and queue primitives (ADR-014).
//!
//! Every variant includes identifiers and operation context so errors are
//! traceable from log output without additional debugging. Matches the
//! error-handling rules in `temps/CLAUDE.md`.

use thiserror::Error;

/// Errors that can occur in the `BackupRunner`, queue primitives, or
/// `enqueue_job`. All variants include the job/backup ID where applicable so
/// log lines are self-contained.
#[derive(Error, Debug)]
pub enum BackupRunnerError {
    /// The database returned an error during a queue operation.
    #[error("Database error during backup queue operation '{operation}': {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sea_orm::DbErr,
    },

    /// A job was claimed successfully but the engine identifier in the row has
    /// no registered implementation in the runner's engine registry.
    #[error(
        "No engine registered for job {job_id} with engine key '{engine}'; \
         registered engines: [{registered}]"
    )]
    EngineNotFound {
        job_id: i64,
        engine: String,
        registered: String,
    },

    /// `enqueue_job` tried to insert a `backup_jobs` row but Sea-ORM returned
    /// `RecordNotInserted` — typically a constraint violation.
    #[error(
        "Failed to insert backup_jobs row for backup_id={backup_id} engine='{engine}': \
         record not inserted (possible constraint violation)"
    )]
    EnqueueFailed { backup_id: i32, engine: String },

    /// A lease-extension UPDATE matched zero rows, meaning the claim_token no
    /// longer matches — the job was reclaimed by another runner.
    #[error(
        "Lease extension failed for job {job_id}: claim_token mismatch \
         (job was reclaimed by another runner)"
    )]
    LeaseLost { job_id: i64 },

    /// A step-persistence transaction was fenced out — the claim_token in the
    /// UPDATE matched zero rows.
    #[error(
        "Step persistence fenced for job {job_id} step '{step}' attempt {attempt}: \
         claim_token mismatch (job was reclaimed)"
    )]
    StepFenced {
        job_id: i64,
        step: String,
        attempt: i32,
    },

    /// `mark_job_completed` or `mark_job_failed` could not locate the parent
    /// `backups` row to update.
    #[error(
        "Parent backup row {backup_id} not found when finalising job {job_id} \
         with state '{final_state}'"
    )]
    ParentBackupNotFound {
        job_id: i64,
        backup_id: i32,
        final_state: &'static str,
    },

    /// Serialisation or deserialisation of JSONB columns failed.
    #[error("JSON error in job {job_id} field '{field}': {source}")]
    Json {
        job_id: i64,
        field: &'static str,
        #[source]
        source: serde_json::Error,
    },

    /// The claim query returned a row whose `step_state` column could not be
    /// deserialised. This usually means schema drift or manual corruption.
    #[error(
        "Claimed job {job_id} has invalid step_state JSON; \
         cannot build StepCursor for resume: {reason}"
    )]
    InvalidStepState { job_id: i64, reason: String },

    /// A backup for this engine + target is already pending or running.
    ///
    /// Returned by `enqueue_job` when a pre-INSERT check finds an in-flight
    /// row with matching `(engine, target_kind, target_id)`. Callers should
    /// surface this as HTTP 409 Conflict.
    #[error(
        "A {engine} backup is already in flight for target {target_id:?}; \
         refusing to enqueue a duplicate (existing job id: {existing_job_id})"
    )]
    AlreadyInFlight {
        engine: String,
        target_id: Option<i32>,
        existing_job_id: i64,
    },
}

impl From<sea_orm::DbErr> for BackupRunnerError {
    fn from(e: sea_orm::DbErr) -> Self {
        BackupRunnerError::Database {
            operation: "unknown",
            source: e,
        }
    }
}
