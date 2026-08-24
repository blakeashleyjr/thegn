# Workspace

## ADDED Requirements

### Requirement: A workspace may belong to a project

A workspace SHALL optionally belong to exactly one project within its profile,
recorded as membership in the state database and never inferred from a
filesystem path. Projects SHALL carry no policy: assigning or unassigning a
project MUST NOT change the workspace's zone, credential, egress, budget,
sandbox, or env-bundle resolution. The `thegn project` command SHALL create,
rename, list, delete, and assign projects; deleting a project with members is
refused unless forced (which unassigns its members first). A workspace MAY
belong to one zone and one project simultaneously.

#### Scenario: Assigning a repo records membership

- **WHEN** `thegn project assign shop <repo>` is run
- **THEN** the repo's workspace belongs to project `shop` and the project's
  member count includes it

#### Scenario: Deleting a non-empty project is refused

- **WHEN** `thegn project rm shop` is run while `shop` has members
- **THEN** the deletion is refused with a message, unless `--force` is given

#### Scenario: Project membership never changes policy

- **WHEN** a workspace in zone `clientA` is assigned to project `shop`
- **THEN** its zone, resolved credentials, egress, budget, and sandbox
  resolution are exactly what they were before the assignment

#### Scenario: Membership is not path-inferred

- **WHEN** a workspace's filesystem path resembles a project name
- **THEN** its project is determined solely by recorded membership, not the
  path

### Requirement: A feature can be created across a project's repos with one linked branch name

thegn SHALL create a feature across a project's member repos in one action:
it resolves exactly one final branch name (the configured branch prefix +
slug, applied once — per-repo prefix overrides are not re-applied) and creates
that branch and a worktree in every member repo, or in an explicitly named
subset. Each member creation runs the existing per-repo pipeline
independently; the action MUST report a per-member outcome, MUST NOT roll
back already-created siblings when one member fails, and a re-run MUST attach
(report `exists` and skip) members that already have the branch, so retrying
after a partial failure completes the set.

#### Scenario: One command, one branch name, N repos

- **WHEN** `thegn wt new payments-retry --project shop` is run and `shop` has
  three member repos
- **THEN** the same resolved branch name is created with a worktree in each
  of the three repos and each member's outcome is reported

#### Scenario: Partial failure is retryable

- **WHEN** the batched creation fails in one member repo after succeeding in
  the others, and the same command is re-run
- **THEN** the succeeded members are reported as existing and skipped, and
  the failed member is attempted again

#### Scenario: A subset of members

- **WHEN** `thegn wt new x --project shop --repos api,web` is run
- **THEN** worktrees are created only in the named member repos

### Requirement: Feature sets are derived from branch-name equality, never persisted

thegn SHALL derive a project's feature sets — groups of worktrees across
member repos whose branches share the same name — as pure logic over each
repo's git-derived worktree list. The grouping MUST be deterministic, MUST
admit sparse sets (a feature present in only some members), and MUST NOT be
persisted as authoritative cross-repo link state: git remains the sole source
of truth in each member repo, so a same-named branch created outside thegn
joins its feature set on the next hydration.

#### Scenario: Same-named branches group across repos

- **WHEN** member repos `api` and `web` each have a worktree on branch
  `tg/payments-retry`
- **THEN** the project's feature sets include one set containing both
  worktrees

#### Scenario: An externally created branch joins its set

- **WHEN** a user runs plain `git worktree add` for `tg/payments-retry` in
  member repo `shared-lib` outside thegn
- **THEN** after the next hydration that worktree appears in the
  `tg/payments-retry` feature set without any thegn-side registration of a
  cross-repo link
