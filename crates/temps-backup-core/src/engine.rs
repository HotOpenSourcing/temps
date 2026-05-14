//! `BackupEngine` trait and associated types (ADR-014 §"`BackupEngine` trait").
//!
//! Engines live in `temps-providers` (or other domain crates) and implement
//! this trait. `temps-backup-core` defines the trait; engines depend on this
//! crate — never the reverse. This keeps the dependency graph acyclic.

use async_trait::async_trait;
use futures::stream::BoxStream;
use sea_orm::DatabaseConnection;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Durable cursor passed into `execute` on the first attempt and on every
/// resume (ADR-014 §"`BackupEngine` trait").
///
/// `current_step` is `None` on the first attempt; set to the last completed
/// step's name on a resume. `durable_state` carries whatever the engine
/// persisted in `backup_job_steps.durable_state` at that step.
#[derive(Debug, Clone)]
pub struct StepCursor {
    /// Name of the last completed step, or `None` if this is the first attempt.
    pub current_step: Option<String>,
    /// Opaque JSON value the engine serialised at the last `StepCompleted` event.
    /// The runner stores this verbatim in `backup_jobs.step_state`.
    pub durable_state: Value,
}

/// Context passed to every engine call (ADR-014 §"Cancellation").
///
/// Contains everything the engine needs to do its work without touching the
/// database directly. The `cancel` token is signalled when the job is
/// `state='cancelled'` or the runner is shutting down.
#[derive(Clone)]
pub struct BackupContext {
    /// The `backup_jobs.id` of the current execution.
    pub job_id: i64,
    /// The current attempt number (`backup_jobs.attempts` after claim increment).
    pub attempt: i32,
    /// Engine-specific parameters from `backup_jobs.params`.
    pub params: Value,
    /// Shared database connection for engines that need to look up service
    /// credentials or write metadata rows.
    pub db: Arc<DatabaseConnection>,
    /// Cancellation token. Engines should check `cancel.is_cancelled()` at
    /// natural checkpoints (e.g., between upload chunks) to respond to
    /// `state='cancelled'` without busy-polling the database.
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for BackupContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupContext")
            .field("job_id", &self.job_id)
            .field("attempt", &self.attempt)
            .field("params", &self.params)
            .field("cancel", &self.cancel.is_cancelled())
            .finish()
    }
}

/// Events emitted by a `BackupEngine::execute` stream (ADR-014 §"Runner loop").
#[derive(Debug)]
pub enum StepEvent {
    /// The engine completed a durable step. The runner persists `step` +
    /// `durable_state` atomically before continuing, so a crash after this
    /// event is yielded but before the runner flushes is safe: the engine will
    /// see the previous step's cursor on resume.
    StepCompleted {
        /// Step name (must be in `BackupEngine::steps()`).
        step: String,
        /// Durable state to store in `backup_job_steps.durable_state`.
        durable_state: Value,
        /// Optional human-readable progress note stored in the step row.
        message: Option<String>,
    },

    /// The engine is alive and making progress but has not crossed a step
    /// boundary. The runner extends the lease without writing a step row.
    ///
    /// Engines that have steps known to take longer than the lease TTL (5 min)
    /// must emit `Heartbeat` events at least every 2 minutes.
    Heartbeat,

    /// The engine finished successfully. The runner stamps `finished_at` on
    /// both the `backup_jobs` row and the parent `backups` row, then sets
    /// both to `state='completed'`.
    Done {
        /// S3 or provider-native location where the backup artifact was stored.
        location: String,
        /// Compressed size in bytes, if known.
        size_bytes: Option<i64>,
        /// Compression algorithm used (e.g., `"gzip"`, `"none"`).
        compression: String,
    },
}

/// Errors an engine can return from its `execute` stream (ADR-014 §"`BackupEngine` trait").
#[derive(thiserror::Error, Debug)]
pub enum BackupEngineError {
    /// Preflight checks failed before any work was done. No cleanup is needed.
    #[error("Preflight failed for job {job_id}: {reason}")]
    Preflight { job_id: i64, reason: String },

    /// A named step failed partway through execution.
    #[error("Step '{step}' failed for job {job_id}: {reason}")]
    StepFailed {
        job_id: i64,
        step: String,
        reason: String,
    },

    /// An I/O error occurred (e.g., writing a temp file, reading a socket).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// An S3 or object-storage error occurred.
    #[error("S3 error during job {job_id}: {reason}")]
    S3 { job_id: i64, reason: String },

    /// The engine key in the dispatched job does not match this implementation.
    #[error("Engine not supported for job {job_id}: engine='{engine}'")]
    Unsupported { job_id: i64, engine: String },
}

/// Trait implemented by each backup engine (ADR-014 §"`BackupEngine` trait").
///
/// Engines stream `StepEvent`s so the runner can persist progress atomically
/// at each step boundary. Crash-resume is handled by passing the last
/// completed step's cursor back via `StepCursor` on the next attempt.
///
/// **Dependency direction:** engines implement this trait (in `temps-providers`
/// or elsewhere) but do NOT depend on `temps-backup-core` for anything beyond
/// the types defined here. The runner (`temps-backup-core`) depends on the trait
/// — never on concrete engine implementations.
#[async_trait]
pub trait BackupEngine: Send + Sync {
    /// Machine-readable engine identifier. Must match `backup_jobs.engine`.
    ///
    /// Examples: `"postgres_walg"`, `"postgres_pgdump"`, `"redis"`, `"mongodb"`.
    fn engine(&self) -> &'static str;

    /// Ordered list of step names this engine will emit, in execution order
    /// (ADR-014 §"Per-engine step definitions").
    ///
    /// Used by the runner to validate `StepCompleted` events and by the UI to
    /// render a progress timeline.
    fn steps(&self) -> &'static [&'static str];

    /// Execute (or resume) the backup, streaming `StepEvent`s.
    ///
    /// The stream must yield at least one `StepCompleted` or `Heartbeat` event
    /// before any wall-clock lease expiry (default 5 minutes) to prevent the
    /// runner from treating the job as stalled.
    ///
    /// If `cursor.current_step` is `Some("dump")`, the engine must skip
    /// straight to the step after `"dump"`. Re-running a completed step after
    /// crash-and-resume must produce the same artifact (idempotence per step).
    fn execute<'a>(
        &'a self,
        ctx: &'a BackupContext,
        cursor: StepCursor,
    ) -> BoxStream<'a, Result<StepEvent, BackupEngineError>>;

    /// Optional rollback hook called when `attempts >= max_attempts` so the
    /// engine can clean up partial uploads. Default is a no-op.
    ///
    /// Failures here are logged but do not change the job's final state.
    async fn rollback(
        &self,
        _ctx: &BackupContext,
        _cursor: StepCursor,
    ) -> Result<(), BackupEngineError> {
        Ok(())
    }
}
