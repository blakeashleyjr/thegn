# Folder-aware sidebar ordering

## Why

Folders, per-worktree `position`, keyboard reorder and mouse drag-drop all
shipped, but the ordering layer was **folder-blind**: every path treated a
workspace as one flat run of worktrees.

- `move_worktree_group` validated only "same workspace slug" and "not home";
  it never compared `folder_id`.
- `sidebar_mouse::worktree_run` and `run::sidebar_worktree_order` collected
  every worktree row in a workspace, loose and filed alike.

`build_rows` then re-partitions those rows by folder when it renders, so a
`position` swap across a folder boundary changed nothing visible while still
reordering the _other_ run on the way; the mouse's bounded step-move loop
spun to `max_steps` and bailed, leaving a partial reorder behind. The code
carried the admission in a comment: _"Move Up/Down on the worktree row will
eventually resequence via `swap_worktree_positions`; for now we preserve the
existing sort for visibility."_

Three adjacent gaps: `folders.position` had **no mutator at all** (assigned
MAX+1 at creation, never written again), so folders could not be reordered;
`Ctrl+Alt+↑/↓` on a worktree row moved the _active_ worktree rather than the
row under the cursor, unlike the workspace and terminal arms; and reorder
required a live `RowTarget::Tab`, so a dormant workspace's worktrees could
not be reordered at all.

## What Changes

- **Sibling runs.** A new pure module (`sidebar_order`) partitions a
  workspace into runs keyed by `(workspace_slug, folder_id)` — the loose
  list first, then each folder in header order, mirroring the order
  `build_rows` emits. Reorder is confined to a run.
- **Edge crossing re-files.** Pushing a worktree past the head or tail of
  its run lands it at the end of the previous run / the head of the next
  one, changing `folder_id` with it — so one key both reorders and files.
  `home` stays anchored at the head of the loose run. A **collapsed** folder
  is stepped over rather than entered, so a worktree can't disappear into a
  closed folder.
- **Folders reorder**, by keyboard (cursor on the header) and by dragging
  the header. Their worktrees travel with them, so no worktree position
  changes.
- **Exact-order persistence.** New `set_worktree_order` / `set_folder_order`
  store methods write `position = index` over the caller's sequence, the
  same reasoning that already justifies `set_workspace_order`. This replaces
  the swap-plus-step-loop, so a drop is one transactional write that cannot
  half-apply.
- **Cursor-based worktree moves.** `Ctrl+Alt+↑/↓` moves the row under the
  sidebar cursor, matching the workspace and terminal arms; the active
  worktree remains the target when the sidebar isn't focused.
- **Dormant workspaces reorder**, because the ordering model is keyed on
  worktree paths (present on dormant rows) rather than session slots.
- **Mouse drops land where the rule shows.** `Spot::Reorder` carries the
  destination run, so releasing between two rows _inside_ a folder files and
  positions in one write, and a bottom-half drop stops at the run boundary
  instead of spilling into the next folder. Header drops (file / unfile)
  now also place the row at the end of its new run rather than leaving a
  stale position to decide.
- **Flat mode** keeps folders hidden but no longer risks dissolving them: it
  is a single run per workspace and membership is preserved.

## Impact

- **tasks.md:** extends group **B** item 22 (manual reorder / pin-to-top),
  which shipped before folders existed.
- **Capabilities:** `sidebar` — ADDED requirements for worktree folders
  (previously unspecified in the main spec) and for run-scoped ordering;
  MODIFIED the stable-creation-order requirement to be phrased in runs.
- **Files:** `sidebar_order.rs` (new, pure + unit-tested),
  `handlers/sidebar_reorder.rs`, `handlers/sidebar_mouse.rs`,
  `store/workspace.rs` + `db_workspace.rs`, `run.rs` dispatch,
  `docs/help/sidebar.md`.
- **No DB schema change.** `worktrees.position` (v8), `folders.position`
  (v17) and `worktrees.folder_id` already exist; no `SCHEMA_VERSION` bump.
