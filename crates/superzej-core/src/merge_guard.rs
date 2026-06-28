//! The in-sandbox merge guard.
//!
//! When an agent or shell runs `git merge` against the **canonical** (primary)
//! checkout from *inside* a superzej sandbox, the canonical worktree's
//! filesystem view can diverge from git's (the sandbox-canonical-worktree
//! incoherence failure mode): the merge half-applies into the working tree and
//! is silently orphaned, corrupting `main`. The blessed, structurally-immune
//! path is `szhost integrate` (an object-DB fold with no checkout).
//!
//! This module ships a `pre-merge-commit` hook that detects exactly that
//! situation and refuses, pointing at `szhost integrate`. szhost installs it
//! into the shared hooks dir (`core.hooksPath` → the canonical `.git/hooks`) at
//! startup, on by default (`[git] merge_guard`). The hook is bind-mounted into
//! sandboxes at the same path and fires for *any* `git merge`, including a raw
//! one typed by a sandboxed agent — which szhost's own (host-side, always
//! coherent) merges never need.
//!
//! `pre-merge-commit` runs in *every* worktree, so the script is doubly scoped:
//! it acts only when `SUPERZEJ_SANDBOX` is set **and** it is running in the
//! primary worktree (git-dir == git-common-dir). `szhost integrate` uses
//! `commit-tree`/`update-ref` plumbing, which never fires hooks, so it is
//! unaffected.
//!
//! **Coexistence.** A pre-commit framework (prek/pre-commit) often already owns
//! the `pre-merge-commit` slot. Rather than skip (which would leave the guard
//! uninstalled) or clobber (which would silence the framework's merge checks),
//! we displace the foreign hook to [`CHAINED_NAME`] and **chain** to it on the
//! allow path. Because szhost reinstalls on every startup, a framework
//! `prek install` that later reclaims the slot is restored on the next launch.

use std::path::Path;

/// Marker embedded in the hook so we only ever refresh **our** script and never
/// clobber a user's hand-written `pre-merge-commit`.
pub const MARKER: &str = "superzej-merge-guard";

/// The hook filename in the hooks directory.
pub const HOOK_NAME: &str = "pre-merge-commit";

/// Where a displaced foreign hook is preserved and chained to.
pub const CHAINED_NAME: &str = "pre-merge-commit.superzej-orig";

/// The `pre-merge-commit` script body. Pure `/bin/sh`, no superzej runtime
/// dependency, so it works inside the network-sealed sandbox.
pub const HOOK_SCRIPT: &str = r#"#!/bin/sh
# superzej-merge-guard
#
# Refuse `git merge` in the canonical (primary) checkout when run from inside a
# superzej sandbox, where the canonical worktree's filesystem view can be
# incoherent and silently corrupt the merge. Use `szhost integrate` (an
# object-DB fold with no checkout) or merge from a host terminal instead.
#
# Installed and refreshed by szhost at startup. A hook this displaced is kept as
# pre-merge-commit.superzej-orig and chained to on the allow path. Escape hatch:
# SUPERZEJ_MERGE_GUARD_OFF=1.

hook_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
chained="$hook_dir/pre-merge-commit.superzej-orig"
# Allow path: delegate to any hook we displaced, else succeed.
pass() { [ -x "$chained" ] && exec "$chained" "$@"; exit 0; }

# Only inside a sandbox; host-side merges are coherent.
[ -z "$SUPERZEJ_SANDBOX" ] && pass "$@"
# Explicit override.
[ -n "$SUPERZEJ_MERGE_GUARD_OFF" ] && pass "$@"
# Only the primary (canonical) worktree: its git-dir IS the git-common-dir.
# Linked worktrees differ, and merges there are fine.
gd=$(git rev-parse --absolute-git-dir 2>/dev/null) || pass "$@"
common=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || pass "$@"
[ "$gd" != "$common" ] && pass "$@"

echo "superzej: refusing to merge in the canonical checkout from inside a sandbox." >&2
echo "  The canonical worktree's filesystem view can be incoherent here, which" >&2
echo "  silently corrupts the merge (an orphaned, half-applied result on main)." >&2
echo "  Use 'szhost integrate' (object-DB fold, no checkout), or merge from a host" >&2
echo "  terminal outside superzej. Override with SUPERZEJ_MERGE_GUARD_OFF=1." >&2
echo "  Then run 'git merge --abort' to clear the partial merge git just staged." >&2
exit 1
"#;

/// What [`install`] did, given the hook already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    /// No hook present, or a stale superzej hook — (re)wrote ours standalone.
    Wrote,
    /// Our hook is already byte-identical — nothing to do.
    AlreadyCurrent,
    /// A foreign hook was present — displaced it to [`CHAINED_NAME`] and wrote
    /// ours, which chains back to it on the allow path.
    Chained,
}

/// Pure decision: given the existing hook file body (if any), decide what
/// [`install`] should do. Never discards a foreign hook (it is chained, not
/// dropped).
pub fn decide(existing: Option<&str>) -> InstallAction {
    match existing {
        None => InstallAction::Wrote,
        Some(body) if body == HOOK_SCRIPT => InstallAction::AlreadyCurrent,
        Some(body) if body.contains(MARKER) => InstallAction::Wrote,
        Some(_) => InstallAction::Chained,
    }
}

/// Install (or refresh) the merge-guard hook into `hooks_dir`. Idempotent; a
/// no-op when our hook is current, and chains (never clobbers) a foreign hook.
/// Returns the action taken. Errors only on a genuine I/O failure (missing
/// hooks dir, permissions) — callers should treat that as "skipped".
pub fn install(hooks_dir: &Path) -> std::io::Result<InstallAction> {
    let path = hooks_dir.join(HOOK_NAME);
    let existing = std::fs::read_to_string(&path).ok();
    let action = decide(existing.as_deref());
    match action {
        InstallAction::AlreadyCurrent => {}
        InstallAction::Wrote => {
            write_hook(&path)?;
        }
        InstallAction::Chained => {
            // Preserve the displaced foreign hook (executable) before we take
            // over the slot, so the allow path can delegate to it.
            let chained = hooks_dir.join(CHAINED_NAME);
            std::fs::copy(&path, &chained)?;
            set_executable(&chained)?;
            write_hook(&path)?;
        }
    }
    Ok(action)
}

fn write_hook(path: &Path) -> std::io::Result<()> {
    std::fs::write(path, HOOK_SCRIPT)?;
    set_executable(path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_writes_when_absent() {
        assert_eq!(decide(None), InstallAction::Wrote);
    }

    #[test]
    fn decide_noop_when_identical() {
        assert_eq!(decide(Some(HOOK_SCRIPT)), InstallAction::AlreadyCurrent);
    }

    #[test]
    fn decide_refreshes_stale_superzej_hook() {
        let stale = format!("#!/bin/sh\n# {MARKER}\nexit 0\n");
        assert_eq!(decide(Some(&stale)), InstallAction::Wrote);
    }

    #[test]
    fn decide_chains_foreign_hook() {
        let foreign = "#!/bin/sh\n# generated by prek\nexec prek hook-impl\n";
        assert_eq!(decide(Some(foreign)), InstallAction::Chained);
    }

    #[test]
    fn script_is_scoped_and_self_describing() {
        // The guards that keep it from firing in the wrong place, the chain
        // delegation, and the redirect must all be present.
        assert!(HOOK_SCRIPT.contains("SUPERZEJ_SANDBOX"));
        assert!(HOOK_SCRIPT.contains("git-common-dir"));
        assert!(HOOK_SCRIPT.contains("szhost integrate"));
        assert!(HOOK_SCRIPT.contains(CHAINED_NAME));
        assert!(HOOK_SCRIPT.contains(MARKER));
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sz-mg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o111 == 0o111
    }

    #[test]
    fn install_writes_executable_then_is_idempotent() {
        let dir = scratch("install");
        assert_eq!(install(&dir).unwrap(), InstallAction::Wrote);
        let path = dir.join(HOOK_NAME);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HOOK_SCRIPT);
        #[cfg(unix)]
        assert!(is_executable(&path), "hook must be executable");
        // Second run is a no-op.
        assert_eq!(install(&dir).unwrap(), InstallAction::AlreadyCurrent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_chains_foreign_hook_and_preserves_it() {
        let dir = scratch("chain");
        let path = dir.join(HOOK_NAME);
        let foreign = "#!/bin/sh\necho framework-check\n";
        std::fs::write(&path, foreign).unwrap();

        assert_eq!(install(&dir).unwrap(), InstallAction::Chained);
        // Ours owns the slot now…
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HOOK_SCRIPT);
        // …and the foreign hook is preserved, executable, ready to chain to.
        let chained = dir.join(CHAINED_NAME);
        assert_eq!(std::fs::read_to_string(&chained).unwrap(), foreign);
        #[cfg(unix)]
        assert!(is_executable(&chained), "chained hook must stay executable");

        // Re-running sees our own hook and refreshes without re-chaining.
        assert_eq!(install(&dir).unwrap(), InstallAction::AlreadyCurrent);
        assert_eq!(std::fs::read_to_string(&chained).unwrap(), foreign);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_refreshes_stale_superzej_hook() {
        let dir = scratch("stale");
        let path = dir.join(HOOK_NAME);
        std::fs::write(&path, format!("#!/bin/sh\n# {MARKER}\nexit 0\n")).unwrap();
        assert_eq!(install(&dir).unwrap(), InstallAction::Wrote);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HOOK_SCRIPT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
