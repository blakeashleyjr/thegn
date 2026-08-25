# Design — the UI component contract

## Context

The audit behind this change walked every chrome zone (masthead, statusbar,
tabbar, pin strip, sidebar, panel + sections, overlays/layers, plugin
segments) and found the same three-layer story everywhere: content, hit
targets, and keys — each solved well **once** and then re-derived by hand in
every other zone. The contract does not invent a model; it names the one that
already won inside the tree and ratchets the stragglers toward it:

| Layer         | Best-in-tree today                                                                                                      | Everyone else                                                                                                                        |
| ------------- | ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Content       | `seg.rs` `Line`/`Seg`/`Tok` (masthead, statusbar, panel); `PanelRow` = line + optional bg + optional hit                | `draw_center_tabs`, `draw_pin_chips`: raw `draw_text` + manual x math                                                                |
| Hit targets   | `statusbar_fit::fit` — one pass feeds painter, hit table, ←/→ nav, Enter                                                | five incompatible span shapes; a ~450-line `else if` chain in `run.rs`; pin chips have no hit-testing at all                         |
| Keys          | `sidebar_keytable` — chord + label + tier + dispatch discriminant in one datum, feeding dispatch and every hint surface | `ActionSpec` (no zone field), `panel::section_keys` (hints only; dispatch is a `run.rs` match guarded by a source-_text_ drift test) |
| Theming       | `Palette` slots → `S` → `Tok`, quantized once in `wire.rs`                                                              | glyphs: a bare `GlyphSet` with no token enum — 31 files of glyph-literal ratchet debt vs the color ratchet's 9                       |
| Popups        | `layer.rs` (24 users)                                                                                                   | — (already unified; out of scope)                                                                                                    |
| Embedded apps | `tg-kit::AppTile`                                                                                                       | — (already a real contract; out of scope)                                                                                            |

## The contract

A chrome **element** is _data_, not behavior — a declaration the compositor
consumes, never a trait object with a `paint()` method:

- **`ElementId`** — stable string id (`"badge:ci"`, `"panel:changes"`,
  `"plugin:<plugin>:<contribution>"`). One namespace across native and plugin
  elements, because placement config addresses elements by id.
- **Zone** — which chrome region owns it (masthead, statusbar-left/right,
  tabbar, pins, sidebar, panel, layer). Zone ids are the same strings the help
  system's `zone:*` / `panel:*` context keys use, so the help-context ratchet
  ties an element to its docs page for free.
- **Content** — rows of the existing `Line`/`Seg`/`Tok` model, generalizing
  `PanelRow` (line + optional row `bg` + optional hit). Nothing new to learn:
  the panel's row model becomes the element row model.
- **Hit spans** — `(Rect, ElementAction)` pairs in **one** shared signature,
  emitted by the same build pass that produced the paintable rows. This is
  `statusbar_fit`'s hard-won rule promoted to a contract: a hit table built
  from a different list than the painter used is the recorded bug class
  (clicks landing on the wrong badge), and the only structural fix is that
  painting and hit-emission are one function's two outputs.
- **Keys** — a zone-local key table in the `sidebar_keytable` shape (chord,
  label, hint tier, dispatch discriminant, zone id). One table feeds dispatch,
  the statusbar hint strip, which-key, and `keymap_merge::collect`.
- **Placement id** — the id the placement grammar (below) orders/hides.

### The builder rule

The load-bearing sentence of the whole contract: **one build produces both
the painted output and the interaction tables.** `panel/frame.rs` and
`statusbar_fit.rs` already work this way; `draw_center_tabs` and
`draw_pin_chips` do not, which is why tabs re-derive x math in their hit
path and pin chips are click-dead. The contract makes the good pattern the
only shape a new element can take.

### What an element is not

- Not a retained-mode widget: no layout engine, no diffing, no virtual DOM.
  Elements are rebuilt when their zone composes, exactly like today's chrome.
- Not a render-decision participant: `render_plan::plan` is untouched. A
  dirty element is a `Full` frame exactly as a dirty badge is today; pane
  output never recomposes chrome. No new damage channel, no new wake path.
- Not a second dispatch system: element actions resolve to the same `Action`
  / zone-table dispatch that exists; the contract changes _where the tables
  come from_, not how the loop consumes them.

## Keys: one shape, honest attribution

Three declaration shapes exist today. The contract keeps `ActionSpec` for
global rebindable actions (it is the action registry, already ratcheted) and
collapses the _zone-local_ shapes into one:

- `sidebar_keytable::SIDEBAR_KEYS` is already the target shape — it drives
  dispatch and every hint surface. It stays, re-expressed as the shared type.
- `panel::section_keys::SectionKey` currently declares hints only; dispatch
  is a giant `run.rs` match guarded by `hint_table_matches_dispatch`, a test
  that reads the match arm **as source text**. Migration point: the section
  tables gain dispatch discriminants, the `run.rs` match arms become lookups,
  and the source-text drift test is deleted — the drift it guarded becomes
  unrepresentable.
- `keymap_merge::collect` already folds zone tables with `Source::ZoneTable`;
  it grows honest zone attribution for panel-section keys instead of the
  current gap where only the sidebar's table is first-class. `thegn keys
list` and the generated keybindings help page then show every zone key
  under its real zone.

Rebindability of zone keys is explicitly out of scope (Non-goals): the shape
lands first, the rebind story rides the shape later.

## Glyph tokens

Colors have `Tok` (slot/hue/heat) resolved once per line against the live
palette; glyphs have only `GlyphSet` field reads scattered across 31
ratcheted files. The contract adds the missing half: a glyph token enum in
`thegn_core::termcaps` beside `GlyphSet` (pure data — core-coverage-gated),
resolved through `caps::active_glyphs()` at the same chokepoint colors
quantize. Element content is then expressible entirely in tokens, and every
migrated draw site deletes its `test/glyph-literal-ratchet.txt` line.

## Placement: the `[bars]` grammar everywhere it applies

`[bars]` is the proven model: ordered id lists per slot, omission hides,
unknown ids warn and are skipped (never a hard error — a stale config must
not blank the chrome). The contract names this **the** placement grammar:

- Statusbar **badges** (`BarBadge`) get placement ids and join the `[bars]`
  lists. Today they are appended in hardcoded source order after the
  configurable widgets; afterwards `bottom_right = ["pr", "badge:ci",
"plugin:hello:seg", "status"]` is expressible. Absent badge ids keep
  today's default order appended after the listed items, so existing configs
  render identically (compat default).
- `[panel] sections` already _is_ this grammar (ordered list, omission
  hides); it is specced as such rather than re-derived.
- Plugin elements are placeable by their `plugin:<plugin>:<contribution>` id
  in the same lists. Placement is visibility/order **only** — listing a
  plugin id grants nothing; scopes and negotiation are untouched.
- Priority shedding (`statusbar_fit`) still applies after placement: config
  orders, the fitter sheds. Placement never overrides `KEEP` items.

## Plugins: the same tools and rules

`plugin-api` goes to **v0.3** — additive per the existing versioning
requirement (snapshot + `API_VERSION` bump; every new field defaults):

- `View` grows optional multi-row content (`rows: Vec<Vec<Span>>`; the
  existing `spans` stays as the one-line compat path) and `Span` grows an
  optional theme-slot name resolved against the palette vocabulary, with the
  5 `StyleRole`s as the fallback for unknown/absent names — an older host
  ignores the new fields, an older plugin never sends them.
- `PanelSection` joins `StatusBarSegment` as the second **wired** extension
  point (SidebarTab and Theme stay declared-but-unsupported vocabulary). Its
  contribution renders as an accordion section through the same element path
  native sections use, with the existing `SurfaceCache` budget/degrade
  machinery bounding render cost.
- Activating a plugin row sends the plugin `on_event` (`kind: Action`,
  `payload.id` = contribution id, plus the row id) — the same shape palette
  actions already use. A plugin never names a host action; there is no path
  from plugin content to host dispatch.

Help context: plugin panel sections map to the `panel:plugins` context key →
`docs/help/plugins.md`, updated in the same change that wires the surface
(the help-context ratchet enforces the claim).

## Migration: ratcheted, not big-bang

A new shrink-only allowlist `test/element-ratchet.txt` pins every legacy
draw site that composes chrome outside the element contract (the
`draw_text`-with-manual-x-math class and the hit tables not emitted by their
painter). Same mechanics as every other ratchet (`test/ratchet.sh`): a new
ad-hoc site fails the gate with a pointer to the contract; migrating a site
deletes its line; the file only shrinks. Overlays (24 `layer.rs` users) and
the sidebar renderer migrate opportunistically under the ratchet — they are
pinned, not rewritten here.

## Alternatives considered

- **A `Widget` trait with `paint(&self, …)`** — rejected. Behavior-shaped
  widgets re-invent the render path per element, defeat the degrade-at-the-
  edges chokepoints (each `paint` becomes a literal-smuggling site), and can
  never be serialized — and `define-gui-frontend-lane` names this contract as
  the future serializable chrome view-model, which only data can be.
- **Big-bang migration of all eight zones** — rejected. The tree's own
  history says ratchets work and rewrites regress; `statusbar_fit`'s
  docstring records exactly the bug class a rushed hit-table rewrite breeds.
- **A new config section for placement** — rejected; `[bars]` + `[panel]
sections` already express it. New grammar would mean two ways to hide a
  badge.
- **Making zone keys rebindable in the same change** — deferred. Rebinding
  needs conflict detection against the global keymap per zone; the one-shape
  consolidation is the prerequisite and stands alone.
- **Extending `plugin_api::View` to arbitrary layout (nested boxes)** —
  rejected. Rows of spans match the element row model exactly; layout stays
  the host's job (budget, truncation, shedding), which is what keeps a
  hostile or buggy plugin unable to break composition.

## Render / event-loop impact

- **Damage channel**: none new. Element changes are chrome changes ⇒ `Full`,
  exactly as today; `render_plan::plan` and its exhaustive tests are
  untouched. Pane-only output still ⇒ `Panes`.
- **Wake paths**: none new. Plugin element updates arrive over the existing
  plugin reader-thread channel + `TerminalWaker` pulse; native element
  builds run on the loop at compose time as chrome always has.
- **SQLite**: no schema change; no `user_version` bump.

## Security

- **No new external door.** No new catalog row, socket, route, or token
  kind; plugin registration rides the negotiated `register`/`update` path and
  `host.call` keeps its `required_scope` check. Nothing here changes what a
  plugin may _do_ — only what it may _look like_.
- **Plugin content is untrusted display data.** Spans are text + role/slot
  names; the host resolves them against its own palette and glyph tokens.
  There is no escape-sequence path from a plugin into the terminal — text is
  cell content, styling is tokens, and the wire types reject nothing by
  panicking (junk lines are kept for diagnostics per plugin-runtime).
- **No confused deputy.** A plugin element's activation is delivered _to the
  plugin_ as `on_event`; plugin content cannot name a host action, so a
  malicious element cannot trick the user into dispatching host authority.
  Contrast: native elements resolve to host actions, but native elements are
  compiled code, not wire input.
- **Placement grants nothing.** Config placement ids control order and
  visibility only. A plugin whose contribution was rejected in negotiation
  renders nothing regardless of placement; scopes gate calls exactly as
  before.
- **Bounded render cost.** `SurfaceCache` budget/degrade applies to plugin
  panel sections as it does to statusbar segments: a plugin that ships a
  pathological view gets degraded, never a stalled compositor. Multi-row
  views get a row cap (host-side truncation to the section budget) so a
  million-row `update` costs the same as a long native list.
- **Credentials**: none involved; no SecretRef surface touched.

## Open questions

- Whether the shared hit-span action type should be one host-wide enum or a
  per-zone associated type erased at the registry boundary — decidable at
  implementation time; the spec pins only "one signature, emitted by the
  painting build".
- Whether `[bars]` badge ids should be bare (`"ci"`) or prefixed
  (`"badge:ci"`) — prefixed avoids colliding with the widget namespace
  (`"disk"` is already both a widget and a shed-priority id); the example
  config decides and documents it.
- How far the glyph token vocabulary goes in its first cut (the full
  `GlyphSet` field list vs the subset the migrated zones need) — the ratchet
  makes either safe; starting with the migrated zones' subset is smaller.
