//! Warn when a session prompt names files in a sandbox-private `/tmp`.

use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorkspaceStore;

/// Print a non-blocking warning for prompt paths that will disappear with the
/// worker's private `/tmp`. All resolution is local and best-effort: a failure
/// to determine the effective sandbox leaves the launch unchanged and quiet.
#[expect(
    clippy::disallowed_macros,
    reason = "user-visible launch warning on stderr"
)]
pub(crate) fn warn_if_private_tmp(cfg: &Config, worktree: &str, prompt: &str) {
    let paths = thegn_core::sandbox_tmp_hint::tmp_paths_in(prompt);
    if paths.is_empty() {
        return;
    }
    let Some((env_name, spec)) = resolve_spec(cfg, worktree) else {
        return;
    };
    let paths = thegn_core::sandbox_tmp_hint::private_tmp_paths(&spec, &paths);
    if paths.is_empty() {
        return;
    }
    for line in thegn_core::sandbox_tmp_hint::warning_lines(&env_name, &paths) {
        eprintln!("{line}");
        tracing::warn!(target: "thegn::session", "{line}");
    }
}

fn resolve_spec(
    cfg: &Config,
    worktree: &str,
) -> Option<(String, thegn_core::sandbox::SandboxSpec)> {
    let path = Path::new(worktree);
    let loc = GitLoc::for_worktree_op(path, "session tmp warning").ok()?;
    // Resolving a remote GitLoc would run git through its transport. The
    // warning is deliberately a cheap offline hint, so leave those launches
    // silent rather than probing a remote environment from the CLI.
    if !matches!(&loc, GitLoc::Local(_)) {
        return None;
    }

    let db = Db::open().ok()?;
    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::merge_ops::repo_root_of(path))?;
    let selection = db
        .effective_env_selection(worktree, &repo_root.to_string_lossy())
        .ok()?;
    let environment = cfg.resolve_env(&repo_root, &loc, path, &selection);
    if environment.unresolved_selection || environment.is_remote() {
        return None;
    }
    let env_name = environment.name.clone();
    let name = thegn_core::sandbox::container_name_for_config_profile(worktree, &cfg.profile);
    let spec = thegn_core::sandbox::resolve(&environment.sandbox, &loc, &name)?;
    Some((env_name, spec))
}
