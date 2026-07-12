//! Image prefetch for OCI sandboxes — split out of the (ratchet-capped)
//! `sandbox.rs`. Ensures the sandbox image is present on the runtime that will
//! actually run the container BEFORE `run -d`, so container-create is pure
//! namespace/cgroup setup (fast, `RUN_TIMEOUT`) rather than a network pull.
//!
//! Two runtime-shape subtleties this handles that a naive `podman image exists`
//! got wrong:
//!   * **docker has no `image exists` subcommand** — it must use `image
//!     inspect` (same 0/1 semantics). A podman-hardcoded probe makes docker
//!     always miss and fall through to a pull, which fails for a locally-loaded
//!     `localhost/…` image (the host-provisioning flow's delivered base image).
//!   * **a transport-wrapped remote runtime** (an ssh-placed `[host.*]` with no
//!     local `oci_host` daemon to drive) can't be probed from here at all — a
//!     local `status_with_timeout` would answer for the WRONG machine. The host
//!     flow already delivered + verified the image there, and `run` auto-pulls
//!     a registry ref, so we skip the local probe entirely.
//!
//! Pulls stream their runtime's per-layer output through
//! [`crate::pull_progress::PullParser`] and emit throttled
//! [`SandboxPhase::PullProgress`] events, so the loading screen can draw a
//! live byte bar for the one legitimately slow phase.

use std::time::{Duration, Instant};

use crate::progress::{self, SandboxPhase};
use crate::pull_progress::PullParser;
use crate::sandbox::{
    Backend, PROBE_TIMEOUT, PULL_TIMEOUT, SandboxSpec, effective_image, oci_prefix,
    status_with_timeout,
};

/// The `<runtime> image <sub> <ref>` existence-probe subcommand for `backend`:
/// podman has the `image exists` sugar (exit 0/1); docker has no such
/// subcommand, so `image inspect` (also exit 0/1) is the portable spelling.
fn image_exists_subcmd(backend: Backend) -> &'static str {
    match backend {
        Backend::Docker => "inspect",
        _ => "exists",
    }
}

/// Whether the local prefetch probe is meaningless and must be skipped: a
/// remote placement with no local daemon to drive (`oci_host` unset) runs its
/// runtime on another machine, so probing locally would query the wrong host.
fn skip_local_probe(is_local: bool, has_oci_host: bool) -> bool {
    !is_local && !has_oci_host
}

/// Ensure `spec`'s image is present on the target runtime (a no-op for non-OCI
/// backends and for transport-wrapped remotes the host flow already provisioned).
pub fn prefetch_image(spec: &SandboxSpec) -> anyhow::Result<()> {
    if !spec.backend.is_oci() {
        return Ok(());
    }
    if skip_local_probe(spec.placement.is_local(), spec.oci_host.is_some()) {
        return Ok(());
    }
    let img = effective_image(spec);
    let rt = spec.backend.binary();
    // Probe/pull through `oci_prefix` so we target the RIGHT daemon (rootful
    // podman's `sudo -n podman`, or an `oci_host` remote daemon), not a bare
    // binary — and spell the existence check per runtime (see helpers).
    let mut exists_argv = oci_prefix(spec);
    exists_argv.extend([
        "image".into(),
        image_exists_subcmd(spec.backend).into(),
        img.clone(),
    ]);
    progress::emit(SandboxPhase::ImageProbe { image: img.clone() });
    match status_with_timeout(&exists_argv, PROBE_TIMEOUT) {
        Some(true) => progress::emit(SandboxPhase::PhaseDone),
        Some(false) => {
            let mut pull_argv = oci_prefix(spec);
            pull_argv.extend(["pull".into(), img.clone()]);
            progress::emit(SandboxPhase::ImagePull { image: img.clone() });
            if !pull_streaming(&pull_argv, PULL_TIMEOUT) {
                let err = format!("{rt} pull {img} failed or timed out");
                progress::emit(SandboxPhase::PhaseFailed { err: err.clone() });
                anyhow::bail!(err);
            }
            progress::emit(SandboxPhase::PhaseDone);
        }
        // The probe itself wedged: the runtime is unhealthy (stuck machine,
        // broken storage) — fail the candidate so the backend chain falls
        // through instead of trusting a pull to behave.
        None => {
            let err = format!("{rt} not responding (image probe timed out)");
            progress::emit(SandboxPhase::PhaseFailed { err: err.clone() });
            anyhow::bail!(err);
        }
    }
    Ok(())
}

/// Run a pull for its exit status with a hard deadline, streaming stdout +
/// stderr lines through the pull parser and emitting throttled
/// [`SandboxPhase::PullProgress`] events as they arrive. `false` on spawn
/// failure, non-zero exit, or timeout (the child is killed and reaped) —
/// matching the `status_with_timeout` contract the caller had before.
/// Subprocess seam (cov_ignore); the parsing it drives is unit-tested in
/// [`crate::pull_progress`].
fn pull_streaming(argv: &[String], timeout: Duration) -> bool {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let Ok(mut child) = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    // Forward both pipes line-by-line over one channel: podman prints progress
    // on stderr, docker on stdout — feed the parser whichever speaks. Reader
    // threads exit on EOF; a send to a dropped receiver (timeout path) is a
    // best-effort no-op.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    let deadline = Instant::now() + timeout;
    let mut parser = PullParser::new();
    let drain = |parser: &mut PullParser| {
        while let Ok(line) = rx.try_recv() {
            if let Some(snap) = parser.feed_line(&line) {
                progress::emit(SandboxPhase::PullProgress(snap));
            }
        }
    };
    let ok = loop {
        drain(&mut parser);
        match child.try_wait() {
            Ok(Some(status)) => {
                // Child exited: the readers hit EOF right away — join them,
                // then drain the tail so a fast pull's lines aren't lost.
                for r in readers.drain(..) {
                    let _ = r.join();
                }
                drain(&mut parser);
                break status.success();
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break false,
        }
    };
    // Reap any remaining reader threads (timeout/error paths; EOF follows the
    // kill immediately).
    for r in readers {
        let _ = r.join();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_probes_with_inspect_podman_with_exists() {
        // The crux of docker support: `docker image exists` is not a command.
        assert_eq!(image_exists_subcmd(Backend::Docker), "inspect");
        assert_eq!(image_exists_subcmd(Backend::Podman), "exists");
        assert_eq!(image_exists_subcmd(Backend::PodmanRootful), "exists");
    }

    #[test]
    fn local_probe_skipped_only_for_transport_wrapped_remotes() {
        // Local (any oci_host) → probe here.
        assert!(!skip_local_probe(true, false));
        assert!(!skip_local_probe(true, true));
        // Remote driving a local `oci_host` daemon (`docker -H`) → probe here.
        assert!(!skip_local_probe(false, true));
        // Remote, transport-wrapped (ssh placement, no oci_host) → skip.
        assert!(skip_local_probe(false, false));
    }

    #[test]
    fn pull_streaming_reports_exit_and_streams_lines() {
        use crate::progress::{SandboxPhase, scoped};
        use std::sync::{Arc, Mutex};
        // A stand-in "runtime" that prints one parseable podman progress line
        // on stderr and exits 0 — exercises the spawn/stream/reap seam without
        // a container runtime.
        let seen: Arc<Mutex<Vec<SandboxPhase>>> = Arc::default();
        let sink_seen = seen.clone();
        let _g = scoped(Box::new(move |ev| sink_seen.lock().unwrap().push(ev)));
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'Copying blob abc123 [=>--] 1.0MiB / 2.0MiB' >&2".to_string(),
        ];
        assert!(pull_streaming(&argv, Duration::from_secs(10)));
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|e| matches!(e, SandboxPhase::PullProgress(_))),
            "progress event from a parseable line: {seen:?}"
        );
        // Non-zero exit → false.
        assert!(!pull_streaming(
            &["false".to_string()],
            Duration::from_secs(10)
        ));
        // Spawn failure → false.
        assert!(!pull_streaming(
            &["/nonexistent-runtime-zz".to_string()],
            Duration::from_secs(1)
        ));
    }
}
