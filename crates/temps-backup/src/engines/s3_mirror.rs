//! `S3MirrorEngine`: `BackupEngine` for S3-compatible object storage services
//! (ADR-014 Phase 4 §"MongoDB, S3 mirror, RustFS engines").
//!
//! Steps: `list_source` → `sync` → `metadata`.
//!
//! ## Design notes
//!
//! Lifts the `mc mirror` approach from
//! `temps-providers/src/externalsvc/s3.rs:1087` (`backup_to_s3`). Uses the
//! MinIO Client (`mc`) Docker container to mirror all objects from the source
//! bucket to a destination prefix in the backup S3 source.
//!
//! Applies to `service_type` in `{"s3", "minio", "blob"}`.
//!
//! ## Heartbeat discipline
//!
//! `sync` runs `mc mirror --overwrite` which can take many minutes for large
//! buckets. Uses the mpsc + select pattern from `control_plane.rs:213–254`.
//! The step polls exit status every 2 seconds and sends a heartbeat tick every
//! [`HEARTBEAT_INTERVAL`].
//!
//! ## Idempotence
//!
//! `mc mirror` is idempotent by design: re-running on resume skips objects that
//! already exist in the destination (unless they changed). The step is always
//! re-run on resume; no state flag is needed.

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

/// MinIO Client Docker image (same constant as in s3.rs).
const MC_IMAGE: &str = "minio/mc:RELEASE.2025-08-13T08-35-41Z";

/// Heartbeat interval during the `sync` step.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(120);

/// Steps emitted by `S3MirrorEngine` in execution order.
const STEPS: &[&str] = &["list_source", "sync", "metadata"];

// ── durable_state keys ────────────────────────────────────────────────────────
const DS_S3_KEY: &str = "s3_key";
const DS_BUCKET: &str = "bucket";
const DS_SIZE_BYTES: &str = "size_bytes";
const DS_DEST_PREFIX: &str = "dest_prefix";

// ── Dependencies ─────────────────────────────────────────────────────────────

/// Dependencies injected into `S3MirrorEngine` at construction time.
pub struct S3MirrorDeps {
    pub db: Arc<DatabaseConnection>,
    pub encryption_service: Arc<EncryptionService>,
    pub docker: bollard::Docker,
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// `BackupEngine` for S3-compatible object storage services.
///
/// Uses `mc mirror --overwrite` to copy all objects from the source service's
/// bucket to a timestamped prefix in the backup destination.
/// Reference: `s3.rs:1087` (`backup_to_s3`).
pub struct S3MirrorEngine {
    deps: Arc<S3MirrorDeps>,
}

impl S3MirrorEngine {
    pub fn new(deps: S3MirrorDeps) -> Self {
        Self {
            deps: Arc::new(deps),
        }
    }
}

#[async_trait::async_trait]
impl BackupEngine for S3MirrorEngine {
    fn engine(&self) -> &'static str {
        "s3_mirror"
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
                    debug!(job_id, step, "S3MirrorEngine: cancellation requested");
                    return;
                }
                info!(job_id, attempt, step, "S3MirrorEngine: executing step");

                match *step {
                    "list_source" => {
                        let state = step_list_source(job_id, service_id, s3_source_id, &deps).await?;
                        accumulated_state = state.clone();
                        yield StepEvent::StepCompleted {
                            step: "list_source".into(),
                            durable_state: state,
                            message: Some(format!(
                                "service {} and S3 source {} validated; destination prefix set",
                                service_id, s3_source_id
                            )),
                        };
                    }

                    "sync" => {
                        // Drive the long-running mc mirror exec with heartbeats.
                        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<()>(8);

                        let mut step_fut = std::pin::pin!(step_sync(
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
                                    debug!(job_id, "S3MirrorEngine sync: Heartbeat");
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
                            step: "sync".into(),
                            durable_state: state,
                            message: Some("mc mirror completed".into()),
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
                        info!(job_id, location = %s3_key, ?size_bytes, "S3MirrorEngine: Done");
                        yield StepEvent::Done {
                            location: s3_key,
                            size_bytes,
                            compression: "none".into(),
                        };
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
        // mc mirror writes to the destination S3 prefix; best-effort S3 cleanup.
        // We intentionally skip deletion here: a partial mirror may be useful for
        // recovery and `mc mirror` is idempotent on re-run. Log for visibility.
        if let Some(prefix) = cursor
            .durable_state
            .get(DS_DEST_PREFIX)
            .and_then(|v| v.as_str())
        {
            warn!(
                job_id,
                dest_prefix = %prefix,
                "S3MirrorEngine rollback: partial mirror objects left at destination (idempotent re-run will complete them)",
            );
        }
        Ok(())
    }
}

// ── Step helpers ──────────────────────────────────────────────────────────────

/// `list_source` step: validate the service and S3 destination, derive the
/// destination prefix, and record the intended location in `durable_state`.
async fn step_list_source(
    job_id: i64,
    service_id: i32,
    s3_source_id: i32,
    deps: &S3MirrorDeps,
) -> Result<Value, BackupEngineError> {
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

    let s3_dest = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
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

    // Verify destination bucket is reachable.
    let dest_client = build_s3_client_from_source(job_id, &s3_dest, deps)?;
    dest_client
        .head_bucket()
        .bucket(&s3_dest.bucket_name)
        .send()
        .await
        .map_err(|e| BackupEngineError::Preflight {
            job_id,
            reason: format!(
                "destination S3 bucket '{}' not reachable: {}",
                s3_dest.bucket_name, e
            ),
        })?;

    // Derive stable destination prefix: external_services/s3/<service-name>/<backup-uuid>/
    let backup_uuid = Uuid::new_v4().to_string();
    let dest_prefix = build_dest_prefix(&s3_dest.bucket_path, &service.name, &backup_uuid);
    // s3_key is the same as dest_prefix (mirrors are "location = prefix").
    let s3_key = dest_prefix.clone();

    info!(
        job_id,
        %s3_key,
        dest_bucket = %s3_dest.bucket_name,
        service_name = %service.name,
        "S3MirrorEngine list_source: validated; destination prefix set",
    );

    // Load the source service's connection parameters so step_sync can build mc env vars.
    // The config is encrypted; decrypt here and pass the JSON in durable_state.
    let config_json = deps
        .encryption_service
        .decrypt_string(service.config.as_deref().unwrap_or("{}"))
        .unwrap_or_else(|_| "{}".to_string());
    let service_params: Value = serde_json::from_str(&config_json).unwrap_or_else(|_| json!({}));

    Ok(json!({
        DS_S3_KEY: s3_key,
        DS_BUCKET: s3_dest.bucket_name,
        DS_DEST_PREFIX: dest_prefix,
        "backup_uuid": backup_uuid,
        "s3_source_id": s3_source_id,
        "service_id": service_id,
        "service_name": service.name,
        "bucket_path": s3_dest.bucket_path,
        // Source service parameters needed by step_sync to connect to the source.
        "source_params": service_params,
    }))
}

/// `sync` step: run `mc mirror --overwrite` from source bucket to destination.
///
/// Launches an ephemeral MinIO Client container, sets up mc aliases for both
/// source and destination, then runs `mc mirror`. Emits heartbeat ticks during
/// polling. Reference: `s3.rs:1087` (`backup_to_s3`).
async fn step_sync(
    job_id: i64,
    durable_state: Value,
    deps: Arc<S3MirrorDeps>,
    _cancel: tokio_util::sync::CancellationToken,
    heartbeat_tx: tokio::sync::mpsc::Sender<()>,
) -> Result<Value, BackupEngineError> {
    let dest_prefix = durable_state
        .get(DS_DEST_PREFIX)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: "durable_state missing dest_prefix".into(),
        })?
        .to_string();

    let s3_source_id: i32 = durable_state
        .get("s3_source_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: "durable_state missing s3_source_id".into(),
        })?;

    let service_id: i32 = durable_state
        .get("service_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: "durable_state missing service_id".into(),
        })?;

    // Load destination S3 credentials.
    let s3_dest = temps_entities::s3_sources::Entity::find_by_id(s3_source_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("db s3_source: {}", e),
        })?
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: "s3_source not found".into(),
        })?;

    let dest_access_key = deps
        .encryption_service
        .decrypt_string(&s3_dest.access_key_id)
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("decrypt dest ak: {}", e),
        })?;
    let dest_secret_key = deps
        .encryption_service
        .decrypt_string(&s3_dest.secret_key)
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("decrypt dest sk: {}", e),
        })?;

    // Load source service parameters (host, port, access_key, secret_key).
    let service = temps_entities::external_services::Entity::find_by_id(service_id)
        .one(deps.db.as_ref())
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("db service: {}", e),
        })?
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("service {} not found", service_id),
        })?;

    let service_config_json = deps
        .encryption_service
        .decrypt_string(service.config.as_deref().unwrap_or("{}"))
        .unwrap_or_else(|_| "{}".to_string());
    let source_params: Value =
        serde_json::from_str(&service_config_json).unwrap_or_else(|_| json!({}));
    let source_access_key = source_params
        .get("access_key")
        .or_else(|| source_params.get("access_key_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source_secret_key = source_params
        .get("secret_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source_host = source_params
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("localhost")
        .to_string();
    let source_port = source_params
        .get("port")
        .and_then(|v| v.as_str().or_else(|| v.as_u64().map(|_| "9000")))
        .unwrap_or("9000")
        .to_string();
    let source_bucket = source_params
        .get("bucket_name")
        .or_else(|| source_params.get("bucket"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // `source_endpoint` is no longer used directly (replaced by `MC_HOST_source`
    // env var below). Kept for diagnostic parity with `dest_endpoint`.
    let _source_endpoint = format!("http://{}:{}", source_host, source_port);
    let dest_endpoint = s3_dest.endpoint.as_deref().unwrap_or("").to_string();
    let dest_endpoint = if dest_endpoint.is_empty() {
        format!("http://{}:9000", s3_dest.bucket_name)
    } else {
        dest_endpoint
    };

    // Pull mc image (best-effort, container may already be present).
    pull_mc_image(job_id, &deps.docker).await?;

    let container_name = format!("temps-s3mirror-backup-{}", Uuid::new_v4());

    // Build `MC_HOST_<alias>` URLs preserving the destination endpoint's
    // original scheme. The previous draft hardcoded `http://` and stripped
    // only `http://` from the endpoint, producing broken URLs like
    // `http://<ak>:<sk>@https://r2.cloudflarestorage.com` when the dest used
    // HTTPS (e.g. Cloudflare R2). mc couldn't parse those and emitted a
    // misleading "Invalid arguments" error during mirror init.
    let (dest_scheme, dest_hostpath) = if let Some(rest) = dest_endpoint.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = dest_endpoint.strip_prefix("http://") {
        ("http", rest)
    } else {
        ("http", dest_endpoint.as_str())
    };
    let env_vars: Vec<String> = vec![
        format!(
            "MC_HOST_source=http://{}:{}@{}:{}",
            source_access_key, source_secret_key, source_host, source_port
        ),
        format!(
            "MC_HOST_dest={}://{}:{}@{}",
            dest_scheme, dest_access_key, dest_secret_key, dest_hostpath
        ),
    ];

    // Override the mc image's default entrypoint with a long sleep so the
    // container stays alive long enough for the alias-set + mirror execs to
    // attach. The previous draft used `entrypoint = ["sh"]` with no command,
    // which is `sh` reading from no stdin → immediate EOF → container exits
    // within ~30ms. Subsequent `docker exec` calls then fail with Docker
    // "409: container not running" (verified in prod log 21:15:15).
    //
    // 24h matches the postgres_pgdump sidecar (`postgres_pgdump.rs:308-309`)
    // — must outlive even very large mirror operations. Reaped explicitly
    // by `cleanup_container` after the mirror completes.
    let container_config = bollard::models::ContainerCreateBody {
        image: Some(MC_IMAGE.to_string()),
        env: Some(env_vars.to_vec()),
        entrypoint: Some(vec!["/bin/sleep".to_string()]),
        cmd: Some(vec!["86400".to_string()]),
        tty: Some(false),
        attach_stdin: Some(false),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        host_config: Some(bollard::models::HostConfig {
            network_mode: Some("host".to_string()),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    let container = deps
        .docker
        .create_container(
            Some(
                bollard::query_parameters::CreateContainerOptionsBuilder::new()
                    .name(&container_name)
                    .build(),
            ),
            container_config,
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("create mc container: {}", e),
        })?;

    deps.docker
        .start_container(
            &container.id,
            None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
        .map_err(|e| BackupEngineError::StepFailed {
            job_id,
            step: "sync".into(),
            reason: format!("start mc container: {}", e),
        })?;

    // `MC_HOST_source` and `MC_HOST_dest` env vars already configure the
    // aliases (set at container creation, line 358-361). Running `mc alias set`
    // again here is redundant and previously caused R2 errors — mc would
    // re-derive endpoint signing without picking up `force_path_style` and
    // emit a misleading `Unable to initialize "dest/<path>"` failure. The env
    // vars are the documented way to configure mc aliases non-interactively.
    let source_path = if source_bucket.is_empty() {
        "source/".to_string()
    } else {
        format!("source/{}/", source_bucket)
    };
    // mc mirror requires the destination to end with `/` when the source is a
    // prefix, otherwise mc treats the dest as a single object key and the
    // bucket-init fails with the same "Invalid arguments" R2 reports.
    let dest_path = format!(
        "dest/{}/{}/",
        s3_dest.bucket_name,
        dest_prefix.trim_matches('/'),
    );
    let mirror_args = vec![
        "mc",
        "mirror",
        "--overwrite",
        source_path.as_str(),
        dest_path.as_str(),
    ];

    // Helper to clean up the container on error.
    let cleanup_container = |docker: bollard::Docker, id: String| async move {
        let _ = docker
            .remove_container(
                &id,
                Some(bollard::query_parameters::RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
    };

    for cmd in [mirror_args] {
        let exec = match deps
            .docker
            .create_exec(
                &container.id,
                bollard::exec::CreateExecOptions {
                    cmd: Some(cmd.clone()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(e) => e,
            Err(e) => {
                cleanup_container(deps.docker.clone(), container.id.clone()).await;
                return Err(BackupEngineError::StepFailed {
                    job_id,
                    step: "sync".into(),
                    reason: format!("create exec: {}", e),
                });
            }
        };

        // For the mirror command, stream output with heartbeats so we can
        // keep the runner lease alive for large buckets.
        let is_mirror = cmd.get(1) == Some(&"mirror");
        if is_mirror {
            let stream_result = match deps
                .docker
                .start_exec(
                    &exec.id,
                    Some(bollard::exec::StartExecOptions {
                        detach: false,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    cleanup_container(deps.docker.clone(), container.id.clone()).await;
                    return Err(BackupEngineError::StepFailed {
                        job_id,
                        step: "sync".into(),
                        reason: format!("start exec: {}", e),
                    });
                }
            };

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
                            error!(
                                job_id,
                                engine = "s3_mirror",
                                "mc mirror exec stream error: {}",
                                e
                            );
                            break;
                        }
                    }
                    if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                        let _ = heartbeat_tx.try_send(());
                        last_hb = Instant::now();
                    }
                }
            }

            let inspect = deps.docker.inspect_exec(&exec.id).await.map_err(|e| {
                BackupEngineError::StepFailed {
                    job_id,
                    step: "sync".into(),
                    reason: format!("inspect exec: {}", e),
                }
            });
            let stdout = stdout_tail.into_string_lossy();
            let stderr = stderr_tail.into_string_lossy();
            match inspect {
                Ok(insp) => {
                    if let Some(code) = insp.exit_code {
                        if code != 0 {
                            cleanup_container(deps.docker.clone(), container.id.clone()).await;
                            return Err(BackupEngineError::StepFailed {
                                job_id,
                                step: "sync".into(),
                                reason: format!(
                                    "mc mirror exited with code {}. stderr: {}. stdout: {}",
                                    code,
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
                    }
                    if !stderr.trim().is_empty() {
                        info!(
                            job_id,
                            engine = "s3_mirror",
                            "mc mirror stderr (warnings): {}",
                            stderr.trim()
                        );
                    }
                }
                Err(e) => {
                    cleanup_container(deps.docker.clone(), container.id.clone()).await;
                    return Err(e);
                }
            }
        } else {
            // For alias setup commands, run attached and check exit code.
            if let Err(e) = deps.docker.start_exec(&exec.id, None).await {
                cleanup_container(deps.docker.clone(), container.id.clone()).await;
                return Err(BackupEngineError::StepFailed {
                    job_id,
                    step: "sync".into(),
                    reason: format!("start exec: {}", e),
                });
            }
            if let Ok(Some(inspect)) = deps
                .docker
                .inspect_exec(&exec.id)
                .await
                .map(|r| r.exit_code)
            {
                if inspect != 0 {
                    cleanup_container(deps.docker.clone(), container.id.clone()).await;
                    return Err(BackupEngineError::StepFailed {
                        job_id,
                        step: "sync".into(),
                        reason: format!(
                            "mc command {:?} exited with code {}",
                            &cmd[..2.min(cmd.len())],
                            inspect
                        ),
                    });
                }
            }
        }
    }

    // Clean up the container (auto_remove=true handles it, but be explicit on success too).
    let _ = deps
        .docker
        .remove_container(
            &container.id,
            Some(bollard::query_parameters::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    // Compute total size of the mirrored prefix.
    let size_bytes = match build_s3_client_from_source(job_id, &s3_dest, &deps) {
        Ok(client) => {
            let total = list_total_s3_size_sync(
                client,
                s3_dest.bucket_name.clone(),
                dest_prefix.trim_matches('/').to_string(),
            )
            .await;
            Some(total)
        }
        Err(e) => {
            warn!(job_id, error = %e, "S3MirrorEngine sync: could not build S3 client for size calculation");
            None
        }
    };

    info!(job_id, %dest_prefix, ?size_bytes, "S3MirrorEngine sync: mc mirror completed");

    let mut new_state = durable_state.clone();
    if let Some(obj) = new_state.as_object_mut() {
        if let Some(sz) = size_bytes {
            obj.insert(DS_SIZE_BYTES.to_string(), json!(sz));
        }
    }
    Ok(new_state)
}

/// `metadata` step: write a `metadata.json` manifest at the destination prefix.
async fn step_metadata(
    job_id: i64,
    s3_source_id: i32,
    durable_state: Value,
    deps: &S3MirrorDeps,
) -> Result<(), BackupEngineError> {
    let dest_prefix = durable_state
        .get(DS_DEST_PREFIX)
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackupEngineError::StepFailed {
            job_id,
            step: "metadata".into(),
            reason: "missing dest_prefix".into(),
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

    let metadata_key = format!("{}/metadata.json", dest_prefix.trim_matches('/'));
    let body = serde_json::to_vec(&json!({
        "type": "full",
        "engine": "s3_mirror",
        "backup_tool": "mc",
        "created_at": Utc::now().to_rfc3339(),
        "size_bytes": durable_state.get(DS_SIZE_BYTES).and_then(|v| v.as_i64()),
        "compression_type": "none",
        "source": { "id": s3_source_id },
        "s3_location": dest_prefix,
    }))
    .map_err(|e| BackupEngineError::StepFailed {
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

    info!(job_id, %bucket, key = %metadata_key, "S3MirrorEngine metadata: written");
    Ok(())
}

// ── Utility helpers ───────────────────────────────────────────────────────────

fn build_dest_prefix(bucket_path: &str, service_name: &str, backup_uuid: &str) -> String {
    let base = bucket_path.trim_matches('/');
    if base.is_empty() {
        format!("external_services/s3/{}/{}", service_name, backup_uuid)
    } else {
        format!(
            "{}/external_services/s3/{}/{}",
            base, service_name, backup_uuid
        )
    }
}

async fn pull_mc_image(job_id: i64, docker: &bollard::Docker) -> Result<(), BackupEngineError> {
    use bollard::query_parameters::CreateImageOptionsBuilder;
    use futures::StreamExt;

    let (image_name, tag) = MC_IMAGE.split_once(':').unwrap_or((MC_IMAGE, "latest"));

    let mut stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::new()
                .from_image(image_name)
                .tag(tag)
                .build(),
        ),
        None,
        None,
    );
    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            warn!(job_id, error = %e, "S3MirrorEngine pull_mc_image: pull warning (may still work if image is cached)");
        }
    }
    Ok(())
}

async fn build_s3_client(
    s3_source_id: i32,
    deps: &S3MirrorDeps,
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
    deps: &S3MirrorDeps,
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
    let creds = aws_sdk_s3::config::Credentials::new(ak, sk, None, None, "s3-mirror-engine");
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

async fn list_total_s3_size_sync(client: S3Client, bucket: String, prefix: String) -> i64 {
    let mut total: i64 = 0;
    let mut continuation: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix);
        if let Some(tok) = continuation {
            req = req.continuation_token(tok);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => break,
        };
        for obj in resp.contents() {
            total += obj.size().unwrap_or(0);
        }
        if resp.is_truncated().unwrap_or(false) {
            continuation = resp.next_continuation_token().map(|s| s.to_string());
        } else {
            break;
        }
    }
    total
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

    /// Test engine matching `S3MirrorEngine`'s step list.
    struct TestS3MirrorEngine {
        call_count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl TestS3MirrorEngine {
        fn new() -> Self {
            Self {
                call_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            }
        }
    }

    impl BackupEngine for TestS3MirrorEngine {
        fn engine(&self) -> &'static str {
            "s3_mirror"
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
                        step: "list_source".into(),
                        durable_state: json!({
                            "s3_key": "external_services/s3/my-svc/uuid123",
                            "bucket": "backup-bucket",
                            "dest_prefix": "external_services/s3/my-svc/uuid123",
                        }),
                        message: None,
                    };
                    // Simulate crash before sync completes.
                    Err(BackupEngineError::StepFailed {
                        job_id: 0,
                        step: "sync".into(),
                        reason: "simulated crash during mc mirror".into(),
                    })?;
                } else {
                    // Resume: cursor.current_step should be "list_source".
                    let current = cursor.current_step.as_deref().unwrap_or("none");
                    if current != "list_source" {
                        Err(BackupEngineError::StepFailed {
                            job_id: 0,
                            step: "resume-check".into(),
                            reason: format!("expected list_source on resume, got: {}", current),
                        })?;
                    }
                    yield StepEvent::StepCompleted {
                        step: "sync".into(),
                        durable_state: json!({"size_bytes": 4096}),
                        message: None,
                    };
                    yield StepEvent::StepCompleted {
                        step: "metadata".into(),
                        durable_state: json!({}),
                        message: None,
                    };
                    yield StepEvent::Done {
                        location: "external_services/s3/my-svc/uuid123".into(),
                        size_bytes: Some(4096),
                        compression: "none".into(),
                    };
                }
            })
        }
    }

    fn make_ctx() -> BackupContext {
        use futures::executor::block_on;
        BackupContext {
            job_id: 1,
            attempt: 1,
            params: json!({"service_id": 1, "s3_source_id": 1}),
            db: Arc::new(block_on(sea_orm::Database::connect("sqlite::memory:")).unwrap()),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn test_engine_key() {
        let engine = TestS3MirrorEngine::new();
        assert_eq!(engine.engine(), "s3_mirror");
    }

    #[test]
    fn test_steps_list() {
        let engine = TestS3MirrorEngine::new();
        assert_eq!(engine.steps(), STEPS);
        assert_eq!(engine.steps()[0], "list_source");
        assert_eq!(engine.steps()[1], "sync");
        assert_eq!(engine.steps()[2], "metadata");
    }

    #[test]
    fn test_build_dest_prefix_with_path() {
        let prefix = build_dest_prefix("backups", "my-svc", "uuid-abc");
        assert_eq!(prefix, "backups/external_services/s3/my-svc/uuid-abc");
    }

    #[test]
    fn test_build_dest_prefix_without_path() {
        let prefix = build_dest_prefix("", "my-svc", "uuid-abc");
        assert_eq!(prefix, "external_services/s3/my-svc/uuid-abc");
    }

    #[tokio::test]
    async fn test_crash_resume_cursor_is_correct() {
        let engine = TestS3MirrorEngine::new();
        let ctx = make_ctx();

        // First attempt: emit list_source, then crash before sync.
        let cursor1 = StepCursor {
            current_step: None,
            durable_state: json!({}),
        };
        let mut stream1 = engine.execute(&ctx, cursor1);

        let mut last_completed = None;
        let mut errored = false;
        while let Some(ev) = stream1.next().await {
            match ev {
                Ok(StepEvent::StepCompleted { ref step, .. }) => {
                    last_completed = Some(step.clone())
                }
                Ok(_) => {}
                Err(_) => {
                    errored = true;
                    break;
                }
            }
        }
        assert!(errored, "first attempt should error");
        assert_eq!(
            last_completed.as_deref(),
            Some("list_source"),
            "cursor should point to list_source"
        );

        // Second attempt: resume from list_source; engine should continue with sync.
        let cursor2 = StepCursor {
            current_step: last_completed,
            durable_state: json!({}),
        };
        let mut stream2 = engine.execute(&ctx, cursor2);
        let mut done = false;
        while let Some(ev) = stream2.next().await {
            match ev {
                Ok(StepEvent::Done { .. }) => done = true,
                Ok(_) => {}
                Err(e) => panic!("resume failed: {}", e),
            }
        }
        assert!(done, "second attempt should complete with Done");
    }
}
