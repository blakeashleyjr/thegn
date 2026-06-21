# Native Git Management — Implementation Plan

**Date**: 2026-06-18  
**Goal**: Full native git management, excellent UI design, intuitive keybinds, complete testing coverage, robust systems.

## Audit Summary

Architecture is 95% complete. Core ops (staging, commits, branches, stash, rebase, patch, undo, bisect, cherry-pick) are fully wired. Graph rendering, staging two-pane view, interactive rebase — all done.

## Remaining Gaps (ordered by impact)

### Gap 1 — Conflict file routing (322)

**Problem**: `Stage::Conflict` files in Changes section: Enter expands inline diff preview (shows conflict markers) but never opens the editor. Hint says "resolve ↵" but routing is broken.  
**Fix**: In `toggle_change_selection`, detect `Stage::Conflict` and instead of expanding the preview, open the file in the user's editor (new pane tab). After editor close, the file gets staged normally. Also: remove `filter(|c| c.stage != Stage::Untracked)` — it's the wrong filter; conflict files need special treatment too.  
**Files**: `run.rs` (toggle_change_selection + changes::Select branch), `panel/sections/changes.rs` (hint text already correct).

### Gap 2 — GitFlow states for Merge / CherryPick / Revert (321)

**Problem**: `GitFlow` only has `None, Rebase, Bisect, Patch, Diffing`. When a merge/cherry-pick/revert hits a conflict, there's no flow-chip rendered in the header and no "m → continue" in the help bar.  
**Fix**:

- Add `GitFlow::Merge { onto: String, conflict: bool }`
- Add `GitFlow::CherryPick { conflict: bool }`
- Add `GitFlow::Revert { conflict: bool }`
- `sync_merge_flow()` — mirrors `sync_rebase_flow()`, reads from `model.panel.merge` on hydration
- `flow_chips()` in `gitfull.rs` — render the new chips
- `context_keys(GitView::Files)` — add "m continue/abort" when a merge/cherry/revert flow is active
- **MergeBanner.total**: on first detection, capture the initial unresolved count so the "X/Y resolved" bar can render.  
  **Files**: `panel/gitui.rs`, `panel/gitfull.rs`, `run.rs` (flow sync on hydration).

### Gap 3 — Blame view (325)

**Problem**: Completely absent. No `GitView::Blame`, no blame data, no render.  
**Implementation**:

- `BlameRow { sha: String, author: String, date: i64, lineno: usize, content: String }` in `panel/mod.rs`
- `blame: Vec<BlameRow>` + `blame_path: Option<String>` in `PanelData`
- `GitView::Blame` enum variant in `panel/gitui.rs`
- `context_keys(GitView::Blame)` — j/k row, Enter open commit, Esc back, q quit
- `GitMsg::Blame` triggered from CommitFiles (`B` key on a file) and Changes (`b` key)
- Hydration: `spawn_blocking(git blame --porcelain <path> [<sha>])` — same async pattern as hunk fetch
- `blame_region()` in `gitfull.rs` — renders per-line blame with author hue, sha prefix, line content
- `GitOp::Blame { path, sha }` is not needed — it's a read, not a write; use a separate channel like `hunk_tx`  
  **Files**: `panel/mod.rs`, `panel/gitui.rs`, `panel/gitfull.rs`, `run.rs` (handler + hydration channel).

### Gap 4 — Push without upstream

**Problem**: Push on a branch with no remote tracking ref errors silently. User has no way to set upstream from the TUI.  
**Fix**:

- In `gitmut.rs`, pattern-match the push error for "no upstream" / "has no upstream" / "set-upstream" text.
- Return a new `GitOpResult::NoUpstream { branch: String }` variant.
- In `run.rs` result handler: show a confirm dialog "No upstream — push to origin/<branch> and set as upstream?". On confirm, enqueue `GitOp::PushSetUpstream { remote: "origin", branch }`.  
  **Files**: `gitmut.rs`, `run.rs`.

### Gap 5 — MergeBanner total conflict tracking

**Problem**: `MergeBanner.total` is always `None` → the "resolved X/Y" progress bar never renders.  
**Fix**: In `hydrate.rs`, when a merge is first detected (MERGE_HEAD exists), count unresolved conflicts and store the initial count. Between hydrations, carry the `total` in `GitUi` state (not in PanelData which is re-derived each time).  
**Files**: `panel/gitui.rs` (new field `merge_conflict_total: Option<usize>`), `hydrate.rs`, `run.rs` (sync total when flow detected).

## Implementation Order

1. **Gap 1** — Conflict routing (small, high-value, clear)
2. **Gap 2** — GitFlow states (medium, foundational for conflict UX)
3. **Gap 5** — MergeBanner total (small, fixes existing render gap)
4. **Gap 4** — Push upstream (small, quality-of-life)
5. **Gap 3** — Blame view (larger, new surface)

## Testing Strategy

- `superzej-core` coverage gate: 95% lines — new core logic needs unit tests.
- Service layer: integration tests using `TestRepo` (existing pattern in `*_test` modules).
- `panel/` unit tests: pure render functions are easy to test in isolation (existing pattern).
- New `GitOpResult::NoUpstream` variant: test in branch.rs.
- Blame parsing: test against known `git blame --porcelain` output.
