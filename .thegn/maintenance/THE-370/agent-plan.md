# THE-370 + THE-374 plan

Audit on main: THE-370 sites (merge_lifecycle worktree_is_dirty, merge_sweep) are already
fixed (cleanup::Verified::probe, KeptDirty/kept_changed/Refused). integrate.rs is_dirty
sites already propagate errors with context. Still fail-open:

- thegn-svc git/undo.rs: `is_dirty().unwrap_or(false)` before `reset --hard`.
- thegn-svc git/stage.rs discard_file: any cat-file error => `git rm -f`.
- integrate.rs: boolean `git_ok(merge-base --is-ancestor)` at fold filter and up-to-date probe.

Scope: fix those three with tri-state semantics (Err on Unknown). Tests: corrupt index
(undo), missing tree object (discard), ancestor helper unit tests.
Out of scope: THE-368 typed exec, THE-372 generation fence, bridge/SSH typed contract, timeouts.

Tests: util::git_is_ancestor_is_tri_state; undo_apply_refuses_when_dirtiness_is_unknown;
discard_file_refuses_when_head_is_unreadable; discard_file_removes_a_staged_new_file.
Left, audited: rebase.rs reword `diff --cached --quiet` unwrap_or(false) (spawn failure only
changes which staged content joins an amend, no deletion); integrate.rs test-helper unwrap_or(false).
