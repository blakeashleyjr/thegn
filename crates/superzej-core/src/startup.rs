//! Host-environment sanity checks run once at startup, before any git
//! operations. Each check is silent on success and emits a `msg::warn` (→
//! stderr / log file) on detection, then attempts a self-repair so the session
//! stays functional.
//!
//! **Why this exists: the Claude Code sandbox artifact.**
//! Claude Code's Bash sandbox (v2.1+) masks a hardcoded list of home-directory
//! dotfiles by creating them as *empty directories* at session start. The list
//! includes `~/.gitconfig`. When the sandbox process exits the directory is left
//! behind, causing every subsequent `git` invocation to die with:
//!
//! ```text
//! fatal: unknown error occurred while reading the configuration files
//! ```
//!
//! This breaks szhost's hydration path: git calls return errors immediately,
//! the waker fires, the loop re-hydrates on the next tick — and the CPU never
//! returns to idle. The repair here (remove the empty dir, replace with a
//! symlink or placeholder) is idempotent and takes microseconds.
//!
//! Other tools with similar sandboxing behaviour (Cursor, Windsurf, …) mask
//! the same paths, so the check is written generically.

use std::path::{Path, PathBuf};

/// The canonical list of home-directory paths that sandbox tools may replace
/// with empty directories. Checked in order; each entry is relative to `$HOME`.
const MASKED_PATHS: &[&str] = &[".gitconfig", ".bash_profile", ".bashrc"];

/// Run all startup environment checks. Call once, early in `main()`, before
/// the first git subprocess or config read. Non-fatal: a failed repair logs a
/// warning and continues; it never aborts the session.
pub fn run_checks() {
    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => return,
    };

    for rel in MASKED_PATHS {
        let path = home.join(rel);
        if is_sandbox_mask(&path) {
            repair_mask(&path, &home, rel);
        }
    }
}

/// Returns `true` when `path` is an empty directory — the hallmark of a
/// sandbox mask. A non-existent path, a regular file, or a symlink all return
/// `false`.
fn is_sandbox_mask(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            // Must be empty: a real `.gitconfig` directory (unusual but
            // theoretically valid) should not be silently wiped.
            std::fs::read_dir(path)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// Attempt to repair a masked path:
///
/// 1. Remove the empty directory.
/// 2. For `.gitconfig` specifically: if `~/.config/git/config` exists, create
///    a symlink `~/.gitconfig → ~/.config/git/config` so both git lookup paths
///    point at the real configuration. Otherwise write a minimal placeholder so
///    git does not error on missing config.
/// 3. For other paths (`.bashrc`, etc.): leave absent after removal — the shell
///    will simply not source a non-existent file, which is harmless.
fn repair_mask(path: &Path, home: &Path, rel: &str) {
    crate::msg::warn(&format!(
        "startup: {rel} is an empty directory (sandbox mask artifact) — repairing"
    ));

    if let Err(e) = std::fs::remove_dir(path) {
        crate::msg::warn(&format!("startup: could not remove {rel} directory: {e}"));
        return;
    }

    if rel == ".gitconfig" {
        repair_gitconfig(path, home);
    }
    // For other masked paths (bashrc, etc.) we leave them absent after removal.
    // The shell handles a missing startup file gracefully.
}

/// Restore `~/.gitconfig` after removing the mask:
///
/// * If `~/.config/git/config` exists → symlink (canonical XDG location).
/// * Else if `~/.config/git/` exists but `config` doesn't → symlink anyway,
///   letting git create the file there when needed.
/// * Otherwise → write a minimal placeholder (`[core]` with `autocrlf = false`)
///   so git reads it without error. Users can overwrite it freely.
fn repair_gitconfig(gitconfig: &Path, home: &Path) {
    let xdg_git_config = home.join(".config/git/config");
    let xdg_git_dir = home.join(".config/git");

    if xdg_git_config.exists() || xdg_git_dir.is_dir() {
        // XDG location is canonical; point ~/.gitconfig at it.
        match std::os::unix::fs::symlink(&xdg_git_config, gitconfig) {
            Ok(()) => {
                crate::msg::warn("startup: restored ~/.gitconfig → ~/.config/git/config symlink");
            }
            Err(e) => {
                crate::msg::warn(&format!(
                    "startup: could not create ~/.gitconfig symlink: {e}"
                ));
            }
        }
    } else {
        // No XDG config — write a minimal placeholder so git doesn't error.
        let placeholder = "[core]\n\tautocrlf = false\n";
        match std::fs::write(gitconfig, placeholder) {
            Ok(()) => {
                crate::msg::warn(
                    "startup: wrote minimal ~/.gitconfig placeholder \
                     (no existing config found at ~/.config/git/config)",
                );
            }
            Err(e) => {
                crate::msg::warn(&format!(
                    "startup: could not write ~/.gitconfig placeholder: {e}"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Serialises HOME mutation against sandbox tests that read HOME — shared
    // crate-wide so readers in other modules lock the SAME mutex.
    use crate::HOME_LOCK;

    /// Create a unique temp directory for each test — avoids cross-test
    /// interference without requiring the `tempfile` crate.
    fn tmp_home(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "szhost-startup-test-{label}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn is_sandbox_mask_empty_dir() {
        let home = tmp_home("empty");
        let gc = home.join(".gitconfig");
        fs::create_dir(&gc).unwrap();
        assert!(is_sandbox_mask(&gc));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn is_sandbox_mask_non_empty_dir() {
        let home = tmp_home("nonempty");
        let gc = home.join(".gitconfig");
        fs::create_dir(&gc).unwrap();
        fs::write(gc.join("something"), b"x").unwrap();
        // A non-empty directory must NOT be treated as a mask.
        assert!(!is_sandbox_mask(&gc));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn is_sandbox_mask_regular_file() {
        let home = tmp_home("file");
        let gc = home.join(".gitconfig");
        fs::write(&gc, b"[user]\n").unwrap();
        assert!(!is_sandbox_mask(&gc));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn is_sandbox_mask_absent() {
        let home = tmp_home("absent");
        assert!(!is_sandbox_mask(&home.join(".gitconfig")));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn repair_creates_symlink_when_xdg_config_exists() {
        let home = tmp_home("symlink");

        // Set up XDG git config.
        let xdg_dir = home.join(".config/git");
        fs::create_dir_all(&xdg_dir).unwrap();
        let xdg_cfg = xdg_dir.join("config");
        fs::write(&xdg_cfg, b"[user]\n\tname = Test\n").unwrap();

        // Plant the mask.
        let gitconfig = home.join(".gitconfig");
        fs::create_dir(&gitconfig).unwrap();
        assert!(is_sandbox_mask(&gitconfig));

        repair_mask(&gitconfig, &home, ".gitconfig");

        // Should now be a symlink pointing at the XDG config.
        let meta = fs::symlink_metadata(&gitconfig).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            ".gitconfig should be a symlink"
        );
        assert_eq!(fs::read_link(&gitconfig).unwrap(), xdg_cfg);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn repair_writes_placeholder_when_no_xdg_config() {
        let home = tmp_home("placeholder");

        // No XDG config at all.
        let gitconfig = home.join(".gitconfig");
        fs::create_dir(&gitconfig).unwrap();

        repair_mask(&gitconfig, &home, ".gitconfig");

        // Should now be a regular file with placeholder content.
        let content = fs::read_to_string(&gitconfig).unwrap();
        assert!(
            content.contains("[core]"),
            "placeholder should contain [core]"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn repair_is_noop_on_nonempty_dir() {
        let home = tmp_home("noop");
        let gitconfig = home.join(".gitconfig");
        fs::create_dir(&gitconfig).unwrap();
        fs::write(gitconfig.join("real_file"), b"data").unwrap();

        // is_sandbox_mask should return false — repair_mask is never called.
        assert!(!is_sandbox_mask(&gitconfig));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn run_checks_fixes_mask_in_home() {
        let home = tmp_home("run-checks");

        // Plant XDG config and the gitconfig mask.
        let xdg_cfg = home.join(".config/git/config");
        fs::create_dir_all(xdg_cfg.parent().unwrap()).unwrap();
        fs::write(&xdg_cfg, b"[user]\n\tname = Blake\n").unwrap();

        let gitconfig = home.join(".gitconfig");
        fs::create_dir(&gitconfig).unwrap();

        // Hold the lock for the entire HOME-mutation window so that the
        // sandbox test, which reads HOME to build toolchain mounts, never
        // sees a temp path that's about to be deleted.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let original_home = std::env::var("HOME").ok();
        // SAFETY: test-only mutation, protected by HOME_LOCK above.
        unsafe { std::env::set_var("HOME", &home) };

        run_checks();

        // Assert before cleanup: the symlink must exist while the temp home dir
        // is still on disk.
        let meta = fs::symlink_metadata(&gitconfig).unwrap();
        assert!(meta.file_type().is_symlink());

        // Restore HOME and remove temp dir while still holding the lock so no
        // concurrent reader can observe a HOME pointing at a deleted path.
        if let Some(h) = original_home {
            unsafe { std::env::set_var("HOME", h) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        let _ = fs::remove_dir_all(&home);
        drop(_guard); // HOME is restored and temp dir is gone
    }

    #[test]
    fn repair_symlinks_when_xdg_dir_exists_without_config_file() {
        let home = tmp_home("xdg-dir-only");
        // The XDG git directory exists but has no `config` file yet.
        let xdg_dir = home.join(".config/git");
        fs::create_dir_all(&xdg_dir).unwrap();
        let xdg_cfg = xdg_dir.join("config");
        assert!(!xdg_cfg.exists());

        let gitconfig = home.join(".gitconfig");
        fs::create_dir(&gitconfig).unwrap();
        assert!(is_sandbox_mask(&gitconfig));

        repair_mask(&gitconfig, &home, ".gitconfig");

        // Should be a symlink even though the target file does not exist yet —
        // git will create it there on demand.
        let meta = fs::symlink_metadata(&gitconfig).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            ".gitconfig should be symlink"
        );
        assert_eq!(fs::read_link(&gitconfig).unwrap(), xdg_cfg);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn repair_non_gitconfig_mask_leaves_path_absent() {
        // A non-.gitconfig masked path (e.g. .bashrc) is just removed, not
        // recreated.
        let home = tmp_home("bashrc");
        let bashrc = home.join(".bashrc");
        fs::create_dir(&bashrc).unwrap();
        assert!(is_sandbox_mask(&bashrc));

        repair_mask(&bashrc, &home, ".bashrc");

        // Removed and not recreated.
        assert!(!bashrc.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn run_checks_is_noop_when_home_empty() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original_home = std::env::var("HOME").ok();
        // An empty HOME triggers the early return; no panic, no work done.
        // SAFETY: test-only mutation, protected by HOME_LOCK.
        unsafe { std::env::set_var("HOME", "") };
        run_checks();
        // Also exercise the fully-unset path.
        unsafe { std::env::remove_var("HOME") };
        run_checks();
        if let Some(h) = original_home {
            unsafe { std::env::set_var("HOME", h) };
        }
        drop(_guard);
    }

    #[test]
    fn run_checks_skips_when_no_mask_present() {
        // HOME points at a dir where ~/.gitconfig is a real file (not a mask):
        // run_checks must leave it untouched.
        let home = tmp_home("no-mask");
        let gitconfig = home.join(".gitconfig");
        fs::write(&gitconfig, b"[user]\n\tname = Real\n").unwrap();

        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original_home = std::env::var("HOME").ok();
        // SAFETY: test-only mutation, protected by HOME_LOCK.
        unsafe { std::env::set_var("HOME", &home) };
        run_checks();
        // Still a regular file with its original contents.
        let content = fs::read_to_string(&gitconfig).unwrap();
        assert!(content.contains("Real"));
        if let Some(h) = original_home {
            unsafe { std::env::set_var("HOME", h) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        let _ = fs::remove_dir_all(&home);
        drop(_guard);
    }
}
