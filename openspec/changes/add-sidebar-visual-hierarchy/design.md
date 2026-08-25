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
and layout_ (bold + indent + gap), so the hierarchy cannot be color-alone —
that is what makes the change safe under `just term-check`'s six
environments.

## The gap row is layout, not paint

The spacer between workspace subtrees is emitted by the row-build/layout pass
(`build_sidebar`), not painted ad hoc, because three consumers must agree on
it: the renderer, `RowHit` hit-testing, and the scroll clamp
(`max_scroll`, hidden-row counts — the "truncation is never silent"
requirement counts laid-out rows). Deriving all three from one pass is the
existing sidebar contract; a paint-only gap would desynchronize click
targets from pixels, the exact bug class `fix-sidebar-drop-position-semantics`
is burning down.

Interaction rules for the gap:

- **Clicks:** resolve as empty space (no row) — mirrors "the affordances are
  not click targets".
- **Cursor:** never lands on a gap; `j/k` skip it (it has no `RowKind` the
  cursor accepts).
- **Drag:** the spot layer treats a gap as the boundary between the runs
  above and below it — a drop over the gap lands exactly where a drop on the
  boundary would have landed with dividers off. This keeps
  `add-sidebar-folder-ordering`'s run semantics untouched.
- **Filter (`/`) and rail:** gaps are suppressed; a filtered list is dense
  and the rail's 4 columns cannot afford blank rows.

Cost: one row of vertical space per additional workspace. With
`sidebar_dividers = false` layout is byte-identical to today — the key exists
precisely so vertically-tight users (many repos, short terminals) can opt
out, and so e2e can pin both shapes.

## Damage and perf

Pure chrome recomposition — a `Full` frame like any sidebar change; no new
wake path, no per-frame allocation beyond the row vector's existing growth
(one spacer entry per workspace boundary). `render_plan` invariants and their
tests untouched.

## Security

None — a render-only change: no new I/O, config beyond one boolean `[ui]`
key, no new write surface, no capability-catalog row. Blast radius is a
misdrawn frame.

## Alternatives considered

- **Hairline divider glyph row** (e.g. `─` across the sidebar) instead of a
  blank gap — rejected as default: a line adds ink to remove noise, and the
  glyph needs a degradation pair; a blank row degrades perfectly everywhere.
  The config key is a boolean, not an enum, until someone actually asks for
  a line style.
- **Background banding per workspace** (alternate `Bg0`/`Panel` bands per
  subtree) — strongest possible grouping but visually heavy, hostile to the
  activity-dot/selection vocabulary layered on row backgrounds
  (`row_bg`'s precedence stack), and ugly in 16-color quantization.
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
