# Stabilize sidebar internals for release

## Why

A full user-perspective audit of the sidebar (pre-release) found the feature
surface largely complete but the internals carrying debt that shows through:
the spec still described tab-nesting the code deliberately dropped; the
default display sort reshuffled rows on its own (alien to users coming from
VS Code-style explorers); persisted view state accumulated tombstones and
orphans; the chrome mixed capability-routed glyphs with hardcoded emoji
(astral-plane, double-width, font-drift-prone — the U+26C1 disk-badge bug
class); and rail mode erased row identity for everything but worktrees.

## What Changes

- **Spec truth:** drop the stale "worktrees nest their tabs" requirement
  (tabs live in the tabbar only — enforced by
  `sidebar.rs::tabs_never_appear_in_the_sidebar`); add the dormant/live
  render-parity clause the code already guarantees.
- **Manual default sort:** the display sort defaults to Manual — an explorer
  that never reorders itself; Attention remains one pick away (sort menu) and
  urgency still surfaces via dots, the needs-you chip, and `Alt a`.
- **Tombstone-free view state:** unpin/uncollapse delete their `ui_state`
  keys instead of writing `"0"`; `load` sweeps legacy tombstones and rewrites
  the legacy `activity` sort value to its canonical spelling; removing a
  workspace/worktree/folder prunes its `collapse:`/`pin:` keys by prefix
  (new `del_ui_state_prefix` store method).
- **Glyph coherence:** every sidebar glyph routes through the capability
  glyph table (`GlyphSet` gains carets, tree connectors, cursor bar, chevron,
  folder/dir/host markers, MQ flag/half-dot, env quotes — all BMP width-1
  with ASCII fallbacks); the 📁📂💻🌐🚀 emoji are gone; the merge-queue
  status vocabulary is one shared `MqStatus::glyph` consumed by both the
  sidebar chip and the panel section.
- **Rail identity:** rail mode shows a bold initial for workspaces and
  dot+initial for terminals; empty hints vanish.
- **TERMINALS visibility:** `[ui] sidebar_terminals_section = "always" |
"nonempty"` (default `always`).
- **SidebarRow hygiene:** dead `agent` field removed; stale
  `#[allow(dead_code)]`s dropped; a `SidebarRow::base` constructor replaces
  eight ~30-field literals.

## Impact

- **tasks.md:** hardens group **B** (workspace bar/tree, items 13–28) for
  release; no new roadmap items.
- **Capabilities:** `sidebar` (REMOVED tab-nesting; MODIFIED tree model,
  creation-order and attention-sort requirements; ADDED view-state hygiene,
  glyph degradation, terminals-section visibility, rail identity).
- **Ratchet extractions** (mechanical): `sidebar_view.rs` (the whole sidebar
  layout/paint block out of chrome.rs), `handlers/sidebar_keys.rs` +
  `handlers/sidebar_persist.rs` (SidebarState interaction + persistence out
  of run.rs). All pinned files net-shrank.
- **No DB schema change.** `del_ui_state_prefix` is a new query on the
  existing `ui_state` table; legacy values migrate by lazy rewrite on load.
