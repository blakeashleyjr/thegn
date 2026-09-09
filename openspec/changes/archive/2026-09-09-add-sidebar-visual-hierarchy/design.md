# Design — sidebar visual hierarchy

## Tiering with existing slots (no theme change)

The palette already carries the distinctions we need; this change only stops
spending them uniformly:

| tier             | today                            | after                                    |
| ---------------- | -------------------------------- | ---------------------------------------- |
| workspace / host | bold `S::Text` on `S::Bg0`       | bold accent-treated label on `S::Bg0`    |
| folder           | bold `S::Text` on `S::Bg0`       | non-bold `S::Dim`→`S::Text` label, count |
| worktree         | `S::Dim`/`S::Text` on `S::Panel` | unchanged                                |

No new `S::` slot, no `Tok::Hue` at rest (hues stay reserved for urgency —
activity dots, the MQ token), no draw-site literal: everything resolves in
`seg::Tok` → `wire.rs::color_spec`, and any glyph tweak goes through
`GlyphSet` with a BMP width-1 + ASCII pair (the `stabilize-sidebar-internals`
rule). In mono/16-color quantization the tiers must still differ by _weight
and layout_ (bold + indent + glyph), so the hierarchy cannot be color-alone —
that is what makes the change safe under `just term-check`'s six
environments.

## The project boundary is a tint, not a row

Each project block — a header row plus every row beneath it up to the next
block head — shares one background tint, and consecutive blocks alternate
between `S::Panel` and a new `S::PanelAlt`. Parity is computed once per layout
pass over the **visible** slice (`block_parity` in `sidebar_view.rs`), after
pins, reordering and filtering, because that is the order actually painted;
computing it over the on-screen window instead would flip a block's tint as
the list scrolled.

Two rules make the tint compose with the header band rather than fight it:

- **`panel_alt` is derived, never authored.** `extend_palette` blends `bg0`
  over `panel` at 0.5, so every preset and every imported user theme gets a
  value with no table edit, and a paper theme steps toward its own light
  `bg0` rather than toward black. It is registered in `theme_contrast`'s
  surface sets, so text drawn on it is gated exactly as hard as text on
  `panel`.
- **It never passes the midpoint.** Half the `bg0`→`panel` gap goes to the
  block boundary and half stays under the header band, so the band is at
  least as distinct from an alt block as the two blocks are from each other.
  A preset sweep pins this.

The tint sits at the BOTTOM of `row_bg`'s precedence stack — under cursor,
active, and multi-select — so it never competes with the selection vocabulary
layered on row backgrounds. And it carries no layout: with the gap gone every
row is a plain 1-row placement again, so hit-testing, the drag spot layer and
the scroll clamp all see the pre-divider geometry.

## Why this replaced the separator gap

The original form of this requirement laid out a one-row gap above each
workspace header. It read correctly, and it shipped — but it costs a screen
row per project, and on a real tree of a dozen-plus repos that is a large
fraction of the column the change exists to make legible. The tint carries the
same signal for no rows, which is also why it can stay on in the rail and
under a `/` filter, where the gap had to be suppressed.

The one thing lost is that a blank row degrades perfectly in mono, where a
tint may not. That is acceptable here because the gap was never the only
signal: the tier ladder is carried by weight, glyph and indent
(`header_tiers_are_distinguishable_without_color`), and the header band
survives. The tint strengthens a hierarchy that already reads without colour;
it does not carry it.

## Alternatives considered

- **Hairline divider glyph row** (e.g. `─` across the sidebar) — rejected: a
  line adds ink to remove noise, it needs a degradation pair, and it costs the
  same screen row the gap did. The config key is a boolean, not an enum, until
  someone actually asks for a line style.
- **Background banding per workspace** — originally rejected here as visually
  heavy, hostile to the selection vocabulary in `row_bg`, and ugly in
  16-color. **Adopted, in a milder form**, once the gap's cost on a large tree
  became clear: the alternation is `panel`/`panel_alt` (a half-step inside the
  existing surface ramp), not the `Bg0`/`Panel` banding first considered; it
  sits below cursor/active/mark in `row_bg`'s precedence so the selection
  vocabulary still wins every contest; and it is additive over a hierarchy
  that already reads by weight and glyph, so a quantized terminal loses the
  tint without losing the tiers.
- **Uppercase folder names / prefix glyphs** — punishes user-chosen names to
  fix a styling problem.
- **Fold into `stabilize-sidebar-internals`** — that change is scoped and
  in-flight; visual hierarchy is additive on top of its glyph/extraction
  work, not a revision of it.

## Open questions

- Should the TERMINALS section heading adopt the workspace-tier treatment for
  consistency, or stay a plain `SectionHeading`? Leaning consistent-quiet
  (headings are titles, not rows) — decide at implementation with screenshots.
- Whether `sidebar_dividers` should also gate a future line style (enum
  upgrade) — deferred; boolean now, enum only on demand (config keys are
  forever).
