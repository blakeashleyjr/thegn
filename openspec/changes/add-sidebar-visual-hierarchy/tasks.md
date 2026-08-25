# Tasks — sidebar visual hierarchy

## 1. Config

- [ ] 1.1 `[ui] sidebar_dividers: bool` (default `true`) in
      `thegn-core/src/config.rs`, documented in
      `config/config.toml.example`; config round-trip test updated.

## 2. Row build + layout (thegn-host)

- [ ] 2.1 Emit separator entries between workspace subtrees (and before the
      TERMINALS section) in the row-build pass, suppressed in rail mode,
      under the `/` filter, and when `sidebar_dividers = false`.
- [ ] 2.2 Scroll geometry: separators count in `max_scroll` and the
      hidden-above/below tallies; cursor movement (`j/k`, quick-jump,
      re-anchor) skips them.
- [ ] 2.3 Hit-testing/drag: a click over a gap resolves to no row; the drag
      spot layer maps a gap to the adjacent run boundary (unit tests beside
      the `sidebar_order`/spot tests; coordinate with
      `fix-sidebar-drop-position-semantics` if it lands first).

## 3. Tier styling (thegn-host)

- [ ] 3.1 `compose_row_lines`: workspace/host header label takes the accent
      tier treatment; folder header label drops bold to the secondary
      treatment (existing `S::` slots only — no new slot, no draw-site
      literal; ratchet files unchanged or shrunk).
- [ ] 3.2 Verify mono/16-color legibility via `thegn doctor` term
      environments (`just term-check` covers it in ci); adjust weight/layout,
      never add a literal.

## 4. Docs + help

- [ ] 4.1 `docs/help/sidebar.md`: describe the tiers and the dividers key (no
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
