# Sidebar — persistent view-mode delta

## ADDED Requirements

### Requirement: Persistent structural view modes have persistent feedback

While flat mode is active, the full sidebar SHALL render a persistent,
unambiguous `FLAT` indicator. Grouped/default mode MAY omit a mode indicator,
and the slim rail SHALL omit mode text. The indicator SHALL derive from the same
state used for row construction and persistence, remain readable under
supported themes, compose with sort/hold and filter-input header states, and
degrade without clipping or false hit targets at narrow widths.

#### Scenario: The user switches to flat mode

- **WHEN** the user invokes the flat/grouped toggle
- **THEN** the row structure changes, the setting persists, and the header continuously indicates flat mode

#### Scenario: The user returns to grouped mode

- **WHEN** the user presses `g` while flat mode is active
- **THEN** grouped rows return and the `FLAT` indicator disappears in the same frame

#### Scenario: The session restores a persisted mode

- **WHEN** a compositor starts with flat mode persisted
- **THEN** the first complete sidebar frame renders flat rows and the matching persistent indicator

#### Scenario: Flat mode is active while filtering

- **WHEN** the filter input is visible while flat mode is active
- **THEN** the full header continues to show `FLAT` without obscuring the input or sort/hold state

#### Scenario: The sidebar is the slim rail

- **WHEN** flat mode is active and sidebar layout is reduced to the label-free rail
- **THEN** the rail adds no mode word and creates no mode-specific hit target
