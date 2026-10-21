## ADDED Requirements

### Requirement: Bounded terminal-safe calendar display text

Calendar-derived strings SHALL be projected through one terminal-safe scalar/cell-bounded display policy before rendering or reminder publication, without changing raw provider values.

#### Scenario: Hostile provider title

- **WHEN** an event title or calendar name contains cursor controls or excess text
- **THEN** agenda cells SHALL emit a bounded single-row projection and SHALL NOT paint outside the popup rectangle

#### Scenario: Configured clock labels

- **WHEN** a legacy world-clock label requires sanitation
- **THEN** validation SHALL issue an advisory warning and width calculation and drawing SHALL use the same sanitized value

#### Scenario: Reminder composition

- **WHEN** a reminder uses an untrusted title, URL or location
- **THEN** each field and the final message SHALL remain bounded and single-row

#### Scenario: Semantic round-trip

- **WHEN** an event is displayed
- **THEN** original provider values SHALL remain available unchanged for semantic operations
