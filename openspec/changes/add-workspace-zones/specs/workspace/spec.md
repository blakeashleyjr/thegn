# Workspace

## ADDED Requirements

### Requirement: A workspace may belong to a zone

A workspace SHALL optionally belong to exactly one zone within its profile,
recorded as membership in the state database and never inferred from a filesystem
path. The `superzej zone` command SHALL create, rename, list, delete, and assign
zones; assigning ensures the workspace is registered, and deleting a zone with
members is refused unless forced (which unassigns its members first).

#### Scenario: Assigning a repo records membership

- **WHEN** `superzej zone assign clientA <repo>` is run
- **THEN** the repo's workspace belongs to zone `clientA` and the zone's member
  count includes it

#### Scenario: Deleting a non-empty zone is refused

- **WHEN** `superzej zone rm clientA` is run while `clientA` has members
- **THEN** the deletion is refused with a message, unless `--force` is given

#### Scenario: Membership is not path-inferred

- **WHEN** a worktree's filesystem path resembles a zone name
- **THEN** its zone is determined solely by recorded membership, not the path
