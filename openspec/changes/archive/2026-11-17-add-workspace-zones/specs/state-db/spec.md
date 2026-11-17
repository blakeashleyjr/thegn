# State DB

## ADDED Requirements

### Requirement: Zones and workspace membership are persisted

The state database SHALL persist zones and workspace membership: a `zones` table
(unique name) and a nullable `workspaces.zone_id` (schema v33, added by the
additive migration ladder; NULL = unzoned). Membership is exclusive (one column,
not a join table). The store SHALL resolve a worktree's zone by mapping the
worktree to its repo's workspace and thence its zone, falling back to treating the
argument as a repo path.

#### Scenario: A worktree resolves to its workspace's zone

- **WHEN** a repo is assigned to a zone and a worktree under that repo is queried
- **THEN** the worktree resolves to that zone

#### Scenario: Membership is added without disturbing existing data

- **WHEN** a pre-v33 database is opened
- **THEN** the `zones` table and `workspaces.zone_id` column are created
  additively and existing rows survive

#### Scenario: An unzoned worktree resolves to no zone

- **WHEN** a worktree whose workspace has no zone is queried
- **THEN** it resolves to no zone
