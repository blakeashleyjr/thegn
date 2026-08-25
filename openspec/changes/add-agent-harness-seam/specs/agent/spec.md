# Agent

## ADDED Requirements

### Requirement: Harness knowledge lives behind one provider seam

thegn SHALL define an object-safe `Harness` provider seam
(`thegn_core::harness`) that carries every per-vendor fact about a coding-agent
CLI — credential home resolution, interactive and headless launch forms, login
argv and auth marker, session-store layout, usage and transcript parsers — with
optional operations advertised as capability bits that MUST agree with the
implemented ops. The registry MUST be closed: an id outside it is an error or a
declared `reserved` entry, and thegn MUST NOT synthesize a command for an
unknown harness. Vendor-specific strings and formats MUST appear only inside
that harness's implementation file, and each configured harness MUST report a
probe in `thegn doctor` (binary found, home present, logged in, session store
found).

#### Scenario: A new harness is one implementation, not a sweep

- **WHEN** a new coding-agent CLI is added to the registry with its home,
  launch forms, and parsers
- **THEN** launch resolution, sandbox login-carry, usage gathering, and session
  discovery all pick it up without changes at their call sites

#### Scenario: An unknown harness is refused, not guessed

- **WHEN** a launch or discovery names a harness id outside the registry
- **THEN** the operation fails with an error naming the id rather than running
  a guessed command

#### Scenario: Doctor probes each configured harness

- **WHEN** `thegn doctor` runs with harnesses configured
- **THEN** each reports a probe row covering binary, credential home, login
  state, and session store

### Requirement: Agent session history is discoverable

thegn SHALL discover session records from each harness's local session store —
harness id, session id, associated worktree or project, last-modified time, and
a truncated one-line summary — and expose the list through an `agent.sessions`
capability row (Read scope) projected across the catalog surfaces, including
`thegn agent sessions --json` and an MCP tool. Discovery MUST be a bounded
read-on-demand filesystem scan that runs off the event loop, MUST NOT spawn the
harness or spend tokens, and MUST NOT include credential material or transcript
bodies in results. Sessions in worktrees thegn does not track SHALL still be
listed, marked as unlinked.

#### Scenario: Listing a worktree's sessions

- **WHEN** a caller invokes `agent.sessions` for a worktree whose harness keeps
  local transcripts
- **THEN** the response lists that worktree's sessions newest-first with ids,
  timestamps, and one-line summaries — and no credential contents

#### Scenario: A harness without a session store degrades honestly

- **WHEN** the configured harness does not advertise the session-store
  capability
- **THEN** the list is empty for that harness and the response says the
  capability is absent rather than erroring

### Requirement: A harness session can be resumed

thegn SHALL let an agent launch resume a prior harness session: the control
plane's agent launch accepts an optional session id resolved through the
harness's resume form, and an explicitly named id that fails validation or is
unknown MUST be refused rather than interpolated into a command. Resume ids
MUST pass the same shell-quoting contract as prompts. Resumed launches MUST
compose the same sandbox, credential, and environment path as a fresh launch.

#### Scenario: Resuming a named session

- **WHEN** an agent launch names a discovered session id and the harness
  supports resume
- **THEN** the spawned command is the harness's resume form for that id, run
  through the same sandbox and credential composition as a fresh launch

#### Scenario: An invalid resume id is refused

- **WHEN** an agent launch names a session id that fails shape validation
- **THEN** the launch errors, naming the id, and no process is spawned

## MODIFIED Requirements

### Requirement: The worktree remembers its agent

thegn SHALL record which agent a worktree runs (the DB `worktrees.agent`
column) so the choice survives across sessions and attributes per-worktree
state: session resurrection relaunches the remembered agent, the sidebar agent
marker reflects it, and activity signals distinguish agent-bearing worktrees
from plain shell/tool panes. When the remembered agent's harness supports
resume and its `[[agents]]` entry opts in (`resume = true`, default off),
resurrection SHALL relaunch by resuming the worktree's most recent discovered
session; if no session is discoverable the relaunch MUST fall back to a cold
launch rather than failing resurrection.

#### Scenario: The agent choice survives a restart

- **WHEN** a worktree was created with a configured agent and thegn restarts
- **THEN** the worktree's remembered agent is restored from `worktrees.agent`
  and used for resurrection and attribution

#### Scenario: Opted-in resurrection resumes the last session

- **WHEN** thegn restarts, the remembered agent's entry sets `resume = true`,
  and the harness has a discoverable session for that worktree
- **THEN** resurrection relaunches the agent resuming that session instead of
  starting cold

#### Scenario: Auto-resume falls back to a cold launch

- **WHEN** resume is opted in but no session can be discovered for the worktree
- **THEN** resurrection launches the agent cold, exactly as before this
  capability existed
