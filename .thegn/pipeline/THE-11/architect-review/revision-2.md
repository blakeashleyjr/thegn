# THE-11 architect revision 2

## Finding 1 — global drawer state is persisted and restored across restart

The accepted design and the synced OpenSpec require a global drawer to be a
process-local local PTY: it may be reused across in-process worktree switches,
but it must not survive detach/restart or be restored from the state database.

The implementation violates that contract:

- `crates/thegn-host/src/drawer_state.rs:81-83` initializes the cache by loading
  every scope, including the fixed `global` key, from disk.
- `crates/thegn-host/src/drawer_state.rs:101-125` writes every scope through
  the same persistent path.
- `crates/thegn-host/src/drawer_state.rs:527-532` uses the loaded global value
  as a startup target, and `DrawerRuntime::reconcile` consequently recreates
  it on the next process invocation.
- `crates/thegn-host/src/drawer_state.rs:630-636` persists an explicit global
  selection through that path.

Fix expected:

1. Keep the global desired occupant in process memory only; do not load or
   write the global slot in the drawer state directory. Worktree state may
   retain the existing write-through behavior.
2. Preserve reuse of the in-memory global PTY during worktree switches and
   preserve worktree-over-global precedence.
3. Add a regression test that selects a global occupant, verifies it is
   available during an in-process switch, then initializes a fresh cache/state
   boundary and verifies no global target is restored. The test must also
   ensure worktree persistence remains intact.

The previous revision's lifecycle and OpenSpec findings are resolved. The
review made one additional small correction in commit `48e12e84`: pooled
drawer exits no longer steal focus/reclaim visible geometry, and cold picker /
cycle transitions correctly retain pending drawer focus.
