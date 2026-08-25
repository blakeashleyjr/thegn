# State DB

## ADDED Requirements

### Requirement: Autopilot runs are journaled in the state database

The state database SHALL carry an `autopilot_runs` table recording, per
claimed issue (unique per issue), the repository, worktree, branch, run
state, attempt count, the pull request number once one exists, timestamps,
and the last error. The claim insert SHALL be the single-host
mutual-exclusion point for pickup, and the table SHALL be added by an
additive migration with a `user_version` bump. The journal is bookkeeping and
audit — git remains the source of truth for the worktree and branch, and the
forge for the pull request.

#### Scenario: An older database gains the table on open

- **WHEN** a database created before autopilot existed is opened
- **THEN** the `autopilot_runs` table is created, `user_version` is advanced,
  and all other tables' rows are preserved

#### Scenario: A duplicate claim is refused

- **WHEN** a second claim is attempted for an issue with a non-terminal run
- **THEN** the insert is refused and no second dispatch occurs

#### Scenario: A crash leaves an auditable run

- **WHEN** thegn restarts after crashing mid-run
- **THEN** the run row is still present and resurfaces as needing a human,
  with its worktree and branch recorded
