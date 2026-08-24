//! The in-sandbox merge guard.
//!
//! When an agent or shell runs `git merge` against the **canonical** (primary)
//! checkout from *inside* a thegn sandbox, the canonical worktree's
//! filesystem view can diverge from git's (the sandbox-canonical-worktree
//! incoherence failure mode): the merge half-applies into the working tree and
//! is silently orphaned, corrupting `main`. The blessed, structurally-immune
//! paths are `thegn integrate` (drain the whole queue) and `thegn land`
//! (one-shot land the current branch) — both object-DB folds with no checkout.
//!
//! **The sandbox boundary, and why the blessed paths work.** A sandbox mounts
//! the canonical's **working tree read-only** (to protect a live instance) but
//! keeps the shared `.git` (object + ref store) **writable**. So a `git merge` /
//! `merge --ff-only` fails — it must rewrite the read-only tree — but the fold
//! path succeeds: it writes objects and advances `refs/heads/main` by
//! compare-and-swap (`update-ref <ref> <new> <old>`), never touching the tree.
//! The tree is then re-cohered by the fold itself: on a successful CAS,
//! `util::resync_branch_checkouts` fast-forwards **every** worktree that has the
//! advanced branch checked out (`git read-tree -m -u`, which aborts rather than
//! clobber). A checkout with genuine uncommitted work is deliberately left
//! alone — and *reported*, with the exact `git reset --keep` that syncs it,
//! because a ref that moved under a live tree makes `git status` show the whole
//! fold as pending deletions. A running instance additionally self-heals on the
//! ref move (`util::heal_main_checkout_worktree`, driven by the ref fs-watcher —
//! see the host's `git_watch::spawn_main_checkout_heal`), but that path is
//! compositor-only and must never be relied on by the CLI. Never hand-roll a
//! `git update-ref` to "merge to main"; use `thegn land`, which folds, gates,
//! CAS-advances, and syncs the checkouts.
//!
//! This module ships a `pre-merge-commit` hook that detects exactly that
//! situation and refuses, pointing at `thegn integrate`. thegn installs it
//! into the shared hooks dir (`core.hooksPath` → the canonical `.git/hooks`) at
//! startup, on by default (`[git] merge_guard`). The hook is bind-mounted into
//! sandboxes at the same path and fires for *any* `git merge`, including a raw
//! one typed by a sandboxed agent — which thegn's own (host-side, always
//! coherent) merges never need.
//!
//! `pre-merge-commit` runs in *every* worktree, so the script is doubly scoped:
//! it acts only when `THEGN_SANDBOX` is set **and** it is running in the
//! primary worktree (git-dir == git-common-dir). `thegn integrate` uses
//! `commit-tree`/`update-ref` plumbing, which never fires hooks, so it is
//! unaffected.
//!
//! # Coexisting with a pre-commit framework
//!
//! A pre-commit framework (prek, or Python pre-commit) very often already owns
//! the `pre-merge-commit` slot — in this repo the flake dev shell installs one.
//! Both frameworks put a tiny **shim** in the slot that delegates to
//! `<tool> hook-impl`, and both, when they install over an existing hook, move
//! it aside to `<hook>.legacy` and keep running it ("migration mode").
//!
//! We used to displace the shim to [`CHAINED_NAME`] and take the slot, chaining
//! to it on the allow path. **That does not work**, and the failure is silent
//! until someone merges: a shim invoked while it is *not* the installed hook
//! detects migration mode, prints `prek's Git shim is installed in migration
//! mode`, and exits non-zero — so every `git merge` in the canonical checkout
//! failed with a message about hook plumbing. Worse, it flip-flopped: the
//! framework would reclaim the slot, then thegn's next startup displaced it
//! again, so the breakage came back on its own.
//!
//! So the shim keeps the slot and **we install at [`LEGACY_NAME`]**, which the
//! shim already execs at runtime — verified against prek: a non-zero exit from
//! `.legacy` blocks the merge, which is exactly the guard's contract. Both
//! systems then run, neither displaces the other, and there is nothing to
//! flip-flop over. When no framework owns the slot we take it as before,
//! chaining a genuine user hook to [`CHAINED_NAME`].
//!
//! [`plan`] also *repairs* a checkout left in the old broken shape (our hook in
//! the slot, a shim parked in [`CHAINED_NAME`]) by handing the slot back to the
//! framework — otherwise the poisoned chain would survive every upgrade.

use std::path::{Path, PathBuf};

/// Marker embedded in the hook so we only ever refresh **our** script and never
/// clobber a user's hand-written `pre-merge-commit`.
pub const MARKER: &str = "thegn-merge-guard";

/// The hook filename in the hooks directory.
pub const HOOK_NAME: &str = "pre-merge-commit";

/// Where a displaced foreign hook is preserved and chained to.
pub const CHAINED_NAME: &str = "pre-merge-commit.thegn-orig";

/// Where a pre-commit framework (prek / pre-commit) parks the hook it displaced,
/// and still execs at runtime. When a framework shim owns the slot, this is
/// where our guard installs itself.
pub const LEGACY_NAME: &str = "pre-merge-commit.legacy";

/// The `pre-merge-commit` script body. Pure `/bin/sh`, no thegn runtime
/// dependency, so it works inside the network-sealed sandbox.
pub const HOOK_SCRIPT: &str = r#"#!/bin/sh
# thegn-merge-guard
#
# Refuse `git merge` in the canonical (primary) checkout when run from inside a
# thegn sandbox, where the canonical worktree's filesystem view can be
# incoherent and silently corrupt the merge. Use `thegn integrate` (an
# object-DB fold with no checkout) or merge from a host terminal instead.
#
# Installed and refreshed by thegn at startup, in one of two places: the
# pre-merge-commit slot, or — when a pre-commit framework (prek/pre-commit)
# owns that slot — pre-merge-commit.legacy, which the framework's shim execs.
# A hook we displaced is kept as pre-merge-commit.thegn-orig and chained to on
# the allow path. Escape hatch: THEGN_MERGE_GUARD_OFF=1.

hook_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
chained="$hook_dir/pre-merge-commit.thegn-orig"
# Allow path: delegate to any hook we displaced, else succeed.
pass() { [ -x "$chained" ] && exec "$chained" "$@"; exit 0; }

# Only inside a sandbox; host-side merges are coherent.
[ -z "$THEGN_SANDBOX" ] && pass "$@"
# Explicit override.
[ -n "$THEGN_MERGE_GUARD_OFF" ] && pass "$@"
# Only the primary (canonical) worktree: its git-dir IS the git-common-dir.
# Linked worktrees differ, and merges there are fine.
gd=$(git rev-parse --absolute-git-dir 2>/dev/null) || pass "$@"
common=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || pass "$@"
[ "$gd" != "$common" ] && pass "$@"

echo "thegn: refusing to merge in the canonical checkout from inside a sandbox." >&2
echo "  The canonical worktree's filesystem view can be incoherent here, which" >&2
echo "  silently corrupts the merge (an orphaned, half-applied result on main)." >&2
echo "  Use 'thegn integrate' (object-DB fold, no checkout), or merge from a host" >&2
echo "  terminal outside thegn. Override with THEGN_MERGE_GUARD_OFF=1." >&2
echo "  Then run 'git merge --abort' to clear the partial merge git just staged." >&2
exit 1
"#;

/// What a hook file on disk *is*, which is what decides whether it may be
/// overwritten, must be preserved, or must be left owning the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// Byte-identical to [`HOOK_SCRIPT`] — nothing to do.
    Current,
    /// Ours, but an older revision — safe to refresh in place.
    StaleOurs,
    /// A pre-commit-framework shim (prek / pre-commit) delegating to
    /// `hook-impl`. **Must own the slot**: invoked from anywhere else it
    /// detects migration mode and fails.
    Shim,
    /// Anything else — a real hook that must be preserved, never clobbered.
    Foreign,
}

/// Classify a hook body. A body that is not valid UTF-8 (e.g. a compiled-binary
/// hook) is [`HookKind::Foreign`]: our script is ASCII, so it can never be ours,
/// and it must be preserved rather than overwritten.
pub fn classify(body: &[u8]) -> HookKind {
    if body == HOOK_SCRIPT.as_bytes() {
        return HookKind::Current;
    }
    match std::str::from_utf8(body) {
        Ok(s) if s.contains(MARKER) => HookKind::StaleOurs,
        // Both frameworks' shims exec `<tool> hook-impl`; the generated-by
        // banner is the other half of the signature. Either is enough — a shim
        // misread as Foreign is the failure this module exists to avoid.
        Ok(s)
            if s.contains("hook-impl")
                || s.contains("generated by prek")
                || s.contains("generated by pre-commit") =>
        {
            HookKind::Shim
        }
        _ => HookKind::Foreign,
    }
}

/// Where our guard script gets written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The `pre-merge-commit` slot itself — nothing else owns it.
    Hook,
    /// `pre-merge-commit.legacy`, because a framework shim owns the slot and
    /// execs this path at runtime.
    Legacy,
}

/// What [`install`] does to the file at [`Plan::placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    /// Absent, ours-but-stale, or a redundant duplicate shim — wrote ours.
    Wrote,
    /// Ours is already byte-identical — nothing to do.
    AlreadyCurrent,
    /// A foreign hook was there — displaced it to [`CHAINED_NAME`] and wrote
    /// ours, which chains back to it on the allow path.
    Chained,
}

/// The full decision: everything [`install`] will do, computed from the three
/// files it looks at. Pure, so the awkward states are unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Hand the slot back to a framework shim we previously displaced into
    /// [`CHAINED_NAME`] (move it back, leaving no `.thegn-orig`). Repairs a
    /// checkout left in the old broken shape.
    pub restore_shim: bool,
    /// Where our script goes.
    pub placement: Placement,
    /// What happens at that path.
    pub action: InstallAction,
}

/// Pure decision from the bodies of the three files involved: the
/// `pre-merge-commit` slot, [`LEGACY_NAME`], and [`CHAINED_NAME`]. `None` means
/// the file is absent. Never discards a foreign hook, and never leaves a
/// framework shim anywhere it would be invoked as a chained hook.
pub fn plan(slot: Option<&[u8]>, legacy: Option<&[u8]>, chained: Option<&[u8]>) -> Plan {
    let slot_kind = slot.map(classify);
    // The old bug's fingerprint: our hook in the slot, a framework shim parked
    // in `.thegn-orig` where the allow path would exec it — which the shim
    // refuses. Give the slot back and install alongside instead.
    let restore_shim = matches!(slot_kind, Some(HookKind::Current | HookKind::StaleOurs))
        && chained.map(classify) == Some(HookKind::Shim);

    let effective = if restore_shim {
        Some(HookKind::Shim)
    } else {
        slot_kind
    };

    if effective == Some(HookKind::Shim) {
        // The framework keeps the slot; we go where its shim already looks.
        let action = match legacy.map(classify) {
            None | Some(HookKind::StaleOurs) => InstallAction::Wrote,
            Some(HookKind::Current) => InstallAction::AlreadyCurrent,
            // A shim here too would double-run the framework, and it is not a
            // user's work to preserve — drop it.
            Some(HookKind::Shim) => InstallAction::Wrote,
            // A real hook the framework displaced: preserve and chain to it.
            Some(HookKind::Foreign) => InstallAction::Chained,
        };
        return Plan {
            restore_shim,
            placement: Placement::Legacy,
            action,
        };
    }

    let action = match slot_kind {
        None | Some(HookKind::StaleOurs) => InstallAction::Wrote,
        Some(HookKind::Current) => InstallAction::AlreadyCurrent,
        Some(HookKind::Foreign) => InstallAction::Chained,
        // Handled above.
        Some(HookKind::Shim) => InstallAction::Wrote,
    };
    Plan {
        restore_shim: false,
        placement: Placement::Hook,
        action,
    }
}

/// Read a hook file, mapping only a genuine `NotFound` to `None`. Read as bytes,
/// not `read_to_string`: a non-UTF-8 (binary) hook must not be mistaken for an
/// absent file and clobbered.
fn read_hook(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Install (or refresh) the merge-guard hook into `hooks_dir`. Idempotent; a
/// no-op when our hook is current, chains (never clobbers) a foreign hook, and
/// leaves a pre-commit framework owning the slot it needs. Returns the [`Plan`]
/// it executed. Errors only on a genuine I/O failure (missing hooks dir,
/// permissions) — callers should treat that as "skipped".
pub fn install(hooks_dir: &Path) -> std::io::Result<Plan> {
    let hook = hooks_dir.join(HOOK_NAME);
    let legacy = hooks_dir.join(LEGACY_NAME);
    let chained = hooks_dir.join(CHAINED_NAME);

    let p = plan(
        read_hook(&hook)?.as_deref(),
        read_hook(&legacy)?.as_deref(),
        read_hook(&chained)?.as_deref(),
    );

    // Restore first: it frees `.thegn-orig` for any displacement below.
    if p.restore_shim {
        std::fs::rename(&chained, &hook)?;
        set_executable(&hook)?;
    }

    let target: PathBuf = match p.placement {
        Placement::Hook => hook,
        Placement::Legacy => legacy,
    };
    match p.action {
        InstallAction::AlreadyCurrent => {}
        InstallAction::Wrote => write_hook(&target)?,
        InstallAction::Chained => {
            // Preserve the displaced hook (executable) before we take the path,
            // so the allow path can delegate to it.
            std::fs::copy(&target, &chained)?;
            set_executable(&chained)?;
            write_hook(&target)?;
        }
    }
    Ok(p)
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

    /// A prek shim, near-verbatim from a real `.git/hooks/pre-merge-commit`.
    const PREK_SHIM: &str = r#"#!/bin/sh
# File generated by prek: https://github.com/j178/prek
# ID: 182c10f181da4464a3eec51b83331688
PREK="/nix/store/xxxx-prek-0.4.14/bin/prek"
exec "$PREK" hook-impl --hook-dir "$HERE" --hook-type=pre-merge-commit -- "$@"
"#;

    /// A Python pre-commit shim.
    const PRE_COMMIT_SHIM: &str = r#"#!/usr/bin/env bash
# File generated by pre-commit: https://pre-commit.com
ARGS=(hook-impl --config=.pre-commit-config.yaml --hook-type=pre-merge-commit)
exec "$INSTALL_PYTHON" -mpre_commit "${ARGS[@]}"
"#;

    const USER_HOOK: &str = "#!/bin/sh\necho my own check\n";

    // ── classify ────────────────────────────────────────────────────────────

    #[test]
    fn classify_recognises_our_current_script() {
        assert_eq!(classify(HOOK_SCRIPT.as_bytes()), HookKind::Current);
    }

    #[test]
    fn classify_recognises_a_stale_thegn_hook() {
        let stale = format!("#!/bin/sh\n# {MARKER}\nexit 0\n");
        assert_eq!(classify(stale.as_bytes()), HookKind::StaleOurs);
    }

    #[test]
    fn classify_recognises_both_framework_shims() {
        assert_eq!(classify(PREK_SHIM.as_bytes()), HookKind::Shim);
        assert_eq!(classify(PRE_COMMIT_SHIM.as_bytes()), HookKind::Shim);
    }

    #[test]
    fn classify_recognises_a_shim_by_banner_alone() {
        // A future shim that no longer spells `hook-impl` must still be a Shim —
        // misreading one as Foreign is exactly the bug this module exists to
        // avoid, so both halves of the signature are accepted independently.
        let banner = "#!/bin/sh\n# File generated by prek: https://x\nexec prek run\n";
        assert_eq!(classify(banner.as_bytes()), HookKind::Shim);
    }

    #[test]
    fn classify_treats_a_user_hook_as_foreign() {
        assert_eq!(classify(USER_HOOK.as_bytes()), HookKind::Foreign);
    }

    #[test]
    fn classify_treats_non_utf8_as_foreign() {
        // An ELF header with a stray 0xff/0xfe/0x80 tail — not valid UTF-8, so it
        // can't be ours (our script is ASCII) and must be preserved, not
        // clobbered. (No `from_utf8(..).is_err()` assert: the literal is invalid
        // at compile time, so clippy's `invalid_from_utf8` rightly calls that
        // assertion dead weight.)
        let binary: &[u8] = &[0x7f, b'E', b'L', b'F', 0xff, 0xfe, 0x00, 0x80];
        assert_eq!(classify(binary), HookKind::Foreign);
    }

    // ── plan ────────────────────────────────────────────────────────────────

    fn b(s: &str) -> &[u8] {
        s.as_bytes()
    }

    #[test]
    fn plan_takes_an_empty_slot() {
        assert_eq!(
            plan(None, None, None),
            Plan {
                restore_shim: false,
                placement: Placement::Hook,
                action: InstallAction::Wrote,
            }
        );
    }

    #[test]
    fn plan_is_a_noop_when_our_hook_owns_the_slot() {
        let p = plan(Some(b(HOOK_SCRIPT)), None, None);
        assert_eq!(p.action, InstallAction::AlreadyCurrent);
        assert_eq!(p.placement, Placement::Hook);
        assert!(!p.restore_shim);
    }

    #[test]
    fn plan_chains_a_user_hook_in_the_slot() {
        let p = plan(Some(b(USER_HOOK)), None, None);
        assert_eq!(p.action, InstallAction::Chained);
        assert_eq!(p.placement, Placement::Hook);
    }

    #[test]
    fn plan_leaves_a_shim_owning_the_slot_and_installs_at_legacy() {
        // The core of the fix: never displace the framework.
        let p = plan(Some(b(PREK_SHIM)), None, None);
        assert_eq!(p.placement, Placement::Legacy);
        assert_eq!(p.action, InstallAction::Wrote);
        assert!(!p.restore_shim);
    }

    #[test]
    fn plan_is_a_noop_when_already_installed_beside_a_shim() {
        // Idempotence in the shim arrangement — the flip-flop used to live here.
        let p = plan(Some(b(PREK_SHIM)), Some(b(HOOK_SCRIPT)), None);
        assert_eq!(p.placement, Placement::Legacy);
        assert_eq!(p.action, InstallAction::AlreadyCurrent);
    }

    #[test]
    fn plan_refreshes_a_stale_guard_sitting_beside_a_shim() {
        // The upgrade path for a checkout already in the good arrangement: the
        // script body changes, so the copy at `.legacy` must be refreshed in
        // place without disturbing the shim or touching `.thegn-orig`.
        let stale = format!("#!/bin/sh\n# {MARKER}\nexit 0\n");
        let p = plan(Some(b(PREK_SHIM)), Some(b(&stale)), None);
        assert_eq!(p.placement, Placement::Legacy);
        assert_eq!(p.action, InstallAction::Wrote);
        assert!(!p.restore_shim);
    }

    #[test]
    fn plan_preserves_a_user_hook_the_framework_displaced_to_legacy() {
        let p = plan(Some(b(PREK_SHIM)), Some(b(USER_HOOK)), None);
        assert_eq!(p.placement, Placement::Legacy);
        assert_eq!(p.action, InstallAction::Chained);
    }

    #[test]
    fn plan_drops_a_redundant_second_shim_at_legacy() {
        // A shim in both places would double-run the framework, and it is not
        // the user's work to preserve.
        let p = plan(Some(b(PREK_SHIM)), Some(b(PRE_COMMIT_SHIM)), None);
        assert_eq!(p.placement, Placement::Legacy);
        assert_eq!(p.action, InstallAction::Wrote);
    }

    #[test]
    fn plan_repairs_the_old_broken_shape() {
        // Regression: our hook in the slot with a shim parked in `.thegn-orig`,
        // which the allow path would exec — and a shim invoked while it is not
        // the installed hook fails "migration mode", blocking every merge.
        // Hand the slot back and install beside it instead.
        let p = plan(Some(b(HOOK_SCRIPT)), None, Some(b(PREK_SHIM)));
        assert!(p.restore_shim, "the shim must get its slot back");
        assert_eq!(p.placement, Placement::Legacy);
        assert_eq!(p.action, InstallAction::Wrote);
    }

    #[test]
    fn plan_repairs_the_old_broken_shape_from_a_stale_hook() {
        let stale = format!("#!/bin/sh\n# {MARKER}\nexit 0\n");
        let p = plan(Some(b(&stale)), None, Some(b(PREK_SHIM)));
        assert!(p.restore_shim);
        assert_eq!(p.placement, Placement::Legacy);
    }

    #[test]
    fn plan_does_not_restore_a_user_hook_from_chained() {
        // `.thegn-orig` holding a genuine user hook is the NORMAL arrangement —
        // it must not be mistaken for the broken shape and yanked into the slot.
        let p = plan(Some(b(HOOK_SCRIPT)), None, Some(b(USER_HOOK)));
        assert!(!p.restore_shim);
        assert_eq!(p.placement, Placement::Hook);
        assert_eq!(p.action, InstallAction::AlreadyCurrent);
    }

    #[test]
    fn plan_does_not_restore_when_a_foreign_hook_owns_the_slot() {
        // Someone else's hook in the slot is not ours to move aside for a shim.
        let p = plan(Some(b(USER_HOOK)), None, Some(b(PREK_SHIM)));
        assert!(!p.restore_shim);
        assert_eq!(p.placement, Placement::Hook);
        assert_eq!(p.action, InstallAction::Chained);
    }

    #[test]
    fn script_is_scoped_and_self_describing() {
        // The guards that keep it from firing in the wrong place, the chain
        // delegation, and the redirect must all be present.
        assert!(HOOK_SCRIPT.contains("THEGN_SANDBOX"));
        assert!(HOOK_SCRIPT.contains("git-common-dir"));
        assert!(HOOK_SCRIPT.contains("thegn integrate"));
        assert!(HOOK_SCRIPT.contains(CHAINED_NAME));
        assert!(HOOK_SCRIPT.contains(LEGACY_NAME));
        assert!(HOOK_SCRIPT.contains(MARKER));
        // Our own script must never be mistaken for a framework shim.
        assert_eq!(classify(HOOK_SCRIPT.as_bytes()), HookKind::Current);
    }

    // ── install (on-disk) ───────────────────────────────────────────────────

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tg-mg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o111 == 0o111
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn install_writes_executable_then_is_idempotent() {
        let dir = scratch("install");
        assert_eq!(install(&dir).unwrap().action, InstallAction::Wrote);
        let path = dir.join(HOOK_NAME);
        assert_eq!(read(&path), HOOK_SCRIPT);
        #[cfg(unix)]
        assert!(is_executable(&path), "hook must be executable");
        assert_eq!(install(&dir).unwrap().action, InstallAction::AlreadyCurrent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_chains_foreign_hook_and_preserves_it() {
        let dir = scratch("chain");
        let path = dir.join(HOOK_NAME);
        std::fs::write(&path, USER_HOOK).unwrap();

        assert_eq!(install(&dir).unwrap().action, InstallAction::Chained);
        assert_eq!(read(&path), HOOK_SCRIPT);
        let chained = dir.join(CHAINED_NAME);
        assert_eq!(read(&chained), USER_HOOK);
        #[cfg(unix)]
        assert!(is_executable(&chained), "chained hook must stay executable");

        // Re-running sees our own hook and refreshes without re-chaining.
        assert_eq!(install(&dir).unwrap().action, InstallAction::AlreadyCurrent);
        assert_eq!(read(&chained), USER_HOOK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_chains_non_utf8_foreign_hook_instead_of_clobbering() {
        // Regression: a non-UTF-8 (binary) foreign hook was silently overwritten
        // (read_to_string → None → Wrote), deleting the user's hook with no
        // backup. It must be displaced to CHAINED_NAME and preserved verbatim.
        let dir = scratch("binhook");
        let path = dir.join(HOOK_NAME);
        let binary: &[u8] = &[0x7f, b'E', b'L', b'F', 0xff, 0xfe, 0x00, 0x80];
        std::fs::write(&path, binary).unwrap();

        assert_eq!(install(&dir).unwrap().action, InstallAction::Chained);
        assert_eq!(read(&path), HOOK_SCRIPT);
        let chained = dir.join(CHAINED_NAME);
        assert_eq!(std::fs::read(&chained).unwrap(), binary);
        #[cfg(unix)]
        assert!(
            is_executable(&chained),
            "chained binary hook must stay executable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_refreshes_stale_thegn_hook() {
        let dir = scratch("stale");
        let path = dir.join(HOOK_NAME);
        std::fs::write(&path, format!("#!/bin/sh\n# {MARKER}\nexit 0\n")).unwrap();
        assert_eq!(install(&dir).unwrap().action, InstallAction::Wrote);
        assert_eq!(read(&path), HOOK_SCRIPT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_leaves_the_shim_in_the_slot_and_lands_at_legacy() {
        let dir = scratch("shim");
        std::fs::write(dir.join(HOOK_NAME), PREK_SHIM).unwrap();

        let p = install(&dir).unwrap();
        assert_eq!(p.placement, Placement::Legacy);
        // The framework still owns its slot…
        assert_eq!(read(&dir.join(HOOK_NAME)), PREK_SHIM);
        // …and we are where its shim execs.
        assert_eq!(read(&dir.join(LEGACY_NAME)), HOOK_SCRIPT);
        #[cfg(unix)]
        assert!(is_executable(&dir.join(LEGACY_NAME)));
        // Nothing was parked in the poisoned chain slot.
        assert!(!dir.join(CHAINED_NAME).exists());

        // Idempotent — this is where the flip-flop used to start.
        assert_eq!(install(&dir).unwrap().action, InstallAction::AlreadyCurrent);
        assert_eq!(read(&dir.join(HOOK_NAME)), PREK_SHIM);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_repairs_the_old_broken_shape_on_disk() {
        // End-to-end repair of a checkout left by the old displace-and-chain
        // code: our hook in the slot, prek's shim parked in `.thegn-orig`.
        let dir = scratch("repair");
        std::fs::write(dir.join(HOOK_NAME), HOOK_SCRIPT).unwrap();
        std::fs::write(dir.join(CHAINED_NAME), PREK_SHIM).unwrap();

        let p = install(&dir).unwrap();
        assert!(p.restore_shim);
        assert_eq!(p.placement, Placement::Legacy);
        // Shim is back in the slot, we moved beside it, poison is gone.
        assert_eq!(read(&dir.join(HOOK_NAME)), PREK_SHIM);
        assert_eq!(read(&dir.join(LEGACY_NAME)), HOOK_SCRIPT);
        assert!(
            !dir.join(CHAINED_NAME).exists(),
            "the parked shim must not be left where the allow path execs it"
        );
        #[cfg(unix)]
        assert!(is_executable(&dir.join(HOOK_NAME)));

        // And it stays repaired.
        assert_eq!(install(&dir).unwrap().action, InstallAction::AlreadyCurrent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_repair_keeps_a_user_hook_the_framework_had_displaced() {
        // Broken shape *and* a real user hook at `.legacy`: the shim gets its
        // slot, the user hook is preserved in the chain, we sit between them.
        let dir = scratch("repair2");
        std::fs::write(dir.join(HOOK_NAME), HOOK_SCRIPT).unwrap();
        std::fs::write(dir.join(CHAINED_NAME), PREK_SHIM).unwrap();
        std::fs::write(dir.join(LEGACY_NAME), USER_HOOK).unwrap();

        let p = install(&dir).unwrap();
        assert!(p.restore_shim);
        assert_eq!(p.action, InstallAction::Chained);
        assert_eq!(read(&dir.join(HOOK_NAME)), PREK_SHIM);
        assert_eq!(read(&dir.join(LEGACY_NAME)), HOOK_SCRIPT);
        assert_eq!(read(&dir.join(CHAINED_NAME)), USER_HOOK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_errors_on_a_missing_hooks_dir() {
        let missing = std::env::temp_dir().join(format!("tg-mg-nope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(install(&missing).is_err());
    }
}
