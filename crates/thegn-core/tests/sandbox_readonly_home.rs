//! Read-only-outside-the-worktree containment.
//!
//! Regression guard for the escape where a bwrap-sandboxed agent could `cd` out
//! of its worktree into `$HOME` and modify/delete arbitrary files. The default
//! `hardened` profile now binds `$HOME` read-only (worktree/caches/carve-outs
//! stay writable), `profile = "open"` restores a writable `$HOME`, and the
//! systemd backend closes the same gap via `ProtectHome=read-only`.

use std::path::PathBuf;

use thegn_core::config::{FileAccess, SandboxBackend, SandboxConfig, SandboxProfile};
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{Backend, Mount, SandboxSpec, enter_argv, resolve};

fn mk_spec(
    backend: Backend,
    file_access: FileAccess,
    mounts: Vec<Mount>,
    ro_root: bool,
) -> SandboxSpec {
    SandboxSpec {
        backend,
        worktree: PathBuf::from("/home/tester/wt"),
        mounts,
        read_only_root: ro_root,
        file_access,
        name: "thegn-test".into(),
        ..SandboxSpec::default()
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

/// A real, canonical synthetic `$HOME`. Local mount sources are canonicalised
/// before emission (`/tmp` is `/private/tmp` and `/home` is an autofs
/// link on macOS), so a hard-coded `/home/tester` would not round-trip there.
fn synthetic_home() -> (tempfile::TempDir, String) {
    let td = tempfile::tempdir().unwrap();
    let home = td
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    (td, home)
}

#[test]
fn bwrap_ro_home_emits_ro_bind_worktree_stays_rw() {
    let (_td, home) = synthetic_home();
    // The read-only $HOME parent must be emitted `--ro-bind` while the worktree
    // (its child) is emitted `--bind` so it overmounts read-write.
    let spec = mk_spec(
        Backend::Bwrap,
        FileAccess::WorktreePlusCaches,
        vec![ro(&home), rw(&format!("{home}/wt"))],
        true,
    );
    let joined = enter_argv(&spec, "exec claude")
        .expect("valid volume names")
        .join(" ");
    assert!(
        joined.contains(&format!("--ro-bind {home} {home}")),
        "expected read-only $HOME bind, got: {joined}"
    );
    assert!(
        !joined.contains(&format!("--bind {home} {home}")),
        "must NOT bind $HOME read-write under a read-only profile: {joined}"
    );
    assert!(
        joined.contains(&format!("--bind {home}/wt {home}/wt")),
        "worktree must stay writable: {joined}"
    );
}

#[test]
fn bwrap_open_home_emits_rw_bind() {
    let (_td, home) = synthetic_home();
    // `profile = "open"` surfaces as a read-write $HOME mount → `--bind`.
    let spec = mk_spec(
        Backend::Bwrap,
        FileAccess::WorktreePlusCaches,
        vec![rw(&home), rw(&format!("{home}/wt"))],
        false,
    );
    let joined = enter_argv(&spec, "exec claude")
        .expect("valid volume names")
        .join(" ");
    assert!(
        joined.contains(&format!("--bind {home} {home}")),
        "open profile must bind $HOME read-write: {joined}"
    );
}

#[test]
fn systemd_readonly_home_keeps_worktree_and_caches_writable() {
    let (_td, home) = synthetic_home();
    let spec = mk_spec(
        Backend::Systemd,
        FileAccess::WorktreePlusCaches,
        vec![
            rw(&format!("{home}/wt")),
            Mount {
                host: format!("{home}/.cargo/registry").into(),
                dest: format!("{home}/.cargo/registry").into(),
                ro: false,
                cache: true,
            },
            ro(&format!("{home}/repo/.git/config")),
        ],
        true,
    );
    let joined = enter_argv(&spec, "exec claude")
        .expect("valid volume names")
        .join(" ");
    assert!(
        joined.contains("ProtectHome=read-only"),
        "must lock $HOME read-only: {joined}"
    );
    assert!(
        joined.contains(&format!("ReadWritePaths={home}/wt")),
        "worktree must be writable: {joined}"
    );
    assert!(
        joined.contains(&format!("ReadWritePaths={home}/.cargo/registry")),
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
