//! Shared helper for ensuring a Docker image is locally cached before
//! `create_container`.
//!
//! Without this, fresh prod hosts 404 on `create_container` with
//! "No such image: …" because the engine assumed Docker would auto-pull
//! (it doesn't — bare `create_container` against a missing image fails).

use bollard::Docker;
use futures::stream::StreamExt as FuturesStreamExt;
use tracing::{debug, info};

use temps_backup_core::engine_v2::BackupError;

/// Ensure `image_tag` is pulled and available locally. No-op if Docker
/// already has the image cached; otherwise streams a pull and returns
/// after the daemon reports completion.
///
/// Pull failures are mapped to `BackupError::Failed` so the engine's caller
/// surfaces a useful message ("failed to pull '…': …") instead of a generic
/// 404 on the subsequent `create_container` call.
///
/// `engine` is the engine key (e.g. `"control_plane"`, `"postgres_pgdump"`);
/// errors carry it in logs so failures land on the right engine in the UI.
pub async fn ensure_image_pulled_v2(image_tag: &str, engine: &str) -> Result<(), BackupError> {
    let docker = Docker::connect_with_local_defaults().map_err(|e| BackupError::Failed {
        reason: format!(
            "ensure_image_pulled_v2 ({}): failed to connect to Docker: {}",
            engine, e
        ),
    })?;

    if docker.inspect_image(image_tag).await.is_ok() {
        return Ok(());
    }

    info!(
        image_tag,
        engine, "ensure_image_pulled_v2: image not cached, pulling"
    );

    use bollard::query_parameters::CreateImageOptionsBuilder;
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
                    debug!(image_tag, engine, "Docker pull: {}", status);
                }
            }
            Err(e) => {
                return Err(BackupError::Failed {
                    reason: format!("failed to pull '{}': {}", image_tag, e),
                });
            }
        }
    }

    info!(image_tag, engine, "ensure_image_pulled_v2: pull complete");
    Ok(())
}
