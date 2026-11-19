# State DB

## MODIFIED Requirements

### Requirement: Single versioned SQLite store

The state database SHALL maintain itself. A UI process (never the daemon) SHALL run a maintenance pass at most every 10 minutes, the first at least 120 seconds after the first frame, off the event loop. At most one process SHALL maintain at a time, enforced by `locks/db-maintain.lock`. A pass SHALL run bounded retention, then the state-file sweep, then one step: convert a legacy database of at most 256 MiB live with a full `VACUUM` (after checking free space, at most 3 attempts per process); toast once for a larger legacy database; or free pages incrementally (2,048 per step, at most 16 steps, stopping when a step exceeds 250 ms) when at least `max(8 MiB, 10% of the file)` is free. A pass that freed pages SHALL truncate the write-ahead log.

#### Scenario: Legacy database converts

- **WHEN** a legacy database of at most 256 MiB live is open in a UI with enough free space
- **THEN** it is converted within one pass, with a toast before and one after

#### Scenario: Another process maintains

- **WHEN** another process holds the maintenance lock
- **THEN** the pass does nothing and `thegn db vacuum` waits at most 30 seconds

#### Scenario: Not enough space

- **WHEN** the database filesystem lacks `2 * live + 64 MiB` or the temp directory lacks `live`
- **THEN** no conversion is attempted and one toast is shown per process

## ADDED Requirements

### Requirement: State-file retention

Each maintenance pass SHALL bound the files kept in state directories, including `pty-shutdown/` (at most 500 entries after two passes), and every retention rule SHALL be bounded per pass and run only inside the maintenance pass.

#### Scenario: Excess shutdown captures

- **WHEN** `pty-shutdown/` holds more than 500 entries
- **THEN** within two passes the oldest are removed down to 500
