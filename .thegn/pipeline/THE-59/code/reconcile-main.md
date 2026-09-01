# THE-59 reconcile — merge current main into the lane

## Files to touch (exact paths)

- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/platform/unix.rs`
- `crates/thegn-host/src/platform/windows.rs`
- `crates/thegn-host/src/run.rs`

## State you are landing in

`git merge main` is ALREADY IN PROGRESS in this worktree and left the five
files above conflicted (7 hunks total). Do not abort or restart the merge.
Resolve the conflicts, then `git commit` the merge.

## Approach

The lane and main both moved; every conflict so far in this drain has been
"both sides added different things in the same place", not a genuine
disagreement. Default to **keeping both sides** unless one side deliberately
DELETED something the other kept — check that with `git log` before dropping
any code.

Named reconcile items:

- `config_validate.rs` carries a comment ladder of marked-enum definitions and
  a **pinned count**. Main has advanced the ladder. Append this lane's entry
  with the next free number and set the count to main's value plus the number
  of marked enums this lane actually adds. `cargo nextest run -p thegn-core
  marked_definition` must pass — it is the gate for this exact mistake.
- `platform/unix.rs` and `platform/windows.rs` are the platform-cfg chokepoint;
  keep both sides' entries and keep the two files symmetrical.
- `run.rs` is the event loop. Both sides add to the same regions; preserve the
  lane's voice wiring AND main's additions. Do not reorder unrelated statements.

## Verification required before you report

- `RUSTC_WRAPPER= just quick thegn-core` and `RUSTC_WRAPPER= just quick thegn-host`
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core marked_definition`
- `RUSTC_WRAPPER= cargo nextest run -p thegn-host voice`
- `git diff --check`

If sccache fails with a read-only cache, re-run with `RUSTC_WRAPPER=` as above;
that is a known sandbox limitation (THE-90), not your bug.
