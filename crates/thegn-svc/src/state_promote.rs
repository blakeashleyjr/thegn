//! Move machine state out of `~/.thegn` into `$XDG_STATE_HOME/thegn` (THE-2057).
//!
//! `~/.thegn` is user config and can be shadowed by a profile HOME; machine
//! ownership identity (the controller id) and provisioning attestations are
//! state and belong beside the DB. [`promote`] performs the one-time move.
//!
//! Follow-up: `vps::registry::promote_legacy` (THE-2050) is the directory-only
//! twin of [`promote`]; unify them once both have landed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// thegn's state root: `$XDG_STATE_HOME/thegn`.
pub fn state_root() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn")
}

/// Kinds (`what`) already warned about in this process.
static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn warn_once(what: &str, legacy: &Path, new: &Path, reason: &str, error: Option<&std::io::Error>) {
    let first = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(what.to_string());
    if !first {
        return;
    }
    match error {
        Some(error) => tracing::warn!(
            target: "thegn::state",
            what,
            %error,
            legacy = %legacy.display(),
            new = %new.display(),
            "{reason}"
        ),
        None => tracing::warn!(
            target: "thegn::state",
            what,
            legacy = %legacy.display(),
            new = %new.display(),
            "{reason}"
        ),
    }
}

/// Choose between a legacy path and its state-root replacement, moving the
/// legacy regular file or directory to `new` by one atomic rename when only the
/// legacy one exists.
///
/// - `new` already exists (it is never followed): `new`, legacy untouched.
/// - `legacy` is a real (non-symlink) file or directory owned by the current
///   user: `new`'s parent is created owner-only if missing, then `legacy` is
///   renamed to `new`. Success or a lost race (`NotFound`) returns `new`; any
///   other failure (EXDEV, a blocked parent, …) warns once per `what` and
///   returns `legacy`, which keeps serving.
/// - otherwise (absent, symlinked or foreign legacy): `new`. A symlinked or
///   foreign legacy is never followed or moved; it warns once per `what`.
///
/// Not cached: the check is a few stats and the environment differs per test.
pub fn promote(legacy: &Path, new: &Path, what: &str) -> PathBuf {
    #[cfg(test)]
    {
        // A test that forgot `IsolatedState` must never move the developer's
        // real state: resolve the path but skip the promotion.
        let ambient = thegn_core::testenv::ambient_state_roots();
        if thegn_core::testenv::is_real_state_path(new, &thegn_core::util::home(), ambient) {
            return new.to_path_buf();
        }
    }
    if std::fs::symlink_metadata(new).is_ok() {
        return new.to_path_buf();
    }
    let Ok(metadata) = std::fs::symlink_metadata(legacy) else {
        return new.to_path_buf();
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        warn_once(
            what,
            legacy,
            new,
            "legacy state path is a symlink; not following or moving it",
            None,
        );
        return new.to_path_buf();
    }
    if !(file_type.is_file() || file_type.is_dir())
        || !thegn_core::fsperm::owned_by_current_user(&metadata)
    {
        warn_once(
            what,
            legacy,
            new,
            "legacy state path is not a regular file or directory owned by this user; not moving it",
            None,
        );
        return new.to_path_buf();
    }
    let result = match new.parent() {
        Some(parent) if std::fs::symlink_metadata(parent).is_err() => {
            thegn_core::fsperm::ensure_private_dir(parent)
        }
        _ => Ok(()),
    }
    // A move, not a write: the bytes and owner-only modes travel unchanged.
    .and_then(|()| std::fs::rename(legacy, new));
    match result {
        Ok(()) => new.to_path_buf(),
        // Another process promoted it first.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => new.to_path_buf(),
        Err(error) => {
            warn_once(
                what,
                legacy,
                new,
                "cannot move legacy state into the state directory; still using the legacy location",
                Some(&error),
            );
            legacy.to_path_buf()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{promote, state_root};

    #[test]
    fn state_root_is_under_xdg_state_home() {
        let state = thegn_core::testenv::IsolatedState::new();
        assert_eq!(state_root(), state.state_home().join("thegn"));
    }

    #[test]
    fn promote_moves_a_legacy_file_with_the_same_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy/controller-id");
        std::fs::create_dir(tmp.path().join("legacy")).unwrap();
        std::fs::write(&legacy, b"0123456789abcdef0123456789abcdef\n").unwrap();
        let new = tmp.path().join("state/thegn/controller-id");

        assert_eq!(promote(&legacy, &new, "test file"), new);
        assert_eq!(
            std::fs::read(&new).unwrap(),
            b"0123456789abcdef0123456789abcdef\n"
        );
        assert!(std::fs::symlink_metadata(&legacy).is_err(), "legacy gone");
        let parent_mode = thegn_core::fsperm::mode_bits(new.parent().unwrap()).unwrap();
        assert!(parent_mode.is_none_or(|bits| bits == 0o700));
    }

    #[test]
    fn promote_prefers_an_existing_new_and_leaves_legacy_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy-id");
        let new = tmp.path().join("new-id");
        std::fs::write(&legacy, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();

        assert_eq!(promote(&legacy, &new, "test both"), new);
        assert_eq!(std::fs::read(&legacy).unwrap(), b"old");
        assert_eq!(std::fs::read(&new).unwrap(), b"new");
    }

    #[test]
    fn promote_never_follows_or_moves_a_symlinked_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::write(&target, b"victim").unwrap();
        let legacy = tmp.path().join("legacy-id");
        if thegn_core::fsperm::create_symlink(&target, &legacy, false).is_err() {
            return; // symlinks unavailable on this platform/account
        }
        let new = tmp.path().join("state/thegn/controller-id");

        assert_eq!(promote(&legacy, &new, "test symlink"), new);
        assert!(
            std::fs::symlink_metadata(&legacy)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"victim");
        assert!(
            std::fs::symlink_metadata(&new).is_err(),
            "nothing was moved"
        );
    }

    #[test]
    fn promote_falls_back_to_legacy_when_the_rename_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy-id");
        std::fs::write(&legacy, b"keep").unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let new = blocker.join("controller-id");

        assert_eq!(promote(&legacy, &new, "test rename failure"), legacy);
        assert_eq!(std::fs::read(&legacy).unwrap(), b"keep");
    }

    #[test]
    fn promote_moves_a_legacy_directory_with_its_files_together() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy/machine0/attest");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("a.json"), b"{\"a\":1}").unwrap();
        std::fs::write(legacy.join("b.json"), b"{\"b\":2}").unwrap();
        let new = tmp.path().join("state/thegn/machine0/attest");

        assert_eq!(promote(&legacy, &new, "test dir"), new);
        assert_eq!(std::fs::read(new.join("a.json")).unwrap(), b"{\"a\":1}");
        assert_eq!(std::fs::read(new.join("b.json")).unwrap(), b"{\"b\":2}");
        assert!(std::fs::symlink_metadata(&legacy).is_err(), "legacy gone");
    }

    #[test]
    fn promote_without_legacy_returns_new_without_creating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let new = tmp.path().join("state/thegn/controller-id");
        assert_eq!(
            promote(&tmp.path().join("absent"), &new, "test absent"),
            new
        );
        assert!(std::fs::symlink_metadata(&new).is_err());
        assert!(std::fs::symlink_metadata(new.parent().unwrap()).is_err());
    }
}
