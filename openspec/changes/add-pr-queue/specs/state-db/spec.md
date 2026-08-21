# State DB

## ADDED Requirements

### Requirement: The PR queue is persisted in the state database

The state database SHALL carry a `pr_queue` table keyed by repository and pull
request number, recording the branch, base branch, forge, status, current
blocker, the worktree the entry was queued from (which MAY be absent), the agent
attempt count, and the last head commit thegn observed. The table SHALL be added
by an additive migration with a `user_version` bump, so an existing database
upgrades in place without losing its other caches.

#### Scenario: An older database gains the table on open

- **WHEN** a database created before the PR queue existed is opened
- **THEN** the `pr_queue` table is created, `user_version` is advanced, and every
  pre-existing row in other tables is preserved

#### Scenario: A queued pull request survives a restart

- **WHEN** a pull request is queued and thegn is restarted
- **THEN** the entry is still present with its status, blocker, and attempt count
