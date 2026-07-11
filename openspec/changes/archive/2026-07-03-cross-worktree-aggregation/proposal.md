## Why

Attention is scattered across worktrees: CI fails on one branch, another has
uncommitted changes, a grep hit lives in a third — but thegn only ever shows
the _active_ worktree's state. Zed's **multibuffer** (collect results from many
files into one excerpt stream, each excerpt a live window onto its source) is the
transferable UI idea: a read-only, cross-worktree "what needs attention
everywhere" surface.

## What Changes

- Add a pure **aggregation model** in `thegn-core` (`aggregate.rs`): an
  `Excerpt { worktree, worktree_label, kind, file, line, text, detail }`, an
  `ExcerptKind` (CI failure / dirty file / content match), and an `Aggregation`
  that holds excerpts sorted + grouped by worktree, exposes a flattened
  `rows()` view (group-divider rows interleaved with excerpt rows) for
  cursor navigation, a `jump_target(index)` resolving the source worktree of an
  excerpt, and per-kind `summary()` counts. Pure builder functions turn CI runs,
  dirty files, and content matches into excerpts.
- Add a new **panel section** (`panel/sections/across.rs`, `Section::Across` in
  the Work tab) that renders the aggregation across the three panel widths as an
  excerpt stream — each row shows `worktree · file:line · text`, grouped by
  worktree with divider headers (the multibuffer shape).
- **Populate it off-loop during hydration** from the cross-worktree CI cache
  (all worktrees' failing runs) — a query-free, cheap DB read. The model is
  built to also take dirty-file and content-match excerpts as those producers
  are added.

Non-goals (deferred): the one-key "jump to source" keystroke (needs a
`run.rs` PanelMsg handler arm; `run.rs` is at its god-file ratchet ceiling — the
model ships `jump_target()` so wiring it later is a one-liner once room is
banked) and interactive cross-worktree content **search** (needs a query input).
This change ships the model + the read-only CI-failure aggregation surface.

## Capabilities

### New Capabilities

- `cross-worktree-aggregation`: the excerpt-stream model (excerpts, kinds,
  sorted grouping, flattened rows, jump-target resolution, summary) and the
  read-only panel section that renders cross-worktree attention items with
  per-row source labels.

## Impact

- **Code:** new `crates/thegn-core/src/aggregate.rs` (+`lib.rs` export);
  new `crates/thegn-host/src/panel/sections/across.rs`; `panel/mod.rs`
  (`Section::Across` registry + a `PanelData.across: Aggregation` field);
  `panel/sections/mod.rs` (dispatch); `hydrate.rs` (populate from the CI cache
  across `db.worktrees()`). None of the ratcheted god-files (`run.rs`,
  `chrome.rs`, `keymap.rs`) grow.
- **Dependencies:** none new.
- **Invariants:** the model is pure + 95%-coverage-gated (sort/group/rows/
  jump-target/summary fully unit-tested); population runs on `spawn_blocking`
  and pulses the waker like the other hydration producers; rendering is generic
  `PanelRow`s (chrome untouched). No idle-loop or render-plan change.
- **Roadmap (`tasks.md`):** advances **L/AF** (cross-cutting status surfaces)
  and the unified-work theme (**Mine**, group I); reuses the CI cache (**AV**).
