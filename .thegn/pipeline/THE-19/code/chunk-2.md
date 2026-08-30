---
files:
  - crates/thegn-host/src/hook_run.rs
  - crates/thegn-host/src/worktree_lifecycle.rs
  - crates/thegn-host/src/main.rs
  - crates/thegn-host/src/wizard.rs
  - crates/thegn-host/src/cmd/wt.rs
  - crates/thegn-host/src/handlers/tracker.rs
  - crates/thegn-host/src/handlers/worktree_delete.rs
  - crates/thegn-host/src/handlers/workspace_remove.rs
  - crates/thegn-host/src/run.rs
  - crates/thegn-host/src/merge_lifecycle.rs
  - crates/thegn-host/src/merge_sweep.rs
overlaps: []
after: [chunk-1]
---

# Chunk 2 — host runner and every lifecycle call site

## Scope and approach

Add `hook_run.rs` for the host-only subprocess seam and
`worktree_lifecycle.rs` for shared create/destroy/session orchestration. Every
hook command must execute off the event loop as `sh -lc`, through
`thegn_core::sandbox_cpucap::wrap_background_argv`, with null stdin, captured
output, timeout/process-group cleanup, curated environment, per-worktree log,
and explicit result reporting. Use the existing `NotifyState::record`/toast
funnel and `TerminalWaker` refresh path; never discard a hook result.

Route the wizard, CLI `wt new`/`--from-issue`/batched creation, issue-panel
dispatch, and daemon `worktrees.create` through one create lifecycle. Preserve
lazy CLI/control sandbox provisioning while placing post-create after
registration and any provisioning actually performed. Make CLI create wait
for its worker before process exit; keep UI post-create asynchronous, with
`wait=true` gating only the first pane.

Route sidebar delete, destructive workspace delete, CLI `wt rm`, and merge
queue/sweep reclaim through one destroy lifecycle. User CLI/sidebar deletion
must stop on blocking `pre_destroy` and use the existing `--force` or delete
confirmation “delete anyway” path. Workspace keep-files is not destruction;
destructive bulk removal uses its explicit destructive confirmation as force
authorization and reports per-path failures. Merge reclaim and internal
rollback use unattended/force cleanup and warn-and-continue. Run
`post_destroy` only after actual removal, from repo root. Do not invoke hooks
from `reconcile_removed_tabs`, which only reconciles tabs after an already
completed physical removal.

Add runtime latches for one `session_start` per worktree session and one
non-blocking `session_end` after the last pane/tab closes. Keep `init_script`
per-pane. Add focused host unit tests around the runner command/env/timeout
contract and lifecycle state transitions; subprocess smoke coverage may use a
marker but must not be run as e2e in this issue.

## Dependencies and overlap

No file overlap with chunks 1 or 3. This chunk depends on chunk 1’s public
`thegn_core::hooks` model and config fields, so run serially after chunk 1.
Chunk 3 owns docs and verification surfaces only; it does not edit any host
file listed here.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host hook_run`
- `cargo nextest run -p thegn-host worktree_lifecycle`
- `cargo nextest run -p thegn-host delete_groups`
- `cargo nextest run -p thegn-host merge_lifecycle`

Do not run a full-workspace build, `just test`, `just ci`, or e2e.

## Done criteria

- All paths in the frontmatter are the only paths touched by this coder.
- No hook subprocess is started on the event loop; every one uses the shared
  background CPU slice wrapper and wakes completion through existing channels.
- Failures, timeouts, trust-pending entries, force skips, and unattended
  warnings are observable; no primary hook result is swallowed.
- All four create paths and all physical-removal paths use the same policy and
  ordering, with no pipeline-specific fork.
- Session hooks are once-per-session and never gate pane spawn/close.
- Scoped tests above pass.
- Commit exactly with subject: `feat(the-19): run lifecycle hooks off-loop`
