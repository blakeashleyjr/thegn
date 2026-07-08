# Agent containment

## ADDED Requirements

### Requirement: Agents run sandboxed-by-default

Agents and worktree interactive processes SHALL run inside a container by default and SHALL NOT execute on the host unless the operator passes an explicit `--no-sandbox` opt-out, which MUST be logged. Backend selection falls back `podman` → `docker` → `bwrap` → `none`, with the worktree bind-mounted at its real path so host-side git reads keep working.

#### Scenario: Agent worktree launches containerized

- **WHEN** an agent worktree launches and `--no-sandbox` has not been passed
- **THEN** its interactive process runs inside a sandbox container (the first available of `podman`/`docker`/`bwrap`) and never gains host execution

#### Scenario: Explicit host opt-out is logged

- **WHEN** the operator launches a worktree with the explicit `--no-sandbox` opt-out
- **THEN** the process runs on the host and superzej logs that sandboxing was explicitly disabled

#### Scenario: Sealed agent has no host exec

- **WHEN** the sealed Bouncer agent makes a tool call
- **THEN** the call is routed over the LLM-proxy / unix-socket chokepoint within the sandbox and never executes on the host

### Requirement: No telemetry

superzej SHALL NOT transmit usage data, source code, or prompts off the machine. State is kept in local SQLite under `$XDG_STATE_HOME`; the only outbound traffic SHALL be to endpoints the user has explicitly configured (git remotes, GitHub via `gh`/octocrab, and the LLM proxy the user points at).

#### Scenario: No background beacon

- **WHEN** superzej runs normally with no user-configured remote operation in flight
- **THEN** the binary makes no outbound network request carrying usage, code, or prompt data

#### Scenario: Only user-configured endpoints

- **WHEN** superzej performs a network operation
- **THEN** the destination is an endpoint the user explicitly configured (a git remote, GitHub, or the LLM proxy), never an analytics or telemetry service

### Requirement: superzej remains a viewer / VCS client

superzej SHALL remain a terminal-native viewer / VCS client and SHALL delegate editing to the user's `$EDITOR`. It SHALL NOT embed an in-app code editor (e.g. Monaco), SHALL NOT embed a browser, and SHALL NOT provide a desktop-automation / computer-use surface.

#### Scenario: Editing hands off to $EDITOR

- **WHEN** the user opens a file to edit from within superzej
- **THEN** superzej launches the configured `$EDITOR` rather than editing the file in an embedded editor

#### Scenario: No embedded browser or computer-use

- **WHEN** an agent or workflow needs web content or GUI automation
- **THEN** superzej provides no embedded browser and no computer-use surface; such capabilities are out of scope
