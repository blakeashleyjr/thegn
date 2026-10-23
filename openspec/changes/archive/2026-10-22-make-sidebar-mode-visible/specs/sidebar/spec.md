# Sidebar — persistent layout chip delta

## ADDED Requirements

### Requirement: The sidebar header names the flat or grouped layout

The full sidebar header SHALL show a layout chip naming the current layout,
`flat` in the accent colour or `grouped` in a quiet colour, to the left of the
sort chip. The chip SHALL derive each frame from the same flat/grouped state
that builds the rows and is persisted, SHALL share one geometry with its mouse
hit target, and clicking it SHALL toggle the layout exactly as `g` does. At
narrow widths chips SHALL drop out whole rather than clip: in grouped mode the
layout chip gives way before the sort chip, and in flat mode it is the last
chip on the header row. While the filter input is open or a filter is set, and
in the slim rail, the header SHALL show no layout chip.

#### Scenario: The user switches to flat mode

- **WHEN** the user invokes `g` to switch to flat mode
- **THEN** the rows change, the setting persists, and the chip reads `flat` in
  the same frame

#### Scenario: The user returns to grouped mode

- **WHEN** the user invokes `g` or clicks the layout chip while flat mode is
  active
- **THEN** grouped rows return, the setting is cleared, and the chip reads
  `grouped` in the same frame

#### Scenario: The session restores a persisted flat mode

- **WHEN** a compositor starts with flat mode persisted
- **THEN** the restored flat rows and `flat` chip appear together in the first
  complete sidebar frame

#### Scenario: The sidebar header has constrained width

- **WHEN** the header is too narrow to show every chip
- **THEN** chips disappear whole rather than being clipped; in grouped mode the
  layout chip gives way before the sort chip, and in flat mode the layout chip
  outlasts the sort chip until it too no longer fits

#### Scenario: Filtering is active

- **WHEN** the filter input is open or a filter is set
- **THEN** the header shows no layout chip, and it returns when filtering ends

#### Scenario: The sidebar is the slim rail

- **WHEN** the sidebar is reduced to the slim rail
- **THEN** the rail shows no layout chip and exposes no layout-chip hit target
