//! The "results not pulled" marker: one empty 0600 file per Vercel sandbox name
//! under `$XDG_STATE_HOME/thegn/vercel/unpulled/<name>`, written when a TTY
//! session ends and cleared by a successful `thegn env pull`.
//!
//! Names are validated with `admit::valid_sandbox_name` (a refused name is a
//! no-op) and every file operation goes through the fsperm beneath-dir helpers.

use std::path::{Path, PathBuf};

use thegn_core::fsperm;
use thegn_core::vercel::admit::valid_sandbox_name;

fn rel() -> PathBuf {
    PathBuf::from("thegn").join("vercel").join("unpulled")
}

fn mark_in(anchor: &Path, sandbox: &str) -> std::io::Result<()> {
    if !valid_sandbox_name(sandbox) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid sandbox name",
        ));
    }
    let dir = fsperm::ensure_private_dir_beneath(anchor, &rel())?;
    match dir.create_owner_only_exclusive(sandbox) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn existing_dir(anchor: &Path) -> Option<fsperm::PrivateDir> {
    let path = anchor.join(rel());
    if !path.is_dir() {
        return None;
    }
    fsperm::open_private_dir(&path).ok()
}

fn clear_in(anchor: &Path, sandbox: &str) {
    if !valid_sandbox_name(sandbox) {
        return;
    }
    if let Some(dir) = existing_dir(anchor)
        && let Err(error) = dir.remove_file(sandbox)
    {
        tracing::debug!(target: "thegn::vercel", %error, "pull marker clear failed");
    }
}

fn is_marked_in(anchor: &Path, sandbox: &str) -> bool {
    valid_sandbox_name(sandbox)
        && existing_dir(anchor)
            .and_then(|dir| dir.entry_exists_nofollow(sandbox).ok())
            .unwrap_or(false)
}

/// Record that `sandbox` may hold commits not yet pulled back.
pub(crate) fn mark(sandbox: &str) {
    if let Err(error) = mark_in(&thegn_core::util::xdg_state_home(), sandbox) {
        tracing::debug!(target: "thegn::vercel", %error, "pull marker write failed");
    }
}

/// Drop `sandbox`'s marker.
pub(crate) fn clear(sandbox: &str) {
    clear_in(&thegn_core::util::xdg_state_home(), sandbox);
}

/// Whether `sandbox` carries the marker.
pub(crate) fn is_marked(sandbox: &str) -> bool {
    is_marked_in(&thegn_core::util::xdg_state_home(), sandbox)
}

/// How many markers exist (doctor).
pub(crate) fn count() -> usize {
    existing_dir(&thegn_core::util::xdg_state_home())
        .and_then(|dir| dir.file_names().ok())
        .map(|names| names.len())
        .unwrap_or(0)
}

/// The sandbox bound to `worktree`'s env, when that env is a provider env.
fn sandbox_for(cfg: &thegn_core::config::Config, worktree: &str, env: &str) -> Option<String> {
    crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env)
        .ok()
        .flatten()
}

/// Whether the sandbox of `worktree` under env `env` is marked.
pub(crate) fn is_marked_for_worktree(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    env: &str,
) -> bool {
    sandbox_for(cfg, worktree, env).is_some_and(|id| is_marked(&id))
}

/// Clear the marker of the sandbox bound to `worktree` (after a pull).
pub(crate) fn clear_for_worktree(cfg: &thegn_core::config::Config, worktree: &str) {
    let Ok(env) = crate::cmd::env::resolve_for(cfg, Some(worktree.to_string())) else {
        return;
    };
    if let Some(id) = sandbox_for(cfg, worktree, &env.name) {
        clear(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_set_query_clear_roundtrip() {
        let _state = crate::testenv::IsolatedState::new();
        assert!(!is_marked("tg-abc"));
        mark("tg-abc");
        mark("tg-abc"); // idempotent
        assert!(is_marked("tg-abc"));
        assert!(!is_marked("tg-other"));
        assert_eq!(count(), 1);
        clear("tg-abc");
        assert!(!is_marked("tg-abc"));
        assert_eq!(count(), 0);
    }

    #[test]
    fn invalid_names_are_refused() {
        let _state = crate::testenv::IsolatedState::new();
        for bad in ["", "../x", "A", "a/b", "-x", &"a".repeat(64)] {
            mark(bad);
            assert!(!is_marked(bad), "{bad:?}");
        }
        assert_eq!(count(), 0);
        assert!(mark_in(&thegn_core::util::xdg_state_home(), "../x").is_err());
    }
}
