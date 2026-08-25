# CLI

## ADDED Requirements

### Requirement: Projects are managed through a `project` namespace

thegn SHALL expose a `project` noun-verb namespace (`list`, `create`,
`rename`, `rm [--force]`, `assign <project|none> [repo]`) following the
established CLI grammar: `assign` defaults the repo from the current
directory, `list` honors the `--json` convention through the one emitter,
and every verb is a capability-catalog row gated by `required_scope` (reads
read-scoped, writes write-scoped) — never a second policy table.

#### Scenario: Listing projects

- **WHEN** `thegn project list --json` is run
- **THEN** projects with their member counts are emitted as machine-readable
  JSON via the standard emitter

#### Scenario: Assign defaults to the cwd repo

- **WHEN** `thegn project assign shop` is run from inside a registered repo
- **THEN** that repo's workspace is assigned to project `shop`

#### Scenario: Verbs are catalog-projected and scope-gated

- **WHEN** a `project` write verb is invoked over an external surface
  without the corresponding write scope
- **THEN** the call is refused by the capability catalog's scope gate

### Requirement: Headless worktree creation accepts a project scope

`thegn wt new` SHALL accept `--project <name>` (with optional `--repos
<a,b>` subset) to run the batched cross-repo creation headlessly, printing a
per-member outcome (created / exists / failed with reason) and, with
`--json`, one machine-readable object covering all members. The exit code
MUST be non-zero when any member failed, so scripts can detect a partial
set and re-run to attach.

#### Scenario: Batched create reports per member

- **WHEN** `thegn wt new x --project shop --json` is run
- **THEN** one JSON object reports each member repo's outcome, and the exit
  code is non-zero if any member failed
