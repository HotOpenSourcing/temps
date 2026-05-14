//! Configuration for the `BackupRunner` (ADR-014 §"Runner loop").
//!
//! All fields have sensible defaults suitable for a single-node Hetzner CPX21
//! deployment. Override via environment variables in `BackupPlugin` (Phase 0+).

use std::time::Duration;

/// Configuration for the `BackupRunner` poll loop.
///
/// See ADR-014 §"Concurrency caps" and §"Lease duration" for the rationale
/// behind the defaults.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// How often the runner polls the queue for claimable jobs.
    /// Default: 5 seconds (ADR-014 §"Runner loop").
    pub poll_interval: Duration,

    /// Duration of the claim lease. Engines must emit a `StepCompleted` or
    /// `Heartbeat` event within this window or the job becomes reclaimable.
    /// Default: 5 minutes (ADR-014 §"Lease duration").
    pub lease_ttl: Duration,

    /// Maximum number of jobs the runner will hold concurrently. Each claimed
    /// job runs in a `tokio::spawn`-ed task.
    /// Default: 4 (ADR-014 §"Concurrency caps", Q1 recommendation).
    pub max_concurrent: usize,

    /// Stable identity for this runner instance. Used as `backup_jobs.claimed_by`
    /// so operators can identify which process holds a running job.
    /// Typically the server hostname or a UUID generated at startup.
    pub instance_id: String,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            lease_ttl: Duration::from_secs(300), // 5 minutes
            max_concurrent: 4,
            instance_id: "temps-server".to_string(),
        }
    }
}

impl RunnerConfig {
    /// Construct with a specific instance identity (e.g., hostname).
    pub fn with_instance_id(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            ..Default::default()
        }
    }
}
