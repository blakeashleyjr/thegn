# Panel

## ADDED Requirements

### Requirement: The panel's blank rows teach the focused zone's keys

When `[panel] context_hints` is on (default), the right panel SHALL show, in
blank tail rows above the Layout keys block, a titled block listing the keys
for the zone that has keyboard focus: a terminal pane (PANE KEYS), the drawer
(DRAWER KEYS), or the masthead or statusbar (BAR KEYS). While the panel itself
has focus it SHALL show its NAVIGATE block whatever the setting. The keys MUST
come from the same live, focus-aware list as the statusbar hint strip, so a
rebind or a mode change shows on the next frame. The block MUST grow into
free rows up to a fixed cap, MUST shrink and then hide as panel content needs
the rows, MUST never cover panel content or the Lead footer, and MUST NOT
repeat a chord that the Layout keys block shows. The sidebar keeps its own
footer, so a focused sidebar adds no panel block.

#### Scenario: A focused pane teaches its keys

- **WHEN** a terminal pane has focus and the panel has blank rows
- **THEN** a PANE KEYS block lists that zone's keys with their live chords

#### Scenario: Content wins

- **WHEN** the panel content grows to fill its rows
- **THEN** the block sheds tips and then disappears without covering content

#### Scenario: No duplicate chords

- **WHEN** a key in the focused zone's list is also in the Layout keys block
- **THEN** the context block skips that key

#### Scenario: Disabled

- **WHEN** `[panel] context_hints = false`
- **THEN** only the panel's own NAVIGATE block appears, and only while the
  panel has focus
