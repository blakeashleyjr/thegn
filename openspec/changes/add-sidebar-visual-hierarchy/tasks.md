# Tasks — sidebar visual hierarchy

## 1. Config

- [ ] 1.1 `[ui] sidebar_dividers: bool` (default `true`) in
      `thegn-core/src/config.rs`, documented in
      `config/config.toml.example`; config round-trip test updated.

## 2. Project-block tint (thegn-core + thegn-host)

- [x] 2.1 Derive a `panel_alt` palette slot in `theme::extend_palette`
      (`blend_over(bg0, panel, 0.5)`), declare it through the slot chain
      (`Palette`, `ThemeColors`, `theme_resolve`, `config::get_dotted`,
      `chrome::S`/`S::ALL`/`slot_rgb`, `config.toml.example`), and register it
      as a gated surface in `theme_contrast::audit`.
- [x] 2.2 `block_parity` in `sidebar_view.rs`: one forward pass over the
      visible slice, advancing on a block head and resetting at a
      `SectionHeading`; threaded into `row_bg` as its lowest-precedence arm,
      gated by `[ui] sidebar_dividers`.
- [x] 2.3 Remove the workspace arm from `lead_gap_rows` so a project header is
      a plain 1-row placement again; the `SectionHeading` breathing gap and
      the clipped-gap trim stay. Update the hit-test/mouse tests that asserted
      a header owned a gap line.

## 3. Tier styling (thegn-host)

- [ ] 3.1 `compose_row_lines`: workspace/host header label takes the accent
      tier treatment; folder header label drops bold to the secondary
      treatment (existing `S::` slots only — no new slot, no draw-site
      literal; ratchet files unchanged or shrunk).
- [ ] 3.2 Verify mono/16-color legibility via `thegn doctor` term
      environments (`just term-check` covers it in ci); adjust weight/layout,
      never add a literal.

## 4. Docs + help

- [x] 4.1 `docs/help/sidebar.md`: describe the tiers and the block tint (no
      new action ids — help ratchets unaffected; prose ratchet satisfied by
      the mention).
- [ ] 4.2 CHANGELOG entry (visual change + the opt-out key).

## 5. Validation

- [ ] 5.1 Re-record e2e baselines (`just e2e-update`; nearly every sidebar
      case changes — review the diff deliberately, and batch with
      `move-merge-queue-ambient-surface` / `rename-workspaces-to-projects`
      if landing in the same window).
- [ ] 5.2 Run `just ci` once (lint ratchets, render-plan tests, term-check,
      openspec validate).
