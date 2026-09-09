# State Database

## Purpose

thegn persists session and UI state in a local SQLite database so a restart
can resurrect the exact working context. The DB is a cache and resurrection
layer over git (the source of truth), with a versioned schema that migrates
forward deterministically.

## Requirements

### Requirement: Single versioned SQLite store

Persistent state SHALL live in a single SQLite database at `$XDG_STATE_HOME/thegn/thegn.db` in WAL mode with the schema version tracked via SQLite `user_version`, and any schema change MUST bump `user_version` and provide a forward migration.

#### Scenario: Forward migration on open

- **WHEN** the host opens a DB whose `user_version` is older than the current
  schema
- **THEN** the migrations run in order to bring the schema up to date before use

### Requirement: DB is a cache, not the source of truth

The database SHALL function as a cache and resurrection layer, and git MUST remain authoritative for worktrees such that the DB never contradicts git's view.

#### Scenario: Resurrection from cache

- **WHEN** the host restarts
- **THEN** it restores the prior workspace/worktree/tab/pane context (including
  pane working directories) from the DB, reconciled against git's actual state

### Requirement: Test isolation of state

Any test or benchmark that opens the DB or spawns the host SHALL isolate `XDG_STATE_HOME` so it MUST NOT read or mutate the user's real session state.

#### Scenario: Isolated test run

- **WHEN** a test that touches the DB runs while a live thegn session exists
- **THEN** it uses an isolated `XDG_STATE_HOME` and leaves the real database
  unchanged

### Requirement: Tabs persist within worktree groups

The persisted layout SHALL model tabs as belonging to worktree groups (one group per worktree, each with at least one tab and its own active tab) rather than a flat tab list, and resurrection MUST restore each group's active tab.

#### Scenario: Resurrect worktree groups

- **WHEN** the host restarts with multiple worktrees each holding multiple tabs
- **THEN** each worktree group and its previously active tab are restored

### Requirement: Per-pane scrollback is captured on snapshot and repainted on restore

thegn SHALL capture a bounded tail of each pane's scrollback when a session is
persisted and repaint it into the pane on restore, so a resurrected pane shows its
recent history rather than a blank screen. The captured tail MUST be bounded by a
configurable cap, and a snapshot taken before this feature (with no scrollback)
MUST restore exactly as before (an empty pane), requiring a `user_version` bump
with an additive, null-defaulted column.

#### Scenario: A restored pane shows its recent history

- **WHEN** a session with a pane containing scrollback is persisted and later
  restored
- **THEN** the restored pane repaints the captured tail of its scrollback

#### Scenario: An old snapshot restores unchanged

- **WHEN** a snapshot persisted before this feature is restored
- **THEN** its panes restore with no scrollback and no error

### Requirement: Stale agent state is coerced to a settled state on restore

thegn SHALL run each persisted "running"/"active" agent or activity state
through an age-based guard at restore, downgrading any state older than a
configurable grace threshold to a settled state, so a session killed mid-run does
not resurrect a phantom forever-running indicator. States fresher than the
threshold MUST pass through unchanged, and the guard MUST run only at resurrection
without altering the live sticky-state machine.

#### Scenario: A stale running state is downgraded

- **WHEN** a session is restored whose persisted agent state was "running" and
  whose dispatch is older than the grace threshold
- **THEN** the restored state is downgraded to a settled state, not shown as
  running

#### Scenario: A fresh running state survives restore

- **WHEN** a session is restored whose persisted "running" state is newer than the
  grace threshold
- **THEN** the state is restored as running

### Requirement: Repo trust-on-first-use approvals are persisted

The state database SHALL record trust-on-first-use decisions for a repo's gated
sandbox requests in a `repo_trust` table (schema v32, added by the additive
migration ladder), keyed by `(repo_root, canonical request JSON)`. The canonical
request JSON is the security match key; the recorded `request_id` is a display
handle only. Reading the approved set for a repo yields the canonical request
strings whose decision is `approved`.

#### Scenario: An approval is recorded and read back

- **WHEN** a gated request is approved for a repo root
- **THEN** the repo's approved set includes that request's canonical JSON

#### Scenario: A denied request is not in the approved set

- **WHEN** a gated request is denied for a repo root
- **THEN** the repo's approved set excludes it, though the decision is listed

#### Scenario: The table is added without disturbing existing data

- **WHEN** a pre-v32 database is opened
- **THEN** the `repo_trust` table is created additively and existing rows survive

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

### Requirement: Calendar events are cached in the state database

The state database SHALL carry a `calendar_events` table holding one row per
event per account, and a `calendar_sync` table holding each account's provider
cursor, last fetch time, last error, and synced horizon. Both SHALL be added by
an additive migration with a `user_version` bump, so an existing database
upgrades in place without losing its other data.

One row per event, rather than one document per account, is required so that an
incremental sync can apply a single deletion without refetching the account, and
so a one-month query does not deserialize a year.

Both tables are caches — the provider is the source of truth — so dropping them
MUST be safe and MUST result only in a re-sync.

#### Scenario: An older database gains the tables on open

- **WHEN** a database created before the calendar existed is opened
- **THEN** both tables are created, `user_version` is advanced, and every
  pre-existing row in other tables is preserved

#### Scenario: Events survive a restart

- **WHEN** events are synced and thegn is restarted
- **THEN** the calendar shows them without waiting for a new fetch

### Requirement: A range query includes recurrence masters

A query for events in a date window SHALL return every recurring event
regardless of its own start and end, in addition to non-recurring events
overlapping the window. A recurrence master's own span is unrelated to when its
occurrences fall, so filtering it by that span would silently hide a repeating
event from every month after the first.

#### Scenario: A recurring event defined years earlier

- **WHEN** a weekly event whose first occurrence was years ago is queried for the
  current month
- **THEN** the event is returned so it can be expanded

#### Scenario: A finished one-off event

- **WHEN** a non-recurring event that ended before the window is queried
- **THEN** it is not returned

### Requirement: A failed sync never damages the cache

Recording a sync failure SHALL leave the account's cached events and its
provider cursor unchanged, so a transient failure degrades to stale data rather
than to no data, and the next attempt can still resume incrementally. A
subsequent success SHALL clear the recorded error.

A full fetch returning no events SHALL be applied only when the account has
nothing cached. When the account does have cached events, the empty result MUST
be treated as suspect and recorded rather than applied, because a provider or
proxy returning an empty body would otherwise erase the calendar with no error
to explain it.

#### Scenario: A transient fetch failure

- **WHEN** an account's fetch fails
- **THEN** its cached events and cursor are unchanged and the error is recorded

#### Scenario: An empty result against a populated cache

- **WHEN** a full fetch returns no events for an account that has cached events
- **THEN** the cached events are kept and the anomaly is recorded

#### Scenario: An empty result against an empty cache

- **WHEN** a full fetch returns no events for an account with nothing cached
- **THEN** the result is accepted and no error is recorded

### Requirement: The event cache is bounded

Cached events SHALL be pruned on a growth bound so a long-lived install does not
accumulate history indefinitely. Pruning MUST NOT remove recurrence masters,
whose old start dates still generate current occurrences.

#### Scenario: Pruning old events

- **WHEN** the prune runs
- **THEN** non-recurring events that ended long ago are removed and recurring
  events are kept

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

### Requirement: The state DB caches only complete identity-bearing PR reviews

The state database SHALL store at most one complete PR review snapshot per
canonical worktree key in additive schema v63. A row SHALL carry worktree,
branch, PR number, head OID, fetched time, and one atomic payload containing the
PR-head diff plus complete conversation. Reads SHALL reject identity-mismatched
or malformed payloads. A partial or transient fetch SHALL NOT overwrite the last
complete row. This table is a best-effort cache; the forge remains authoritative.

#### Scenario: A complete review refresh succeeds

- **WHEN** both the PR diff and conversation are fetched for the same worktree, branch, PR, and head
- **THEN** one atomic cache row replaces the prior matching review snapshot

#### Scenario: A refresh is partial

- **WHEN** either the diff or conversation fetch fails after a complete row exists
- **THEN** the complete row remains unchanged and may be presented with stale/error status

#### Scenario: Cached identity no longer matches

- **WHEN** a cached row's branch, PR number, or head OID differs from the active review
- **THEN** the row is ignored or labeled stale and its feedback is not attached to the active diff

#### Scenario: A pre-v63 database migrates

- **WHEN** an older database containing unrelated state is opened by an authorized migrator
- **THEN** the `pr_review_cache` table is added idempotently, existing rows survive, and `user_version` advances through the normal migration ladder

### Requirement: The agent-dispatch roster is durable and parseable

The state database's `agent_dispatches` table SHALL serve as the durable
orchestration roster — issue, worktree, agent, dispatch time, status — that a
restarted supervisor reads back to resume without re-dispatching. Statuses
SHALL be a closed, parseable set including terminal outcomes (done, failed)
alongside the lifecycle states, every writer MUST go through the typed status
(never a free string), and reads MUST tolerate legacy or unknown stored
strings by presenting them visibly rather than erroring — the
never-reset-user-data contract applies.

#### Scenario: A finished worker's row is parseable

- **WHEN** a dispatched agent's pane exits and the exit handler records the
  outcome
- **THEN** the stored status is a member of the closed set (done or failed) and
  round-trips through the status parser

#### Scenario: A supervisor resumes from the roster

- **WHEN** a supervisor restarts and lists dispatches
- **THEN** rows still running are distinguishable from finished and abandoned
  ones, so no running row is dispatched twice

#### Scenario: A legacy status string does not break the roster

- **WHEN** a row written before the closed set existed carries an unrecognized
  status string
- **THEN** listing still succeeds, presenting the raw value as unknown rather
  than failing the read

### Requirement: The dispatch roster records pipeline structure

The `agent_dispatches` roster SHALL carry, per row, the pipeline stage it
belongs to, the row it was chunked out of, the session running it, and the path
of its handoff artifact. All four are optional: a dispatch made outside a
pipeline, and every row written before the columns existed, MUST read back as
absent with no change in behaviour. The artifact field SHALL hold a **pointer**
to a file committed in the worktree, never the artifact's content — git remains
the source of truth for what a stage produced.

These columns are **structure, not judgment**: the system SHALL store, group and
render them, and MUST NOT advance a stage, enforce a stage's concurrency, or
expire a stage on a timeout. Stage transitions belong to the supervising agent.

#### Scenario: A pre-existing database gains the columns without losing data

- **WHEN** a state database written before the pipeline columns existed is
  opened
- **THEN** every existing dispatch row survives with its issue, worktree, agent
  and status intact, its pipeline fields read as absent, and the schema version
  stamp advances

#### Scenario: A chunk row records its parent and stage

- **WHEN** a supervisor records a dispatch with a stage, a parent row, a session
  and an artifact path
- **THEN** reading the roster back returns all four alongside the existing
  fields, from both the whole-roster read and the by-id read

#### Scenario: The roster is not a scheduler

- **WHEN** a stage's dispatch reaches a terminal status
- **THEN** no row's stage advances on its own — the next stage exists only when
  a supervisor records it

### Requirement: A finished worker's outcome is attributed to its own row

When a worker finishes, the system SHALL resolve which roster row it was by its
recorded session first, and only otherwise by the most recent **active** row for
its worktree. Rows in a terminal status MUST NOT be selected, and when no row
matches the outcome MUST be treated as "not an agent worker" rather than as an
error or a guess.

#### Scenario: Two stages share one worktree

- **WHEN** two active dispatches exist for the same worktree and one of their
  workers finishes
- **THEN** the outcome is stamped on the row whose session ran that worker, not
  on whichever row is newest

#### Scenario: A finished row is not re-stamped

- **WHEN** an ordinary program exits in a worktree whose dispatches have all
  reached a terminal status
- **THEN** no roster row is modified and no agent-finished notification is
  raised; the exit is handled as an ordinary process exit

#### Scenario: A worker with no recorded session

- **WHEN** a worker launched without a recorded session finishes in a worktree
  with one active dispatch
- **THEN** that active row receives the outcome
