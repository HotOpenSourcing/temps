//! Sidecar-shell detection for backup engines.
//!
//! Some engines need to invoke a shell pipeline inside a sidecar container
//! (e.g. `pg_dumpall | gzip`). Different sidecar images ship different
//! default shells:
//!
//! - Debian-based images (`postgres-walg:18-bookworm`, etc.) ship `bash`.
//! - Alpine-based and slimmer images may have only `dash`/`ash` mounted at
//!   `/bin/sh`. `bash` is not present.
//!
//! When `bash` is available we use it because it supports `set -o pipefail`,
//! which surfaces the real exit code of every command in a pipe. Without
//! `pipefail`, a `pg_dumpall | gzip` pipeline reports success even if
//! `pg_dumpall` failed — `gzip` exits 0 on empty input and the shell only
//! reports the last command's status. That's exactly the failure mode that
//! produced 20-byte "successful" backups in prod.
//!
//! When `bash` is not available, callers fall back to a POSIX-portable
//! two-stage `&&` form (dump to uncompressed file, then gzip it). `&&`
//! short-circuits, so a `pg_dumpall` failure skips `gzip` and the compound
//! exit code is `pg_dumpall`'s real one.

use bollard::exec::{CreateExecOptions, StartExecOptions};
use bollard::Docker;

/// Which shell is available in a sidecar container.
///
/// Pass to `cmd: vec![shell_kind.as_str(), "-c", ...]` for docker exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// `bash` is installed; supports `set -o pipefail` and array vars.
    Bash,
    /// Only POSIX `sh` (could be `dash`, `ash`, or busybox). No bashisms.
    Sh,
}

impl ShellKind {
    /// The CLI name to pass as `argv[0]` of the docker exec.
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Sh => "sh",
        }
    }
}

/// Probe the running sidecar container for `bash`. Returns
/// [`ShellKind::Bash`] when present, [`ShellKind::Sh`] otherwise.
///
/// On any docker-exec failure we conservatively report [`ShellKind::Sh`] —
/// `sh` is guaranteed to exist on every container image we run backups
/// against, and the sh-path is safe (it just trades a single pipe for
/// transient 2x disk usage during the dump+gzip step).
///
/// The probe is short: `command -v bash`, polled for up to ~2 seconds.
/// In practice it returns in tens of milliseconds because the only thing
/// it runs is shell builtin command lookup.
pub async fn detect_shell(docker: &Docker, container_name: &str) -> ShellKind {
    // `command -v bash` is a POSIX builtin that exits 0 if `bash` is on
    // $PATH and non-zero otherwise. We deliberately do NOT call `which`
    // (not always installed) or `[ -x /usr/bin/bash ]` (path varies).
    let exec = match docker
        .create_exec(
            container_name,
            CreateExecOptions {
                cmd: Some(vec!["sh", "-c", "command -v bash"]),
                attach_stdout: Some(false),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
    {
        Ok(e) => e,
        Err(_) => return ShellKind::Sh,
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
        return ShellKind::Sh;
    }

    // Poll inspect for up to ~2s; `command -v` is near-instant in practice.
    for _ in 0..10u32 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        match docker.inspect_exec(&exec.id).await {
            Ok(info) if info.running == Some(false) => {
                return if info.exit_code == Some(0) {
                    ShellKind::Bash
                } else {
                    ShellKind::Sh
                };
            }
            Ok(_) => continue,
            Err(_) => return ShellKind::Sh,
        }
    }

    // Took longer than 2s — extremely unusual for a builtin. Fall back to
    // sh rather than block the backup; the sh path always works.
    ShellKind::Sh
}
