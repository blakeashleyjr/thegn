# THE-11 architect revision 1

## Findings

### 1. Global occupants are not reachable from the product UI

`crates/thegn-core/src/config_drawer.rs:75-78,161-164` makes a global tool
available only to the `Global` scope. However, every product entry point passes
`DrawerScope::Worktree`: `crates/thegn-host/src/palette.rs:780-801`,
`crates/thegn-host/src/run.rs:19517-19528` (cycle), and
`crates/thegn-host/src/run.rs:19544-19550` (picker selection). Consequently a
`drawer_scope = "global"` entry is absent from the picker and cycle list and
there is no path that writes its global state slot or opens its global pane.

Fix expected: define the active-scope effective registry so it includes the
built-in files occupant plus both worktree and global configured occupants;
selection/cycle must persist a worktree choice under the active worktree key
and a global choice under `GLOBAL_SCOPE_KEY`, then exercise this with focused
policy/runtime tests. Switching worktrees must preserve the single global pane
and still give an open destination worktree occupant precedence.

### 2. The old drawer lifecycle remains live beside the new runtime

The new `DrawerRuntime` is reconciled only once at startup at
`crates/thegn-host/src/run.rs:6690`. The loop still owns a second legacy
`DrawerPool`/pending state at `crates/thegn-host/src/run.rs:6631-6637`; worktree
activation and tab switching continue to call `sync_drawer_persistence` through
the old arguments (`run.rs:6905-6907` and `run.rs:13562-13570`), the old
file-reveal path still calls `request_spawn` and `drawer_show_pending`
(`run.rs:18079-18095`), and the old result drain remains active
(`run.rs:9129-9193`). The old prewarm branch can also resolve/spawn yazi while
a configured occupant is selected (`run.rs:8154-8164`).

This violates the one-pool/one-transition invariant: after a switch the new
runtime can retain a visible pane that the old path has already stashed, and a
configured drawer selection can create an unrelated legacy yazi pane. It also
means switch reconciliation, process-exit cleanup, persistence, and geometry
can disagree.

Fix expected: remove the legacy drawer channel/pool/pending path and route all
activation, tab/worktree switching, file reveal, prewarm, exit, and geometry
updates through one `DrawerRuntime` boundary; invoke runtime reconciliation on
every active-directory change, not just startup. Add a focused regression test
covering a visible drawer across a worktree switch and a configured occupant
selection with `drawer.prewarm = true`.

### 3. The OpenSpec change is not synchronized with the implemented design

`openspec/changes/add-drawer-tool-registry/{proposal.md,design.md,tasks.md}`
still require `[[drawer.tools]]`, inline-command/tool-reference XOR, and the
old persistence/security model, while the implementation and architect design
deliberately use metadata on the existing `[[tools]]` catalog. The task file
also leaves the implementation tasks unchecked. `openspec validate --all
--strict` passes syntax/schema validation, but it does not reconcile these
contradictory requirements.

Fix expected: sync the in-flight change and its drawer delta spec with the
accepted THE-11 design (metadata on `[[tools]]`, process-local global panes,
no repo-local command registry, and the deferred snapshot follow-up), mark or
rewrite the task statuses accurately, and rerun strict validation.

## Scope

These are semantic integration gaps, not mechanical gate failures. The
architect fixed only the invalid test channel constant and the missing default
indicator wiring; see commits `62b07ecd` and `3ccbc35c`.
