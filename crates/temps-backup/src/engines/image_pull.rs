//! Shared helper for ensuring a Docker image is locally cached before
//! `create_container`.
//!
//! Without this, fresh prod hosts 404 on `create_container` with
//! "No such image: …" because the engine assumed Docker would auto-pull
//! (it doesn't — bare `create_container` against a missing image fails).
//!
//! The helper is engine-agnostic so every sidecar-creating engine can
//! call into the same pre-pull + error-mapping code. Each engine passes
//! its own `step` name + `job_id` so the resulting `StepFailed` error
//! lands on the correct step in the UI.

use bollard::Docker;
use futures::stream::StreamExt as FuturesStreamExt;
use tracing::{debug, info};

use temps_backup_core::BackupEngineError;

/// Ensure `image_tag` is pulled and available locally. No-op if Docker
/// already has the image cached; otherwise streams a pull and returns
/// after the daemon reports completion.
///
/// Pull failures are mapped to `BackupEngineError::StepFailed` so the
/// engine's caller surfaces a useful message ("failed to pull sidecar
/// image '…': …") instead of a generic 404 on the subsequent
/// `create_container` call.
///
/// `step` is the engine's current step name (e.g. `"dump"`,
/// `"pg_dumpall"`); errors carry it verbatim so the UI shows
/// "Step '<step>' failed: …".
pub async fn ensure_image_pulled(
    job_id: i64,
    docker: &Docker,
    image_tag: &str,
    step: &str,
) -> Result<(), BackupEngineError> {
    if docker.inspect_image(image_tag).await.is_ok() {
        // Image is already present — nothing to do.
        return Ok(());
    }

    info!(
        job_id,
        image_tag, step, "ensure_image_pulled: image not cached, pulling"
    );

    pull_image(job_id, docker, image_tag, step).await
}

/// Stream a Docker image pull. Split out from `ensure_image_pulled` so
/// callers that already know the image is missing (e.g. retry paths)
/// can skip the inspect probe.
async fn pull_image(
    job_id: i64,
    docker: &Docker,
    image_tag: &str,
    step: &str,
) -> Result<(), BackupEngineError> {
    use bollard::query_parameters::CreateImageOptionsBuilder;

    // Split `image:tag` so Bollard asks the registry for the right manifest.
    // Fall back to `:latest` when the caller passed a bare image name (matches
    // Docker CLI behaviour; shouldn't happen for engine sidecars but harmless).
    let (image, tag) = match image_tag.split_once(':') {
        Some((i, t)) => (i, t),
        None => (image_tag, "latest"),
    };

    let options = CreateImageOptionsBuilder::new()
        .from_image(image)
        .tag(tag)
        .build();

    let mut stream = docker.create_image(Some(options), None, None);

    while let Some(result) = FuturesStreamExt::next(&mut stream).await {
        match result {
            Ok(info) => {
                if let Some(status) = info.status {
                    debug!(job_id, image_tag, step, "Docker pull: {}", status);
                }
            }
            Err(e) => {
                return Err(BackupEngineError::StepFailed {
                    job_id,
                    step: step.to_string(),
                    reason: format!("failed to pull sidecar image '{}': {}", image_tag, e),
                });
            }
        }
    }

    info!(
        job_id,
        image_tag, step, "ensure_image_pulled: pull complete"
    );
    Ok(())
}
