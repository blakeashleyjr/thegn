//! Suite F — Health degradation (Tier 2, podman required).
//!
//! Tests health_check() returns true for running containers and false for
//! stopped or mount-broken ones.

use std::path::PathBuf;

use thegn_core::config::{FileAccess, Network};
use thegn_core::sandbox::{Backend, Mount, SandboxSpec, ensure, health_check};

#[path = "support/bounded_run.rs"]
mod bounded_run;

fn skip() -> bool {
    bounded_run::sandbox_backend_forced_none()
        || !thegn_core::util::have("podman")
        || std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
}

fn force_rm(name: &str) {
    // best-effort: test scratch: best-effort by design
    let _ = bounded_run::output(std::process::Command::new("podman").args(["rm", "-f", name]));
}

fn base_spec(name: &str) -> SandboxSpec {
    SandboxSpec {
        backend: Backend::Podman,
        image: Some("docker.io/library/alpine:latest".into()),
        worktree: PathBuf::from("/tmp/tg-e2e-health"),
        network: Network::None,
        file_access: FileAccess::None,
        name: name.into(),
        ..SandboxSpec::default()
    }
}

// ── F1: running container → health_check true ───────────────────────────────

#[test]
fn f1_running_container_is_healthy() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-f1";
    force_rm(name);
    let spec = base_spec(name);
    ensure(&spec).expect("ensure failed");
    let healthy = health_check(&spec);
    force_rm(name);
    assert!(healthy, "running container must be healthy");
}

// ── F2: stopped container → health_check false ──────────────────────────────

#[test]
fn f2_stopped_container_is_unhealthy() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-f2";
    force_rm(name);
    let spec = base_spec(name);
    ensure(&spec).expect("ensure failed");
    // Stop the container.
    // best-effort: test scratch: best-effort by design
    let _ = bounded_run::output(std::process::Command::new("podman").args(["stop", name]));
    let healthy = health_check(&spec);
    force_rm(name);
    assert!(!healthy, "stopped container must not be healthy");
}

// ── F3: stale mount → health_check false ────────────────────────────────────

#[test]
fn f3_stale_mount_is_unhealthy() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-f3";
    force_rm(name);
    let mut spec = base_spec(name);
    // Add a mount to a host path that does not exist.
    spec.mounts.push(Mount {
        host: "/nonexistent/tg-e2e-path-f3".into(),
        dest: "/nonexistent/tg-e2e-path-f3".into(),
        ro: false,
        cache: false,
    });
    // ensure() itself might fail because of the missing mount path; that is also
    // acceptable — a missing mount is a health failure by definition.
    if ensure(&spec).is_ok() {
        let healthy = health_check(&spec);
        force_rm(name);
        assert!(
            !healthy,
            "container with missing mount host-path must not be healthy"
        );
    } else {
        force_rm(name);
        // ensure() rejected the spec — mount validation works, test passes.
    }
}

// ── F4: non-OCI backend → health_check always true ──────────────────────────

#[test]
fn f4_non_oci_backend_always_healthy() {
    // No podman needed — this is a unit-level check on the function's fast path.
    let mut spec = base_spec("thegn-e2e-f4-noop");
    spec.backend = Backend::Bwrap;
    spec.image = None;
    assert!(
        health_check(&spec),
        "non-OCI backends must always report healthy"
    );

    spec.backend = Backend::None;
    assert!(
        health_check(&spec),
        "Backend::None must always report healthy"
    );
}
