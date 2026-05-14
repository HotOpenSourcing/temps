//! `MongodbEngine`: `BackupEngine` for MongoDB external services
//! (ADR-014 Phase 4 §"MongoDB, S3 mirror, RustFS engines").
//!
//! Steps: `preflight` → `mongodump` → `upload` → `metadata`.
//!
//! ## Design notes
//!
//! Lifts the legacy mongodump logic from
//! `temps-providers/src/externalsvc/mongodb.rs:1373` (`backup_to_s3_legacy`).
//! The WAL-G path (`mongodb.rs:2069`) is not implemented here because the
//! ADR defines a single `"mongodb"` engine key — the BGSAVE-equivalent
//! mongodump path is the most portable and requires no special image.
//!
//! ## Heartbeat discipline
//!
//! `mongodump` streams output from a Docker exec. We use the mpsc + select
//! pattern from `control_plane.rs:213–254` to emit heartbeats during the dump.
//!
//! ## Idempotence
//!
//! - `preflight`: re-validates S3 source; safe to re-run.
//! - `mongodump`: checks for an existing non-empty temp file at
//!   `durable_state.temp_path` before re-running.
//! - `upload`: S3 HEAD check before upload.
//! - `metadata`: PUT is always overwrite.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aws_sdk_s3::Client as S3Client;
use chrono::Utc;
use futures::stream::BoxStream;
use sea_orm::{DatabaseConnection, EntityTrait};
use serde_json::{json, Value};
use tracing::{debug, info, warn};
use uuid::Uuid;

use temps_backup_core::{BackupContext, BackupEngine, BackupEngineError, StepCursor, StepEvent};
use temps_core::EncryptionService;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(120);

const STEPS: &[&str] = &["preflight", "mongodump", "upload", "metadata"];

const DS_S3_KEY: &str = "s3_key";
const DS_BUCKET: &str = "bucket";
const DS_SIZE_BYTES: &str = "size_bytes";
const DS_TEMP_PATH: &str = "temp_path";

// ── Dependencies ─────────────────────────────────────────────────────────────

pub struct MongodbDeps {
    pub db: Arc<DatabaseConnection>,
    pub encryption_service: Arc<EncryptionService>,
    pub docker: bollard::Docker,
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// `BackupEngine` for MongoDB external services using mongodump.
///
/// Runs `mongodump --archive --gzip` via docker exec and streams the output
/// to a temp file, then uploads to S3.
/// Reference: `mongodb.rs:1373` (`backup_to_s3_legacy`).
pub struct MongodbEngine {
    deps: Arc<MongodbDeps>,
}

impl MongodbEngine {
    pub fn new(deps: MongodbDeps) -> Self {
        Self {
            deps: Arc::new(deps),
        }
    }
}

#[async_trait::async_trait]
impl BackupEngine for MongodbEngine {
    fn engine(&self) -> &'static str {
        "mongodb"
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
                STEPS.iter().position(|&s| s == last.as_str())
                    .map(|i| i + 1)
                    .ok_or_else(|| BackupEngineError::StepFailed {
                        job_id, step: last.clone(),
                        reason: format!("unknown step '{}'; known: {:?}", last, STEPS),
                    })?
            } else { 0 };

            let service_id: i32 = params.get("service_id").and_then(|v| v.as_i64()).map(|v| v as i32)
                .ok_or_else(|| BackupEngineError::Preflight { job_id, reason: "params.service_id missing".into() })?;
            let s3_source_id: i32 = params.get("s3_source_id").and_then(|v| v.as_i64()).map(|v| v as i32)
                .ok_or_else(|| BackupEngineError::Preflight { job_id, reason: "params.s3_source_id missing".into() })?;

            for step in &STEPS[start_idx..] {
                if cancel.is_cancelled() {
                    debug!(job_id, step, "MongodbEngine: cancellation requested");
                    return;
                }
                info!(job_id, attempt, step, "MongodbEngine: executing step");

                match *step {
                    "preflight" => {
                        let state = step_preflight(job_id, service_id, s3_source_id, &deps).await?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "preflight".into(),
                            durable_state: state,
                            message: Some(format!("service {} and S3 source {} validated", service_id, s3_source_id)),
                        };
                    }

                    "mongodump" => {
                        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<()>(8);
                        let mut step_fut = std::pin::pin!(step_mongodump(
                            job_id, accumulated_state.clone(), Arc::clone(&deps), cancel.clone(), heartbeat_tx,
                        ));

                        let step_result: Result<Value, BackupEngineError> = loop {
                            tokio::select! {
                                biased;
                                Some(()) = heartbeat_rx.recv() => {
                                    debug!(job_id, "MongodbEngine mongodump: Heartbeat");
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
                            step: "mongodump".into(),
                            durable_state: state,
                            message: Some("mongodump completed".into()),
                        };
                    }

                    "upload" => {
                        yield StepEvent::Heartbeat;
                        let state = step_upload(job_id, accumulated_state.clone(), &deps, cancel.clone()).await?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "upload".into(),
                            durable_state: state,
                            message: Some("dump uploaded to S3".into()),
                        };
                    }

                    "metadata" => {
                        step_metadata(job_id, s3_source_id, accumulated_state.clone(), &deps).await?;
                        yield StepEvent::StepCompleted {
                            step: "metadata".into(),
                            durable_state: accumulated_state.clone(),
                            message: Some("metadata.json written".into()),
                        };
                        let s3_key = accumulated_state.get(DS_S3_KEY).and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let size_bytes = accumulated_state.get(DS_SIZE_BYTES).and_then(|v| v.as_i64());
                        info!(job_id, location = %s3_key, ?size_bytes, "MongodbEngine: Done");
                        yield StepEvent::Done { location: s3_key, size_bytes, compression: "gzip".into() };
                    }

                    other => {
                        Err(BackupEngineError::StepFailed {
                            job_id, step: other.to_string(), reason: format!("unexpected step '{}'", other),
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
        if let Some(p) = cursor
            .durable_state
            .get(DS_TEMP_PATH)
            .and_then(|v| v.as_str())
        {
            if let Err(e) = tokio::fs::remove_file(p).await {
                warn!(job_id, path = %p, error = %e, "MongodbEngine rollback: cleanup failed");
            }
        }
        rollback_s3_object(job_id, ctx, &cursor, &self.deps).await;
        Ok(())
    }
}

// ── Step helpers ──────────────────────────────────────────────────────────────

async fn step_preflight(
    job_id: i64,
    service_id: i32,
    s3_source_id: i32,
    deps: &MongodbDeps,
) -> Result<Value, BackupEngineError> {
    let service = temps_entities::external_services::Entity::find_by_id(service_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("db service {}: {}", service_id, e),
        })?
        .ok_or_else(|| BackupEngineError::Preflight {
            job_id,
            reason: format!("service {} not found", service_id),
        })?;

    let s3_source = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("db s3_source {}: {}", s3_source_id, e),
        })?
        .ok_or_else(|| BackupEngineError::Preflight {
            job_id,
            reason: format!("s3_source {} not found", s3_source_id),
        })?;

    let s3_client = build_s3_client_from_source(job_id, &s3_source, deps)?;
    s3_client
        .head_bucket()
        .bucket(&s3_source.bucket_name)
        .send()
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("bucket not reachable: {}", e),
        })?;

    let backup_uuid = Uuid::new_v4().to_string();
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let s3_key = build_s3_key(
        &s3_source.bucket_path,
        &service.name,
        &format!("mongodb_backup_{}.gz", timestamp),
    );

    info!(job_id, %s3_key, "MongodbEngine preflight: validated");

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

async fn step_mongodump(
    job_id: i64,
    durable_state: Value,
    deps: Arc<MongodbDeps>,
    _cancel: tokio_util::sync::CancellationToken,
    heartbeat_tx: tokio::sync::mpsc::Sender<()>,
) -> Result<Value, BackupEngineError> {
    use bollard::exec::CreateExecOptions;
    use futures::StreamExt;
    use std::io::Write;

    // Idempotence: skip if temp file already exists and is non-empty.
    if let Some(p) = durable_state.get(DS_TEMP_PATH).and_then(|v| v.as_str()) {
        let path = std::path::Path::new(p);
        if path.exists() {
            let meta = tokio::fs::metadata(path).await.ok();
            if meta.map(|m| m.len() > 0).unwrap_or(false) {
                info!(job_id, temp_path = %p, "MongodbEngine mongodump: existing dump found, skipping");
                return Ok(durable_state);
            }
        }
    }

    let service_id: i32 = durable_state
        .get("service_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: "missing service_id".into(),
        })?;

    let service = temps_entities::external_services::Entity::find_by_id(service_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: format!("db: {}", e),
        })?
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: format!("service {} not found", service_id),
        })?;

    let config_json = deps
        .encryption_service
        .decrypt_string(service.config.as_deref().unwrap_or("{}"))
        .unwrap_or_else(|_| "{}".to_string());
    let params: Value = serde_json::from_str(&config_json).unwrap_or_else(|_| json!({}));
    let mut username = params
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut password = params
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // NOTE: we intentionally do NOT read `database` from the service config
    // for backup purposes. The config's `database` field is the default DB
    // the application's runtime connection points at — it does NOT mean
    // "back up only this database." For a backup, we always dump every
    // accessible database (mongodump's natural default when `--db` is
    // omitted). Reading the config value here causes the engine to pass
    // `--db admin` and silently emit a 927-byte archive containing only
    // the admin system collections — verified on 2026-05-14 with job 23
    // (username=root, password_set=true, database=admin).
    let database = String::new();

    // Container naming matches temps-providers/src/externalsvc/mongodb.rs:321
    // (`temps-mongodb-{name}`).
    let container_name = format!("temps-mongodb-{}", service.name);

    // Prefer the container's MONGO_INITDB_ROOT_USERNAME / _PASSWORD env vars
    // over whatever's in the encrypted service config. The container's root
    // creds always have full `root` role — every database, every collection.
    // The service config user may have been provisioned with a narrower role
    // (e.g. read-only on a single db) which causes mongodump to silently
    // succeed but only emit the admin system collections (~927 bytes) — see
    // prod incident 2026-05-14T21:06:48 where 100k tempstest.users docs were
    // skipped because the configured user couldn't read them.
    //
    // Falls back to the config creds if container inspect fails or the env
    // vars aren't set (older deployments).
    match deps
        .docker
        .inspect_container(
            &container_name,
            None::<bollard::query_parameters::InspectContainerOptions>,
        )
        .await
    {
        Ok(inspect) => {
            if let Some(env_vec) = inspect.config.as_ref().and_then(|c| c.env.as_ref()) {
                for env in env_vec {
                    if let Some(v) = env.strip_prefix("MONGO_INITDB_ROOT_USERNAME=") {
                        username = v.to_string();
                    } else if let Some(v) = env.strip_prefix("MONGO_INITDB_ROOT_PASSWORD=") {
                        password = v.to_string();
                    }
                }
            }
        }
        Err(e) => {
            warn!(job_id, container = %container_name, error = %e,
                "MongodbEngine: could not inspect container for root creds; falling back to service config");
        }
    }
    if username.is_empty() {
        username = "admin".to_string();
    }
    // Diagnostic: log which user mongodump is actually being called with
    // (password redacted). Use this to verify the env-var lookup landed.
    info!(
        job_id,
        container = %container_name,
        username = %username,
        password_set = !password.is_empty(),
        database = %if database.is_empty() { "<all-dbs>" } else { database.as_str() },
        "MongodbEngine: mongodump credentials resolved"
    );

    // Build mongodump command. The `--db` flag scopes the dump to a single
    // database; omitting it makes mongodump dump every accessible database,
    // which is the right behavior for a "full backup" when no specific
    // database is configured on the service. An earlier draft passed
    // `--db "--"` as a sentinel when the database was empty — mongodump
    // interpreted the literal `"--"` as a database name, found nothing,
    // and silently produced a ~1 KB archive containing only the `admin`
    // system collections that the credentials authenticated against.
    let mut mongodump_args: Vec<&str> = vec![
        "mongodump",
        "--archive",
        "--gzip",
        "-u",
        username.as_str(),
        "-p",
        password.as_str(),
        "--authenticationDatabase",
        "admin",
    ];
    if !database.is_empty() {
        mongodump_args.push("--db");
        mongodump_args.push(database.as_str());
    }
    let exec = deps
        .docker
        .create_exec(
            &container_name,
            CreateExecOptions {
                cmd: Some(mongodump_args),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: format!("create exec: {}", e),
        })?;

    let output = deps.docker.start_exec(&exec.id, None).await.map_err(|e| {
        BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: format!("start exec: {}", e),
        }
    })?;

    // Stream mongodump stdout to a temp file (avoids buffering multi-GB dumps in memory).
    let temp_dir = std::env::temp_dir().join("temps-mongo-backup");
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: format!("create temp dir: {}", e),
        })?;
    let dump_filename = format!("{}.gz", Uuid::new_v4());
    let host_dump_path = temp_dir.join(&dump_filename);

    let mut file =
        std::fs::File::create(&host_dump_path).map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: format!("create temp file: {}", e),
        })?;
    let mut total_bytes: u64 = 0;
    let mut last_heartbeat = Instant::now();

    if let bollard::exec::StartExecResults::Attached { mut output, .. } = output {
        while let Some(result) = output.next().await {
            match result {
                Ok(bollard::container::LogOutput::StdOut { message }) => {
                    file.write_all(&message)
                        .map_err(|e| BackupEngineError::StepFailed {
                            job_id,
                            step: "mongodump".into(),
                            reason: format!("write dump: {}", e),
                        })?;
                    total_bytes += message.len() as u64;
                    if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                        last_heartbeat = Instant::now();
                        let _ = heartbeat_tx.try_send(());
                    }
                }
                Ok(bollard::container::LogOutput::StdErr { message }) => {
                    debug!(
                        job_id,
                        "mongodump stderr: {}",
                        String::from_utf8_lossy(&message)
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(BackupEngineError::StepFailed {
                        job_id,
                        step: "mongodump".into(),
                        reason: format!("stream: {}", e),
                    })
                }
            }
        }
    }
    drop(file);

    if total_bytes == 0 {
        let _ = tokio::fs::remove_file(&host_dump_path).await;
        return Err(BackupEngineError::StepFailed {
            job_id,
            step: "mongodump".into(),
            reason: "mongodump produced empty output".into(),
        });
    }

    let host_dump_str = host_dump_path.to_str().unwrap_or("").to_string();
    info!(job_id, path = %host_dump_str, size_bytes = total_bytes, "MongodbEngine mongodump: completed");

    let mut new_state = durable_state.clone();
    if let Some(obj) = new_state.as_object_mut() {
        obj.insert(DS_TEMP_PATH.to_string(), json!(host_dump_str));
    }
    Ok(new_state)
}

async fn step_upload(
    job_id: i64,
    durable_state: Value,
    deps: &MongodbDeps,
    _cancel: tokio_util::sync::CancellationToken,
) -> Result<Value, BackupEngineError> {
    let s3_key = durable_state
        .get(DS_S3_KEY)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload".into(),
            reason: "missing s3_key".into(),
        })?
        .to_string();
    let bucket = durable_state
        .get(DS_BUCKET)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload".into(),
            reason: "missing bucket".into(),
        })?
        .to_string();
    let temp_path = durable_state
        .get(DS_TEMP_PATH)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload".into(),
            reason: "missing temp_path".into(),
        })?
        .to_string();
    let s3_source_id: i32 = durable_state
        .get("s3_source_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "upload".into(),
            reason: "missing s3_source_id".into(),
        })?;

    let s3_client =
        build_s3_client(s3_source_id, deps)
            .await
            .map_err(|e| BackupEngineError::S3 {
                job_id,
                reason: format!("build S3 client: {}", e),
            })?;

    // Idempotence check.
    if let Some(size) = check_s3_object_exists(&s3_client, &bucket, &s3_key).await {
        info!(job_id, %bucket, %s3_key, "MongodbEngine upload: already exists, skipping");
        let _ = tokio::fs::remove_file(&temp_path).await;
        let mut ns = durable_state.clone();
        if let Some(o) = ns.as_object_mut() {
            o.insert(DS_SIZE_BYTES.to_string(), json!(size));
        }
        return Ok(ns);
    }

    let meta =
        tokio::fs::metadata(&temp_path)
            .await
            .map_err(|e| BackupEngineError::StepFailed {
                job_id,
                step: "upload".into(),
                reason: format!("stat: {}", e),
            })?;
    let file_size = meta.len() as i64;

    let body = aws_sdk_s3::primitives::ByteStream::from_path(std::path::Path::new(&temp_path))
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id,
            reason: format!("byte stream: {}", e),
        })?;
    s3_client
        .put_object()
        .bucket(&bucket)
        .key(&s3_key)
        .body(body)
        .content_type("application/x-gzip")
        .send()
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id,
            reason: format!("upload: {}", e),
        })?;

    if let Err(e) = tokio::fs::remove_file(&temp_path).await {
        warn!(job_id, path = %temp_path, error = %e, "MongodbEngine upload: cleanup failed");
    }

    let mut ns = durable_state.clone();
    if let Some(o) = ns.as_object_mut() {
        o.insert(DS_SIZE_BYTES.to_string(), json!(file_size));
    }
    info!(job_id, %bucket, %s3_key, "MongodbEngine upload: completed");
    Ok(ns)
}

async fn step_metadata(
    job_id: i64,
    s3_source_id: i32,
    durable_state: Value,
    deps: &MongodbDeps,
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
    let body = serde_json::to_vec(&json!({
        "backup_uuid": durable_state.get("backup_uuid").and_then(|v| v.as_str()).unwrap_or("unknown"),
        "type": "full",
        "engine": "mongodb",
        "backup_tool": "mongodump",
        "created_at": Utc::now().to_rfc3339(),
        "size_bytes": durable_state.get(DS_SIZE_BYTES).and_then(|v| v.as_i64()),
        "compression_type": "gzip",
        "source": { "id": s3_source_id },
        "s3_location": s3_key,
    })).map_err(|e| BackupEngineError::StepFailed { job_id, step: "metadata".into(), reason: format!("serialize: {}", e) })?;

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

    info!(job_id, %bucket, key = %metadata_key, "MongodbEngine metadata: written");
    Ok(())
}

// ── Utility helpers ───────────────────────────────────────────────────────────

fn build_s3_key(bucket_path: &str, service_name: &str, filename: &str) -> String {
    let prefix = bucket_path.trim_matches('/');
    let date = Utc::now().format("%Y/%m/%d");
    if prefix.is_empty() {
        format!(
            "external_services/mongodb/{}/{}/{}",
            service_name, date, filename
        )
    } else {
        format!(
            "{}/external_services/mongodb/{}/{}/{}",
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
    deps: &MongodbDeps,
) -> Result<S3Client, BackupEngineError> {
    let src = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::S3 {
            job_id: 0,
            reason: format!("db: {}", e),
        })?
        .ok_or_else(|| BackupEngineError::S3 {
            job_id: 0,
            reason: format!("s3_source {} not found", s3_source_id),
        })?;
    build_s3_client_from_source(0, &src, deps)
}

fn build_s3_client_from_source(
    job_id: i64,
    s3_source: &temps_entities::s3_sources::Model,
    deps: &MongodbDeps,
) -> Result<S3Client, BackupEngineError> {
    use aws_sdk_s3::Config;
    let ak = deps
        .encryption_service
        .decrypt_string(&s3_source.access_key_id)
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("decrypt ak: {}", e),
        })?;
    let sk = deps
        .encryption_service
        .decrypt_string(&s3_source.secret_key)
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!("decrypt sk: {}", e),
        })?;
    let creds = aws_sdk_s3::config::Credentials::new(ak, sk, None, None, "mongodb-engine");
    let mut b = Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(s3_source.region.clone()))
        .force_path_style(s3_source.force_path_style.unwrap_or(true))
        .credentials_provider(creds);
    if let Some(ep) = &s3_source.endpoint {
        let url = if ep.starts_with("http") {
            ep.clone()
        } else {
            format!("http://{}", ep)
        };
        b = b.endpoint_url(url);
    }
    Ok(S3Client::from_conf(b.build()))
}

async fn check_s3_object_exists(client: &S3Client, bucket: &str, key: &str) -> Option<i64> {
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(r) => r.content_length(),
        Err(_) => None,
    }
}

async fn rollback_s3_object(
    job_id: i64,
    ctx: &BackupContext,
    cursor: &StepCursor,
    deps: &MongodbDeps,
) {
    let key = cursor
        .durable_state
        .get(DS_S3_KEY)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let bucket = cursor
        .durable_state
        .get(DS_BUCKET)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let (Some(k), Some(b)) = (key, bucket) {
        let s3_source_id = ctx
            .params
            .get("s3_source_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0);
        if s3_source_id > 0 {
            if let Ok(client) = build_s3_client(s3_source_id, deps).await {
                if let Err(e) = client.delete_object().bucket(&b).key(&k).send().await {
                    warn!(job_id, %b, %k, error = %e, "MongodbEngine rollback: S3 delete failed");
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use temps_backup_core::{
        BackupContext, BackupEngine, BackupEngineError, StepCursor, StepEvent,
    };
    use tokio_util::sync::CancellationToken;

    struct TestMongodbEngine {
        call_count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl TestMongodbEngine {
        fn new() -> Self {
            Self {
                call_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            }
        }
    }

    impl BackupEngine for TestMongodbEngine {
        fn engine(&self) -> &'static str {
            "mongodb"
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
                    yield StepEvent::StepCompleted { step: "preflight".into(), durable_state: json!({"s3_key": "k", "bucket": "b"}), message: None };
                    yield StepEvent::StepCompleted { step: "mongodump".into(), durable_state: json!({"temp_path": "/tmp/m.gz"}), message: None };
                    Err(BackupEngineError::StepFailed { job_id: 0, step: "upload".into(), reason: "crash".into() })?;
                } else {
                    let current = cursor.current_step.as_deref().unwrap_or("none");
                    if current != "mongodump" {
                        Err(BackupEngineError::StepFailed { job_id: 0, step: "check".into(), reason: format!("expected mongodump, got {}", current) })?;
                    }
                    yield StepEvent::StepCompleted { step: "upload".into(), durable_state: json!({"size_bytes": 256}), message: None };
                    yield StepEvent::StepCompleted { step: "metadata".into(), durable_state: json!({}), message: None };
                    yield StepEvent::Done { location: "k".into(), size_bytes: Some(256), compression: "gzip".into() };
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
        assert_eq!(TestMongodbEngine::new().engine(), "mongodb");
    }

    #[test]
    fn test_steps_list() {
        let e = TestMongodbEngine::new();
        assert_eq!(e.steps(), STEPS);
        assert_eq!(e.steps()[1], "mongodump");
    }

    #[tokio::test]
    async fn test_crash_resume_cursor_is_correct() {
        let engine = TestMongodbEngine::new();
        let ctx = make_ctx();
        let mut stream = engine.execute(
            &ctx,
            StepCursor {
                current_step: None,
                durable_state: json!({}),
            },
        );
        let mut last = None;
        let mut errored = false;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StepEvent::StepCompleted { ref step, .. }) => last = Some(step.clone()),
                Ok(_) => {}
                Err(_) => {
                    errored = true;
                    break;
                }
            }
        }
        assert!(errored);
        assert_eq!(last.as_deref(), Some("mongodump"));

        let mut stream2 = engine.execute(
            &ctx,
            StepCursor {
                current_step: last,
                durable_state: json!({}),
            },
        );
        let mut done = false;
        while let Some(ev) = stream2.next().await {
            match ev {
                Ok(StepEvent::Done { .. }) => done = true,
                Ok(_) => {}
                Err(e) => panic!("resume: {}", e),
            }
        }
        assert!(done);
    }
}
