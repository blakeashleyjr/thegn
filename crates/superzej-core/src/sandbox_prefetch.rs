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
    match status_with_timeout(&exists_argv, PROBE_TIMEOUT) {
        Some(true) => {}
        Some(false) => {
            let mut pull_argv = oci_prefix(spec);
            pull_argv.extend(["pull".into(), img.clone()]);
            if status_with_timeout(&pull_argv, PULL_TIMEOUT) != Some(true) {
                anyhow::bail!("{rt} pull {img} failed or timed out");
            }
        }
        // The probe itself wedged: the runtime is unhealthy (stuck machine,
        // broken storage) — fail the candidate so the backend chain falls
        // through instead of trusting a pull to behave.
        None => anyhow::bail!("{rt} not responding (image probe timed out)"),
    }
    Ok(())
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
}
