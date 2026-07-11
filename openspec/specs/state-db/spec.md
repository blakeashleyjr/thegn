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
