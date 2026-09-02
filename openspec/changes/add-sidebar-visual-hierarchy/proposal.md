# Sidebar visual hierarchy: stop the folders running together

Linear: THE-64

## Why

In the left bar, every structural row currently gets the same treatment:
workspace headers, terminal-host headers and folder headers all render as a
bold `S::Text` label on the same recessed `S::Bg0` band, distinguished only
by 2–3 cells of indent and a dim `▪` folder glyph
(`sidebar_view.rs::compose_row_lines` / `row_bg`). Consecutive workspaces
abut with no separation at all. With several repos open, each carrying
lifecycle folders ("Merging", "Merged", "Needs attention" — created by
default by the merge queue) plus user folders, the tree reads as one
undifferentiated stack of bold bands: THE-64's "all the folders run
together". The user cannot see at a glance where one project ends, the next
begins, or which bold line is a repo versus a folder inside one.

## What Changes

Three render-only tiers, all through the existing chokepoints (theme slots +
`caps::active_glyphs()`; the color/glyph literal ratchets stay shrink-only):

- **Workspace headers become the loudest tier.** The workspace (and
  terminal-host) label keeps bold and gains the accent treatment on its
  band, so repo boundaries are the strongest lines in the tree.
- **Folder headers drop a tier.** Folder labels lose bold and render in the
  secondary text slot with their filed-count; the caret and glyph stay. A
  folder now reads as a grouping _inside_ a project, not a peer of one.
- **Adjacent projects separate, in colour.** Each project block — its header
  and every row beneath it — shares one background tint, and consecutive
  blocks alternate between `panel` and a derived `panel_alt` slot, gated by a
  new `[ui] sidebar_dividers` key (default on). Headers keep their `bg0` band
  on both parities, and `panel_alt` is derived to sit between `bg0` and
  `panel` and never past their midpoint, so the header stays the block start
  either way. Because the separation costs no layout rows it applies in rail
  mode and under the `/` filter too, and geometry is identical with the key on
  and off.

  **This replaces a one-row separator gap** (the original form of this
  requirement, as shipped): the gap worked, but it spent a screen row per
  project, and on a real tree of a dozen-plus repos that consumed a large
  fraction of the very column it was meant to make legible. A tint says the
  same thing for nothing.

Out of scope, noted as related: per-workspace icon/color labels (roadmap C 39) would further strengthen identity but need per-workspace config and is
its own change; the merge-queue project token is
`move-merge-queue-ambient-surface`.

## Impact

- **tasks.md:** hardens group **B** (items 13–28, workspace bar/tree) for
  release; related to **C** 39 (icon/color label, untouched) and **N**
  (theming — consumes existing slots, adds none).
- **Capabilities:** `sidebar` — ADDED requirements (header tiering; workspace
  separation). `config` — one new documented `[ui]` key (spec'd in the
  sidebar delta's scenarios; documented in `config/config.toml.example`).
- **Depends on / reconciles:** `stabilize-sidebar-internals` (this builds on
  its glyph-table routing and `sidebar_view.rs` extraction; its rail-identity
  and glyph requirements are untouched). Coordinates with
  `add-sidebar-actions-and-mouse` + `fix-sidebar-drop-position-semantics`
  (unaffected now that the boundary adds no rows: with no gap to hit-test or
  drop onto, every row is a plain 1-row placement again)
  and `add-sidebar-folder-ordering` (runs and membership unaffected — this
  change never reorders anything). `move-merge-queue-ambient-surface` and
  `rename-workspaces-to-projects` touch the same header row/labels; batch the
  e2e re-record when landing together.
- **Code:** `sidebar_view.rs` (compose + `row_bg` + `block_parity`),
  `theme.rs`/`theme_contrast.rs`/`config_theme.rs`/`chrome.rs` (the derived
  `panel_alt` slot, registered as a gated surface), `config` `[ui]
sidebar_dividers`, `docs/help/sidebar.md`.
- **e2e:** baseline-affecting for nearly every case that shows the sidebar —
  full re-record with `just e2e-update`, diff reviewed.
