# State upgrade

## ADDED Requirements

### Requirement: A stale daemon blocking a migration is explained and offered a restart

When a schema migration is refused because another thegn process still holds the old schema, thegn SHALL explain in plain words that an older daemon is running and SHALL name `thegn daemon stop`. On a terminal it SHALL ask before stopping (stopping ends that instance's panes); non-interactively it SHALL print the exact command and change nothing. A running host SHALL show a persistent banner.

#### Scenario: Non-interactive

- **WHEN** stdin is not a terminal and the refusal occurs
- **THEN** the command `thegn daemon stop` is printed and nothing is stopped

#### Scenario: Declined

- **WHEN** the user answers anything but `y` or `yes`
- **THEN** no process is stopped

### Requirement: Forward migrations are preceded by a rotated backup

Before migrating an existing database to a newer schema, thegn SHALL write a verified copy named `thegn.db.pre-migrate-<UTC stamp>-v<old>.bak` beside it and SHALL keep only the newest three. A failed backup SHALL warn and SHALL NOT block the migration.

#### Scenario: Rotation

- **WHEN** a fourth backup is written
- **THEN** the oldest is deleted and files that are not backups are untouched
