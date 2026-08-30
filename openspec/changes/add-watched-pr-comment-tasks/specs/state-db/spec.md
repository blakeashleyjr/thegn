# State DB

## ADDED Requirements

### Requirement: The shared dispatch roster stores durable review-task state

The state database SHALL represent watched-PR review tasks as a nullable
projection of `agent_dispatches`, leaving ordinary issue/pipeline dispatch rows
unchanged. Each review row SHALL durably store its task kind, canonical source
key, source revision, bounded prompt, expected PR head, forge-action attempt
count, and optional next forge-action time. A partial unique index over review
task kind and source key SHALL enforce one row per provider thread while
allowing non-review rows to retain NULL review metadata.

These fields SHALL be introduced by additive schema v64 after THE-27's v63
`pr_review_cache` schema. Migration SHALL be idempotent and preserve existing
rows; the change MUST NOT add a per-PR thread fingerprint or agent override to
`pr_queue`.

#### Scenario: A THE-27 database upgrades without losing dispatches

- **WHEN** a schema-v63 database containing ordinary `agent_dispatches` and
  cached review snapshots is opened
- **THEN** schema v64 adds nullable review metadata and the partial unique index,
  preserves every prior row/cache entry, and ordinary dispatch projections
  remain unchanged

#### Scenario: Reconciliation revises one durable row

- **WHEN** a review task with a known source key receives a new source revision,
  prompt, role, and expected head
- **THEN** an atomic upsert updates the same roster id and a second row with that
  task-kind/source-key pair cannot be inserted

#### Scenario: Forge cooldown survives restart

- **WHEN** a transient provider failure records an attempt count and next action
  time for a review task
- **THEN** both values are present after restart and prevent resolution retry
  before the durable cooldown expires

#### Scenario: Resolution is scoped to the current source

- **WHEN** a resolved transition names the durable task id and canonical source
  key
- **THEN** only that matching row becomes done and its forge retry bookkeeping
  is cleared
