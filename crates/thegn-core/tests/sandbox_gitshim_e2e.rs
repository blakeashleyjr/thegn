//! Suite: git-metadata shim end-to-end (Tier 2, podman required).
//!
//! Runs the REAL pipeline on a REAL linked worktree — `resolve` a spec,
//! `ensure` a live container, then exec git inside it — because the bug this
//! guards against was shipped once already by a change that was only ever
//! reasoned about, never executed. `thegn doctor` on that box said
//! `podman-rootless  not installed`.
//!
//! Every thegn tab is a *linked* worktree, whose `.git` is a pointer file
//! carrying an absolute host path. Without the shim a sandboxed pane gives
//! `fatal: not a git repository: (null)`, and an in-container
//! `git worktree prune` deletes the metadata of every sibling tab.
//!
//! Skips unless `PODMAN_E2E_FORCE` is set (same gate as `sandbox_lifecycle.rs`),
//! so it never runs in the machine-independent CI.

use std::path::{Path, PathBuf};

use thegn_core::config::{SandboxBackend, SandboxConfig, SandboxProfile};
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{self, container_name};

/// An image that has git. It also declares `ENTRYPOINT ["git"]`, which is
/// deliberate: thegn clears the image entrypoint when it creates the keep-alive
/// container, and this suite is what proves it. Before that fix the container
/// ran `git sleep infinity`, exited at once, and `ensure` failed with nothing
/// pointing at the cause.
const IMAGE: &str = "docker.io/alpine/git:latest";

/// Pull [`IMAGE`] once. A no-op after the first run.
fn ensure_image() {
    if podman(&["image", "exists", IMAGE]).0 {
        return;
    }
    let (ok, out) = podman(&["pull", "-q", IMAGE]);
    assert!(ok, "could not pull the test image: {out}");
}

fn skip() -> bool {
    !thegn_core::util::have("podman")
        || std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
}

fn podman(args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("podman").args(args).output();
    match out {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
            .trim()
            .to_string(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// Exec a shell snippet in the running container, returning trimmed output.
fn exec_in(name: &str, script: &str) -> String {
    podman(&["exec", name, "sh", "-lc", script]).1
}

fn force_rm(name: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", name])
        .output();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with TWO linked worktrees, so sibling protection is observable.
/// Pointers are forced ABSOLUTE — the layout every pre-2.48 git produces, and
/// the one the shim exists for. (thegn's own `worktree add` now passes
/// `worktree.useRelativePaths=true`, which sidesteps the problem at creation;
/// this fixture reproduces the worktrees a user already has.)
fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    // Per-TEST, not per-process: these run in parallel in one binary, and a
    // shared path means a shared container name — each test would `force_rm`
    // the other's container mid-run.
    let base = std::env::temp_dir().join(format!("sz-gitshim-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "."]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    // NOTE: `core.autocrlf` is deliberately left at the host default. On Windows
    // that is `true` (Git for Windows sets it system-wide), so the worktree is
    // checked out with CRLF while the blobs are LF — and a Linux container's git
    // would call every file modified. The clean-status assertion below therefore
    // depends on `SandboxSpec::git_autocrlf` being resolved and applied.
    std::fs::write(repo.join("a.txt"), "hi\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "init"]);
    for (rel, branch) in [("../wt", "feat"), ("../wt2", "feat2")] {
        git(
            &repo,
            &[
                "-c",
                "worktree.useRelativePaths=false",
                "worktree",
                "add",
                "-q",
                rel,
                "-b",
                branch,
            ],
        );
    }
    (base.clone(), base.join("wt"), base.join("wt2"))
}

/// Replicate the one line of pane setup a raw `podman exec` skips.
///
/// `sandbox::wrap_script` emits `git config --global --add safe.directory '*'`
/// at the top of every sandboxed pane script, because a bind-mounted worktree
/// has a different owner uid inside the container and git otherwise refuses it
/// as "dubious ownership". Without this the assertions below would report the
/// ownership error instead of the thing under test — and, worse, the prune test
/// would pass vacuously because git never runs at all.
fn pane_preamble(name: &str, spec: &sandbox::SandboxSpec) {
    let out = exec_in(name, "git config --global --add safe.directory '*'");
    assert!(out.is_empty(), "pane preamble failed: {out}");
    // The other half of the preamble: the host's line-ending policy. Without it
    // a Windows-checked-out worktree reads as entirely modified inside the
    // container. `resolve` is what decides this, so applying `spec` here (rather
    // than a hardcoded value) tests that decision too.
    if let Some(v) = &spec.git_autocrlf {
        let out = exec_in(name, &format!("git config --global core.autocrlf {v}"));
        assert!(out.is_empty(), "autocrlf preamble failed: {out}");
    }
}

fn spec_for(worktree: &Path, name: &str) -> sandbox::SandboxSpec {
    let sb = SandboxConfig {
        enabled: true,
        backend: SandboxBackend::Podman,
        image: IMAGE.to_string(),
        // Open profile: no cap drops / read-only-root surprises for the asserts.
        profile: SandboxProfile::Open,
        ..SandboxConfig::default()
    };
    let loc = GitLoc::for_worktree(worktree);
    sandbox::resolve(&sb, &loc, name).expect("resolve produced no spec")
}

/// The whole point: git must work inside the sandbox, and the host must agree.
#[test]
fn git_resolves_inside_the_sandbox_and_the_host_agrees() {
    if skip() {
        return;
    }
    ensure_image();
    let (base, wt, _wt2) = fixture("resolve");
    let name = container_name(&wt.to_string_lossy());
    force_rm(&name);
    let spec = spec_for(&wt, &name);
    sandbox::ensure(&spec).expect("ensure failed");
    pane_preamble(&name, &spec);

    let wt_dest = sandbox::container_path(&wt.to_string_lossy());
    let g = |args: &str| exec_in(&name, &format!("cd {wt_dest} && git {args} 2>&1"));

    assert_eq!(
        g("rev-parse --abbrev-ref HEAD"),
        "feat",
        "git must resolve the linked worktree inside the sandbox"
    );
    assert!(
        g("status --short").is_empty(),
        "clean worktree, got: {}",
        g("status --short")
    );
    // `git worktree list` must report the MAPPED paths, not the host's.
    assert!(
        g("worktree list").contains(&wt_dest),
        "worktree list should name the mapped destination; got {}",
        g("worktree list")
    );

    // A commit made inside the sandbox must land in the host's repository —
    // that is the "same repository" invariant, not merely "git ran".
    let out = g("-c user.email=c@c -c user.name=c commit -qam from-sandbox --allow-empty");
    assert!(
        out.is_empty() || !out.contains("fatal"),
        "commit inside the sandbox failed: {out}"
    );
    assert_eq!(
        git(&wt, &["log", "--oneline", "-1", "--format=%s"]),
        "from-sandbox",
        "the host must see the commit made inside the sandbox"
    );

    // The host's own pointer file must be untouched — the shim binds over the
    // container's view only.
    let pointer = std::fs::read_to_string(wt.join(".git")).unwrap();
    assert!(
        pointer.contains("worktrees/wt"),
        "host pointer rewritten: {pointer}"
    );

    force_rm(&name);
    let _ = std::fs::remove_dir_all(&base);
}

/// A sandboxed `git worktree prune` must not be able to delete the metadata of
/// any other tab. Measured behaviour without the shim: it reports every sibling
/// as prunable ("gitdir file points to non-existent location") and removes it.
#[test]
fn in_sandbox_prune_cannot_reach_sibling_tabs() {
    if skip() {
        return;
    }
    ensure_image();
    let (base, wt, wt2) = fixture("prune");
    let name = container_name(&wt.to_string_lossy());
    force_rm(&name);
    let spec = spec_for(&wt, &name);
    sandbox::ensure(&spec).expect("ensure failed");
    pane_preamble(&name, &spec);

    // Drive prune through the mounted git-common dir, NOT the worktree. That is
    // the realistic reach — `<git-common>` is bind-mounted whole, so it holds
    // every sibling tab's metadata — and, critically, it is a real git directory
    // that resolves with or without the shim. Running prune from the worktree
    // instead makes this test vacuous: with the shim removed, git there fails
    // with `not a git repository` and prune never executes, so the assertions
    // below would pass while proving nothing.
    let gc_dest = sandbox::container_path(&base.join("repo").join(".git").to_string_lossy());
    let out = exec_in(
        &name,
        &format!("git --git-dir={gc_dest} worktree prune -v 2>&1; true"),
    );

    // The sibling's metadata — and the sibling itself — must survive.
    assert!(
        wt2.join(".git").exists(),
        "sibling worktree pointer deleted by an in-sandbox prune; prune said: {out}"
    );
    assert_eq!(
        git(&wt2, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat2",
        "sibling worktree no longer resolves after an in-sandbox prune; prune said: {out}"
    );

    force_rm(&name);
    let _ = std::fs::remove_dir_all(&base);
}
