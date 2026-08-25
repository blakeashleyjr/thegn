# State DB

## ADDED Requirements

### Requirement: Projects and workspace membership are persisted

The state database SHALL persist projects and workspace membership: a
`projects` table (unique name, manual `position`) and a nullable
`workspaces.project_id` (added by the additive migration ladder with a
`user_version` bump; NULL = unprojected). Membership is exclusive (one
column, not a join table). The store SHALL resolve a workspace's project and
list projects with member counts, and deleting a project SHALL be refused
while it has members unless forced, in which case its members are unassigned
first. Project rows record grouping only — no policy — and no cross-repo
feature link rows are stored (feature sets are derived from git).

#### Scenario: Membership is added without disturbing existing data

- **WHEN** a database from before this migration is opened
- **THEN** the `projects` table and `workspaces.project_id` column are
  created additively and existing rows survive

#### Scenario: An unprojected workspace resolves to no project

- **WHEN** a workspace that has never been assigned is queried
- **THEN** it resolves to no project

#### Scenario: Forced delete unassigns members

- **WHEN** `delete` is called with force on a project that has members
- **THEN** the members' `project_id` is cleared and the project row is
  removed
