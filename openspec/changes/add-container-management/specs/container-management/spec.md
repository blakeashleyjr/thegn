# Container Management

## ADDED Requirements

### Requirement: Management operations are capability-flagged ops on the backend profile table

Container management operations — list, stats, disk usage, logs, lifecycle
control (stop/start/restart/remove), and prune — SHALL be modelled as
optional operations on the existing sandbox backend profile table, where each
backend advertises the ops it supports and an unsupported op is absent rather
than failing. Argv construction and output parsing MUST be pure functions in
`thegn-core` with unit tests; vendor CLI dialects (docker, podman, apple
`container`, …) MUST appear only in the profile table and its builders.
`thegn doctor` SHALL report, per detected backend, which management ops it
supports.

#### Scenario: A backend without a disk-usage op degrades

- **WHEN** the aggregate footprint is computed and a detected backend lacks
  the disk-usage op
- **THEN** the aggregate shows the backends that support it and marks the
  total partial, with no error spam

#### Scenario: Dialects stay in the table

- **WHEN** a management op runs against the apple backend
- **THEN** its dialect differences are expressed by that backend's table
  entry and builders, with no vendor strings at call sites

### Requirement: thegn controls only the containers and resources it owns

Lifecycle control and cleanup SHALL apply only to thegn-owned resources:
container name families thegn creates (the thegn prefix, including the agent
and VPN-sidecar suffix families) and images/volumes labelled `thegn.managed`.
Resources thegn creates SHALL carry the `thegn.managed` label at creation.
Foreign containers MAY be listed read-only for context but MUST be offered no
control or cleanup action on any surface. Every destructive argv MUST carry
the ownership filter by construction: prune builders hard-code the label
filter, control builders refuse names outside the owned families, and unit
tests SHALL assert no destructive argv can be constructed without its filter.

#### Scenario: Foreign container offers no actions

- **WHEN** a container not created by thegn appears in the Containers list
- **THEN** it renders read-only and lifecycle/remove actions are not offered
  for it

#### Scenario: Prune argv always carries the filter

- **WHEN** any prune argv is built for any backend and kind
- **THEN** it includes the thegn ownership filter, and a builder invoked for
  a foreign container name yields no command

### Requirement: The monitor has a Containers tab with per-container stats and lifecycle actions

The system monitor SHALL include a Containers tab, hidden when no container
engine is detected, listing containers across detected backends ours-first
with status, health, backend, and per-container CPU, memory, and network
readings; the tab header SHALL show the aggregate thegn footprint (owned
containers, images, and volumes — counts and bytes) where backends support
the disk-usage op. Owned rows SHALL offer lifecycle actions — shell-in as a
pane, logs tail, stop/restart, and remove with confirmation (a second
confirmation when the container is running). Actions MUST run off the event
loop and report outcomes through the standard status path, surfacing
failures rather than swallowing them.

#### Scenario: Stopping an owned container

- **WHEN** the user invokes stop on an owned container row and it succeeds
- **THEN** the row's status updates on the next refresh and the outcome is
  reported without blocking the loop

#### Scenario: Shell into an owned container

- **WHEN** the user activates shell-in on an owned running container
- **THEN** a pane opens exec'd into that container via the backend's exec
  path

#### Scenario: No engine, no tab

- **WHEN** no container engine is detected on the machine
- **THEN** the Containers tab does not appear

### Requirement: Expensive container sampling is gated on visibility

The ambient refresh SHALL keep only the cheap container listing (names,
status, health) on its fixed cadence. Per-container stats sampling (`stats`)
SHALL run only while a surface displaying per-container readings is visible,
no faster than a minimum interval, and SHALL stop entirely when that surface
closes; aggregate disk usage SHALL be computed on tab open and refreshed at a
slow cadence only while the tab remains open. All engine subprocesses MUST
run on background threads with background QoS and deliver via channel and
waker.

#### Scenario: Closed monitor costs no stats subprocesses

- **WHEN** no per-container-stats surface is open
- **THEN** the periodic refresh runs no `stats` or disk-usage subprocesses,
  only the container listing

#### Scenario: Opening the tab starts sampling

- **WHEN** the Containers tab opens
- **THEN** stats sampling begins off-thread and readings appear as they
  arrive, without a synchronous wait

### Requirement: Orphan GC and prune are on-demand cleanup verbs

thegn SHALL keep the startup orphan sweep (removing thegn containers whose
worktree no longer exists in the registry, across every available OCI
backend) and expose it on demand as `thegn sandbox gc`, reporting what was
removed per backend. `thegn sandbox prune` SHALL remove stopped owned
containers and `thegn.managed` images and volumes, with per-kind narrowing
flags, a dry-run mode, and — on a TTY — a listing plus confirmation before
executing (`--yes` for non-interactive use); volumes whose role labels mark
persistent user state SHALL be skipped and named in the listing. With
`--host <name>`, the same ownership-filtered prune SHALL execute on the
provisioned host over its existing control channel with bounded timeouts.
Cleanup MUST only ever run when explicitly invoked (or the specced startup
sweep) — never on a background schedule.

#### Scenario: On-demand GC removes an orphan

- **WHEN** a thegn container's worktree has been removed from the registry
  and the user runs `thegn sandbox gc`
- **THEN** the container is removed and named in the report

#### Scenario: Dry-run mutates nothing

- **WHEN** the user runs `thegn sandbox prune --dry-run`
- **THEN** the would-be removals are listed with kinds and sizes where known
  and nothing is removed

#### Scenario: Host prune stays inside the label

- **WHEN** the user runs `thegn sandbox prune --host build1 --yes`
- **THEN** only `thegn.managed`-labelled resources on that host are removed,
  via the host control channel

### Requirement: Container management projects three scoped catalog rows

The externally invokable container operations SHALL be capability-catalog
rows dispatched through the single scope policy: `containers.list` requiring
the read scope, `containers.control` (stop/start/restart/logs) requiring the
write scope, and `containers.prune` (gc and prune, with a dry-run parameter)
requiring the admin scope. All surfaces (CLI, control API, gRPC, MCP,
plugins) MUST route through `required_scope(verb)`; the ownership rule
applies unchanged on every surface.

#### Scenario: Prune needs admin

- **WHEN** a control client whose token lacks the admin scope invokes
  `containers.prune`
- **THEN** the call is rejected by the scope check and nothing runs

#### Scenario: One policy table

- **WHEN** the same verb is invoked via CLI and via the control API
- **THEN** both doors enforce the same scope derived from the catalog row,
  with no surface-local policy
