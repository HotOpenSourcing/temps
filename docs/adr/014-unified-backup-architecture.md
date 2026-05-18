# ADR-014: Unified Backup Execution Architecture

**Status:** Proposed
**Date:** 2026-05-14
**Author:** David Viejo

## Context

A production incident exposed three compounding structural problems in the current
backup system.

### Incident timeline

A Redis backup row was stuck in `state='running'` for 31 hours. When the temps
server restarted, `reconcile_orphan_backups` swept the row and marked it
`state='failed'` with the canned message "Backup was in progress when the temps
server restarted. The worker process died before the backup could complete."
(`crates/temps-backup/src/services/reconcile.rs:17–19`). The `finished_at`
timestamp was stamped at reconcile time — i.e., now — not at the moment the
worker actually stopped. Every metric downstream of that row now shows a 31-hour
duration and a misleading cause.

During those 31 hours, scheduled backups stopped firing entirely because the
temps server itself was down and the scheduler lives in-process. No backup ran,
no alert was raised, no operator was paged.

### Problem 1: Reconcile produces fabricated metadata

`reconcile_orphan_backups` (`reconcile.rs:27–83`) runs once at boot and applies
an unconditional sweep:

```rust
update.state = Set("failed".to_string());
update.error_message = Set(Some(ORPHAN_REASON.to_string()));
update.finished_at = Set(Some(now));   // 'now' is boot time, not work-stop time
```

It does not inspect `last_heartbeat_at` to estimate when the work actually
stopped. It does not know what step the previous attempt had reached. It cannot
distinguish a backup that died 31 seconds ago from one that died 31 hours ago.
The result is a `finished_at` that may be hours off and an `error_message` that
tells the operator nothing about the failure mode.

### Problem 2: Scheduler is wedged when any backup blocks

`start_backup_scheduler` (`backup.rs:4468–4570`) is a single sequential
`tokio` loop. Its inner method `process_backup_schedule` (`backup.rs:4588–4673`)
calls `self.create_backup(...).await` inline — no `tokio::spawn`, no timeout.
If `create_backup` blocks (a hung S3 upload, a stalled Docker exec, an
unresponsive replica), the entire scheduler is wedged. No subsequent cron tick
fires until that one returns. With one engine per invocation, N slow backups
serially starve every schedule.

### Problem 3: No resume after crash

`HeartbeatGuard` (`heartbeat.rs:29–75`) keeps a `last_heartbeat_at` column
fresh but records no progress state. When the process dies, the next boot's
reconcile marks the row failed and the next scheduled run starts from the
beginning. A 2-hour pg_dump-to-S3 that died at the upload step restarts the
entire dump. For large databases, this means a single server restart during a
backup window can push a cold recovery window from minutes to hours.

### Problem 4: `backup_to_s3` is a monolith with no step boundaries

`ExternalService::backup_to_s3` (`crates/temps-providers/src/externalsvc/mod.rs:832–844`)
is a single `async fn` that returns `Result<BackupOutcome>`. Each engine
implements it as a monolithic procedure. There is no step boundary that a
subsequent attempt could resume from. The function either succeeds completely
or fails completely with no observable checkpoint.

### Precedents already in the codebase

Two existing patterns demonstrate the right direction:

- **Claim-based worker loop.** `ChFanoutWorker::process_one_batch`
  (`crates/temps-analytics-events/src/services/ch_fanout.rs:214–232`) uses
  `FOR UPDATE SKIP LOCKED` to claim outbox batches, preventing double-processing
  across multiple workers without a distributed lock service.

- **Spawn-per-job row pattern.** `RestoreService::start_restore_run`
  (`crates/temps-backup/src/services/restore.rs:817–826`) inserts a row then
  immediately spawns a `tokio::spawn`-ed worker that owns the row's lifecycle.
  The scheduler does not await the work inline.

## Decision

Introduce a **claim-based, step-aware backup queue** implemented as two new
tables and a new crate (`temps-backup-core`). The scheduler becomes a pure
enqueuer. A `BackupRunner` claim-polls the queue and dispatches to a
`BackupEngine` trait whose engines stream `StepEvent`s. Crashes are handled
by lease expiry and resume rather than by boot-time reconcile.

### Schema

#### `backup_jobs`

One row per execution attempt (including retries).

```sql
CREATE TABLE backup_jobs (
    id               BIGSERIAL PRIMARY KEY,
    backup_id        INTEGER NOT NULL REFERENCES backups(id) ON DELETE CASCADE,
    engine           TEXT NOT NULL,           -- 'postgres_walg' | 'postgres_pgdump' |
                                              -- 'postgres_cluster' | 'redis' |
                                              -- 'mongodb' | 's3_mirror' | 'rustfs' |
                                              -- 'control_plane'
    target_kind      TEXT NOT NULL,           -- 'control_plane' | 'external_service'
    target_id        INTEGER,                 -- NULL for control_plane, FK to external_services otherwise
    params           JSONB NOT NULL DEFAULT '{}',
    state            TEXT NOT NULL DEFAULT 'pending'
                         CHECK (state IN ('pending','running','completed','failed','cancelled')),
    step             TEXT,                    -- last completed step, NULL = not started
    step_state       JSONB NOT NULL DEFAULT '{}',  -- durable cursor for resume
    attempts         INTEGER NOT NULL DEFAULT 0,
    max_attempts     INTEGER NOT NULL DEFAULT 3,
    claim_token      UUID,                    -- rotated on every claim
    claimed_by       TEXT,                    -- hostname or instance id of claimant
    leased_until     TIMESTAMPTZ,             -- NULL when unclaimed
    next_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error_message    TEXT,
    started_at       TIMESTAMPTZ,
    finished_at      TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Primary polling index: find claimable rows cheaply.
CREATE INDEX backup_jobs_claimable_idx
    ON backup_jobs (next_attempt_at)
    WHERE state = 'pending';

-- Secondary index for parent-row lookups (UI, retention).
CREATE INDEX backup_jobs_backup_id_idx ON backup_jobs (backup_id);
```

#### `backup_job_steps`

Append-only audit of every step transition, including resume events.

```sql
CREATE TABLE backup_job_steps (
    id             BIGSERIAL PRIMARY KEY,
    job_id         BIGINT NOT NULL REFERENCES backup_jobs(id) ON DELETE CASCADE,
    attempt        INTEGER NOT NULL,
    step           TEXT NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('started','completed','failed','resumed')),
    durable_state  JSONB NOT NULL DEFAULT '{}',
    message        TEXT,
    occurred_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX backup_job_steps_job_id_idx ON backup_job_steps (job_id, occurred_at);
```

#### `backup_schedules` amendment

Add a column to track the `backup_job` that was last enqueued, so the UI can
show "queued but not yet started" separately from "never ran".

```sql
ALTER TABLE backup_schedules
    ADD COLUMN last_job_id BIGINT REFERENCES backup_jobs(id) ON DELETE SET NULL;
```

### Crate layout

#### New crate: `temps-backup-core`

Lives at `crates/temps-backup-core/`. Engine-agnostic. Target: ~600 LOC.
Owns:

- `BackupEngine` trait and associated types (`StepEvent`, `StepCursor`,
  `BackupContext`, `BackupEngineError`).
- `BackupRunner`: the poll-claim-dispatch-persist loop.
- Queue primitives: claim query, lease extension, step persistence, retry
  accounting.
- No HTTP handlers, no notification sending, no retention logic.

`Cargo.toml` dependencies: `sea-orm`, `serde_json`, `async-trait`, `tokio`,
`tokio-stream`, `uuid`, `chrono`, `thiserror`. No dependency on `temps-providers`
(engines live there and depend on `temps-backup-core`, not the other way around).

#### `temps-backup` (existing, slimmed)

Retains: HTTP handlers, `BackupService` entry points for manual triggers,
`start_backup_scheduler` (now an enqueuer only), retention enforcement,
notification dispatch.

Removes (phased): `HeartbeatGuard`, `reconcile_orphan_backups`, inline
`create_backup` / `backup_external_service` execution paths.

Gains: dependency on `temps-backup-core`; instantiates `BackupRunner` and
registers engine impls.

#### `temps-providers/src/externalsvc/*`

Each engine module gains an `impl BackupEngine for <EngineType>`. The old
`ExternalService::backup_to_s3` method is deprecated in Phase 1 and removed
in Phase 5. No new crate boundary is introduced for the engine impls.

### `BackupEngine` trait

```rust
// crates/temps-backup-core/src/engine.rs

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;
use std::sync::Arc;
use sea_orm::DatabaseConnection;

/// Durable cursor passed into `execute` on the first attempt and on every
/// resume. `current_step` is `None` on the first attempt; set to the last
/// completed step's name on a resume. `durable_state` carries whatever the
/// engine persisted in `backup_job_steps.durable_state` at that step.
pub struct StepCursor {
    pub current_step: Option<String>,
    pub durable_state: Value,
}

/// Context passed to every engine call. Contains everything the engine needs
/// to do its work without touching the database directly.
pub struct BackupContext {
    pub job_id: i64,
    pub attempt: i32,
    pub params: Value,
    pub db: Arc<DatabaseConnection>,
}

pub enum StepEvent {
    /// The engine completed a durable step. The runner persists `step` +
    /// `durable_state` atomically before yielding to the next poll, so a
    /// crash after this event is yielded but before the runner flushes is
    /// safe: the engine will see the previous step's cursor on resume.
    StepCompleted {
        step: String,
        durable_state: Value,
        message: Option<String>,
    },
    /// The engine is alive and making progress but has not completed a
    /// step boundary. The runner extends the lease without writing a step
    /// row.
    Heartbeat,
    /// The engine finished successfully. The runner writes
    /// `backups.state='completed'` and real `finished_at`.
    Done {
        location: String,
        size_bytes: Option<i64>,
        compression: String,
    },
}

#[derive(thiserror::Error, Debug)]
pub enum BackupEngineError {
    #[error("Preflight failed for job {job_id}: {reason}")]
    Preflight { job_id: i64, reason: String },
    #[error("Step '{step}' failed for job {job_id}: {reason}")]
    StepFailed { job_id: i64, step: String, reason: String },
    #[error("IO error during job {job_id}: {0}")]
    Io(#[from] std::io::Error),
    #[error("S3 error during job {job_id}: {reason}")]
    S3 { job_id: i64, reason: String },
    #[error("Engine not supported for job {job_id}: {engine}")]
    Unsupported { job_id: i64, engine: String },
}

#[async_trait]
pub trait BackupEngine: Send + Sync {
    /// Machine-readable engine identifier. Must match `backup_jobs.engine`.
    fn engine(&self) -> &'static str;

    /// Ordered list of step names this engine will emit, in execution order.
    /// Used by the runner to validate `StepCompleted` events and by the UI
    /// to render a progress timeline.
    fn steps(&self) -> &'static [&'static str];

    /// Execute (or resume) the backup. The stream must yield at least one
    /// `StepCompleted` or `Heartbeat` event before any wall-clock lease
    /// expiry (default 5 minutes) to prevent the runner from treating the
    /// job as stalled.
    ///
    /// Engines own idempotence at each step boundary. If `cursor.current_step`
    /// is `Some("dump")`, the engine must skip straight to the step after
    /// `"dump"` — re-running `"dump"` after a crash-and-resume must produce
    /// the same artifact as the original run.
    fn execute<'a>(
        &'a self,
        ctx: &'a BackupContext,
        cursor: StepCursor,
    ) -> BoxStream<'a, Result<StepEvent, BackupEngineError>>;

    /// Optional rollback hook. Called when `attempts >= max_attempts` so
    /// the engine can clean up partial uploads. Default is a no-op.
    async fn rollback(
        &self,
        _ctx: &BackupContext,
        _cursor: StepCursor,
    ) -> Result<(), BackupEngineError> {
        Ok(())
    }
}
```

### Claim query

The runner polls on a configurable interval (default 5 seconds). Each poll
issues a single atomic `UPDATE ... RETURNING *` that claims one job. Rotating
`claim_token` on every claim makes duplicate-claim impossible even if two
runners race.

```sql
UPDATE backup_jobs
SET
    state        = 'running',
    attempts     = attempts + 1,
    claim_token  = gen_random_uuid(),
    claimed_by   = $1,
    leased_until = NOW() + ($2 * interval '1 second'),
    started_at   = COALESCE(started_at, NOW()),
    updated_at   = NOW()
WHERE id = (
    SELECT id
    FROM   backup_jobs
    WHERE  state           = 'pending'
      AND  next_attempt_at <= NOW()
    ORDER  BY next_attempt_at
    LIMIT  1
    FOR UPDATE SKIP LOCKED
)
RETURNING *;
```

Parameters: `$1` = `claimed_by` (instance identity string), `$2` = lease TTL
in seconds (default `300`).

The runner issues this query inside `sea_orm::DatabaseConnection::execute_unprepared`
or via `Statement::from_sql_and_values` with `DatabaseBackend::Postgres`, matching
the pattern used in `ch_fanout.rs:214–232`.

### Runner loop

```
loop every POLL_INTERVAL:
    row = claim_one_job(claimed_by, lease_ttl_secs)
    if row is None:
        sleep(POLL_INTERVAL); continue

    cursor = StepCursor {
        current_step: row.step,
        durable_state: row.step_state,
    }
    ctx = BackupContext { job_id: row.id, attempt: row.attempts, params: row.params, db }

    engine = engine_registry.get(row.engine)
    stream = engine.execute(&ctx, cursor)

    for event in stream:
        match event:
            StepCompleted { step, durable_state, message } =>
                persist_step(row.id, row.attempts, step, durable_state, message)
                extend_lease(row.id, row.claim_token, lease_ttl_secs)

            Heartbeat =>
                extend_lease(row.id, row.claim_token, lease_ttl_secs)

            Done { location, size_bytes, compression } =>
                mark_job_completed(row.id, row.backup_id, location, size_bytes, compression)
                break

        Err(engine_error) =>
            if row.attempts >= row.max_attempts:
                engine.rollback(&ctx, cursor)
                mark_job_failed(row.id, engine_error, row.backup_id)
            else:
                schedule_retry(row.id, backoff(row.attempts))
```

`persist_step` is a transaction:
```sql
BEGIN;
UPDATE backup_jobs
   SET step = $1, step_state = $2, leased_until = NOW() + interval '300 seconds', updated_at = NOW()
 WHERE id = $3 AND claim_token = $4;
INSERT INTO backup_job_steps (job_id, attempt, step, state, durable_state, message, occurred_at)
VALUES ($3, $5, $1, 'completed', $2, $6, NOW());
COMMIT;
```

The `claim_token` check in the `WHERE` clause of the `UPDATE` is a fencing
token: a stale runner that was presumed dead and had its job re-claimed cannot
overwrite the new owner's progress.

### Backoff schedule

```
attempt 1 → next_attempt_at = NOW() + 1 min
attempt 2 → next_attempt_at = NOW() + 5 min
attempt 3 → next_attempt_at = NOW() + 20 min
```

Formula: `min(1 * 5^(attempt-1), 60) minutes`, capped at 60 minutes.
`max_attempts` defaults to 3; schedulers may override per engine via `params`.

### Lease duration

Default `leased_until = NOW() + 5 minutes`. Engines that contain a step known
to be long (e.g., `walg_push` for multi-GB databases) emit `Heartbeat` events
at most every 2 minutes to keep the lease alive. The runner never extends the
lease in the background — extension is explicit, driven by engine events.

The runner's claim poll also checks for stale leases:

```sql
WHERE state = 'pending' AND next_attempt_at <= NOW()
-- OR: expired lease on running row
UNION ALL
SELECT id FROM backup_jobs
WHERE state = 'running' AND leased_until < NOW()
LIMIT 1 FOR UPDATE SKIP LOCKED
```

This union query replaces the boot-time reconcile: any row whose lease expired
is fair game for the next claim poll without a server restart.

### Scheduler as enqueuer

`process_backup_schedule` is rewritten to a single transactional INSERT pair:

```sql
BEGIN;
UPDATE backup_schedules
   SET next_run = $1, last_run = NOW(), last_job_id = $3
 WHERE id = $2;
INSERT INTO backup_jobs (backup_id, engine, target_kind, target_id, params, next_attempt_at)
VALUES ($4, $5, $6, $7, $8, NOW())
RETURNING id;
COMMIT;
```

The scheduler never calls `create_backup` or `backup_external_service` inline.
It never `.await`s backup execution. A scheduler tick that enqueues 50 schedules
takes milliseconds; the actual work happens in the runner loop, which runs
concurrently.

### Concurrency caps

Two independent limits govern parallelism:

| Setting | Default | Scope |
|---|---|---|
| `TEMPS_BACKUP_RUNNER_MAX_CONCURRENT` | `4` | Global: max parallel claims across all engines |
| Per-engine `max_concurrent` in `params` | `1` | Per-engine: prevents double-backup of the same target |

The runner spawns `min(RUNNER_MAX_CONCURRENT, claimed_count)` tasks per poll
cycle. The per-engine cap is enforced by the claim query: a second `WHERE`
clause filters out engines already running at their concurrency limit.

```sql
AND (
    SELECT COUNT(*) FROM backup_jobs j2
    WHERE j2.engine = backup_jobs.engine
      AND j2.state  = 'running'
) < COALESCE((backup_jobs.params->>'max_concurrent')::int, 1)
```

### Cancellation

`UPDATE backup_jobs SET state = 'cancelled' WHERE id = $1` is the API surface.
The runner checks for cancellation at each `StepCompleted` / `Heartbeat` event
by re-fetching the row state. If `state = 'cancelled'`, the runner aborts the
stream and calls `engine.rollback`. A `tokio_util::sync::CancellationToken` is
threaded through `BackupContext` so engine implementations can respond to
cancellation mid-step without polling the database.

```rust
pub struct BackupContext {
    pub job_id: i64,
    pub attempt: i32,
    pub params: Value,
    pub db: Arc<DatabaseConnection>,
    pub cancel: tokio_util::sync::CancellationToken,
}
```

### Relationship to existing `backups` and `external_service_backups`

`backups` and `external_service_backups` remain the canonical records that
users and the UI interact with: they hold the final `state`, `s3_location`,
`size_bytes`, `compression_type`, and `finished_at`. `backup_jobs` is
implementation detail: the machinery that drives execution and retry. The
runner writes final state back to the parent row on `Done` or terminal
`failed`; the UI reads from `backups`, not `backup_jobs`.

`external_service_backups` continues to receive one row per completed
external-service backup (written by the engine on `Done`). `backup_jobs` is
a sibling, not a replacement.

### Per-engine step definitions

| Engine (`backup_jobs.engine`) | Steps |
|---|---|
| `control_plane` | `preflight` → `pg_dumpall` → `upload` → `metadata` |
| `postgres_pgdump` | `preflight` → `dump` → `upload` → `metadata` |
| `postgres_walg` | `preflight` → `walg_push` → `record_lsn` → `metadata` |
| `postgres_cluster` | `find_primary` → `preflight` → `walg_push` → `record_lsn` → `metadata` |
| `redis` | `preflight` → `trigger_bgsave` → `wait_for_rdb` → `upload_rdb` → `metadata` |
| `mongodb` | `preflight` → `mongodump` → `upload` → `metadata` |
| `s3_mirror` | `list_source` → `sync` → `metadata` |
| `rustfs` | `preflight` → `snapshot` → `upload` → `metadata` |

`steps()` must return these in execution order. The runner uses this list to
validate that a resumed cursor's `current_step` is a known step name and to
render the progress timeline in the UI.

### Where the runner runs

In-process for Phase 0–4: the runner is started as a `tokio::spawn`-ed task
inside the existing `BackupPlugin::start` method, alongside the scheduler.

The runner is designed to be stateless with respect to the database — it can
run on any node that has a database connection. A future phase can move it to
a dedicated worker process or to per-node agents without changing the
`BackupEngine` trait or the schema. That extraction is explicitly out of scope
for this ADR.

## Consequences

### Positive

- `finished_at` is stamped by the runner at the exact moment of `Done` or
  terminal failure, not at the next boot. Duration metrics are accurate.
- `error_message` reflects the actual last step the engine reached and the
  actual error it returned, not a canned "worker process died" string.
- Scheduler ticks never block: the scheduler only writes rows. A hung backup
  cannot prevent other schedules from firing.
- A crash during backup no longer loses work. After lease expiry (≤ 5 minutes),
  the next runner tick picks up the job and passes the last completed step's
  cursor to the engine, which resumes from there.
- `backup_job_steps` provides a complete audit trail: operators can see
  exactly which step each attempt reached, its durable state, and any message.
- Multiple runner instances (e.g., one per worker node) can process jobs
  concurrently without coordination beyond the claim query.
- `reconcile_orphan_backups` is eliminated (Phase 5). Boot-time sweep is
  replaced by lease-expiry reclaim, which is continuous rather than
  one-shot.

### Negative

- Two new tables and a new crate add schema migration surface. The migration
  must be coordinated with a deploy; it is additive-only (no column drops)
  until Phase 5.
- Engine authors must decompose their monolithic `backup_to_s3` into step
  boundaries and implement idempotence per step. The Redis `trigger_bgsave`
  → `wait_for_rdb` step, for example, must be reentrant: if the runner
  crashes while waiting for the RDB file, the next attempt must re-trigger
  bgsave rather than assuming the previous one is still pending.
- The `BackupEngine` stream contract requires engines to emit a `Heartbeat`
  or `StepCompleted` within the lease TTL. An engine that has no natural
  checkpoints (e.g., a single-step WAL-G push taking 45 minutes) must emit
  periodic `Heartbeat` events explicitly.
- The in-process runner is still subject to server downtime — it simply
  recovers via lease expiry rather than requiring a restart. Downtime longer
  than `leased_until` causes a 5-minute gap in backup execution, not a
  complete loss of scheduled windows.

### Code removed (Phase 5)

- `crates/temps-backup/src/services/heartbeat.rs` — entire file.
- `crates/temps-backup/src/services/reconcile.rs` — entire file.
- `ExternalService::backup_to_s3` default impl in
  `crates/temps-providers/src/externalsvc/mod.rs:832–844`.
- `BackupService::backup_external_service`
  (`backup.rs:4213`) — replaced by `BackupRunner` dispatch.
- `backups.last_heartbeat_at` column — migration drops it.

## Alternatives Considered

### Option A: Keep in-process, add per-backup timeouts only

Add `tokio::time::timeout` around each `create_backup` call in the scheduler
and add a `tokio::spawn` to stop one backup from wedging the loop.

**Pros:** Minimal code change. Unblocks the scheduler wedge issue.

**Cons:** Does not fix fabricated `finished_at`. Does not provide resume after
crash — the next invocation still starts from scratch. Does not provide
per-step visibility. The "stuck running forever" problem becomes "stuck running
until timeout, then marked failed with a timeout message and retried from the
beginning." For large databases this is not meaningfully better than the status
quo. Rejected because it treats symptoms without fixing the root cause.

### Option B: Extract to a separate worker binary today

Move all backup execution to a standalone `temps-backup-worker` binary with a
queue in Postgres. The main server only enqueues; the worker only consumes.

**Pros:** Complete isolation — server restarts do not affect in-flight backups.
True multi-node fan-out without code changes.

**Cons:** Significant ops burden for a self-hosted product. Every `deploy.sh`
and `worker.sh` invocation must now manage two binaries. Documentation doubles.
The multi-node story does not actually require a separate binary — the in-process
runner can be stateless (and it is, by design here). Rejected for this release;
the design explicitly leaves the door open by keeping the runner stateless. A
future ADR can extract it.

### Option C: Per-engine state machines without a central queue

Each engine maintains its own state machine in a table: `redis_backup_state`,
`postgres_backup_state`, etc. A per-engine scheduler polls its own table.

**Pros:** Engines are fully decoupled. Schema changes to one engine do not touch
others.

**Cons:** N copies of claim logic, retry logic, lease logic, and cancellation
logic — one per engine. No unified `backup_job_steps` audit trail. Cross-engine
queries ("show me all running backups") require N-way `UNION` queries. Adding a new engine
means duplicating all the machinery. The existing `ExternalService::backup_to_s3`
already proved that a monolithic function per engine without shared infrastructure
is where the current problems originate. Rejected.

### Option D: Use an external job queue (Redis, SQS, NATS)

Enqueue jobs via an external message broker. Workers subscribe and consume.

**Pros:** Battle-tested retry semantics. Natural multi-node fan-out.

**Cons:** Introduces a new required infrastructure dependency. Temps is
explicitly a single-binary, single-database product. The claim-based Postgres
queue is a well-understood pattern (used already in `ch_fanout.rs`) with no
additional operational surface. Rejected.

## Open Questions

The following questions were open during design. Each carries a recommendation
that the implementer should treat as the default unless there is a specific
reason to deviate.

**Q1: Global concurrency cap default.**
Recommendation: `4`. Rationale: a single-node Hetzner CPX21 (4 vCPU) can
sustain 4 concurrent backup streams without saturating disk I/O, and most
Temps Cloud deployments are single-node. Expose as `TEMPS_BACKUP_RUNNER_MAX_CONCURRENT`.

**Q2: Lease duration for known-slow steps.**
Recommendation: keep the lease TTL uniform at 5 minutes and require engines to
emit `Heartbeat` events if a step takes longer. Do not introduce per-step TTL
overrides in Phase 0–4; they add schema and logic complexity for a marginal
benefit. Revisit only if a specific engine cannot emit heartbeats within 5
minutes (e.g., WAL-G push on a 100 GB database over a slow link).

**Q3: Cancellation — does the API block until the engine acknowledges?**
Recommendation: no. `PATCH /backup-jobs/{id}/cancel` writes `state='cancelled'`
and returns immediately. The runner acknowledges cancellation on its next
heartbeat check (within the lease TTL). The UI shows `cancelling` state (a
`state='running'` row with a `cancelled` flag in `params`) until the runner
confirms. Add a `cancellation_requested_at` column to `backup_jobs` rather than
overloading `state`.

**Q4: Relationship between `backup_jobs` and `backups`/`external_service_backups` during migration.**
Recommendation: during Phases 1–4, both the old path and the new path write to
`backups`/`external_service_backups`. The old path is gated behind
`TEMPS_BACKUP_RUNNER_ENABLED=false` (the default until Phase 3). The new path is
gated behind `TEMPS_BACKUP_RUNNER_ENABLED=true`. There is never a moment where
both paths run simultaneously for the same backup target.

**Q5: Where does the runner run — and when does it move?**
Recommendation: in-process for all phases in this ADR. A future ADR should
consider per-node agent dispatch once the multi-node worker story (ADR-008, `temps
agent`) stabilises. The runner is already stateless; the only change needed is
wiring the runner into the agent's plugin set.

**Q6: Backoff schedule — exponential or fixed?**
Recommendation: exponential with a 60-minute ceiling, as specified in the Decision
section. Fixed backoff is simpler but causes thundering-herd retries when many
schedules fail simultaneously (e.g., S3 unreachable). The ceiling prevents
indefinite postponement.

## Migration Plan

Each phase is independently deployable and reversible. A rollback of any phase
does not require dropping tables (only disabling the flag).

### Phase 0: Schema + scaffolding (no engines wired)

**What changes:**
- Migration adds `backup_jobs`, `backup_job_steps`, and
  `backup_schedules.last_job_id`.
- New crate `temps-backup-core` is created with `BackupEngine` trait,
  `StepCursor`, `StepEvent`, `BackupEngineError`, `BackupContext`, and
  `BackupRunner` struct with the claim query. No engines registered.
- `BackupPlugin` registers the runner but immediately returns if
  `TEMPS_BACKUP_RUNNER_ENABLED` is not set.

**What's reversible:** Drop the three schema objects and remove the crate.
No existing code paths are touched.

**Acceptance criteria:** `cargo check --lib` passes. Migration applies cleanly
against an existing production database. No test regressions.

### Phase 1: Control-plane backup as pilot engine

**What changes:**
- `ControlPlaneEngine` (in `crates/temps-backup/src/engines/control_plane.rs`)
  implements `BackupEngine` with steps `preflight → pg_dumpall → upload → metadata`.
- `BackupPlugin` registers `ControlPlaneEngine` with the runner.
- `BackupService::create_backup` gains a branch: when
  `TEMPS_BACKUP_RUNNER_ENABLED=true`, it inserts a `backup_jobs` row and returns
  immediately; when `false`, it executes inline (old path unchanged).
- Smoke test: trigger a manual backup, verify `backup_job_steps` rows are written,
  verify `backups.finished_at` matches the real end time.

**What's reversible:** `TEMPS_BACKUP_RUNNER_ENABLED=false` falls back to old
path. No old code removed.

**Acceptance criteria:** `backup_job_steps` shows 4 step rows for a completed
control-plane backup. `finished_at` is within 1 second of the `Done` event time.
Crash test: kill the server mid-upload, restart, verify the runner resumes from
`upload` step rather than re-running `pg_dumpall`.

### Phase 2: Redis engine + crash-resume integration test

**What changes:**
- `RedisEngine` implements `BackupEngine` with steps
  `preflight → trigger_bgsave → wait_for_rdb → upload_rdb → metadata`.
- `BackupService::backup_external_service` branches on
  `TEMPS_BACKUP_RUNNER_ENABLED` the same way as Phase 1.
- Integration test: simulate a crash between `wait_for_rdb` and `upload_rdb`,
  verify the engine receives `StepCursor { current_step: Some("wait_for_rdb"), ... }`
  on resume and skips straight to `upload_rdb`.

**What's reversible:** flag off restores old path.

**Acceptance criteria:** crash-resume test passes. Redis backup in `backup_job_steps`
shows `state='resumed'` row on the retried attempt. No duplicate RDB uploads.

### Phase 3: Postgres engines (pg_dump, WAL-G, cluster)

**What changes:**
- `PostgresPgDumpEngine`, `PostgresWalgEngine`, `PostgresClusterEngine` each
  implement `BackupEngine`.
- All three registered with the runner.
- Scheduler's `process_backup_schedule` is rewritten to enqueue-only (no inline
  `.await` of backup execution). Old inline path removed behind the flag.

**What's reversible:** flag off. Note: after this phase, the scheduler will only
enqueue when the flag is on; when the flag is off, scheduled Postgres backups
no longer fire. This is acceptable because Phase 3 targets staging environments
before the flag is flipped in production.

**Acceptance criteria:** a 3-schedule stress test (all fire at the same minute)
completes all three without the scheduler wedging. `backup_job_steps` for a
WAL-G run shows `record_lsn.durable_state` containing the LSN.

### Phase 4: MongoDB, S3 mirror, RustFS engines

**What changes:**
- `MongodbEngine`, `S3MirrorEngine`, `RustFsEngine` implement `BackupEngine`.
- All registered with the runner.
- `TEMPS_BACKUP_RUNNER_ENABLED` becomes the default `true` in
  `crates/temps-config/src/lib.rs`.

**What's reversible:** `TEMPS_BACKUP_RUNNER_ENABLED=false` still works. Old
inline paths are still present.

**Acceptance criteria:** each engine's backup completes end-to-end with runner.
S3 mirror `sync` step stores the list of synced keys in `durable_state` so a
partial mirror resumes from the last synced key.

### Phase 5: Decommission old infrastructure

**What changes:**
- `HeartbeatGuard` removed (`heartbeat.rs` deleted).
- `reconcile_orphan_backups` removed (`reconcile.rs` deleted).
- `ExternalService::backup_to_s3` default impl removed; the method is removed
  from the trait entirely (each engine's backup is now in `BackupEngine`).
- `BackupService::backup_external_service` inline path removed.
- `BackupService::create_backup` inline path removed; method becomes a thin
  wrapper that enqueues only.
- Migration drops `backups.last_heartbeat_at`.
- `TEMPS_BACKUP_RUNNER_ENABLED` flag removed; runner is always on.

**What's reversible:** this phase is not reversible without restoring the deleted
code. Run it only after Phase 4 has been in production for at least two weeks
with no regressions.

**Acceptance criteria:** `cargo check --lib` passes with no references to
`HeartbeatGuard`, `reconcile_orphan_backups`, or `backup_to_s3` (verify via
`grep -r` in CI). Migration applies cleanly.

## References

- `crates/temps-backup/src/services/backup.rs:510` — `create_backup` entry point
- `crates/temps-backup/src/services/backup.rs:4213` — `backup_external_service` entry point
- `crates/temps-backup/src/services/backup.rs:4468` — `start_backup_scheduler` (sequential loop)
- `crates/temps-backup/src/services/backup.rs:4572` — `process_scheduled_backups` (inline `.await`)
- `crates/temps-backup/src/services/backup.rs:4636–4643` — inline `create_backup` call inside scheduler
- `crates/temps-backup/src/services/heartbeat.rs:29–75` — `HeartbeatGuard` (liveness only, no progress)
- `crates/temps-backup/src/services/reconcile.rs:17–19` — `ORPHAN_REASON` canned message
- `crates/temps-backup/src/services/reconcile.rs:27–83` — `reconcile_orphan_backups` (stamps `finished_at = now()`)
- `crates/temps-backup/src/services/restore.rs:817–826` — `tokio::spawn` pattern for restore workers
- `crates/temps-providers/src/externalsvc/mod.rs:719` — `ExternalService` trait definition
- `crates/temps-providers/src/externalsvc/mod.rs:832–844` — `backup_to_s3` default impl
- `crates/temps-analytics-events/src/services/ch_fanout.rs:214–232` — `FOR UPDATE SKIP LOCKED` claim pattern
- ADR-008 — PTY agent (`008-pty-agent.md`) — future home of per-node runner dispatch
- ADR-010 — Provider boundary traits (`010-provider-boundary-traits.md`) — same trait-per-domain pattern applied here to `BackupEngine`
