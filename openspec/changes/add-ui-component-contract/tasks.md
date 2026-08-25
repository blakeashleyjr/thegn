# Tasks — add-ui-component-contract

## 1. The element contract (host module + core vocabulary)

- [ ] 1.1 Add the element contract module (`crates/thegn-host/src/element/`):
      `ElementId`, zone ids (aligned with the help `zone:*`/`panel:*` context
      vocabulary), element rows (generalizing `panel::sections::PanelRow`),
      the shared hit-span signature `(Rect, element action)`, and the builder
      shape whose one pass returns rows + hit spans together.
- [ ] 1.2 Add the glyph token enum to `thegn_core::termcaps` beside `GlyphSet`
      (pure data), with resolution through `caps::active_glyphs()` in the
      host; unit tests in core for token → glyph-set-field mapping across all
      glyph sets (95% line gate applies).
- [ ] 1.3 Unit-test the builder rule: a build's hit spans reference only rows
      it painted (construct a shedding case; assert span list == painted
      list), in the style of the existing `statusbar_fit` tests.

## 2. One zone key-table shape

- [ ] 2.1 Extract the shared table type (chord, label, `HintTier`, dispatch
      discriminant, zone id) from `sidebar_keytable`; re-express
      `SIDEBAR_KEYS` and `panel::gitui::context_keys` in it (no behaviour
      change).
- [ ] 2.2 Give `panel::section_keys` dispatch discriminants; convert the
      per-section `run.rs` match arms into table lookups in a
      `handlers/` sibling module; delete `hint_table_matches_dispatch` (the
      source-text drift test) once the tables drive dispatch.
- [ ] 2.3 Extend `keymap_merge::collect` to fold the section tables with
      honest zone attribution (`Source::ZoneTable`, real zone ids); verify
      `thegn keys list` and the generated keybindings help page show them
      (the generated page's completeness test covers this).
- [ ] 2.4 Update `docs/help/` pages whose sections gained honestly-attributed
      keys; run `just help-ratchet-update` only to shrink.

## 3. Hit unification

- [ ] 3.1 Migrate the statusbar/masthead item spans and `center_tab_hit` to
      the shared hit-span signature; collapse the corresponding `run.rs`
      mouse `else if` arms into the shared lookup.
- [ ] 3.2 Migrate `draw_center_tabs` and `draw_pin_chips` to element builders
      (rows + spans from one pass); pin chips gain hit-testing (click
      activates the pin).
- [ ] 3.3 Add `test/element-ratchet.txt` (shrink-only allowlist of remaining
      legacy draw sites: overlays, sidebar renderer, any zone not yet
      migrated) wired into `test/ratchet.sh` / `just lint`; seed it from the
      audit list with a reason header.

## 4. Placement grammar

- [ ] 4.1 Give statusbar badges placement ids and honour them in the `[bars]`
      lists (unlisted badges keep today's default order appended after listed
      items); shedding priorities unchanged and applied after placement.
- [ ] 4.2 Accept `plugin:<plugin>:<contribution>` ids in `[bars]` lists and
      `[panel] sections`; unknown/stale ids warn + skip.
- [ ] 4.3 Document every new placeable id and the grammar in
      `config/config.toml.example` (badge ids, plugin ids, the omission-hides
      rule); config-reference help page is generated, so no hand edits.
- [ ] 4.4 Unit tests: badge reorder via config, compat default with no config,
      stale-id skip with warning.

## 5. Plugin surface (api v0.3 + runtime)

- [ ] 5.1 `plugin_api` v0.3: `View.rows`, `Span` theme-slot name (role
      fallback), `PanelSection` extension point; bump `API_VERSION`,
      regenerate the schema snapshot, add decode tests for v0.2 compat and
      unknown-slot fallback (core coverage gate applies).
- [ ] 5.2 Runtime: negotiate `PanelSection`, render its cached view through
      the element path with `SurfaceCache` budget/degrade + host-side row
      truncation; disabled/crashed plugin's section vanishes from the
      accordion.
- [ ] 5.3 Route plugin-row activation to `on_event` (`kind: Action`,
      contribution + row id) — never a host action; extend the resident-
      plugin golden test (`examples/plugins/hello.sh` or a sibling example)
      to cover a panel section register → update → activate round-trip.
- [ ] 5.4 Update `docs/help/plugins.md` for the new surface and claim the
      `panel:plugins` context key (help-context ratchet enforces it).

## 6. Validation

- [ ] 6.1 Re-record e2e baselines for any altered frames (`just e2e-update`,
      review the diff) — pin any new volatile chrome in `e2e_freeze` first.
- [ ] 6.2 Run `just ci` once (lint incl. the new element ratchet, tests incl.
      keymap/help/plugin snapshot gates, coverage, openspec-validate).
