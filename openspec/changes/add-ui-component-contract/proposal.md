# UI component contract — one way to build a chrome element, native or plugin

Linear: THE-43

## Why

THE-43 asks for "modular and standardized UI elements that inherit the same
tools and rules, open to unlimited customizations and configurations, either
native or plugin". The audit behind this proposal found that thegn has **no
widget contract today** — it has three good partial layers and roughly eight
hand-rolled zone implementations that each re-derive layout, hit spans, key
tables and hint strips:

- **Content**: `seg.rs` (`Line`/`Seg`/`Tok`) is used by masthead, statusbar
  and panel — but `draw_center_tabs` and `draw_pin_chips` bypass it with raw
  `draw_text` + manual x math. `panel/sections/mod.rs::PanelRow`
  (`line` + optional `hit` + optional `bg`) is the best declarative row model
  in the tree; nothing else uses it.
- **Hit targets**: there is no registry and no shared shape — five
  incompatible span signatures (`Vec<(BarItemId, Rect)>`,
  `Vec<(Range<usize>, PanelTab)>`, `(x, w, idx)` triples, row indices,
  `RowHit`), resolved by a ~450-line `else if` chain in `run.rs`. Pin chips
  have **no hit-testing at all**. The statusbar carries a module docstring
  (`statusbar_fit.rs`) recording the bug class this breeds: a hit table built
  from a different list than the painter used ⇒ clicks land on the wrong
  badge.
- **Keys**: three disjoint declaration shapes — `ActionSpec` (no zone field;
  `keymap_merge` hardcodes `zone: "global"`), `sidebar_keytable::SIDEBAR_KEYS`
  (the only table that actually drives dispatch), and
  `panel::section_keys::SectionKey` (hints only; dispatch is a giant `run.rs`
  match guarded by a _source-text drift test_ that reads the match as a
  string).
- **Theming**: colors have a real token vocabulary (`Palette` slots → `S` →
  `Tok`, quantized once in `wire.rs`), but glyphs have only a bare `GlyphSet`
  struct with no token enum — which is why the glyph-literal ratchet carries
  **31 files** of debt against the color ratchet's 9.
- **Plugins**: `plugin_api::View` is one flat line of spans with 5 style
  roles, painted only into the statusbar's leftover gap. `SidebarTab` and
  `Theme` extension points are declared wire vocabulary with nothing behind
  them. A plugin cannot contribute a panel section, cannot be placed by the
  user, and cannot express anything richer than one line.
- **Placement config**: `[bars]` is the one good model — ordered id lists per
  slot, unknown ids warn+skip — but statusbar _badges_ are appended in
  hardcoded source order and are not configurable at all, and plugin segments
  cannot be placed among them.

The pattern that provably works in this codebase, and that this change
elevates to a contract, already exists in the best zones: **one builder
produces both the painted output and the hit table** (`panel/frame.rs`,
`statusbar_fit.rs`), **one table feeds dispatch and every hint surface**
(`sidebar_keytable.rs`), **one popup contract with 24 users** (`layer.rs`),
and **one real trait for embedded apps** (`tg-kit::AppTile`).

## What Changes

- **A new `ui-components` capability**: the element contract. A chrome element
  declares — through one shape — a stable id, its zone, its content (rows of
  the `Line`/`Tok` model, generalizing `PanelRow`), its hit spans (emitted by
  the same build that paints, as `(rect, element action)` in one shared
  signature), its zone-local keys (one table shape that feeds dispatch, hints,
  which-key and `keymap_merge::collect` — replacing the three shapes and the
  source-text drift test), and its placement id. Migration is **ratcheted,
  not big-bang**: a new shrink-only allowlist pins the legacy draw sites, per
  house style (`architecture-gates`).
- **Placement/visibility is config, one grammar** — the `[bars]` model
  (ordered id lists, omission hides, unknown warns) becomes the placement
  grammar everywhere it applies: statusbar **badges** get ids and join the
  `[bars]` lists; `[panel] sections` (which already resolves an ordered list)
  is specced as the same grammar; plugin elements are placeable by their
  `plugin:<id>:<contribution>` id in the same lists. "Unlimited
  customization" = any element, native or plugin, ordered/hidden/shown by
  config — never by code edits, and always inside the ratchet rules (tokens
  only, caps chokepoints, no new literals).
- **Plugins inherit the same tools and rules** — `plugin-api` v0.3
  (additive, snapshot + `API_VERSION` bump per the existing spec): `View`
  grows multi-row content and optional per-span theme-slot naming (roles stay
  the compat path); a `PanelSection` contribution joins `StatusBarSegment` as
  the second _wired_ rendering surface, rendered through the same element
  path with the existing `SurfaceCache` budget/degrade machinery; activating
  a plugin row sends the plugin an `on_event`, never a host action.
- **Glyphs become tokens like colors are slots** — a glyph token vocabulary
  beside `Tok`'s color tokens, resolved once through `caps::active_glyphs()`,
  so element content is expressible entirely in tokens and the 31-file glyph
  ratchet can start burning down.
- **Out of scope, deliberately**: rewriting the ~20 overlays (they already
  share `layer.rs`; they migrate opportunistically under the ratchet), the
  sidebar drag state machine (`handlers/sidebar_mouse.rs` stays; only row hit
  shapes unify), `AppTile` (already a real contract), and any change to the
  render-decision invariants (`Skip`/`Panes`/`Full` are untouched — elements
  are chrome; a dirty element is a `Full` frame exactly as today).

## Impact

- Roadmap: **L 159** (composable widget config), **L 160** (click-through to
  detail views — the hit-span contract is its substrate), **P 201**
  (status-bar widget plugins — generalized to elements), **P 202/168**
  (palette plugins already exist; unaffected), **P 209** (plugin config
  surface — placement ids). Cross-cited: **K 142** (larger touch hit-targets
  becomes tractable once hit spans are data).
- Specs: **new `ui-components`**; ADDED requirements in `theming` (glyph
  tokens), `keybindings` (one zone key-table shape), `panel` (placement
  grammar + plugin sections), `plugin-api` (v0.3 view rows + PanelSection),
  `plugin-runtime` (rendering + placement of plugin elements). `rendering`'s
  existing requirements are intentionally untouched.
- In-flight changes reconciled: `add-sidebar-visual-hierarchy` and
  `stabilize-sidebar-internals` (sidebar internals — this change unifies only
  the sidebar's _hit shape_, not its renderer), `add-theme-builder-overlay` /
  `add-theme-contrast-contract` (theme values; this change is about the token
  _vocabulary_, complementary), `add-drawer-tool-registry` (drawer occupants
  are panes, not chrome elements — no overlap), `add-multiplexer-parity`
  (pane geometry, no overlap), `define-gui-frontend-lane` (THE-40 — names
  this contract as the future serializable chrome view-model; this change
  does not scope any GUI).
- Capability catalog: **no new externally invokable operation** — plugin
  registration rides the existing negotiated `register`/`update` path and
  `host.call` scope checks; nothing new to project across surfaces.
- Config: new ids in existing `[bars]` lists (badges, plugin ids) and
  `[panel] sections` (plugin ids) — documented in
  `config/config.toml.example`; no new config section.
- Help: the contract ties element zones to the existing `zone:*` /
  `panel:*` context ratchets; new plugin surfaces get a
  `docs/help/plugins.md` update in the same change that wires them.

## Phased delivery

This change is **phased — each phase lands alone** (see `tasks.md`). The
migration is ratcheted, not big-bang, precisely so a partial is a coherent,
shippable increment rather than a half-rewrite.

- **Phase 1 (landed): the contract substrate + first migration.** The element
  contract module (`crates/thegn-host/src/element/` — `ElementId`, `Zone`,
  `ElementRow`, `HitSpan`/`ElementAction`, `ElementBuild`, and the `ChipRow`
  builder whose one pass emits rows **and** hit spans), the glyph token
  vocabulary (`thegn_core::termcaps::Glyph` + `caps::glyph`), the builder-rule
  test that makes the drift bug unrepresentable, the shrink-only
  `test/element-ratchet.txt` wired into `just lint`, and one live-zone
  migration: **pin chips**, which were entirely click-dead — they now build
  through the contract and a click summons the pin, with the painted frame
  preserved. This is the foundational layer the sidebar/theming/drawer work
  builds on, and it closes the pins' hit-testing gap. The **plugin API v0.3**
  additive bump also landed here (wire types only): `View.rows`, `Span.slot`
  (theme-slot name, `StyleRole` fallback), `ExtensionPoint::PanelSection`,
  `API_VERSION` → 0.3.0 with a regenerated schema snapshot and v0.2-compat
  decode tests — so a v0.2 plugin and an older host keep working. The runtime
  that renders/negotiates plugin panel sections is Phase 2.
- **Phase 2+ (tracked in `tasks.md`):** the remaining hit unification (statusbar
  item spans + `center_tab_hit`, then the `draw_center_tabs` paint — the only
  frame-touching migration, which needs an e2e re-record), the one zone
  key-table shape (with the `run.rs` dispatch-match → table-lookup swap and the
  deletion of the source-text drift test), the placement grammar for badges +
  plugin ids, and the plugin API v0.3 + runtime `PanelSection` surface. Each is
  additive on the Phase 1 substrate; none is blocked by another.

The one contract, the glyph tokens, and the ratchet are the load-bearing
decisions; the per-zone migrations adopt them incrementally under the ratchet,
exactly as the tree's own history says works.

## Non-goals

- **A retained-mode widget framework** — no layout engine, no view diffing,
  no virtual DOM. Elements are declarative _data_ handed to the existing
  compositor; `render_plan` stays the only render decision.
- **Rebindable zone keys** — zone tables gain one shape and honest zone
  attribution in `thegn keys list`; making them user-rebindable is a
  follow-up once the shape exists.
- **Plugin sidebar tabs / theme plugins** — `SidebarTab` and `Theme` stay
  declared-but-unsupported wire vocabulary; wiring them is future work on
  top of this contract.
- **Touching `render_plan`, damage channels, or the 0%-idle loop.**
