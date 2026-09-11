## ADDED Requirements

### Requirement: Compatibility warnings preserve advisory severity

Configuration validation SHALL represent accepted compatibility spellings as
typed warnings, not errors. It SHALL retain the runtime canonical-wins policy
for canonical/legacy collisions during the compatibility window.

#### Scenario: Warning-only configuration

- **WHEN** the selected configuration layers contain only valid settings and
  accepted compatibility spellings
- **THEN** validation reports warnings, counts zero problems, succeeds, and does
  not rewrite those files

#### Scenario: Warning combined with invalid configuration

- **WHEN** an accepted compatibility spelling appears beside a real schema,
  syntax, type or semantic error
- **THEN** validation counts the warning separately and fails on the error
