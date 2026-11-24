# Vercel selected runner

## ADDED Requirements

### Requirement: A project-selected Vercel Sandbox executes pinned selected Jobs without a double run

The Vercel selected runner SHALL execute only `Recipe::argv` for a pinned source
on a fresh sandbox it creates for that one attempt, for at most the deadline the
route forwards (clamped to the binding's maximum). Before any ledger write or
request the provider's Vercel team and project MUST equal the binding's, and the
ledger directory MUST be pinned to the first team and project it served. The
runner MUST reserve the attempt number and spend cap in a durable ledger before
any provider call, MUST persist the sandbox name before the create request, MUST
write a dispatch fence before starting the command, and MUST persist the sandbox
and command handles before it trusts a result. A failure to persist the command
handle MUST read back as indeterminate. An unclear command start MUST NOT be
dispatched again. The command request MUST carry no environment and the provider
token, agent home and model credentials MUST NOT enter the sandbox.

#### Scenario: Identity mismatch is refused before any create

- **WHEN** the provider's team or project differs from the binding's
- **THEN** the attempt is refused with no ledger write and no request to Vercel

#### Scenario: Success reports exit, digest and cleanup

- **WHEN** the recipe exits
- **THEN** the root digest of the materialized checkout equals the pinned source,
  the exit code is reported, the sandbox is stopped and deleted, and cleanup is
  done only when the delete was confirmed (otherwise pending, for the reaper)

#### Scenario: Deadline stops owned work

- **WHEN** the forwarded deadline passes before the recipe exits
- **THEN** the runner kills the command, stops the session, deletes the sandbox,
  reports `TimedOut` and does not read outputs

### Requirement: Recorded attempts are reconciled and cancelled only for the recorded identity

Cancel and reconcile SHALL verify the recorded Vercel team and project against
the provider, and the controller tag on the sandbox, before any provider call
that changes state. A live attempt MUST become cancelled or indeterminate and
MUST NOT be rerun. A non-terminal row younger than its staleness age (deadline
plus transfer budget, kill budget and a clock margin) MUST be refused, and every
recovery write MUST be a compare-and-swap under the ledger lock that leaves a
changed row as it is.

#### Scenario: Recovery under another team is refused

- **WHEN** a recorded attempt names a team the provider is not bound to
- **THEN** reconcile and cancel refuse before any request and leave the record
  unchanged
