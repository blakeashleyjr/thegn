# Sidebar actions redesign + full mouse parity

## Why

The sidebar audit's user-perspective findings: every power (sort, pin, menu,
filter, marks, bulk close/delete) was keyboard-invisible; `X` (close) and `D`
(delete-from-disk) sat one shift apart with no disambiguation; rename and
branch-from were menu-only; folder creation was reachable only through the
palette; and mouse support stopped at left-click. Users coming from VS Code
expect a right-click menu, double-click, drag-to-organize, F2-rename, and a
Delete key that asks before destroying anything.

## What Changes

- **Unified `d` / `Delete` chooser** replaces `X`/`D`: a row-kind-aware
  disambiguation modal — Close (safe default, keeps branch + files) vs
  Delete (danger arm) vs Cancel; the dirty-tree safety net (shouted names,
  Cancel pre-selected, never skippable) is preserved verbatim. Workspace rows
  get the remove-workspace modal with the SAFE arm pre-selected (and an
  unprompted removal never deletes from disk); folders and terminals get
  their own confirms.
- **New keys:** `r`/F2 rename (worktree branch, folder), `n` new worktree
  here / new terminal (terminals region + EmptyHint Enter), `N` new
  workspace, `b` branch-from-this, `f` move-to-folder / new-folder, `c` copy
  path, `?` help. `s` opens an explicit sort menu (radio, current noted)
  instead of blind cycling.
- **Context menu v2** is the canonical action catalog: grouped entries with
  right-aligned key chips (the menu doubles as key discovery), red danger
  rows, separators; per row kind including folders, terminal hosts and
  terminals. `run_menu_action` and the keyboard share one dispatch
  (`dispatch_sidebar_outcome!`), so the surfaces cannot diverge.
- **Discoverability:** a `?` help card (grouped cheatsheet on the layer
  machinery, any-key dismiss) plus curated statusbar essentials while the
  sidebar owns focus (`↵ open · n new · d delete · m menu · ? help`).
- **Mouse parity:** right-click opens the context menu at the row; the open
  menu takes clicks/wheel; caret-click folds; Ctrl-click marks; double-click
  commits focus to the center (or folds headers); press-drag-release
  reorders worktrees/workspaces and drops onto a folder (file) or the
  workspace header (unfile), with a live insertion rule/target highlight —
  all through the same `build_sidebar` geometry the renderer paints, and all
  drops through the keyboard machinery (sort→Manual flip, home anchoring,
  optimistic folder moves). SGR mouse reporting is now gated on the detected
  terminal capability; every gesture keeps a keyboard twin.
- **Vocabulary:** "Fork worktree" → "Branch from this…", "File into" →
  "Move to folder…", close/delete labels state their disk consequences.

## Impact

- **tasks.md:** completes the discoverability/ergonomics tail of group **B**
  (items 13–28) for release.
- **Capabilities:** `sidebar` (ADDED: chooser, canonical menu, creation/
  rename reachability, sort menu, help overlay, mouse parity + degradation).
- **Files:** `handlers/sidebar_keys.rs` (key surface), `handlers/
sidebar_actions.rs` (folder/terminal action bodies), `handlers/
sidebar_mouse.rs` (gesture state machine), `sidebar_view.rs` (menu v2
  render, hit-testing, drag viz), `sidebar_help.rs`, `menu.rs` constructors;
  run.rs net-shrank (dispatch hoisted into one macro).
- **No DB schema change**; `del_folder`/`del_terminal`/`rename_folder`
  already existed in the store.
