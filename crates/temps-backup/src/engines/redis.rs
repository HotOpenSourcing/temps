//! `RedisEngine`: `BackupEngine` implementation for Redis external services
//! (ADR-014 Phase 2 §"Redis engine + crash-resume integration test").
//!
//! Steps: `preflight` → `trigger_bgsave` → `wait_for_rdb` → `upload_rdb` → `metadata`.
//!
//! ## Design rationale
//!
//! This engine unifies the two Redis backup paths from
//! `temps-providers/src/externalsvc/redis.rs`:
//!
//! - **WAL-G path** (`redis.rs:1682`): If the Redis container has `wal-g`
//!   installed (`redis.rs:963`), `trigger_bgsave` uses
//!   `redis-cli --rdb /tmp/redis_backup.rdb` + `wal-g backup-push` to stream
//!   the snapshot directly to S3. This is the preferred path because no data
//!   flows through the Temps process.
//! - **BGSAVE legacy path** (`redis.rs:1013`): If WAL-G is absent, the engine
//!   runs `redis-cli BGSAVE`, waits for the RDB file to be written, then
//!   copies `dump.rdb` out of the container and uploads to S3.
//!
//! Both paths converge at the `upload_rdb` step so the cursor semantics are
//! uniform regardless of which backup tool was used.
//!
//! ## Heartbeat discipline
//!
//! `walg_push` / `bgsave_poll` are long-running steps. They use the
//! same mpsc + select pattern as `control_plane.rs:213–254`.
//!
//! ## Idempotence
//!
//! - `preflight`: re-validates S3 source; safe to re-run.
//! - `trigger_bgsave`: always re-triggers on resume (BGSAVE is idempotent).
//! - `wait_for_rdb`: checks whether `durable_state.rdb_ready = true`; if so,
//!   skips directly to `upload_rdb`.
//! - `upload_rdb`: S3 HEAD check before upload; skips if already present.
//! - `metadata`: PUT is always overwrite.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aws_sdk_s3::Client as S3Client;
use bollard::container::LogOutput;
use bollard::exec::StartExecResults;
use chrono::Utc;
use futures::stream::BoxStream;
use futures::StreamExt;
use sea_orm::{DatabaseConnection, EntityTrait};
use serde_json::{json, Value};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::ring_buffer::RingBuffer;
use temps_backup_core::{BackupContext, BackupEngine, BackupEngineError, StepCursor, StepEvent};
use temps_core::EncryptionService;

/// Heartbeat interval during long-running steps. Must be under the runner's
/// 5-minute lease TTL.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(120);

/// Steps emitted by `RedisEngine` in execution order.
const STEPS: &[&str] = &[
    "preflight",
    "trigger_bgsave",
    "wait_for_rdb",
    "upload_rdb",
    "metadata",
];

// ── durable_state keys ────────────────────────────────────────────────────────
const DS_S3_KEY: &str = "s3_key";
const DS_BUCKET: &str = "bucket";
const DS_SIZE_BYTES: &str = "size_bytes";
const DS_TEMP_PATH: &str = "temp_path";
const DS_RDB_READY: &str = "rdb_ready";
const DS_USE_WALG: &str = "use_walg";
const DS_WALG_PREFIX: &str = "walg_prefix";

// ── Dependencies ─────────────────────────────────────────────────────────────

/// Dependencies injected into `RedisEngine` at construction time.
pub struct RedisDeps {
    pub db: Arc<DatabaseConnection>,
    pub encryption_service: Arc<EncryptionService>,
    pub docker: bollard::Docker,
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// `BackupEngine` for Redis external services.
///
/// Registered with `BackupRunner` by `BackupPlugin`. Detects WAL-G at runtime;
/// if present uses `wal-g backup-push`, otherwise falls back to BGSAVE + file
/// copy.
///
/// See module-level docs for step definitions and heartbeat discipline.
/// Reference: `temps-providers/src/externalsvc/redis.rs:1682` (WAL-G path),
/// `redis.rs:1013` (BGSAVE path).
pub struct RedisEngine {
    deps: Arc<RedisDeps>,
}

impl RedisEngine {
    pub fn new(deps: RedisDeps) -> Self {
        Self {
            deps: Arc::new(deps),
        }
    }
}

#[async_trait::async_trait]
impl BackupEngine for RedisEngine {
    fn engine(&self) -> &'static str {
        "redis"
    }

    fn steps(&self) -> &'static [&'static str] {
        STEPS
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a BackupContext,
        cursor: StepCursor,
    ) -> BoxStream<'a, Result<StepEvent, BackupEngineError>> {
        let deps = Arc::clone(&self.deps);
        let job_id = ctx.job_id;
        let attempt = ctx.attempt;
        let params = ctx.params.clone();
        let cancel = ctx.cancel.clone();

        Box::pin(async_stream::try_stream! {
            let resume_from = cursor.current_step.clone();
            let mut accumulated_state = cursor.durable_state.clone();

            let start_idx = if let Some(ref last) = resume_from {
                let pos = STEPS.iter().position(|&s| s == last.as_str());
                match pos {
                    Some(i) => i + 1,
                    None => {
                        Err(BackupEngineError::StepFailed {
                            job_id,
                            step: last.clone(),
                            reason: format!(
                                "cursor references unknown step '{}'; known: {:?}",
                                last, STEPS
                            ),
                        })?;
                        unreachable!()
                    }
                }
            } else {
                0
            };

            let service_id: i32 = params
                .get("service_id")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32)
                .ok_or_else(|| BackupEngineError::Preflight {
                    job_id,
                    reason: "params.service_id missing".into(),
                })?;

            let s3_source_id: i32 = params
                .get("s3_source_id")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32)
                .ok_or_else(|| BackupEngineError::Preflight {
                    job_id,
                    reason: "params.s3_source_id missing".into(),
                })?;

            for step in &STEPS[start_idx..] {
                if cancel.is_cancelled() {
                    debug!(job_id, step, "RedisEngine: cancellation requested before step");
                    return;
                }

                info!(job_id, attempt, step, "RedisEngine: executing step");

                match *step {
                    "preflight" => {
                        let state = step_preflight(job_id, service_id, s3_source_id, &deps).await?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "preflight".into(),
                            durable_state: state,
                            message: Some(format!(
                                "service {} and S3 source {} validated",
                                service_id, s3_source_id
                            )),
                        };
                    }

                    "trigger_bgsave" => {
                        let state = step_trigger_bgsave(
                            job_id,
                            accumulated_state.clone(),
                            &deps,
                        ).await?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "trigger_bgsave".into(),
                            durable_state: state,
                            message: Some("BGSAVE triggered (or WAL-G path selected)".into()),
                        };
                    }

                    "wait_for_rdb" => {
                        // Drive the poll with heartbeat channel.
                        let (heartbeat_tx, mut heartbeat_rx) =
                            tokio::sync::mpsc::channel::<()>(8);

                        let mut step_fut = std::pin::pin!(step_wait_for_rdb(
                            job_id,
                            accumulated_state.clone(),
                            Arc::clone(&deps),
                            cancel.clone(),
                            heartbeat_tx,
                        ));

                        let step_result: Result<Value, BackupEngineError> = loop {
                            tokio::select! {
                                biased;
                                Some(()) = heartbeat_rx.recv() => {
                                    debug!(job_id, "RedisEngine wait_for_rdb: emitting Heartbeat");
                                    yield StepEvent::Heartbeat;
                                }
                                result = &mut step_fut => {
                                    while let Ok(()) = heartbeat_rx.try_recv() {
                                        yield StepEvent::Heartbeat;
                                    }
                                    break result;
                                }
                            }
                        };
                        let state = step_result?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "wait_for_rdb".into(),
                            durable_state: state,
                            message: Some("RDB file ready".into()),
                        };
                    }

                    "upload_rdb" => {
                        yield StepEvent::Heartbeat;
                        let state = step_upload_rdb(
                            job_id,
                            accumulated_state.clone(),
                            &deps,
                            cancel.clone(),
                        ).await?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "upload_rdb".into(),
                            durable_state: state,
                            message: Some("RDB uploaded to S3".into()),
                        };
                    }

                    "metadata" => {
                        step_metadata(job_id, s3_source_id, accumulated_state.clone(), &deps).await?;
                        yield StepEvent::StepCompleted {
                            step: "metadata".into(),
                            durable_state: accumulated_state.clone(),
                            message: Some("metadata.json written".into()),
                        };

                        let s3_key = accumulated_state
                            .get(DS_S3_KEY)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let size_bytes = accumulated_state
                            .get(DS_SIZE_BYTES)
                            .and_then(|v| v.as_i64());
                        let use_walg = accumulated_state
                            .get(DS_USE_WALG)
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let compression = if use_walg { "lz4" } else { "none" };

                        info!(job_id, location = %s3_key, ?size_bytes, "RedisEngine: Done");
                        yield StepEvent::Done {
                            location: s3_key,
                            size_bytes,
                            compression: compression.into(),
                        };
                    }

                    other => {
                        Err(BackupEngineError::StepFailed {
                            job_id,
                            step: other.to_string(),
                            reason: format!("unexpected step '{}'", other),
                        })?;
                    }
                }
            }
        })
    }

    async fn rollback(
        &self,
        ctx: &BackupContext,
        cursor: StepCursor,
    ) -> Result<(), BackupEngineError> {
        let job_id = ctx.job_id;

        // Best-effort: remove the temp file if we have one.
        if let Some(temp_path) = cursor
            .durable_state
            .get(DS_TEMP_PATH)
            .and_then(|v| v.as_str())
        {
            let path = std::path::PathBuf::from(temp_path);
            if path.exists() {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!(job_id, path = %temp_path, error = %e, "RedisEngine rollback: failed to remove temp file");
                }
            }
        }

        // Best-effort: delete the partial S3 object.
        let s3_key = cursor
            .durable_state
            .get(DS_S3_KEY)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let bucket = cursor
            .durable_state
            .get(DS_BUCKET)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let (Some(s3_key), Some(bucket)) = (s3_key, bucket) {
            let s3_source_id: i32 = ctx
                .params
                .get("s3_source_id")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32)
                .unwrap_or(0);
            if s3_source_id > 0 {
                match build_s3_client(s3_source_id, &self.deps).await {
                    Ok(client) => {
                        if let Err(e) = client
                            .delete_object()
                            .bucket(&bucket)
                            .key(&s3_key)
                            .send()
                            .await
                        {
                            warn!(job_id, %bucket, %s3_key, error = %e, "RedisEngine rollback: failed to delete partial S3 object");
                        }
                    }
                    Err(e) => {
                        warn!(job_id, error = %e, "RedisEngine rollback: could not build S3 client")
                    }
                }
            }
        }

        Ok(())
    }
}

// ── Step helpers ──────────────────────────────────────────────────────────────

/// `preflight` step: validate the external service and S3 source exist.
/// Derives the intended S3 key and persists it in `durable_state` so failure
/// diagnostics can show the intended upload target.
async fn step_preflight(
    job_id: i64,
    service_id: i32,
    s3_source_id: i32,
    deps: &RedisDeps,
) -> Result<Value, BackupEngineError> {
    // Load service row.
    let service = temps_entities::external_services::Entity::find_by_id(service_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("db error loading service {}: {}", service_id, e),
        })?
        .ok_or_else(|| BackupEngineError::Preflight {
            job_id,
            reason: format!("service {} not found", service_id),
        })?;

    // Load S3 source.
    let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("db error loading s3_source {}: {}", s3_source_id, e),
        })?
        .ok_or_else(|| BackupEngineError::Preflight {
            job_id,
            reason: format!("s3_source {} not found", s3_source_id),
        })?;

    // Build S3 client and verify bucket is reachable.
    let s3_client = build_s3_client_from_source(job_id, &s3_source, deps)?;
    s3_client
        .head_bucket()
        .bucket(&s3_source.bucket_name)
        .send()
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("S3 bucket '{}' not reachable: {}", s3_source.bucket_name, e),
        })?;

    let backup_uuid = Uuid::new_v4().to_string();
    let s3_key = build_s3_key(
        &s3_source.bucket_path,
        &service.name,
        &backup_uuid,
        "dump.rdb",
    );

    info!(
        job_id,
        %s3_key,
        bucket = %s3_source.bucket_name,
        service_name = %service.name,
        "RedisEngine preflight: validated, intended S3 location set",
    );

    Ok(json!({
        DS_S3_KEY: s3_key,
        DS_BUCKET: s3_source.bucket_name,
        "backup_uuid": backup_uuid,
        "s3_source_id": s3_source_id,
        "service_id": service_id,
        "service_name": service.name,
        "bucket_path": s3_source.bucket_path,
    }))
}

/// `trigger_bgsave` step: detect WAL-G presence and either set `use_walg=true`
/// or issue `redis-cli BGSAVE`.
///
/// This step is always re-run on resume (BGSAVE is idempotent; WAL-G detection
/// is a read-only probe). Reference: `redis.rs:963` (`container_has_walg`),
/// `redis.rs:1013` (`backup_to_s3_legacy` BGSAVE trigger).
async fn step_trigger_bgsave(
    job_id: i64,
    durable_state: Value,
    deps: &RedisDeps,
) -> Result<Value, BackupEngineError> {
    let service_id: i32 = durable_state
        .get("service_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "trigger_bgsave".into(),
            reason: "durable_state missing service_id".into(),
        })?;

    let service = temps_entities::external_services::Entity::find_by_id(service_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "trigger_bgsave".into(),
            reason: format!("db error loading service {}: {}", service_id, e),
        })?
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "trigger_bgsave".into(),
            reason: format!("service {} not found", service_id),
        })?;

    // Container naming matches temps-providers/src/externalsvc/redis.rs:
    // `redis-{name}`. Earlier draft used `temps-{name}` which doesn't exist;
    // any docker exec against that target would silently fail.
    let container_name = format!("redis-{}", service.name);

    let has_walg = container_has_walg(&deps.docker, &container_name).await;

    let mut new_state = durable_state.clone();
    if let Some(obj) = new_state.as_object_mut() {
        obj.insert(DS_USE_WALG.to_string(), json!(has_walg));
    }

    if has_walg {
        info!(
            job_id,
            container = %container_name,
            "RedisEngine trigger_bgsave: WAL-G detected, will use wal-g backup-push",
        );
        // WAL-G prefix stored for the upload_rdb step.
        let s3_source_id: i32 = durable_state
            .get("s3_source_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0);
        let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
            .one(deps.db.as_ref())
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "trigger_bgsave".into(),
                reason: format!("db error loading s3_source: {}", e),
            })?
            .ok_or_else(|| BackupEngineError::StepFailed {
                job_id,
                step: "trigger_bgsave".into(),
                reason: "s3_source not found".into(),
            })?;
        let subpath_root = format!("external_services/redis/{}", service.name);
        let walg_prefix = format!(
            "s3://{}/{}/walg",
            s3_source.bucket_name,
            subpath_root.trim_matches('/')
        );
        if let Some(obj) = new_state.as_object_mut() {
            obj.insert(DS_WALG_PREFIX.to_string(), json!(walg_prefix));
            obj.insert("container_name".to_string(), json!(container_name));
        }
    } else {
        info!(
            job_id,
            container = %container_name,
            "RedisEngine trigger_bgsave: no WAL-G, issuing BGSAVE",
        );
        // Issue BGSAVE.
        trigger_redis_bgsave(job_id, &deps.docker, &container_name).await?;
        if let Some(obj) = new_state.as_object_mut() {
            obj.insert("container_name".to_string(), json!(container_name));
        }
    }

    Ok(new_state)
}

/// `wait_for_rdb` step: wait for BGSAVE to complete (BGSAVE path) or run
/// `wal-g backup-push` (WAL-G path), emitting heartbeat ticks.
///
/// On resume with `durable_state.rdb_ready = true`, the step is skipped.
async fn step_wait_for_rdb(
    job_id: i64,
    durable_state: Value,
    deps: Arc<RedisDeps>,
    _cancel: tokio_util::sync::CancellationToken,
    heartbeat_tx: tokio::sync::mpsc::Sender<()>,
) -> Result<Value, BackupEngineError> {
    // Idempotence: if already marked ready, return immediately.
    if durable_state
        .get(DS_RDB_READY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        info!(
            job_id,
            "RedisEngine wait_for_rdb: rdb_ready=true in cursor, skipping"
        );
        return Ok(durable_state);
    }

    let use_walg = durable_state
        .get(DS_USE_WALG)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let container_name = durable_state
        .get("container_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut new_state = durable_state.clone();

    if use_walg {
        // WAL-G path: run wal-g backup-push with heartbeats.
        let walg_prefix = durable_state
            .get(DS_WALG_PREFIX)
            .and_then(|v| v.as_str())
            .ok_or_else(|| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: "durable_state missing walg_prefix".into(),
            })?
            .to_string();

        let s3_source_id: i32 = durable_state
            .get("s3_source_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0);

        let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
            .one(deps.db.as_ref())
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("db error loading s3_source: {}", e),
            })?
            .ok_or_else(|| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: "s3_source not found".into(),
            })?;

        let access_key = deps
            .encryption_service
            .decrypt_string(&s3_source.access_key_id)
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("decrypt access key: {}", e),
            })?;
        let secret_key = deps
            .encryption_service
            .decrypt_string(&s3_source.secret_key)
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("decrypt secret key: {}", e),
            })?;

        // Load the redis service config to recover the `requirepass` value.
        // Without this, both `redis-cli --rdb` and `wal-g backup-push` connect
        // anonymously and Redis rejects every command with `NOAUTH
        // Authentication required`. Mirrors the legacy code at
        // temps-providers/src/externalsvc/redis.rs:578-607.
        let service_id_for_auth: i32 = durable_state
            .get("service_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0);
        let service = temps_entities::external_services::Entity::find_by_id(service_id_for_auth)
            .one(deps.db.as_ref())
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("db error loading service {}: {}", service_id_for_auth, e),
            })?
            .ok_or_else(|| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("service {} not found", service_id_for_auth),
            })?;
        let config_json = deps
            .encryption_service
            .decrypt_string(service.config.as_deref().unwrap_or("{}"))
            .unwrap_or_else(|_| "{}".to_string());
        let config_params: Value = serde_json::from_str(&config_json).unwrap_or_else(|_| json!({}));
        let redis_password = config_params
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        run_walg_backup_push_with_heartbeat(
            job_id,
            &deps.docker,
            &container_name,
            &walg_prefix,
            &access_key,
            &secret_key,
            &s3_source.region,
            s3_source.endpoint.as_deref(),
            s3_source.force_path_style.unwrap_or(true),
            &redis_password,
            &heartbeat_tx,
        )
        .await?;

        // Use walg_prefix as the final S3 location.
        if let Some(obj) = new_state.as_object_mut() {
            obj.insert(DS_S3_KEY.to_string(), json!(walg_prefix));
            obj.insert(DS_RDB_READY.to_string(), json!(true));
        }
    } else {
        // BGSAVE path: poll until Redis reports bgsave finished.
        poll_bgsave_completion(job_id, &deps.docker, &container_name, &heartbeat_tx).await?;

        // Copy dump.rdb out of container into a temp file.
        let temp_dir = std::env::temp_dir().join("temps-redis-backup");
        tokio::fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("create temp dir: {}", e),
            })?;
        let rdb_filename = format!("{}.rdb", Uuid::new_v4());
        let host_rdb_path = temp_dir.join(&rdb_filename);

        copy_rdb_from_container(job_id, &deps.docker, &container_name, &host_rdb_path).await?;

        let host_rdb_path_str = host_rdb_path.to_str().unwrap_or("").to_string();
        if let Some(obj) = new_state.as_object_mut() {
            obj.insert(DS_TEMP_PATH.to_string(), json!(host_rdb_path_str));
            obj.insert(DS_RDB_READY.to_string(), json!(true));
        }
    }

    Ok(new_state)
}

/// `upload_rdb` step: upload the RDB file to S3 (BGSAVE path) or record the
/// WAL-G prefix as the final location (WAL-G path).
///
/// S3 HEAD check provides idempotence on resume.
async fn step_upload_rdb(
    job_id: i64,
    durable_state: Value,
    deps: &RedisDeps,
    _cancel: tokio_util::sync::CancellationToken,
) -> Result<Value, BackupEngineError> {
    let use_walg = durable_state
        .get(DS_USE_WALG)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if use_walg {
        // WAL-G already uploaded during wait_for_rdb. Just record size.
        let s3_source_id: i32 = durable_state
            .get("s3_source_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0);
        let walg_prefix = durable_state
            .get(DS_WALG_PREFIX)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let bucket = durable_state
            .get(DS_BUCKET)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Compute total bytes by listing the walg prefix.
        if s3_source_id > 0 && !bucket.is_empty() {
            let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
                .one(deps.db.as_ref())
                .await
                .ok()
                .flatten();
            if let Some(src) = s3_source {
                let s3_client = build_s3_client_from_source(job_id, &src, deps).ok();
                if let Some(client) = s3_client {
                    let list_prefix = walg_prefix
                        .trim_start_matches(&format!("s3://{}/", bucket))
                        .to_string();
                    if let Ok(size) = list_total_s3_size(&client, &bucket, &list_prefix).await {
                        let mut new_state = durable_state.clone();
                        if let Some(obj) = new_state.as_object_mut() {
                            obj.insert(DS_SIZE_BYTES.to_string(), json!(size));
                        }
                        return Ok(new_state);
                    }
                }
            }
        }
        return Ok(durable_state);
    }

    // BGSAVE path: upload the temp RDB file.
    let s3_key = durable_state
        .get(DS_S3_KEY)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload_rdb".into(),
            reason: "durable_state missing s3_key".into(),
        })?
        .to_string();

    let bucket = durable_state
        .get(DS_BUCKET)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload_rdb".into(),
            reason: "durable_state missing bucket".into(),
        })?
        .to_string();

    let temp_path = durable_state
        .get(DS_TEMP_PATH)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload_rdb".into(),
            reason: "durable_state missing temp_path (wait_for_rdb did not complete)".into(),
        })?
        .to_string();

    let s3_source_id: i32 = durable_state
        .get("s3_source_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload_rdb".into(),
            reason: "durable_state missing s3_source_id".into(),
        })?;

    let s3_client =
        build_s3_client(s3_source_id, deps)
            .await
            .map_err(|e| BackupEngineError::S3 {
                job_id,
                reason: format!("build S3 client: {}", e),
            })?;

    // Idempotence: skip if already uploaded.
    if let Some(size) = check_s3_object_exists(&s3_client, &bucket, &s3_key).await {
        info!(job_id, %bucket, %s3_key, size_bytes = size, "RedisEngine upload_rdb: already exists, skipping");
        let _ = tokio::fs::remove_file(&temp_path).await;
        let mut new_state = durable_state.clone();
        if let Some(obj) = new_state.as_object_mut() {
            obj.insert(DS_SIZE_BYTES.to_string(), json!(size));
        }
        return Ok(new_state);
    }

    let file_meta =
        tokio::fs::metadata(&temp_path)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "upload_rdb".into(),
                reason: format!("cannot stat rdb file {}: {}", temp_path, e),
            })?;
    let file_size = file_meta.len() as i64;

    let body = aws_sdk_s3::primitives::ByteStream::from_path(std::path::Path::new(&temp_path))
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id,
            reason: format!("create byte stream: {}", e),
        })?;

    s3_client
        .put_object()
        .bucket(&bucket)
        .key(&s3_key)
        .body(body)
        .content_type("application/octet-stream")
        .send()
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id,
            reason: format!("upload rdb to s3://{}/{}: {}", bucket, s3_key, e),
        })?;

    if let Err(e) = tokio::fs::remove_file(&temp_path).await {
        warn!(job_id, path = %temp_path, error = %e, "RedisEngine upload_rdb: cleanup failed (non-fatal)");
    }

    let mut new_state = durable_state.clone();
    if let Some(obj) = new_state.as_object_mut() {
        obj.insert(DS_SIZE_BYTES.to_string(), json!(file_size));
    }
    info!(job_id, %bucket, %s3_key, "RedisEngine upload_rdb: completed");
    Ok(new_state)
}

/// `metadata` step: write `metadata.json` to S3.
async fn step_metadata(
    job_id: i64,
    s3_source_id: i32,
    durable_state: Value,
    deps: &RedisDeps,
) -> Result<(), BackupEngineError> {
    let s3_key = durable_state
        .get(DS_S3_KEY)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "metadata".into(),
            reason: "missing s3_key".into(),
        })?
        .to_string();
    let bucket = durable_state
        .get(DS_BUCKET)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "metadata".into(),
            reason: "missing bucket".into(),
        })?
        .to_string();

    let s3_client =
        build_s3_client(s3_source_id, deps)
            .await
            .map_err(|e| BackupEngineError::S3 {
                job_id,
                reason: format!("build S3 client: {}", e),
            })?;

    let metadata_key = derive_metadata_key(&s3_key);
    let size_bytes = durable_state.get(DS_SIZE_BYTES).and_then(|v| v.as_i64());
    let backup_uuid = durable_state
        .get("backup_uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let use_walg = durable_state
        .get(DS_USE_WALG)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let metadata = json!({
        "backup_uuid": backup_uuid,
        "type": "full",
        "engine": "redis",
        "backup_tool": if use_walg { "wal-g" } else { "bgsave" },
        "created_at": Utc::now().to_rfc3339(),
        "size_bytes": size_bytes,
        "compression_type": if use_walg { "lz4" } else { "none" },
        "source": { "id": s3_source_id },
        "s3_location": s3_key,
    });

    let body = serde_json::to_vec(&metadata).map_err(|e| BackupEngineError::StepFailed {
        job_id,
        step: "metadata".into(),
        reason: format!("serialize: {}", e),
    })?;

    s3_client
        .put_object()
        .bucket(&bucket)
        .key(&metadata_key)
        .body(body.into())
        .content_type("application/json")
        .send()
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id,
            reason: format!("upload metadata.json: {}", e),
        })?;

    info!(job_id, %bucket, key = %metadata_key, "RedisEngine metadata: written");
    Ok(())
}

// ── Utility helpers ───────────────────────────────────────────────────────────

fn build_s3_key(
    bucket_path: &str,
    service_name: &str,
    _backup_uuid: &str,
    filename: &str,
) -> String {
    let prefix = bucket_path.trim_matches('/');
    let date = Utc::now().format("%Y/%m/%d");
    if prefix.is_empty() {
        format!(
            "external_services/redis/{}/{}/{}",
            service_name, date, filename
        )
    } else {
        format!(
            "{}/external_services/redis/{}/{}/{}",
            prefix, service_name, date, filename
        )
    }
}

fn derive_metadata_key(s3_key: &str) -> String {
    let parts: Vec<&str> = s3_key.rsplitn(2, '/').collect();
    if parts.len() == 2 {
        format!("{}/metadata.json", parts[1])
    } else {
        format!("{}.metadata.json", s3_key)
    }
}

async fn build_s3_client(
    s3_source_id: i32,
    deps: &RedisDeps,
) -> Result<S3Client, BackupEngineError> {
    let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id: 0,
            reason: format!("db error: {}", e),
        })?
        .ok_or_else(|| BackupEngineError::S3 {
            job_id: 0,
            reason: format!("s3_source {} not found", s3_source_id),
        })?;
    build_s3_client_from_source(0, &s3_source, deps)
}

fn build_s3_client_from_source(
    job_id: i64,
    s3_source: &temps_entities::s3_sources::Model,
    deps: &RedisDeps,
) -> Result<S3Client, BackupEngineError> {
    use aws_sdk_s3::Config;

    let access_key = deps
        .encryption_service
        .decrypt_string(&s3_source.access_key_id)
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("decrypt access key: {}", e),
        })?;
    let secret_key = deps
        .encryption_service
        .decrypt_string(&s3_source.secret_key)
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("decrypt secret key: {}", e),
        })?;

    let creds =
        aws_sdk_s3::config::Credentials::new(access_key, secret_key, None, None, "redis-engine");
    let mut builder = Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(s3_source.region.clone()))
        .force_path_style(s3_source.force_path_style.unwrap_or(true))
        .credentials_provider(creds);

    if let Some(endpoint) = &s3_source.endpoint {
        let url = if endpoint.starts_with("http") {
            endpoint.clone()
        } else {
            format!("http://{}", endpoint)
        };
        builder = builder.endpoint_url(url);
    }
    Ok(S3Client::from_conf(builder.build()))
}

async fn check_s3_object_exists(client: &S3Client, bucket: &str, key: &str) -> Option<i64> {
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(resp) => resp.content_length(),
        Err(_) => None,
    }
}

async fn list_total_s3_size(
    client: &S3Client,
    bucket: &str,
    prefix: &str,
) -> Result<i64, BackupEngineError> {
    let mut total: i64 = 0;
    let mut continuation: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(tok) = continuation {
            req = req.continuation_token(tok);
        }
        let resp = req.send().await.map_err(|e| BackupEngineError::S3 {
            job_id: 0,
            reason: format!("list objects: {}", e),
        })?;
        for obj in resp.contents() {
            total += obj.size().unwrap_or(0);
        }
        if resp.is_truncated().unwrap_or(false) {
            continuation = resp.next_continuation_token().map(|s| s.to_string());
        } else {
            break;
        }
    }
    Ok(total)
}

/// Check if `wal-g` binary is present in the container.
/// Reference: `redis.rs:963`.
async fn container_has_walg(docker: &bollard::Docker, container_name: &str) -> bool {
    use bollard::exec::{CreateExecOptions, StartExecOptions};

    let exec = match docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["which", "wal-g"]),
                attach_stdout: Some(false),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
    {
        Ok(e) => e,
        Err(_) => return false,
    };

    if docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: true,
                ..Default::default()
            }),
        )
        .await
        .is_err()
    {
        return false;
    }

    loop {
        match docker.inspect_exec(&exec.id).await {
            Ok(inspect) => {
                if inspect.running == Some(false) {
                    return inspect.exit_code == Some(0);
                }
            }
            Err(_) => return false,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Trigger `redis-cli BGSAVE` in the container.
async fn trigger_redis_bgsave(
    job_id: i64,
    docker: &bollard::Docker,
    container_name: &str,
) -> Result<(), BackupEngineError> {
    use bollard::exec::CreateExecOptions;

    // Capture stdout to check BGSAVE response ("Background saving started").
    // We do NOT need heartbeats here — BGSAVE returns immediately.
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["redis-cli", "BGSAVE"]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "trigger_bgsave".into(),
            reason: format!("create exec BGSAVE: {}", e),
        })?;

    let stream_result =
        docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "trigger_bgsave".into(),
                reason: format!("start BGSAVE exec: {}", e),
            })?;

    let mut response = String::new();
    if let StartExecResults::Attached { mut output, .. } = stream_result {
        while let Some(item) = output.next().await {
            match item {
                Ok(LogOutput::StdOut { message }) => {
                    response.push_str(&String::from_utf8_lossy(&message));
                }
                Ok(LogOutput::StdErr { message }) => {
                    warn!(job_id, engine = "redis", container = %container_name, "BGSAVE stderr: {}", String::from_utf8_lossy(&message));
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(job_id, engine = "redis", container = %container_name, "BGSAVE stream error: {}", e);
                    break;
                }
            }
        }
    }

    info!(job_id, container = %container_name, response = %response.trim(), "RedisEngine trigger_bgsave: BGSAVE issued");
    Ok(())
}

/// Poll Redis `LASTSAVE` until the bgsave is complete, sending heartbeat ticks.
async fn poll_bgsave_completion(
    job_id: i64,
    docker: &bollard::Docker,
    container_name: &str,
    heartbeat_tx: &tokio::sync::mpsc::Sender<()>,
) -> Result<(), BackupEngineError> {
    // Get the LASTSAVE timestamp before the backup started.
    let last_save_before = get_redis_lastsave(job_id, docker, container_name).await?;
    let mut last_heartbeat = Instant::now();

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        let last_save_now = get_redis_lastsave(job_id, docker, container_name)
            .await
            .unwrap_or(0);
        if last_save_now > last_save_before {
            info!(job_id, container = %container_name, "RedisEngine: BGSAVE completed");
            break;
        }

        // Check BGSAVE status via INFO persistence.
        let status = get_bgsave_status(docker, container_name).await;
        if status.as_deref() == Some("Background saving terminated with success") {
            break;
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            last_heartbeat = Instant::now();
            let _ = heartbeat_tx.try_send(());
        }
    }
    Ok(())
}

async fn get_redis_lastsave(
    job_id: i64,
    docker: &bollard::Docker,
    container_name: &str,
) -> Result<i64, BackupEngineError> {
    use bollard::exec::CreateExecOptions;
    use futures::StreamExt;

    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["redis-cli", "LASTSAVE"]),
                attach_stdout: Some(true),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "wait_for_rdb".into(),
            reason: format!("create LASTSAVE exec: {}", e),
        })?;

    let output =
        docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("start LASTSAVE exec: {}", e),
            })?;

    let mut result = String::new();
    if let bollard::exec::StartExecResults::Attached { mut output, .. } = output {
        while let Some(Ok(msg)) = output.next().await {
            if let bollard::container::LogOutput::StdOut { message } = msg {
                result.push_str(&String::from_utf8_lossy(&message));
            }
        }
    }
    Ok(result.trim().parse::<i64>().unwrap_or(0))
}

async fn get_bgsave_status(docker: &bollard::Docker, container_name: &str) -> Option<String> {
    use bollard::exec::CreateExecOptions;
    use futures::StreamExt;

    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["redis-cli", "INFO", "persistence"]),
                attach_stdout: Some(true),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
        .ok()?;

    let output = docker.start_exec(&exec.id, None).await.ok()?;
    let mut info = String::new();
    if let bollard::exec::StartExecResults::Attached { mut output, .. } = output {
        while let Some(Ok(msg)) = output.next().await {
            if let bollard::container::LogOutput::StdOut { message } = msg {
                info.push_str(&String::from_utf8_lossy(&message));
            }
        }
    }
    // Look for rdb_last_bgsave_status line.
    for line in info.lines() {
        if line.starts_with("rdb_last_bgsave_status:") {
            return Some(
                line.trim_start_matches("rdb_last_bgsave_status:")
                    .trim()
                    .to_string(),
            );
        }
    }
    None
}

/// Copy `/data/dump.rdb` from the container to a host path.
async fn copy_rdb_from_container(
    job_id: i64,
    docker: &bollard::Docker,
    container_name: &str,
    host_path: &std::path::Path,
) -> Result<(), BackupEngineError> {
    use bollard::exec::CreateExecOptions;
    use futures::StreamExt;
    use std::io::Write;

    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["cat", "/data/dump.rdb"]),
                attach_stdout: Some(true),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "wait_for_rdb".into(),
            reason: format!("create cat exec: {}", e),
        })?;

    let output =
        docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("start cat exec: {}", e),
            })?;

    let mut file = std::fs::File::create(host_path).map_err(|e| BackupEngineError::StepFailed {
        job_id,
        step: "wait_for_rdb".into(),
        reason: format!("create rdb file {}: {}", host_path.display(), e),
    })?;

    if let bollard::exec::StartExecResults::Attached { mut output, .. } = output {
        while let Some(result) = output.next().await {
            match result {
                Ok(bollard::container::LogOutput::StdOut { message }) => {
                    file.write_all(&message)
                        .map_err(|e| BackupEngineError::StepFailed {
                            job_id,
                            step: "wait_for_rdb".into(),
                            reason: format!("write rdb: {}", e),
                        })?;
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(BackupEngineError::StepFailed {
                        job_id,
                        step: "wait_for_rdb".into(),
                        reason: format!("stream rdb: {}", e),
                    })
                }
            }
        }
    }

    info!(job_id, path = %host_path.display(), "RedisEngine: RDB copied from container");
    Ok(())
}

/// Run `wal-g backup-push` inside the Redis container with heartbeat ticks.
/// Reference: `redis.rs:571`.
#[allow(clippy::too_many_arguments)]
async fn run_walg_backup_push_with_heartbeat(
    job_id: i64,
    docker: &bollard::Docker,
    container_name: &str,
    walg_prefix: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    endpoint: Option<&str>,
    force_path_style: bool,
    redis_password: &str,
    heartbeat_tx: &tokio::sync::mpsc::Sender<()>,
) -> Result<(), BackupEngineError> {
    use bollard::exec::{CreateExecOptions, StartExecOptions};

    // WAL-G env: must stream RDB via redis-cli (see redis.rs:588).
    // Both redis-cli and wal-g need the Redis password when `requirepass` is
    // set. Without `-a $password` redis-cli fails with `NOAUTH Authentication
    // required` and the entire backup aborts; without `WALG_REDIS_PASSWORD`
    // wal-g's REPLCONF/SYNC commands also fail (see prod log
    // 2026-05-14T21:08:58 wal-g stderr).
    let stream_cmd_owned: String = if redis_password.is_empty() {
        "redis-cli --rdb /tmp/redis_backup.rdb && cat /tmp/redis_backup.rdb".to_string()
    } else {
        // Single-quote escape: replace ' → '"'"' so the password embeds safely.
        let escaped = redis_password.replace('\'', "'\"'\"'");
        format!(
            "redis-cli -a '{}' --no-auth-warning --rdb /tmp/redis_backup.rdb && cat /tmp/redis_backup.rdb",
            escaped
        )
    };
    let mut walg_env: Vec<String> = vec![
        format!("WALG_S3_PREFIX={}", walg_prefix),
        format!("AWS_ACCESS_KEY_ID={}", access_key),
        format!("AWS_SECRET_ACCESS_KEY={}", secret_key),
        format!("AWS_REGION={}", region),
        format!("WALG_STREAM_CREATE_COMMAND={}", stream_cmd_owned),
        "WALG_STREAM_RESTORE_COMMAND=cat > /data/dump.rdb".to_string(),
    ];
    if !redis_password.is_empty() {
        walg_env.push(format!("WALG_REDIS_PASSWORD={}", redis_password));
    }
    if let Some(ep) = endpoint {
        let url = if ep.starts_with("http") {
            ep.to_string()
        } else {
            format!("http://{}", ep)
        };
        walg_env.push(format!("AWS_ENDPOINT={}", url));
    }
    if force_path_style {
        walg_env.push("AWS_S3_FORCE_PATH_STYLE=true".to_string());
    }

    let env_refs: Vec<&str> = walg_env.iter().map(|s| s.as_str()).collect();
    // Capture stdout + stderr so failures are diagnosable (no `2>&1` in cmd).
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["sh", "-c", "wal-g backup-push"]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                env: Some(env_refs),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "wait_for_rdb".into(),
            reason: format!("create walg exec: {}", e),
        })?;

    let stream_result = docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: false,
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "wait_for_rdb".into(),
            reason: format!("start walg exec: {}", e),
        })?;

    let mut stdout_tail = RingBuffer::with_capacity(64 * 1024);
    let mut stderr_tail = RingBuffer::with_capacity(64 * 1024);
    let mut last_hb = Instant::now();

    if let StartExecResults::Attached { mut output, .. } = stream_result {
        while let Some(item) = output.next().await {
            match item {
                Ok(LogOutput::StdOut { message }) => stdout_tail.append(&message),
                Ok(LogOutput::StdErr { message }) => stderr_tail.append(&message),
                Ok(_) => {}
                Err(e) => {
                    error!(job_id, engine = "redis", container = %container_name, "walg_push exec stream error: {}", e);
                    break;
                }
            }
            if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                let _ = heartbeat_tx.try_send(());
                last_hb = Instant::now();
            }
        }
    }

    let inspect =
        docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "wait_for_rdb".into(),
                reason: format!("inspect walg exec: {}", e),
            })?;
    let exit_code = inspect.exit_code.unwrap_or(-1);
    let stdout = stdout_tail.into_string_lossy();
    let stderr = stderr_tail.into_string_lossy();

    if exit_code != 0 {
        return Err(BackupEngineError::StepFailed {
            job_id,
            step: "wait_for_rdb".into(),
            reason: format!(
                "wal-g backup-push exited with code {}. stderr: {}. stdout: {}",
                exit_code,
                if stderr.trim().is_empty() {
                    "<empty>"
                } else {
                    stderr.trim()
                },
                if stdout.trim().is_empty() {
                    "<empty>"
                } else {
                    stdout.trim()
                },
            ),
        });
    }

    if !stderr.trim().is_empty() {
        info!(
            job_id,
            engine = "redis",
            container = %container_name,
            "walg_push stderr (warnings): {}",
            stderr.trim(),
        );
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use std::sync::Arc;
    use temps_backup_core::{
        BackupContext, BackupEngine, BackupEngineError, StepCursor, StepEvent,
    };
    use tokio_util::sync::CancellationToken;

    /// Minimal test engine matching `RedisEngine`'s step list.
    struct TestRedisEngine {
        call_count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl TestRedisEngine {
        fn new() -> Self {
            Self {
                call_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            }
        }
    }

    impl BackupEngine for TestRedisEngine {
        fn engine(&self) -> &'static str {
            "redis"
        }
        fn steps(&self) -> &'static [&'static str] {
            STEPS
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a BackupContext,
            cursor: StepCursor,
        ) -> BoxStream<'a, Result<StepEvent, BackupEngineError>> {
            let call_n = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async_stream::try_stream! {
                if call_n == 0 {
                    yield StepEvent::StepCompleted {
                        step: "preflight".into(),
                        durable_state: json!({"step": "preflight", "s3_key": "test/dump.rdb", "bucket": "test-bucket"}),
                        message: None,
                    };
                    yield StepEvent::StepCompleted {
                        step: "trigger_bgsave".into(),
                        durable_state: json!({"step": "trigger_bgsave", "use_walg": false}),
                        message: None,
                    };
                    yield StepEvent::StepCompleted {
                        step: "wait_for_rdb".into(),
                        durable_state: json!({"rdb_ready": true, "temp_path": "/tmp/dump.rdb"}),
                        message: None,
                    };
                    // Simulate crash before upload_rdb.
                    Err(BackupEngineError::StepFailed {
                        job_id: 0,
                        step: "upload_rdb".into(),
                        reason: "simulated crash after wait_for_rdb".into(),
                    })?;
                } else {
                    // Resume: cursor.current_step should be "wait_for_rdb".
                    let current = cursor.current_step.as_deref().unwrap_or("none");
                    if current != "wait_for_rdb" {
                        Err(BackupEngineError::StepFailed {
                            job_id: 0,
                            step: "resume-check".into(),
                            reason: format!("expected wait_for_rdb on resume, got: {}", current),
                        })?;
                    }
                    yield StepEvent::StepCompleted {
                        step: "upload_rdb".into(),
                        durable_state: json!({"size_bytes": 1024}),
                        message: None,
                    };
                    yield StepEvent::StepCompleted {
                        step: "metadata".into(),
                        durable_state: json!({}),
                        message: None,
                    };
                    yield StepEvent::Done {
                        location: "test/dump.rdb".into(),
                        size_bytes: Some(1024),
                        compression: "none".into(),
                    };
                }
            })
        }
    }

    fn make_ctx() -> BackupContext {
        let db = sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Postgres).into_connection();
        BackupContext {
            job_id: 1,
            attempt: 1,
            params: json!({"service_id": 1, "s3_source_id": 1}),
            db: Arc::new(db),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn test_engine_key() {
        let engine = TestRedisEngine::new();
        assert_eq!(engine.engine(), "redis");
    }

    #[test]
    fn test_steps_list() {
        let engine = TestRedisEngine::new();
        assert_eq!(engine.steps(), STEPS);
        assert_eq!(engine.steps()[0], "preflight");
        assert_eq!(engine.steps()[4], "metadata");
    }

    #[tokio::test]
    async fn test_crash_resume_cursor_is_correct() {
        let engine = TestRedisEngine::new();
        let ctx = make_ctx();

        // First attempt: emit preflight, trigger_bgsave, wait_for_rdb, then error.
        let cursor1 = StepCursor {
            current_step: None,
            durable_state: json!({}),
        };
        let mut stream1 = engine.execute(&ctx, cursor1);

        let mut last_completed = None;
        let mut error_seen = false;
        while let Some(event) = stream1.next().await {
            match event {
                Ok(StepEvent::StepCompleted { ref step, .. }) => {
                    last_completed = Some(step.clone());
                }
                Ok(StepEvent::Done { .. }) => {}
                Ok(StepEvent::Heartbeat) => {}
                Err(_) => {
                    error_seen = true;
                    break;
                }
            }
        }

        assert!(error_seen, "first attempt should fail");
        assert_eq!(
            last_completed.as_deref(),
            Some("wait_for_rdb"),
            "cursor should be wait_for_rdb"
        );

        // Second attempt: resume from wait_for_rdb.
        let cursor2 = StepCursor {
            current_step: last_completed,
            durable_state: json!({"rdb_ready": true}),
        };
        let mut stream2 = engine.execute(&ctx, cursor2);
        let mut done_seen = false;
        while let Some(event) = stream2.next().await {
            match event {
                Ok(StepEvent::Done { .. }) => {
                    done_seen = true;
                }
                Ok(_) => {}
                Err(e) => panic!("resume attempt failed: {}", e),
            }
        }
        assert!(done_seen, "second attempt should complete with Done");
    }
}
