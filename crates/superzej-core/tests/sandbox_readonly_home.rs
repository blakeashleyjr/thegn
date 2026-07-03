//! Read-only-outside-the-worktree containment.
//!
//! Regression guard for the escape where a bwrap-sandboxed agent could `cd` out
//! of its worktree into `$HOME` and modify/delete arbitrary files. The default
//! `hardened` profile now binds `$HOME` read-only (worktree/caches/carve-outs
//! stay writable), `profile = "open"` restores a writable `$HOME`, and the
//! systemd backend closes the same gap via `ProtectHome=read-only`.

use std::collections::HashMap;
use std::path::PathBuf;

use superzej_core::config::{FileAccess, Network, SandboxBackend, SandboxConfig, SandboxProfile};
use superzej_core::placement::Placement;
use superzej_core::remote::GitLoc;
use superzej_core::sandbox::{Backend, Mount, SandboxLimits, SandboxSpec, enter_argv, resolve};

fn mk_spec(
    backend: Backend,
    file_access: FileAccess,
    mounts: Vec<Mount>,
    ro_root: bool,
) -> SandboxSpec {
    SandboxSpec {
        backend,
        placement: Placement::Local,
        image: None,
        worktree: PathBuf::from("/home/tester/wt"),
        mounts,
        env: vec![],
        env_overrides: HashMap::new(),
        env_block: vec![],
        network: Network::Nat,
        network_allow: vec![],
        network_block: vec![],
        read_only_root: ro_root,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: vec![],
        add_capabilities: vec![],
        file_access,
        ports: vec![],
        gpu: None,
        limits: SandboxLimits::default(),
        volumes: vec![],
        compose: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: "superzej-test".into(),
        vpn: None,
        oci_host: None,
    }
}

fn rw(host: &str) -> Mount {
    Mount {
        host: host.into(),
        dest: host.into(),
        ro: false,
        cache: false,
    }
}
fn ro(host: &str) -> Mount {
    Mount {
        host: host.into(),
        dest: host.into(),
        ro: true,
        cache: false,
    }
}

// ── enter_argv emission (deterministic; no backend binary needed) ────────────

#[test]
fn bwrap_ro_home_emits_ro_bind_worktree_stays_rw() {
    // The read-only $HOME parent must be emitted `--ro-bind` while the worktree
    // (its child) is emitted `--bind` so it overmounts read-write.
    let spec = mk_spec(
        Backend::Bwrap,
        FileAccess::WorktreePlusCaches,
        vec![ro("/home/tester"), rw("/home/tester/wt")],
        true,
    );
    let joined = enter_argv(&spec, "exec claude").join(" ");
    assert!(
        joined.contains("--ro-bind /home/tester /home/tester"),
        "expected read-only $HOME bind, got: {joined}"
    );
    assert!(
        !joined.contains("--bind /home/tester /home/tester"),
        "must NOT bind $HOME read-write under a read-only profile: {joined}"
    );
    assert!(
        joined.contains("--bind /home/tester/wt /home/tester/wt"),
        "worktree must stay writable: {joined}"
    );
}

#[test]
fn bwrap_open_home_emits_rw_bind() {
    // `profile = "open"` surfaces as a read-write $HOME mount → `--bind`.
    let spec = mk_spec(
        Backend::Bwrap,
        FileAccess::WorktreePlusCaches,
        vec![rw("/home/tester"), rw("/home/tester/wt")],
        false,
    );
    let joined = enter_argv(&spec, "exec claude").join(" ");
    assert!(
        joined.contains("--bind /home/tester /home/tester"),
        "open profile must bind $HOME read-write: {joined}"
    );
}

#[test]
fn systemd_readonly_home_keeps_worktree_and_caches_writable() {
    let spec = mk_spec(
        Backend::Systemd,
        FileAccess::WorktreePlusCaches,
        vec![
            rw("/home/tester/wt"),
            Mount {
                host: "/home/tester/.cargo/registry".into(),
                dest: "/home/tester/.cargo/registry".into(),
                ro: false,
                cache: true,
            },
            ro("/home/tester/repo/.git/config"),
        ],
        true,
    );
    let joined = enter_argv(&spec, "exec claude").join(" ");
    assert!(
        joined.contains("ProtectHome=read-only"),
        "must lock $HOME read-only: {joined}"
    );
    assert!(
        joined.contains("ReadWritePaths=/home/tester/wt"),
        "worktree must be writable: {joined}"
    );
    assert!(
        joined.contains("ReadWritePaths=/home/tester/.cargo/registry"),
        "build caches must stay writable: {joined}"
    );
}

// ── resolve() wiring (tolerant of a missing bwrap binary in CI) ──────────────

fn resolve_bwrap(profile: SandboxProfile) -> Option<SandboxSpec> {
    let cfg = SandboxConfig {
        backend: SandboxBackend::Bwrap,
        file_access: FileAccess::WorktreePlusCaches,
        auto_caches: true,
        profile,
        ..Default::default()
    };
    let loc = GitLoc::from_db("/home/tester/wt", None);
    // Tolerant: on a host without bwrap the chain falls back to another backend
    // (or None); only assert when bwrap was actually selected.
    resolve(&cfg, &loc, "test").filter(|s| s.backend == Backend::Bwrap)
}

#[test]
fn resolve_wires_home_ro_from_profile() {
    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && std::path::Path::new(&h).exists() => h,
        _ => return,
    };
    let home_ro = |spec: &SandboxSpec| spec.mounts.iter().find(|m| m.host == home).map(|m| m.ro);

    if let Some(spec) = resolve_bwrap(SandboxProfile::Hardened) {
        assert_eq!(
            home_ro(&spec),
            Some(true),
            "hardened profile must bind $HOME read-only"
        );
        // $HOME parent is emitted before the worktree child (overmount ordering).
        let hi = spec.mounts.iter().position(|m| m.host == home);
        let wi = spec.mounts.iter().position(|m| m.host == "/home/tester/wt");
        if let (Some(hi), Some(wi)) = (hi, wi) {
            assert!(hi < wi, "$HOME must be mounted before the worktree");
        }
    }
    if let Some(spec) = resolve_bwrap(SandboxProfile::Open) {
        assert_eq!(
            home_ro(&spec),
            Some(false),
            "open profile must bind $HOME read-write"
        );
    }
}
