//! Suite D — Container lifecycle (Tier 2, podman required).
//!
//! Tests the full ensure → health_check → teardown cycle. All tests skip when
//! podman is unavailable or PODMAN_E2E_FORCE is not set.

use std::path::PathBuf;

use superzej_core::config::{FileAccess, Network};
use superzej_core::sandbox::{
    Backend, SandboxLimits, SandboxSpec, Transport, container_name, ensure, health_check, run_gc,
    teardown_by_path,
};

fn container_running(name: &str) -> bool {
    std::process::Command::new("podman")
        .args(["inspect", "--format", "{{.State.Status}}", name])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "running")
        .unwrap_or(false)
}

fn skip() -> bool {
    !superzej_core::util::have("podman")
        || std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
}

fn force_rm(name: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", name])
        .output();
}

fn alpine_spec(name: &str, worktree: &str) -> SandboxSpec {
    SandboxSpec {
        backend: Backend::Podman,
        transport: Transport::Local,
        image: Some("docker.io/library/alpine:latest".into()),
        worktree: PathBuf::from(worktree),
        mounts: vec![],
        env: vec![],
        env_overrides: std::collections::HashMap::new(),
        env_block: vec![],
        network: Network::Nat,
        network_allow: vec![],
        network_block: vec![],
        read_only_root: false,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: vec![],
        add_capabilities: vec![],
        file_access: FileAccess::None,
        ports: vec![],
        gpu: None,
        limits: SandboxLimits::default(),
        volumes: vec![],
        compose: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: name.into(),
        vpn: None,
    }
}

// ── D1: full lifecycle ───────────────────────────────────────────────────────

#[test]
fn d1_full_lifecycle_ensure_health_teardown() {
    if skip() {
        return;
    }
    let name = "superzej-e2e-d1";
    force_rm(name);
    let spec = alpine_spec(name, "/tmp/sz-e2e-d1");
    ensure(&spec).expect("ensure failed");
    assert!(
        container_running(name),
        "container should be running after ensure"
    );
    assert!(
        health_check(&spec),
        "health_check should return true for running container"
    );
    teardown_by_path("/tmp/sz-e2e-d1");
    assert!(
        !container_running(name),
        "container should be gone after teardown"
    );
}

// ── D2: ensure is idempotent ─────────────────────────────────────────────────

#[test]
fn d2_ensure_idempotent() {
    if skip() {
        return;
    }
    let name = "superzej-e2e-d2";
    force_rm(name);
    let spec = alpine_spec(name, "/tmp/sz-e2e-d2");
    ensure(&spec).expect("first ensure failed");
    ensure(&spec).expect("second ensure must not error (idempotent)");
    assert!(container_running(name));
    force_rm(name);
}

// ── D3: concurrent ensure ────────────────────────────────────────────────────

#[test]
fn d3_concurrent_ensure() {
    if skip() {
        return;
    }
    let name = "superzej-e2e-d3";
    force_rm(name);
    let spec = alpine_spec(name, "/tmp/sz-e2e-d3");
    let results: Vec<bool> = std::thread::scope(|s| {
        let spec = &spec;
        (0..3)
            .map(|_| s.spawn(move || ensure(spec).is_ok()))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .collect()
    });
    assert!(
        results.iter().all(|&ok| ok),
        "all concurrent ensures must succeed"
    );
    assert!(container_running(name));
    force_rm(name);
}

// ── D4: teardown removes the correct container ───────────────────────────────

#[test]
fn d4_teardown_removes_correct_container() {
    if skip() {
        return;
    }
    let name_a = container_name("/tmp/sz-e2e-d4a");
    let name_b = container_name("/tmp/sz-e2e-d4b");
    force_rm(&name_a);
    force_rm(&name_b);
    let spec_a = alpine_spec(&name_a, "/tmp/sz-e2e-d4a");
    let spec_b = alpine_spec(&name_b, "/tmp/sz-e2e-d4b");
    ensure(&spec_a).expect("ensure a failed");
    ensure(&spec_b).expect("ensure b failed");
    assert!(container_running(&name_a));
    assert!(container_running(&name_b));
    teardown_by_path("/tmp/sz-e2e-d4a");
    assert!(!container_running(&name_a), "container a should be gone");
    assert!(
        container_running(&name_b),
        "container b should still be running"
    );
    force_rm(&name_b);
}

// ── D5: startup orphan GC integration ────────────────────────────────────────

#[test]
fn d5_startup_orphan_gc() {
    if skip() {
        return;
    }
    let orphan = "superzej-e2e-orphan-d5";
    force_rm(orphan);
    // Create a container that has no DB entry.
    let spec = alpine_spec(orphan, "/tmp/sz-e2e-orphan");
    ensure(&spec).expect("ensure orphan failed");
    assert!(container_running(orphan));

    // GC considers containers not matching any live worktree as orphans.
    // Pass an active worktree list that doesn't include this container's worktree.
    let removed = run_gc(&["/tmp/sz-e2e-real-worktree".into()]);
    // The orphan should be in the removed list and no longer running.
    assert!(
        removed.iter().any(|n| n.contains("orphan")),
        "run_gc should remove orphan; got {removed:?}"
    );
    assert!(
        !container_running(orphan),
        "orphan container should be gone after gc"
    );
}
