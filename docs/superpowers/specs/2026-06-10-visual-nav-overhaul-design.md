# Visual & Navigational Overhaul — Design

Date: 2026-06-10 · Status: approved (option C, full model refactor)

## Problem

1. **Tab model is wrong.** The tabbar renders every session tab (one per
   worktree) — `run.rs:321` feeds `session.tabs` straight into
   `FrameModel.tabs`. Tabs must live _within_ a worktree; the tabbar shows only
   the active worktree's tabs. The `·N` page-suffix string parsing
   (`sidebar.rs split_tab/split_page`) is a workaround for the missing model.
2. **Navigation is broken/unergonomic.** No unified focus graph; sidebar/panel
   focus are disconnected booleans; focus border was never rendered
   (`chrome.rs:975`).
3. **No focus visuals.** No pane borders at all; `theme::BORDER` is unused;
   the `◂` markers are easy to miss.
4. **Layout corrupts over time** — duplicate tabbars, doubled panel headers:
   stale cells from a previous geometry survive damage-tracked rendering.

## Decisions (user-approved)

- **Ctrl+direction** (arrows _and_ h/j/k/l) = one fully spatial focus graph:
  sidebar ← center panes → panel; up/down moves within the zone
  (sidebar rows / stacked panes / panel widgets).
- **Ctrl+g** toggles a keybind lock: while locked, every key except Ctrl+g
  passes through to the focused pane. Statusbar shows `⌁ LOCKED` (amber).
- **Alt+Left/Right** = prev/next tab within the active worktree.
  **Alt+Up/Down** = prev/next worktree (each group restores its own active
  tab). Alt+h/j/k/l pane-focus binds are removed (Ctrl owns focus, Alt owns
  tabs/worktrees/launchers). Vim-normal/emacs modes unchanged.
- **Palette moves Ctrl+k → Ctrl+Space** (Ctrl+k is now focus-up).
- **Borders on all panes, always**: 1-cell shared gutters in the center area;
  light-grey lines by default, focused pane's ring in a configurable light
  blue (`#9bd1ff` default). Sidebar/panel edges get a 1-col separator that
  turns focus-blue when that zone owns focus. Active tab chip and active
  worktree sidebar row get the focus-tint pill highlight.
- **Fully configurable palette**: `theme.rs` constants become a `Palette`
  struct; `[theme] focus_border` + `[theme.colors]` overrides for every
  surface/text color; live-reload via the existing config fs-watch.

## Data model (host `session.rs`)

```rust
pub struct Session { pub id: String, pub worktrees: Vec<WorktreeGroup>, pub active: usize }
pub struct WorktreeGroup {
    pub name: String,      // "app/feat"
    pub kind: GroupKind,   // Home | Branch
    pub path: String,      // worktree dir
    pub tabs: Vec<Tab>,    // ≥ 1 always
    pub active_tab: usize,
}
pub struct Tab { pub title: String, pub center: CenterTree, pub focused_pane: u32 }
```

`next_tab`/`prev_tab` cycle within the active group; `next_worktree`/
`prev_worktree` move between groups. No `worktree` field on Tab; no `·N`
parsing anywhere.

## DB schema v6 (core `db.rs`)

Replace `tab_layout` with:

```sql
CREATE TABLE tab_groups (
  session_name TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
  worktree TEXT NOT NULL, ordinal INTEGER NOT NULL, active_tab INTEGER NOT NULL,
  PRIMARY KEY (session_name, name));
CREATE TABLE group_tabs (
  session_name TEXT NOT NULL, group_name TEXT NOT NULL, ordinal INTEGER NOT NULL,
  title TEXT NOT NULL, pane_tree TEXT NOT NULL, focused_pane INTEGER NOT NULL,
  PRIMARY KEY (session_name, group_name, ordinal));
```

Migration v5→v6 in one transaction: parse legacy `{repo}/{branch}[ ·N]` tab
names into (group, ordinal); `Extra`/`Pinned` rows become single-tab groups;
malformed rows degrade to a single-pane tab; `session_state` active-tab name
maps to (group, tab). On transaction failure the DB stays v5 and the host
boots with a fresh layout. The git-registry `worktrees` table is untouched.

## Focus manager (host `focus.rs`)

```rust
pub enum Zone { Sidebar, Center, Panel }
pub struct FocusState { pub zone: Zone, pub locked: bool }
```

Single source of truth (replaces `sb.focused` / `model.panel_focused`).
Pure router `move_focus(zone, dir, ctx) -> FocusMove` unit-tested over the
zone × direction × visibility matrix:

- Center: `center::neighbor()` first; at edges Left→Sidebar (if visible),
  Right→Panel (if visible), else no-op.
- Sidebar: Up/Down = tree cursor; Right→Center; Left no-op.
- Panel: Up/Down = panel widgets; Left→Center; Right no-op.
- Alt+s / Alt+p remain direct jumps; Esc returns to Center.

Kitty keyboard protocol is enabled on the outer terminal when supported so
Ctrl+h/j/k/l are unambiguous; on legacy terminals those degrade to
passthrough and Ctrl+arrows (always unambiguous) carry the feature.

## Rendering

- **`borders.rs`**: center layout reserves 1-cell shared gutters (outer ring +
  between siblings). Thin box-drawing lines (`│─┌┐└┘├┤┬┴┼`) in
  `palette.border`; focused pane's ring drawn last in `palette.focus_border`.
- **Chrome edges**: 1-col separators sidebar|center and center|panel; the
  focused zone's separator renders in `focus_border`. The `◂` markers are
  removed.
- **Tabbar**: `{worktree name} │ tab chips… [pins right-aligned]`. Active chip:
  focus-color text on `blend(focus_border, 0.16)` pill; inactive DIM on BG1.
- **Sidebar**: active worktree row gets the same focus-tint pill.
- **Statusbar**: gains `⌁ LOCKED` indicator while the keybind lock is on.

## Theme config (core)

```toml
[theme]
accent = "#76eede"
focus_border = "#9bd1ff"
[theme.colors]   # all optional
bg0 = "…" bg1 = "…" panel = "…" panel2 = "…" raise = "…" border = "…"
text = "…" dim = "…" faint = "…" ghost = "…"
```

Default `border` becomes a light grey (`#aab1c4`) per user direction. Bad hex
falls back to defaults; env overrides (`SUPERZEJ_THEME_FOCUS_BORDER`, …) and
`config get/set` keys included. Resolved `Palette` rides in `FrameModel`;
chrome stops referencing `theme::` constants directly.

## Layout-corruption fix (host `run.rs` render path)

Invariant: **no cell from a previous geometry survives a layout change.**

1. Full scratch-surface clear + repaint of every region whenever computed
   geometry differs from the previous frame (geometry changes are rare).
2. Recompute layout from the current terminal size at the top of every render
   pass; on any size mismatch, discard and redraw instead of diffing.
3. `szhost::frame=debug` logs `geometry_changed → full_repaint`.

## Testing

- Core (95% gate): palette/hex parsing, `[theme.colors]` resolution, v6
  migration fixtures (suffixes, Extra/Pinned, malformed JSON, txn failure).
- Host: session group ops + persist/resurrect round-trip on v6; `focus.rs`
  router matrix; `borders.rs` glyph layout on a fixture tree.
- Smoke: seed a v5 DB, boot, assert migrated groups.
- Perf invariants hold: no polling added; borders render in the existing
  damage-tracked pass; full repaint only on geometry change; `just bench`
  before/after.
