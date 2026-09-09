# Sidebar

## ADDED Requirements

### Requirement: Header rows read in tiers

The full sidebar SHALL render its structural rows in visually distinct tiers:
workspace (and terminal-host) headers as the strongest tier, folder headers
as a clearly secondary tier, and worktree/terminal rows as the body tier. A
workspace header and a folder header MUST be distinguishable at a glance by
more than indentation alone, and the distinction MUST NOT rely on color
alone — it survives 16-color and mono quantization through weight and
layout. All styling MUST resolve through the theme slot / capability-glyph
chokepoints; no color or glyph literal at a draw site.

#### Scenario: A repo and its folder are told apart

- **WHEN** a workspace containing a "Merged" folder is rendered in the full
  sidebar
- **THEN** the workspace header and the folder header use visibly different
  emphasis (not merely different indent), with the folder subordinate

#### Scenario: The hierarchy survives a mono terminal

- **WHEN** the same tree renders with colors quantized to mono
- **THEN** workspace headers, folder headers and worktree rows remain
  distinguishable by weight and layout

### Requirement: Adjacent projects are visibly separated

The sidebar SHALL separate one project's subtree from the next by an
alternating background tint, gated by `[ui] sidebar_dividers` (default on).
Each project block — its header row and every row beneath it up to the next
block head — SHALL share one tint, and consecutive blocks SHALL alternate
between the `panel` and `panel_alt` palette slots; a section banner SHALL
reset the alternation so a following region always opens on the base tint.
Project and terminal-host headers SHALL keep their recessed `bg0` band on
both parities, and `panel_alt` SHALL be derived to sit between `bg0` and
`panel` and never past their midpoint, so a header still reads as the start
of its block whichever tint the block took.

Because the separation costs no layout rows, it SHALL apply in rail mode and
while the `/` filter is active, and row geometry SHALL be identical with
`sidebar_dividers` on and off. With `sidebar_dividers = false` every block
SHALL render on the base tint.

The separation MUST NOT be a blank separator row: an earlier form of this
requirement spent one screen row per project, which on a tree of a dozen or
more repos consumed a large fraction of the column it was meant to make
legible.

#### Scenario: Two repos no longer read as one

- **WHEN** two projects render consecutively with `sidebar_dividers = true`
- **THEN** the second project's rows carry a different background tint from
  the first's, and no blank row lies between them

#### Scenario: A project block is tinted as a unit

- **WHEN** a project block contains worktree rows, folder headers and a
  derived `Pipelines` folder
- **THEN** every one of those rows carries the same tint as its project
  header's block, so the block reads as one thing

#### Scenario: The header band survives both parities

- **WHEN** a project header renders on an alternate-tinted block
- **THEN** it keeps the `bg0` band, which stays at least as distinct from the
  block tint as the two block tints are from each other

#### Scenario: Separation costs no rows

- **WHEN** the same tree is laid out with `sidebar_dividers` on and off
- **THEN** the two layouts have identical row heights and scroll geometry,
  differing only in the background tint of alternate blocks

#### Scenario: The tint survives the rail and a filter

- **WHEN** the sidebar is in rail mode, or a `/` filter is active
- **THEN** consecutive project blocks still alternate tint

#### Scenario: Alternation can be turned off

- **WHEN** `[ui] sidebar_dividers = false`
- **THEN** every project block renders on the base `panel` tint
