# Agent

## ADDED Requirements

### Requirement: A stage dispatch is performed atomically when asked

thegn SHALL let a supervisor ask it to perform one stage dispatch
(`thegn session open --stage --issue`) and SHALL compose the whole mechanism
itself: insert the roster row, render the stage's configured prompt from the
bindings the caller provides, refuse to launch on an empty render, derive the
row's artifact path, open the daemon session (the launch layers the stage's
`model` / `env` / `permissions` over the agent entry and seeds the effective
allow-list — the same path every launch takes), stamp the row with the session
id and artifact path, and only then mark the row running. thegn MUST NOT
decide whether the dispatch is worth making, which stage comes next, or
whether the result is good — those are the supervising agent's judgment. A
stage prompt that renders empty MUST be refused with no session opened (an
empty prompt means an interactive launch, silently — the pilot's
silent-failure mode).

#### Scenario: A stage's empty rendered prompt is refused

- **WHEN** a stage's rendered prompt is empty
- **THEN** the dispatch is refused with an error naming the stage, and no
  session is opened and no row is stamped running

#### Scenario: The dispatch is one atomic step per worker

- **WHEN** a supervisor dispatches one worker for a stage
- **THEN** the roster row is inserted before the session opens, the row is
  stamped with the session id and artifact path once the session exists, and
  the row reads `running` with a resumable identity afterwards

#### Scenario: A failed session open leaves a failed row

- **WHEN** the session open fails after the roster row was inserted
- **THEN** the row is left `failed` (not `queued`), the error names the row id,
  and nothing is left running that the roster does not record

#### Scenario: A row survives a crash between insert and open

- **WHEN** the process dies after the row was inserted but before the session
  opened
- **THEN** the operator is left with a visible re-drivable row, never a live
  agent nobody has a record of

#### Scenario: Issue content is data, never a template

- **WHEN** an issue body contains literal braces (including placeholder-shaped
  text such as `{issue_body}`)
- **THEN** the rendered stage prompt contains them verbatim — a substituted
  value is never re-parsed, so a value cannot inject a placeholder

### Requirement: Stage permissions ride the launch, never a second seeder

`[[pipeline.stages]]` SHALL carry an optional `permissions` list of
tool-permission patterns in the harness's own vocabulary, and a stage dispatch
SHALL apply them through the daemon's launch path — the stage's list replaces
the agent entry's when non-empty, and the *effective* list is written into the
harness's per-worktree settings file by the one seeder every launch path uses
(`agent_permissions`, harness-aware). thegn MUST NOT keep a second,
CLI-side seeder for the dispatch: one file, one writer. The file contract is
the launcher's: every other key and value already in the document is
preserved, and a file thegn cannot parse or whose shape it does not understand
is refused rather than overwritten. thegn MUST NOT interpret the patterns.

#### Scenario: Unrelated keys survive the seed

- **WHEN** an existing settings file holds unrelated keys (model, MCP toggles,
  a deny list) alongside a permissions allow-list
- **THEN** after the seed every unrelated key holds the same value; only
  `permissions.allow` is rewritten to the effective list (the entry's, or the
  stage's when the stage configures one)

#### Scenario: A file thegn does not understand is refused, not overwritten

- **WHEN** the existing settings file is not valid JSON, or its required
  nesting is not the expected shape
- **THEN** the seed is refused (best-effort at launch: a warning, and the
  launch proceeds), and the file is left byte-identical

#### Scenario: A stage with no permissions inherits the entry's list

- **WHEN** a stage's `permissions` list is empty or omitted
- **THEN** the launch seeds the agent entry's list (if any), no settings file
  is created for the stage's sake alone, and the config validates clean

### Requirement: Run completion is verified, not claimed

The roster's `done` outcome SHALL be gated on the row's recorded artifact: for
a row that carries an `artifact_path`, `set-status done` MUST verify the
artifact exists under the worktree AND is tracked by git, and MUST refuse with
the reason(s) printed verbatim otherwise — an untracked artifact is not a
handoff (git is the source of truth). A row with no artifact MUST NOT be gated
(plain dispatches predate stages; gating them breaks every non-pipeline user
while catching nothing). Uncommitted changes in the worktree MUST be reported
but MUST never block. Every non-`done` outcome (`failed`, `abandoned`,
`merged`) MUST remain ungated so a supervisor can always record a bad outcome.

#### Scenario: A written-but-uncommitted artifact is refused and named

- **WHEN** a roster row's artifact is written but not committed
- **THEN** `set-status done` is refused and the reason names the artifact and
  says to commit it

#### Scenario: A row without an artifact is not gated

- **WHEN** a row carries no artifact
- **THEN** `set-status done` proceeds without a verification step

#### Scenario: A dirty worktree is reported, never blocking

- **WHEN** the artifact exists, is tracked, and the worktree has uncommitted
  changes
- **THEN** the verification passes and reports the dirt for the supervisor to
  judge

#### Scenario: Verification is inspectable without mutating

- **WHEN** a supervisor asks whether a row's completion claim would pass
- **THEN** a read-only verification reports ok / reasons / dirty without
  changing the row

### Requirement: A wake primitive waits on live workers only

thegn SHALL let a supervisor block until a dispatched worker's session exits —
for one explicit roster row, or for any current worker (`--any`). A row is
waitable only while its status is `Spawning` or `Running` AND it carries a
non-empty session id; each unwaitable case MUST be its own named error (no such
row; row not spawning/running; row has no session; nothing active) so the
operator message is specific. `WaitingHuman`/`PrOpen` rows MUST NOT be
waitable even though they count as active: their worker already finished, so
including them would make an any-wait return instantly and forever, starving
the real wait.

#### Scenario: An any-wait targets exactly the live workers

- **WHEN** the roster holds queued, spawning, running, parked and finished rows
- **THEN** the any-wait selects, in roster order, only the spawning/running
  rows that carry a session id, each with its id, session, stage and issue for
  the wake message

#### Scenario: An unwaitable row is explained, not guessed

- **WHEN** an explicit row id names a row that does not exist, is not
  spawning/running, or has no session
- **THEN** the wait is refused with the corresponding named error and the row
  id in operator language

### Requirement: Agent resolution sees a fresh registry without a reload verb

The daemon's agent resolution SHALL read a fresh agent/tool/pipeline registry
per request — the daemon's boot config with ONLY its `agents`, `tools` and
`pipeline` registries replaced from the current on-disk config — so an
`[[agents]]` rename after daemon boot resolves instead of failing stale. The
refresh MUST be narrow: any `--set`/`--config` override the daemon booted with
MUST survive, so a wholesale re-load MUST NOT be used.

#### Scenario: A renamed agent resolves after daemon boot

- **WHEN** an `[[agents]]` entry a stage names is renamed after the daemon
  started
- **THEN** the next stage dispatch resolves against the renamed registry
  without restarting the daemon or calling a reload verb

#### Scenario: Boot-time overrides survive the refresh

- **WHEN** the daemon was started with `--set`/`--config` overrides and a
  registry refresh runs
- **THEN** only the agent/tool/pipeline registries are taken from disk and
  every other configured value is still the daemon's booted one
