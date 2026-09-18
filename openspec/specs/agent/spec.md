# Agent

## Purpose

thegn's agent surface is a thin, config-driven launcher: coding agents are
external CLIs the user declares in config, launched as ordinary pane processes
inside the worktree's sandbox boundary. thegn remembers which agent a worktree
runs, carries agent logins into sandboxes, and folds agent output into the
per-worktree activity signal. There is no embedded agent harness and no model
traffic routing — the AI/agent layer (LLM proxy, ACP harness, managed pi) was
removed from the codebase before the public alpha; the shell is AI-free and any
future AI layer must be strictly additive.

## Requirements

### Requirement: Agents and tools are config-driven argv launchers

thegn SHALL let users declare coding agents (`[[agents]]`) and per-worktree
tools (`[[tools]]`) in config as named argv commands, and the picker SHALL
offer every configured agent, then every tool, then a literal `shell` entry
(the `__shell__` sentinel resolves to a login shell). Launching an entry MUST
compose its sandbox-wrapped argv + env and spawn it as the pane's own process.

#### Scenario: A configured agent launches as the pane process

- **WHEN** a user picks a configured `[[agents]]` entry for a worktree
- **THEN** its command is resolved to a sandbox-wrapped argv and spawned as
  that pane's own process

#### Scenario: The shell sentinel launches a plain shell

- **WHEN** a user picks the `shell` entry (or an agent whose command is
  `__shell__`)
- **THEN** the pane runs a plain login shell rather than an agent

### Requirement: The worktree remembers its agent

thegn SHALL record which agent a worktree runs (the DB `worktrees.agent`
column) so the choice survives across sessions and attributes per-worktree
state: session resurrection relaunches the remembered agent, the sidebar agent
marker reflects it, and activity signals distinguish agent-bearing worktrees
from plain shell/tool panes.

#### Scenario: The agent choice survives a restart

- **WHEN** a worktree was created with a configured agent and thegn restarts
- **THEN** the worktree's remembered agent is restored from `worktrees.agent`
  and used for resurrection and attribution

### Requirement: Agent logins sync into sandboxes

When a worktree's interactive process runs in a provider sandbox, thegn SHALL
upload the relevant agents' host config/credential files so the agent is
logged in there. Auth-critical files (a small explicit allowlist) MUST always
be uploaded first without a budget check; the remaining config tree is
uploaded best-effort under a time budget with bounded concurrency, and
executable bits MUST be preserved.

#### Scenario: Auth-critical files guarantee a usable agent

- **WHEN** an agent's login sync runs against a slow provider and the full
  config tree cannot finish within the budget
- **THEN** the auth-critical allowlist has already been uploaded, so the agent
  is authenticated and usable; only non-critical extras may be missing

### Requirement: Agent output feeds the activity signal

Unsolicited PTY output from panes of an agent-bearing worktree SHALL count as
a busy signal for the activity state machine, so an agent that is working but
using ~0% CPU (blocked on a model response, redrawing a spinner) is not marked
waiting mid-turn. Output within the solicited-echo gap after user input,
output from freshly spawned panes (spawn grace), and panes whose spawn program
is a shell or a configured tool MUST NOT count.

#### Scenario: A spinner keeps the agent working

- **WHEN** an agent-bearing worktree's pane keeps emitting unsolicited output
  with no user keystrokes
- **THEN** the worktree's activity stays busy rather than flipping to waiting

#### Scenario: A quiet agent flips to waiting

- **WHEN** the agent stops emitting output and its CPU signal is quiet past
  the grace period
- **THEN** the worktree's activity flips to waiting (unread)

### Requirement: Sandboxes provision the configured agents

The `[[agents]]` list SHALL be the source of truth for which coding-agent CLIs
a sandbox provisions (install + login carry), deduplicated by agent kind
(`provider`, else the command's program basename). The `[sandbox.home] agents`
list MUST override it with an explicit install list, and only when no
`[[agents]]` are configured at all does thegn fall back to detecting the
host's agents.

#### Scenario: An explicit install list overrides the picker set

- **WHEN** `[sandbox.home] agents = ["claude"]` is set alongside several
  `[[agents]]` entries
- **THEN** the sandbox installs and carries login for `claude` only

### Requirement: The agent layer is strictly additive

The shell SHALL function fully with no agent configured; agent features MUST
NOT be a hard dependency of the AI-free shell.

#### Scenario: No agent configured

- **WHEN** no agent is configured
- **THEN** the shell operates normally with agent features simply unavailable

### Requirement: The pipeline board has a door that does not depend on tab position

The agent-pipeline board SHALL be reachable by a single named action
(`open-pipeline-board`) that opens the system monitor directly on the board,
independently of where the board sits in the monitor's tab order.

The action MUST be a first-class action — a keymap variant with a round-tripping
id, an `ActionSpec` carrying a label, a hint and search keywords, a palette
entry, and a help page that both claims the id and mentions it in prose — so it
is rebindable, discoverable and gated like every other action.

Its default chord MUST be deliverable by a legacy-encoding terminal: it SHALL
NOT be a `Ctrl`+letter chord whose control code collides with an existing
control character, nor a `Ctrl`+digit.

Invoking it while the monitor already shows the board SHALL close the monitor;
invoking it while the monitor shows another tab SHALL move to the board rather
than closing. When the board is not present on this machine — no roster row and
no configured pipeline — the action SHALL say so rather than landing the user on
an unrelated tab with no explanation.

#### Scenario: The board is reachable when no digit indexes it

- **WHEN** every monitor tab family is present, so the board sits past the
  ninth visible tab and no digit key selects it
- **THEN** the `open-pipeline-board` action still opens the monitor on the board

#### Scenario: The same door closes what it opened

- **WHEN** the action is invoked while the monitor is already showing the board
- **THEN** the monitor closes

#### Scenario: An open monitor jumps rather than closing

- **WHEN** the action is invoked while the monitor is open on another tab
- **THEN** the monitor moves to the board and stays open

#### Scenario: No pipeline is honest about it

- **WHEN** the action is invoked with an empty roster and no configured stages
- **THEN** the board is not shown and the user is told why

### Requirement: The monitor hands back the keys it does not own

The system-monitor modal SHALL treat a key it does not implement as **not
handled** and let the global keymap have it, rather than consuming it silently.

Chords in the `Alt`/`Super` layer (including `Ctrl Alt …`) belong to the
compositor and MUST be handed back, so the chord that opened the monitor can
close it. The global key-lock chord MUST reach the key lock rather than closing
the monitor. `Ctrl-C` remains a close, and a plain `Ctrl` chord the monitor does
not implement remains consumed — the modal owns the keyboard except where it
explicitly does not.

#### Scenario: The opening chord toggles the modal shut

- **WHEN** an `Alt`-layer chord is delivered to the open monitor
- **THEN** the monitor reports the key as unhandled and the global keymap runs
  the action bound to it

#### Scenario: Key lock is not a close

- **WHEN** the key-lock chord is pressed while the monitor is open
- **THEN** the monitor stays open and the key lock toggles

### Requirement: The monitor reopens on the tab it was left on

The tab shown when the monitor closes SHALL be recorded as the tab the next open
lands on. Every path that moves the tab — cycling, the tab digits, and a direct
jump to a named tab — MUST record it, and the recording MUST reach the same
persistence path the monitor's other remembered preferences use.

#### Scenario: A tab switch is remembered

- **WHEN** the user switches the monitor to another tab and closes it
- **THEN** the next open shows that tab

### Requirement: A dispatch records when it was dispatched, in milliseconds

A roster row's dispatch timestamp SHALL be stored in the unit its column
declares (milliseconds). A row written now MUST read back as seconds old on the
board and MUST NOT read as blocked since the epoch on the sidebar.

#### Scenario: A fresh row is not two decades old

- **WHEN** a dispatch is recorded and the board renders it immediately
- **THEN** its age reads in seconds

### Requirement: The monitor is findable from the palette by any of its tabs

The system-monitor action's search keywords SHALL name every tab family it can
show, so a palette query for a family — containers, the container engines, the
pipeline board — finds the monitor.

#### Scenario: A tab name finds the monitor

- **WHEN** the command palette is queried for a monitor tab family by name
- **THEN** the system-monitor action is among the results

### Requirement: A configured agent can be resolved to a headless command

thegn SHALL let a background task name a configured `[[agents]]`/`[[tools]]`
entry instead of restating a command line, and SHALL resolve that name to a
non-interactive command that accepts a task prompt as an argument. Resolution
MUST derive the entry's provider from its explicit `provider` field or, absent
one, from its command's program basename, and MUST fall back to appending the
prompt as an argument (with a config warning) for a provider it does not
recognize, so an unrecognized agent still runs rather than being refused. An
explicit command template MUST take precedence over a named agent, so any agent
remains configurable regardless of what thegn knows about it.

#### Scenario: A named agent resolves to its headless form

- **WHEN** a background task is configured with `agent = "claude"` and no command
  template, and `[[agents]]` declares an entry named `claude`
- **THEN** the task runs that entry's program with its provider's non-interactive
  flags and the rendered prompt as an argument

#### Scenario: An unrecognized provider still runs

- **WHEN** the named entry's provider is one thegn has no headless flags for
- **THEN** the entry's command is run with the prompt appended as an argument and
  a configuration warning is recorded, rather than the task being skipped

#### Scenario: An explicit command template wins

- **WHEN** both a command template and a named agent are configured
- **THEN** the command template is used verbatim and the named agent is ignored

#### Scenario: Neither configured means no agent

- **WHEN** neither a command template nor a named agent is configured
- **THEN** no agent is dispatched and the task falls back to notifying

### Requirement: A configured agent launches by name through the control plane

thegn SHALL let a control-plane session open name a configured agent and an
optional prompt, and resolve it through the same composition an interactive
pane gets — sandbox wrapping, credential directories, bundle/identity
environment, resource cap, and the `worktrees.agent` binding — never a
reimplementation of that path. A recognized provider id MUST resolve even when
no matching `[[agents]]` entry exists; an unknown name MUST be an error, never
a guessed command. When a prompt is given the launch defaults to the harness's
headless form (overridable), with the prompt passed under the agent-task
engine's shell-quoting contract.

#### Scenario: A daemon-launched agent equals a TUI-launched agent

- **WHEN** a caller opens a session with an agent name and a worktree
- **THEN** the spawned process has the same sandbox, credentials, and
  environment as if the agent were launched from the worktree wizard, and the
  worktree's agent binding is recorded when requested

#### Scenario: A bare provider id works without config

- **WHEN** the caller names a recognized provider id on a host whose config has
  no `[[agents]]` entries
- **THEN** the launch resolves to that provider's command rather than failing
  on the operator's naming choices

#### Scenario: An unknown agent name is refused

- **WHEN** the caller names an agent that is neither configured nor a
  recognized provider id
- **THEN** the open fails with an error naming the agent, and nothing is
  spawned

### Requirement: An issue task kind seeds dispatched workers

The agent-task engine SHALL provide an issue task kind whose prompt template
renders the issue's number, title, body, URL, branch, and worktree, with a
built-in default prompt and the engine's quoting contract, so a worker
dispatched against an issue starts with the task in its prompt rather than
only environment variables. Issue dispatch MUST resolve the configured agent
(or a named one) rather than hardcoding a vendor, and MUST keep recording the
dispatch in the roster and linking the issue to the worktree.

#### Scenario: A dispatched worker receives the rendered issue prompt

- **WHEN** an issue is dispatched to an agent in a worktree
- **THEN** the agent launches with a prompt rendered from the issue's fields
  (quoted safely), the dispatch is recorded, and the issue is linked to the
  worktree

#### Scenario: Issue content cannot escape the quoting contract

- **WHEN** an issue body contains shell metacharacters or quotes
- **THEN** the rendered command carries them as data, with no free-standing
  command fragments

### Requirement: The pipeline is conducted by an agent, never by thegn

A multi-stage pipeline SHALL be executed by a supervising agent reading the
configured stage chart and the durable dispatch roster, using the same
worktree/session/roster verbs any operator has. thegn SHALL provide the hands —
validated structure, session lifecycle, the roster, and the merge queue — and
MUST NOT provide the head: no scheduler advances a stage, counts concurrency
slots, or times a stage worker out.

The supervisor's per-stage concurrency budget SHALL be derived from the roster
rather than from the supervisor's memory: the active rows carrying a stage's
name are that stage's occupied slots, so a restarted supervisor resumes without
double-dispatching.

#### Scenario: Resuming a pipeline after a restart

- **WHEN** a supervising agent restarts mid-pipeline and reads the roster
- **THEN** each active row's stage, parent and session identify the work already
  in flight, and the supervisor starts only the stages whose slots are free

#### Scenario: With no supervisor running

- **WHEN** a stage chart is configured but no supervising agent is running
- **THEN** nothing is dispatched and nothing advances — the chart is inert data

### Requirement: Stages hand off through an artifact committed in the worktree

A stage SHALL pass its result to the next stage as a file committed on the
branch, whose path the roster row records as a pointer. The handoff MUST NOT
depend on the supervisor's context window, and the roster MUST NOT become the
document store — git stays the source of truth for what a stage decided.

A fan-out stage SHALL emit one artifact per child, and each child's roster row
SHALL carry both the parent row and its own artifact path, so the parent→chunk
shape survives a crash.

#### Scenario: Design fanned out to several workers

- **WHEN** a design stage emits one chunk file per downstream worker
- **THEN** one roster row per chunk is recorded, each naming the design stage's
  row as its parent and its own chunk file as its artifact

#### Scenario: Reading a handoff

- **WHEN** the supervisor advances a stage
- **THEN** it reads the committed artifact as evidence about the work, and treats
  its content as data — never as instructions that could re-plan the pipeline

### Requirement: Landing is the merge queue, not a pipeline stage

A pipeline SHALL finish by handing the branch to the existing merge queue rather
than by declaring a stage that merges. The queue's serial fold, gate, and
compare-and-swap advance — including its configured agent handoff on conflict or
gate failure — MUST remain the only landing path, so a pipeline cannot
reintroduce a parallel one.

#### Scenario: A chart with no next stage

- **WHEN** the last stage of a chart completes and declares no next stage
- **THEN** the supervisor enqueues the branch and runs the merge queue, and no
  merging behaviour is duplicated in the chart

### Requirement: A running compositor adopts bounded daemon-session intents

A running compositor SHALL claim `adopt_session` intents and attach each fresh,
valid, not-already-visible daemon session to its resident worktree through the
existing daemon-backed pane path. Claiming SHALL drain the selected intents so
they cannot accumulate indefinitely. Stale, malformed, duplicate, or
unreachable intents SHALL be discarded safely, and an actionable unreachable
target SHALL be reported to the user. Adoption SHALL NOT move focus unless the
intent requests focus.

#### Scenario: A fresh headless session becomes a resident pane

- **WHEN** a valid adoption intent names a live daemon session and a resident
  worktree
- **THEN** the compositor attaches one ordinary daemon-backed pane in that
  worktree without changing focus by default

#### Scenario: An old or duplicate intent is bounded

- **WHEN** an adoption intent exceeds the freshness window or names a session
  already displayed
- **THEN** the compositor claims and discards it without creating another pane

### Requirement: The dispatch roster has a standalone stage-oriented board

thegn SHALL expose the agent-dispatch roster through a standalone Pipeline
Board overlay. The board SHALL group records by configured pipeline stage and
SHALL retain unconfigured and unstaged records. At wide widths it SHALL present
stage columns; at narrow widths it SHALL present an equivalent stacked layout.
Configured stages with no current record SHALL remain visible.

Rows SHALL expose the dispatch identity and bounded operational facts available
from the roster and configuration, including status, stalled state, agent,
concurrency, issue or artifact, age, and next-stage information. Selection
SHALL be preserved by stable dispatch ID across refreshes when the selected
record remains present.

The board SHALL be read-only over dispatch state: rendering or activating it
MUST NOT advance a stage or enforce a concurrency policy.

#### Scenario: Wide and narrow layouts preserve stage meaning

- **WHEN** the same roster is rendered at wide and narrow terminal widths
- **THEN** the board uses columns and stacked groups respectively without
  dropping configured, unconfigured, or unstaged work

#### Scenario: A refresh preserves a surviving selection

- **WHEN** roster ordering changes but the selected dispatch ID remains present
- **THEN** the board keeps that dispatch selected

#### Scenario: Empty configured stages communicate pipeline shape

- **WHEN** a configured stage has no current dispatch
- **THEN** the stage remains represented with an empty-state treatment

### Requirement: Board activation uses the shared worktree path

Activating a board row SHALL navigate through the shared worktree activation
path. A resident target SHALL open directly. A dormant target that can be
materialized from persisted worktree evidence SHALL use the shared fallback.
An unreachable target SHALL surface failure instead of silently doing nothing.

#### Scenario: A dormant dispatch target can be opened

- **WHEN** the selected dispatch references a known worktree that is not
  currently resident
- **THEN** activation materializes and opens that worktree through the shared
  fallback path

### Requirement: Roster hydration adds no new wake source

Roster reads SHALL execute off the event loop and return through the existing
refresh and waker path. The compositor MAY take an initial or dirty-triggered
sample while the board is closed, but periodic sampling SHALL run only while
the board is open. An unchanged or stale result SHALL NOT request a repaint.

#### Scenario: A closed board has no polling cadence

- **WHEN** the Pipeline Board is closed and the roster is not marked dirty
- **THEN** no periodic roster read is scheduled for the board

#### Scenario: An unchanged sample does not repaint

- **WHEN** a current roster sample equals the compositor's existing roster
- **THEN** the sample produces no frame damage

### Requirement: Pipeline roster damage follows visible consumers

When the Pipeline Board is closed, a roster change that affects only derived
sidebar stage evidence SHALL produce at most a sidebar-scoped incremental
frame. When the Pipeline Board is open, a changed roster SHALL request a full
frame under the existing boxed-overlay rule. Pane output without an open
overlay SHALL remain eligible for pane-scoped incremental rendering.

#### Scenario: Closed-board stage evidence is bounded

- **WHEN** a roster refresh changes only a worktree's derived sidebar stage and
  the board is closed
- **THEN** rendering is limited to a sidebar-scoped incremental frame

#### Scenario: An open board follows the overlay rule

- **WHEN** a changed roster is delivered while the board is open
- **THEN** the compositor requests a full frame for the boxed overlay

### Requirement: Dispatch timestamps have one rendering unit

Dispatch persistence and hydration SHALL present Unix-millisecond timestamps to
age and stalled calculations. Historical plausible Unix-second values SHALL be
normalized at the persistence or read boundary before reaching board rendering.

#### Scenario: A historical seconds value has a truthful age

- **WHEN** a historical dispatch row stores its timestamp in Unix seconds
- **THEN** hydration normalizes it to milliseconds before the board computes
  age or stalled state

### Requirement: Worktrees expose derived live-stage evidence

A worktree with an active staged dispatch SHALL expose its current stage as
derived sidebar evidence. Terminal dispatches SHALL NOT leave a live-stage
claim behind. Presentation hierarchy and placement are governed by the
separate sidebar-pipeline contract.

#### Scenario: A terminal dispatch clears live-stage evidence

- **WHEN** a worktree has no non-terminal staged dispatch
- **THEN** its derived live-stage evidence is absent

### Requirement: A stage dispatch is performed atomically when asked

thegn SHALL let a supervisor ask it to perform one stage dispatch
(`thegn session open --stage --issue`) and SHALL compose the whole mechanism
itself: insert the roster row, render the stage's configured prompt from the
bindings the caller provides, refuse to launch on an empty render, derive the
row's artifact path, open the daemon session (the launch layers the stage's
`model` / `env` / `permissions` over the agent entry and carries the effective
allow-list as a command-scoped grant — the same path every launch takes), stamp the row with the session
id and artifact path, and only then mark the row running. thegn MUST NOT
decide whether the dispatch is worth making, which stage comes next, or
whether the result is good — those are the supervising agent's judgment. A
stage prompt that renders empty MUST be refused with no session opened (an
empty prompt means an interactive launch, silently — the pilot's
silent-failure mode). Publishing the opened process MUST transition only its
still-queued/spawning reservation. If a concurrent supervisor already failed,
abandoned, or otherwise moved the row, thegn MUST preserve that verdict and
tear the newly opened process down rather than resurrecting the row.

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

#### Scenario: A concurrent verdict is not resurrected after open

- **WHEN** a supervisor changes the reserved row while the daemon session is
  opening
- **THEN** publication refuses without overwriting that status and the newly
  opened session is torn down as an orphan

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

### Requirement: Stage permissions ride the launch command, never the worktree

`[[agents]]` entries and `[[pipeline.stages]]` SHALL carry an optional
`permissions` list of tool-permission patterns in the harness's own
vocabulary; the stage's list replaces the agent entry's when non-empty. The
_effective_ list SHALL be granted to exactly one process through the
harness's documented, command-scoped mechanism
(`Harness::session_permission_args`, the `PERMISSIONS` capability — for
claude, `--settings` carrying `{"permissions":{"allow":[…]}}`), rendered as
argv with every token shell-quoted, on every launch shape (fresh interactive,
headless, resume, continue, fork; local, sandboxed and remote alike). Launch
preparation MUST NOT read, create or write any file under the worktree for
this purpose, so repository-controlled paths (symlinks, FIFOs, hard links)
cannot redirect it, concurrent launches cannot inherit one another's grant,
and nothing persists after the process. The user's and repository's own
harness settings (deny rules, hooks, unknown keys) are never modified and keep
applying: the grant is an additional layer, not a replacement. A harness
without an attested command-scoped mechanism MUST refuse a non-empty list
(fail closed) — never drop it, never write it to a file, never substitute a
skip-permissions mode — and a stage dispatch MUST be refused before its roster
row is claimed. thegn MUST NOT interpret the patterns.

#### Scenario: A permissioned launch leaves the repository untouched

- **WHEN** an agent with a non-empty effective list launches in a worktree
  whose `.claude/settings.local.json` is a symlink to a missing outside path,
  whose `.claude` is a symlink to an outside directory, or whose leaf is a FIFO
- **THEN** the launch command carries the grant, no outside path is created,
  the FIFO is never opened, and `git status --porcelain=v1 -z` is
  byte-identical before and after

#### Scenario: Concurrent launches keep their own grants

- **WHEN** two stages with different lists launch concurrently in one worktree
- **THEN** each launch command carries exactly its own list and never the
  other's

#### Scenario: A harness that cannot grant command-scoped is refused

- **WHEN** the effective harness (including one a stage swaps in) has no
  command-scoped permission mechanism and the effective list is non-empty
- **THEN** the launch command is refused with a "permission policy hold"
  error, a stage dispatch claims no roster row and spawns nothing, and
  `thegn config validate` reports the entry or stage

#### Scenario: A stage with no permissions inherits the entry's list

- **WHEN** a stage's `permissions` list is empty or omitted
- **THEN** the launch grants the agent entry's list (if any), and a stage
  whose harness can grant it validates clean

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
