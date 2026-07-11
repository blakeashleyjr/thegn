# Worktree ordering: stable creation-order default + manual reorder

**Date:** 2026-06-12
**Status:** Approved (design)

## Problem

Branch worktrees under a repo in the sidebar shuffle order between launches and
refreshes. The root cause is non-deterministic ordering:

- `db.worktrees()` (`crates/thegn-core/src/db.rs:709`) has **no `ORDER BY`**,
  so SQLite returns rows in an unspecified order.
- The unloaded-workspace branch in `build_rows`
  (`crates/thegn-host/src/sidebar.rs:309`) lists those rows in raw DB order,
  bypassing `sort_groups` entirely.
- The resurrect adopt loop (`crates/thegn-host/src/session.rs:180`) appends
  worktrees not yet in `tab_groups` in that same arbitrary order.

Loaded worktrees inside an open workspace are already stable (default
`SortMode::Name`, alphabetical, `home` first), but that is not the order the
user wants.

## Goal

1. **Default ordering = creation order**, stable across launches/refreshes.
2. **Manual reorder** that is easy to drive and persists, working uniformly for
   loaded _and_ unloaded workspaces.

## Decisions (from brainstorming)

- Reorder trigger: **Shift+Alt+↑/↓** (mirrors the existing `Alt+↑/↓` worktree
  navigation).
- When a computed sort (Name/Recent/Activity) is active and a move happens, the
  move **switches the workspace to manual order** and sticks.
- `home` is a fixed top anchor: it does not move and nothing moves above it.
- Top-level workspace MRU ordering (`workspaces ORDER BY last_active DESC`) is
  **not** in scope — that was not the reported symptom.

## Design

### 1. Data model

The `worktrees` table is the only table with a row per worktree (loaded or not),
so it owns the durable order. Add one column:

```sql
ALTER TABLE worktrees ADD COLUMN position INTEGER;
```

This follows the existing additive-migration pattern (`db.rs:105`, run
unconditionally with the error ignored). Bump `SCHEMA_VERSION` 7 → 8.

`position` is the **single source of truth** for sidebar order.

- **Backfill on open:** any `NULL` positions are assigned deterministically by
  `ORDER BY created_at, worktree`, giving existing users a stable creation-order
  snapshot on first launch after upgrade.
- **New worktrees** are inserted with
  `position = COALESCE(MAX(position), -1) + 1` (append at the bottom) so newly
  created worktrees land last, in creation order.

### 2. Default ordering = creation order

- `db.worktrees()` gains `ORDER BY position`. This single change removes all
  three shuffle sources (unloaded listing, resurrect adopt loop, and any other
  consumer of `worktrees()`).
- A new `SortMode::Manual` becomes the `#[default]` (replacing `Name`). Manual
  mode **trusts the underlying order** — it does not re-sort. It keeps `home`
  pinned first and otherwise preserves the order it is handed:
  - loaded workspaces → `session.worktrees` group order,
  - unloaded workspaces → `position` order (already applied by the DB query).
- `Name` / `Recent` / `Activity` remain as opt-in modes via the existing
  sort-cycle keybind. The mode is persisted in `ui_state` as today; the new
  stored value is `"manual"`. `SortMode::from_str` maps unknown/`"manual"` →
  `Manual` (so `Manual` is the safe default for any unrecognized value).
- At resurrect, after the adopt loop, `session.worktrees` is sorted by
  `position` (joined on `tab_name` / worktree path), so the in-memory order
  matches the DB and worktree navigation (`Alt+↑/↓`) agrees with the sidebar.

### 3. Manual reorder — Shift+Alt+↑/↓

- Two new keymap actions: `MoveWorktreeUp` / `MoveWorktreeDown`, bound to
  `Shift Alt Up` / `Shift Alt Down`.
- The handler moves the **focused** worktree group one slot within its
  workspace's sibling list:
  - swaps `position` with the adjacent sibling in the DB (a small renumber if
    positions collide), and
  - swaps the entries in `session.worktrees` so the live view updates instantly.
- `home` is a fixed anchor: a worktree cannot move above `home`, and `home`
  itself does not move.
- If a computed sort (`Name`/`Recent`/`Activity`) is active when a move happens,
  the handler first flips the workspace's `sort_mode` back to `Manual`, then
  applies the move — so the move always sticks.
- Boundary moves (already first / already last within siblings) are no-ops.

### 4. Persistence & event-loop fit

- A move mutates `session.worktrees` (in-memory → instant re-render via the
  normal dirty/redraw path) and writes `position` to the DB.
- DB writes stay **off the event loop**, following the existing layout-persist
  seam (`Session::persist` / the debounced persist path). No blocking DB/git on
  the loop — the ~0% idle / event-driven invariant is preserved; no polling
  timeout is added.
- `tab_groups.ordinal` continues to record layout but is **no longer the order
  authority**; resurrect orders by `position`. To avoid drift, `persist()`
  writes each group's `position` (from its `session.worktrees` index) alongside
  the existing `ordinal` write.

### 5. Testing

- **Core (`thegn-core`, 95% line gate):**
  - migration backfill assigns deterministic positions by `created_at, worktree`;
  - `worktrees()` returns rows in `position` order;
  - new-worktree insert appends (`MAX(position)+1`);
  - a `move`/swap helper renumbers correctly, including collision and boundary
    cases.
- **Host unit tests** (extend existing `sidebar.rs` `build_rows` tests):
  - `SortMode::Manual` preserves order with `home` first for both loaded and
    unloaded workspaces;
  - the reorder handler swaps adjacent groups and is a no-op at edges;
  - a move under an active computed sort flips the mode to `Manual`.
- **Smoke:** covered by the existing hermetic CLI path; no new subprocess seam.

## Scope / non-goals

- No drag-and-drop (keybind only; palette wiring is incidental, not required).
- No cross-workspace moves.
- No change to top-level workspace MRU ordering.
- Computed sort modes are retained, not removed.

## Affected files (anticipated)

- `crates/thegn-core/src/db.rs` — migration (column + `SCHEMA_VERSION` 8 +
  backfill), `worktrees()` `ORDER BY position`, `put_worktree` position
  assignment, a position-swap/move helper.
- `crates/thegn-host/src/sidebar.rs` — `SortMode::Manual` (new default),
  `sort_groups` handling, unloaded-branch ordering.
- `crates/thegn-host/src/session.rs` — resurrect sorts by `position`;
  `persist()` writes `position`.
- `crates/thegn-host/src/keymap.rs` — `MoveWorktreeUp`/`MoveWorktreeDown` +
  `Shift Alt Up`/`Shift Alt Down` chords.
- `crates/thegn-host/src/run.rs` — action handlers wiring the move + persist.
- `config/config.toml.example` — document the new keybinds / `manual` sort mode.
