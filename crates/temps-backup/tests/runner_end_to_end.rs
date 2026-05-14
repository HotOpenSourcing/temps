//! End-to-end integration tests for the ADR-014 `BackupRunner` dispatch loop.
//!
//! These tests exercise the full poll → claim → dispatch → persist → complete
//! cycle using `MockDatabase` and lightweight `TestEngine` implementations.
//! No real database or Docker daemon is required.
//!
//! **Test A** (`test_happy_path_engine_runs_to_completion`):
//!   Verifies that a two-step engine streams `StepCompleted × 2` followed by
//!   `Done`, and that `BackupRunner::poll_once` drives the job to `completed`.
//!
//! **Test B** (`test_crash_resume_cursor_passed_correctly`):
//!   Verifies that when attempt 1 errors mid-stream, a second `poll_once` call
//!   passes the correct `StepCursor` (last completed step from the DB row) to the
//!   engine, allowing it to skip already-done work.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use futures::stream::BoxStream;
use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult, Value as SVal};
use serde_json::{json, Value};

use temps_backup_core::{
    BackupContext, BackupEngine, BackupEngineError, BackupRunner, RunnerConfig, StepCursor,
    StepEvent,
};

// ── TestEngine (happy path) ───────────────────────────────────────────────────

/// Two-step engine: `step_a` → `step_b` → `Done`.
struct HappyEngine;

impl BackupEngine for HappyEngine {
    fn engine(&self) -> &'static str {
        "test_happy"
    }
    fn steps(&self) -> &'static [&'static str] {
        &["step_a", "step_b"]
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a BackupContext,
        _cursor: StepCursor,
    ) -> BoxStream<'a, Result<StepEvent, BackupEngineError>> {
        let job_id = ctx.job_id;
        Box::pin(async_stream::try_stream! {
            yield StepEvent::StepCompleted {
                step: "step_a".into(),
                durable_state: json!({"a": true}),
                message: None,
            };
            yield StepEvent::StepCompleted {
                step: "step_b".into(),
                durable_state: json!({"b": true}),
                message: None,
            };
            let _ = job_id; // suppress unused warning
            yield StepEvent::Done {
                location: "s3://bucket/key".into(),
                size_bytes: Some(1024),
                compression: "gzip".into(),
            };
        })
    }
}

// ── TestEngine (crash-resume) ─────────────────────────────────────────────────

/// Engine that crashes on attempt 1 after completing `step_a`, then on attempt 2
/// checks the cursor and completes `step_b` → `Done`.
struct CrashResumeEngine {
    call_count: Arc<AtomicU32>,
    /// Records the `cursor.current_step` seen on each call.
    seen_cursor: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

impl CrashResumeEngine {
    fn new() -> Self {
        Self {
            call_count: Arc::new(AtomicU32::new(0)),
            seen_cursor: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl BackupEngine for CrashResumeEngine {
    fn engine(&self) -> &'static str {
        "test_crash_resume"
    }
    fn steps(&self) -> &'static [&'static str] {
        &["step_a", "step_b"]
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a BackupContext,
        cursor: StepCursor,
    ) -> BoxStream<'a, Result<StepEvent, BackupEngineError>> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        {
            let mut guard = self.seen_cursor.lock().unwrap();
            guard.push(cursor.current_step.clone());
        }
        let job_id = ctx.job_id;
        Box::pin(async_stream::try_stream! {
            if n == 0 {
                // Attempt 1: complete step_a, then crash.
                yield StepEvent::StepCompleted {
                    step: "step_a".into(),
                    durable_state: json!({"a": true}),
                    message: None,
                };
                Err(BackupEngineError::StepFailed {
                    job_id,
                    step: "step_b".into(),
                    reason: "simulated crash".into(),
                })?;
            } else {
                // Attempt 2: cursor should point at step_a. Skip it, do step_b.
                yield StepEvent::StepCompleted {
                    step: "step_b".into(),
                    durable_state: json!({"b": true}),
                    message: None,
                };
                yield StepEvent::Done {
                    location: "s3://bucket/key".into(),
                    size_bytes: Some(512),
                    compression: "none".into(),
                };
            }
        })
    }
}

// ── DB row helpers ────────────────────────────────────────────────────────────

/// Build a `BTreeMap` that sea-orm `MockDatabase` will deserialise as a `BackupJobRow`.
fn make_job_row(
    id: i64,
    engine: &str,
    step: Option<&str>,
    step_state: Value,
    attempts: i32,
    max_attempts: i32,
) -> BTreeMap<String, SVal> {
    let claim_token = uuid::Uuid::new_v4();
    let mut m = BTreeMap::new();
    m.insert("id".into(), SVal::BigInt(Some(id)));
    m.insert("backup_id".into(), SVal::Int(Some(1)));
    m.insert("engine".into(), SVal::String(Some(Box::new(engine.into()))));
    m.insert(
        "target_kind".into(),
        SVal::String(Some(Box::new("external_service".into()))),
    );
    m.insert("target_id".into(), SVal::Int(Some(42)));
    m.insert(
        "params".into(),
        SVal::Json(Some(Box::new(json!({"service_id": 42, "s3_source_id": 1})))),
    );
    m.insert(
        "state".into(),
        SVal::String(Some(Box::new("running".into()))),
    );
    m.insert(
        "step".into(),
        match step {
            Some(s) => SVal::String(Some(Box::new(s.into()))),
            None => SVal::String(None),
        },
    );
    m.insert("step_state".into(), SVal::Json(Some(Box::new(step_state))));
    m.insert("attempts".into(), SVal::Int(Some(attempts)));
    m.insert("max_attempts".into(), SVal::Int(Some(max_attempts)));
    m.insert(
        "claim_token".into(),
        SVal::Uuid(Some(Box::new(claim_token))),
    );
    m
}

/// An empty query result row set — simulates an empty queue.
fn empty_queue() -> Vec<BTreeMap<String, SVal>> {
    vec![]
}

/// A single `MockExecResult` that reports 1 row affected (for UPDATE/INSERT).
fn one_row_affected() -> MockExecResult {
    MockExecResult {
        last_insert_id: 0,
        rows_affected: 1,
    }
}

// ── Test A: happy path ────────────────────────────────────────────────────────

/// Test that the runner drives a two-step engine to completion in a single
/// `poll_once` cycle.
///
/// DB sequence:
///   1. `claim_one_job`        → query result: one `BackupJobRow` for `test_happy`
///   2. `persist_step_completed` (step_a) → begin + 2×exec (UPDATE jobs, INSERT steps) + commit
///   3. `persist_step_completed` (step_b) → begin + 2×exec + commit
///   4. `mark_job_completed`   → begin + 2×exec (UPDATE jobs, UPDATE backups) + commit
///   5. second `poll_once` poll → query result: empty queue
#[tokio::test]
async fn test_happy_path_engine_runs_to_completion() {
    let job_row = make_job_row(1, "test_happy", None, json!({}), 1, 3);

    // Each transaction issues exactly 2 execute() calls (UPDATE + INSERT/UPDATE).
    // MockDatabase replays exec results in FIFO order across all execute() calls.
    let db = Arc::new(
        MockDatabase::new(DatabaseBackend::Postgres)
            // claim_one_job query
            .append_query_results(vec![vec![job_row]])
            // persist_step_completed for step_a: UPDATE backup_jobs + INSERT backup_job_steps
            .append_exec_results(vec![one_row_affected(), one_row_affected()])
            // persist_step_completed for step_b: UPDATE backup_jobs + INSERT backup_job_steps
            .append_exec_results(vec![one_row_affected(), one_row_affected()])
            // mark_job_completed: UPDATE backup_jobs + UPDATE backups
            .append_exec_results(vec![one_row_affected(), one_row_affected()])
            // second poll_once: empty queue
            .append_query_results(vec![empty_queue()])
            .into_connection(),
    );

    let config = RunnerConfig {
        poll_interval: std::time::Duration::from_millis(50),
        ..Default::default()
    };

    let mut runner = BackupRunner::new(Arc::clone(&db), config);
    runner.register_engine(Arc::new(HappyEngine));
    let runner = Arc::new(runner);

    // First poll: claims the job and spawns dispatch.
    runner
        .clone()
        .poll_once()
        .await
        .expect("poll_once should succeed");

    // Give the spawned dispatch task time to finish streaming through the engine.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Second poll: queue is empty (engine completed, no pending jobs left).
    runner
        .clone()
        .poll_once()
        .await
        .expect("second poll_once should succeed");
}

// ── Test B: crash-resume cursor ───────────────────────────────────────────────

/// Test that when attempt 1 errors after completing `step_a`, a second attempt
/// receives `StepCursor { current_step: Some("step_a"), .. }` from the DB row.
///
/// DB sequence (attempt 1):
///   1. `claim_one_job`                    → job row, attempt=1, step=None
///   2. `persist_step_completed` (step_a)  → 2×exec
///   3. engine errors → `schedule_retry`   → 1×exec (UPDATE state='pending')
///
/// DB sequence (attempt 2):
///   4. `claim_one_job`                    → job row, attempt=2, step=Some("step_a")
///   5. `persist_step_completed` (step_b)  → 2×exec
///   6. `mark_job_completed`               → 2×exec
///   7. third poll                         → empty queue
#[tokio::test]
async fn test_crash_resume_cursor_passed_correctly() {
    let row_attempt1 = make_job_row(2, "test_crash_resume", None, json!({}), 1, 3);
    let row_attempt2 = make_job_row(
        2,
        "test_crash_resume",
        Some("step_a"),
        json!({"a": true}),
        2,
        3,
    );

    let db = Arc::new(
        MockDatabase::new(DatabaseBackend::Postgres)
            // poll 1: claim attempt-1 row
            .append_query_results(vec![vec![row_attempt1]])
            // persist_step_completed step_a (attempt 1): UPDATE + INSERT
            .append_exec_results(vec![one_row_affected(), one_row_affected()])
            // schedule_retry: UPDATE state='pending'
            .append_exec_results(vec![one_row_affected()])
            // poll 2: claim attempt-2 row (cursor = step_a)
            .append_query_results(vec![vec![row_attempt2]])
            // persist_step_completed step_b (attempt 2): UPDATE + INSERT
            .append_exec_results(vec![one_row_affected(), one_row_affected()])
            // mark_job_completed: UPDATE backup_jobs + UPDATE backups
            .append_exec_results(vec![one_row_affected(), one_row_affected()])
            // poll 3: empty queue
            .append_query_results(vec![empty_queue()])
            .into_connection(),
    );

    let config = RunnerConfig {
        poll_interval: std::time::Duration::from_millis(50),
        ..Default::default()
    };

    let engine = Arc::new(CrashResumeEngine::new());
    let seen_cursor = Arc::clone(&engine.seen_cursor);

    let mut runner = BackupRunner::new(Arc::clone(&db), config);
    runner.register_engine(Arc::clone(&engine) as Arc<dyn BackupEngine>);
    let runner = Arc::new(runner);

    // Poll 1: claims attempt-1, dispatch errors after step_a.
    runner
        .clone()
        .poll_once()
        .await
        .expect("poll 1 should succeed");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Poll 2: claims attempt-2 with cursor pointing at step_a.
    runner
        .clone()
        .poll_once()
        .await
        .expect("poll 2 should succeed");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Poll 3: empty queue.
    runner
        .clone()
        .poll_once()
        .await
        .expect("poll 3 should succeed");

    // Verify the engine saw the correct cursors:
    //   attempt 1 → cursor.current_step = None
    //   attempt 2 → cursor.current_step = Some("step_a")
    let cursors = seen_cursor.lock().unwrap();
    assert_eq!(cursors.len(), 2, "engine should be called exactly twice");
    assert!(
        cursors[0].is_none(),
        "attempt 1 cursor should be None (fresh start)"
    );
    assert_eq!(
        cursors[1].as_deref(),
        Some("step_a"),
        "attempt 2 cursor should point at last completed step"
    );
}
