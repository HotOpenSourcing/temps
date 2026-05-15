//! Per-engine default wall-clock timeouts and the three-tier resolution helper.
//!
//! ## Why per-engine defaults instead of one global constant?
//!
//! The original `DEFAULT_JOB_MAX_RUNTIME = 30 min` was sized for the
//! control-plane backup (a small SQLite/Postgres dump that typically completes
//! in seconds). When a 200 GB Postgres cluster was backed up via WAL-G over a
//! slow S3 upload link, the job was killed at 30m1s with no way to recover
//! without operator intervention.
//!
//! The new defaults are conservative upper bounds sized to the **worst observed
//! real-world case** for each engine family. They are not "expected duration"
//! estimates — they are circuit breakers that prevent zombie jobs from holding
//! the queue slot forever if the engine hangs.

/// Default wall-clock timeout for a given engine key (seconds).
///
/// These values are conservative upper bounds, not expected durations. The
/// intent is to protect against truly wedged or zombie jobs while giving
/// legitimately slow backups (multi-hundred-GB databases, cross-region S3
/// mirrors) enough headroom to complete.
///
/// Operators who need a longer or shorter timeout for a specific schedule can
/// set `backup_schedules.max_runtime_secs`. Ad-hoc triggers can pass
/// `EnqueueJobParams::max_runtime_secs`.
///
/// | Engine key              | Default | Rationale                                                       |
/// |-------------------------|---------|------------------------------------------------------------------|
/// | `postgres_walg`         | 24 h    | WAL-G uploads scale with DB size; 200 GB+ over slow links.      |
/// | `postgres_pgdump`       | 24 h    | pg_dump is CPU- and IO-bound; same size ceiling as WAL-G.       |
/// | `postgres_cluster`      | 24 h    | HA cluster backup path through sidecar; same ceiling.           |
/// | `control_plane`         |  4 h    | Small DB; typical run is seconds. 4 h is generous headroom.     |
/// | `redis`                 |  4 h    | RDB snapshot + upload; bounded by memory size, typically small. |
/// | `mongodb`               |  4 h    | `mongodump` + upload; similar profile to Redis.                 |
/// | `s3_mirror`             | 12 h    | `mc mirror` is bandwidth-bound; bucket-to-bucket across regions. |
/// | _(unknown)_             | 24 h    | Permissive fallback; engine's own timeouts still apply.         |
pub fn default_max_runtime_secs(engine: &str) -> i64 {
    match engine {
        // Postgres backups via WAL-G or pg_dump scale with database size.
        // 24 h handles multi-hundred-GB clusters over slow upload links.
        "postgres_walg" | "postgres_pgdump" | "postgres_cluster" => 24 * 3600,
        // Control-plane DB is small but still goes through the same
        // sidecar + upload path. 4 h is generous; typical run is seconds.
        "control_plane" => 4 * 3600,
        // Redis RDB snapshots and Mongo dumps are smaller and faster.
        "redis" | "mongodb" => 4 * 3600,
        // S3 mirror via `mc mirror` is bandwidth-bound; bucket-to-bucket
        // transfers across regions can take a long time.
        "s3_mirror" => 12 * 3600,
        // Unknown engine — be permissive; the engine's own internal
        // timeouts and the heartbeat-stall sweeper still apply.
        _ => 24 * 3600,
    }
}

/// Resolve the wall-clock timeout (seconds) for a new job.
///
/// ## Resolution order
///
/// 1. `params_override` — caller-supplied value from `EnqueueJobParams::max_runtime_secs`.
/// 2. `schedule_override` — the schedule's `backup_schedules.max_runtime_secs`.
/// 3. `default_max_runtime_secs(engine)` — engine-family conservative default.
///
/// The first `Some` value wins. A floor of 60 seconds is applied so a
/// zero or corrupt value never instantly fails every job.
pub fn resolve_max_runtime(
    params_override: Option<i64>,
    schedule_override: Option<i64>,
    engine: &str,
) -> i64 {
    let raw = params_override
        .or(schedule_override)
        .unwrap_or_else(|| default_max_runtime_secs(engine));

    // Floor at 60 seconds: a corrupt or zero value would instantly fail
    // every job. One minute is the absolute minimum we ever accept.
    raw.max(60)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── default_max_runtime_secs ──────────────────────────────────────────────

    #[test]
    fn test_postgres_walg_default() {
        assert_eq!(default_max_runtime_secs("postgres_walg"), 86_400);
    }

    #[test]
    fn test_postgres_pgdump_default() {
        assert_eq!(default_max_runtime_secs("postgres_pgdump"), 86_400);
    }

    #[test]
    fn test_postgres_cluster_default() {
        assert_eq!(default_max_runtime_secs("postgres_cluster"), 86_400);
    }

    #[test]
    fn test_control_plane_default() {
        assert_eq!(default_max_runtime_secs("control_plane"), 4 * 3600);
    }

    #[test]
    fn test_redis_default() {
        assert_eq!(default_max_runtime_secs("redis"), 4 * 3600);
    }

    #[test]
    fn test_mongodb_default() {
        assert_eq!(default_max_runtime_secs("mongodb"), 4 * 3600);
    }

    #[test]
    fn test_s3_mirror_default() {
        assert_eq!(default_max_runtime_secs("s3_mirror"), 12 * 3600);
    }

    #[test]
    fn test_unknown_engine_default_is_permissive() {
        // Unknown engines get the same 24 h ceiling as Postgres so we
        // never accidentally kill a legitimate long-running new engine.
        assert_eq!(default_max_runtime_secs("some_new_engine"), 86_400);
        assert_eq!(default_max_runtime_secs(""), 86_400);
    }

    // ── resolve_max_runtime ───────────────────────────────────────────────────

    #[test]
    fn test_resolve_params_override_wins() {
        // Tier 1: explicit caller override beats schedule and engine default.
        let result = resolve_max_runtime(Some(7200), Some(3600), "postgres_walg");
        assert_eq!(result, 7200, "params_override should win");
    }

    #[test]
    fn test_resolve_schedule_override_wins_when_no_params() {
        // Tier 2: schedule override beats engine default when no params override.
        let result = resolve_max_runtime(None, Some(3600), "postgres_walg");
        assert_eq!(
            result, 3600,
            "schedule_override should win when params is None"
        );
    }

    #[test]
    fn test_resolve_engine_default_wins_when_nothing_set() {
        // Tier 3: engine default used when both overrides are None.
        let result = resolve_max_runtime(None, None, "postgres_walg");
        assert_eq!(
            result, 86_400,
            "engine default (86400) should be used when no overrides"
        );
    }

    #[test]
    fn test_resolve_floor_prevents_zero_from_killing_jobs() {
        // A corrupt zero value must be raised to 60 s, not used as-is.
        let result = resolve_max_runtime(Some(0), None, "control_plane");
        assert_eq!(result, 60, "zero must be floored to 60");
    }

    #[test]
    fn test_resolve_floor_prevents_negative_from_killing_jobs() {
        let result = resolve_max_runtime(Some(-100), None, "redis");
        assert_eq!(result, 60, "negative must be floored to 60");
    }
}
