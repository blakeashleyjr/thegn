# panel — delta for add-ui-component-contract

## ADDED Requirements

### Requirement: Section placement follows the shared placement grammar

`[panel] sections` SHALL be specified as the shared placement grammar (the `[bars]` model): an ordered id list where order is display order, omission hides a section, and an unknown id warns and is skipped rather than erroring. Plugin panel sections SHALL be placeable in the same list by their `plugin:<plugin>:<contribution>` id, interleaved freely with native section ids. With no `sections` key configured, the default set and order render as today.

#### Scenario: A plugin section is placed among native sections

- **WHEN** `[panel] sections` lists `["changes", "plugin:hello:todo", "pr"]`
- **THEN** the plugin's section renders between Changes and PR, and all unlisted sections are hidden

#### Scenario: A stale section id is skipped

- **WHEN** the list names a section id that no longer exists
- **THEN** a warning is logged and the remaining sections render in order

### Requirement: Plugin panel sections render through the element path

A plugin with an accepted `PanelSection` contribution SHALL appear as an accordion section rendered through the same element path native sections use: its view rows become panel rows (line + optional hit) under the host's layout, truncation and row budget. Activating a plugin section row SHALL send the owning plugin an `on_event` notification (`kind: Action`, `payload.id` naming the contribution and row) and MUST NOT dispatch a host action. A disabled or crashed plugin's section SHALL disappear from the accordion rather than rendering blank. The section maps to the `panel:plugins` help context key, documented in `docs/help/plugins.md`.

#### Scenario: A plugin row activates

- **WHEN** the user presses Enter on a plugin section's row
- **THEN** the plugin receives `on_event` with the contribution id and row id, and no host action fires

#### Scenario: A crashed plugin's section vanishes

- **WHEN** a resident plugin exceeds its crash backoff cap while its section is placed
- **THEN** the section is absent from the accordion and the remaining sections render normally
