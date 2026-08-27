# CLI

## ADDED Requirements

### Requirement: The roster is writable from the CLI, pipeline columns included

The CLI SHALL append a dispatch row — issue, worktree, agent — and SHALL accept
the pipeline fields (stage, parent row, session, artifact path) on that same
command, so recording a pipeline dispatch needs no second verb and no HTTP
transport. A parent that names no existing row MUST be rejected before anything
is written. The command SHALL support machine-readable output under the CLI's
one-document `--json` convention, and the human roster listing SHALL show each
row's stage and parent.

#### Scenario: Recording a chunk dispatch

- **WHEN** `thegn dispatch put <issue> <worktree> <agent> --stage code --parent
<id> --session <s> --artifact <p> --json` runs
- **THEN** one row is appended and emitted with its new id, its queued status,
  and all four pipeline fields

#### Scenario: A parent that does not exist

- **WHEN** `thegn dispatch put … --parent <unknown-id>` runs
- **THEN** the command fails naming that id, and no row is written

#### Scenario: Listing a mixed roster

- **WHEN** `thegn dispatch list` runs over a roster holding both pipeline and
  plain dispatches
- **THEN** each row shows its stage and parent, with absent values rendered as a
  placeholder so the table stays aligned

### Requirement: A CLI-launched agent can be adopted into a pane

`thegn session open` SHALL accept a flag asking a running compositor to graft the
new session into a real pane, instead of leaving it headless. The flag SHALL
default to off, so a fan-out never takes over the user's screen unasked, and
requesting it MUST remain a nudge rather than a dependency: with no compositor
running the session still opens and stays headless.

#### Scenario: Opening a watchable stage agent

- **WHEN** `thegn session open --agent <a> --worktree <w> --adopt` runs against
  the daemon
- **THEN** the session opens and the request to graft it into a pane is recorded
  for the compositor

#### Scenario: Opening with no compositor attached

- **WHEN** the same command runs with no compositor attached
- **THEN** the session still opens headless and the command succeeds
