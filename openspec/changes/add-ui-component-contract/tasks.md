# Tasks — add-ui-component-contract

**Delivery is phased (each phase lands alone).** Phase 1 lands the core contract
substrate + glyph tokens + the ratchet + one live-zone migration (pins, which
were click-dead). Phases 2–5 are tracked follow-ups; see the "Phase 2+" note in
`proposal.md`. Frame-altering zone migrations need e2e re-record — noted per zone.

## Phase 1 — contract substrate + first migration (DONE)

### 1. The element contract (host module + core vocabulary)

- [x] 1.1 Element contract module (`crates/thegn-host/src/element/mod.rs`):
      `ElementId` (native + `plugin:<plugin>:<contribution>` in one namespace),
      `Zone` (with `context_key()` aligned to the help `zone:*` vocabulary and
      asserted against it), `ElementRow` (generalizing `PanelRow`: line + optional
      bg + optional hit), the shared hit-span signature `HitSpan { rect, action }`
      with `ElementAction`, `ElementBuild`, and the `ChipRow` builder whose one
      pass returns rows + hit spans together (plus `build_chip_strip` / `hit_at`).
- [x] 1.2 Glyph token enum (`thegn_core::termcaps::Glyph`, pure data) beside
      `GlyphSet`, with `resolve(&GlyphSet)` and `Glyph::ALL`; host `caps::glyph()`
      resolves through `active_glyphs()`. Unit tests in core map every token →
      field across all sets (95% line gate); host test proves ASCII degrade.
- [x] 1.3 Unit-test the builder rule: a build's hit spans reference only rows it
      painted (shedding case; span list == painted list), in the style of the
      `statusbar_fit` tests (`element::tests::hits_reference_only_painted_chips`).

### 3. Hit unification (partial — pins)

- [x] 3.2 (pins) Migrate `draw_pin_chips` to the element builder (`build_pin_strip`:
      rows + hit spans from one pass); pin chips gain hit-testing — a click summons
      the pin (`pin_chip_hit` → `Action::SummonPin` path in `run.rs`). Painted
      output preserved (same glyphs/colors, now via tokens).
- [x] 3.3 Add `test/element-ratchet.txt` (shrink-only allowlist of legacy
      `draw_text` interactive draw sites: chrome.rs, sidebar_view.rs, tabbar_env.rs;
      decorative art + tests excluded) wired into `test/ratchet.sh` / `just lint`
      / `just ratchet-update`, seeded with a reason header.

## Phase 2 — hit unification, remainder (DEFERRED)

- [ ] 3.1 Migrate the statusbar/masthead item spans and `center_tab_hit` to the
      shared `HitSpan` signature; collapse the corresponding `run.rs` mouse
      `else if` arms into `element::hit_at`. (`center_tab_hit`/`strip_chip_spans`
      already share one span source — lower urgency; the pin migration proves the
      pattern.) **Frame-neutral** (no paint change); no e2e re-record.
- [ ] 3.2 (tabs) Migrate `draw_center_tabs` (worktree label + tab pills + issue
      badge) to element rows. **Frame-touching** (pill layout) — e2e re-record.

## Phase 3 — one zone key-table shape (DEFERRED)

- [ ] 2.1 Extract the shared table type (chord, label, `HintTier`, dispatch
      discriminant, zone id) from `sidebar_keytable`; re-express `SIDEBAR_KEYS`
      and `panel::gitui::context_keys` in it (no behaviour change).
- [ ] 2.2 Give `panel::section_keys` dispatch discriminants; convert the
      per-section `run.rs` match arms into table lookups in a `handlers/` sibling;
      delete `hint_table_matches_dispatch` (the source-text drift test) once the
      tables drive dispatch.
- [ ] 2.3 Extend `keymap_merge::collect` to fold the section tables with honest
      zone attribution (`Source::ZoneTable`, real zone ids); verify `thegn keys
list` and the generated keybindings help page show them.
- [ ] 2.4 Update `docs/help/` pages whose sections gained honestly-attributed
      keys; run `just help-ratchet-update` only to shrink.

## Phase 4 — placement grammar (DEFERRED)

- [ ] 4.1 Give statusbar badges placement ids and honour them in `[bars]` lists
      (unlisted badges keep today's default order appended); shedding unchanged and
      applied after placement.
- [ ] 4.2 Accept `plugin:<plugin>:<contribution>` ids in `[bars]` and
      `[panel] sections`; unknown/stale ids warn + skip.
- [ ] 4.3 Document every new placeable id + the grammar in
      `config/config.toml.example`.
- [ ] 4.4 Unit tests: badge reorder via config, compat default with no config,
      stale-id skip with warning.

## Phase 5 — plugin surface (api v0.3 + runtime)

- [x] 5.1 `plugin_api` v0.3 (DONE, additive): `View.rows` (+ `View::multi`/
      `effective_rows`), `Span.slot` theme-slot name with `StyleRole` fallback
      (+ `Span::slotted`), `ExtensionPoint::PanelSection` (→ `surface:panel`
      capability); `API_VERSION` bumped to 0.3.0, schema snapshot regenerated
      (`docs/api/plugin-api-0.3.json`), decode tests for v0.2 compat,
      byte-identical single-line serialization, unknown-slot preservation, and
      multi-row round-trip. All fields default → a v0.2 plugin/older host keep
      working (negotiation accepts lower-or-equal minor).
- [ ] 5.2 (DEFERRED) Runtime: negotiate `PanelSection`, render its cached view through the
      element path with `SurfaceCache` budget/degrade + host-side row truncation;
      disabled/crashed plugin's section vanishes from the accordion.
- [ ] 5.3 (DEFERRED) Route plugin-row activation to `on_event` (`kind: Action`,
      contribution + row id) — never a host action; extend the resident-plugin
      golden test.
- [ ] 5.4 (DEFERRED) Update `docs/help/plugins.md` for the new surface; claim the
      `panel:plugins` context key. (Runtime-gated: lands with 5.2/5.3, since the
      help-context ratchet requires the surface to actually be wired.)

## 6. Validation

- [ ] 6.1 Re-record e2e baselines for any altered frames (Phase 2 tabs, and the
      pin paint if its cell attributes shift) — `just e2e-update`, review the diff.
      (e2e is currently broken/skipped per CLAUDE.md; the pin migration is
      frame-neutral by construction — same glyphs/colors/positions.)
- [ ] 6.2 Run `just ci` once at the end of each phase (lint incl. the element
      ratchet, tests incl. keymap/help/plugin snapshot gates, coverage,
      openspec-validate).
