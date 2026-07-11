# Control Plane

## ADDED Requirements

### Requirement: Headless daemon owns PTYs

A long-lived daemon SHALL own the `portable-pty` panes and their emulator state, registering itself in the `daemons` table so that panes survive a UI client detaching, and this MUST NOT alter the event loop's ~0%-idle contract: the loop still blocks on `poll_input(None)` with no polling timeout, and all daemon I/O reaches it off-loop via the tokio mpsc channel plus a `TerminalWaker` pulse.

#### Scenario: Pane survives client detach

- **WHEN** the only attached UI client detaches while an agent process keeps writing to a pane
- **THEN** the daemon keeps the PTY and its emulator state alive (the process is not killed) and continues recording its output

#### Scenario: Daemon work stays off the render loop

- **WHEN** the daemon is attached but no pane output, chrome, or geometry change occurs
- **THEN** the UI event loop receives zero wakes from the daemon and the render plan is `Skip`

### Requirement: Warm-reattach to a running session

A client SHALL be able to warm-reattach to a daemon-owned session and MUST receive the current emulator screen as an initial snapshot followed by a live delta stream, and an inbound pane delta MUST mark only the affected pane dirty so the render plan is `Panes` (a bounded pane diff) rather than a chrome recompose.

#### Scenario: Reattach restores live screen

- **WHEN** a client reattaches to a session whose agent has been running while detached
- **THEN** the client first renders the daemon's current emulator snapshot and then applies subsequent live deltas

#### Scenario: Streaming output is a pane-only frame

- **WHEN** a pane delta arrives from the daemon with no chrome/geometry change
- **THEN** the render plan resolves to `Panes` and does not recompose chrome

### Requirement: Control API drives a running instance

The daemon SHALL expose a control API (HTTP/gRPC plus an SSE/WebSocket event feed) gated by scoped tokens, and `thegn` CLI verbs (open worktree, send-to-terminal, snapshot, drive-browser) MUST drive a running instance through this API, degrading gracefully when no daemon is running; the API transport runs entirely off the render loop and never introduces a polling timeout.

#### Scenario: CLI verb reaches the live instance

- **WHEN** the user runs a `thegn` send-to-terminal verb against a running daemon
- **THEN** the input is delivered to the live pane over the control API and reflected in the attached UI

#### Scenario: Scope is enforced

- **WHEN** a client calls a control verb with a token lacking the required scope
- **THEN** the request is rejected without performing the action

#### Scenario: No daemon present

- **WHEN** a `thegn` control verb runs and no daemon is running
- **THEN** the CLI degrades gracefully with a clear message rather than crashing

### Requirement: Serve mode pairs thin clients over a pairing URL

`thegn serve` SHALL advertise a pairing URL that desktop, web, or mobile thin clients use to pair and attach over the control API, where each pairing issues a scoped token stored hashed in the `pairings` table, and the pairing/approval prompt MUST render as a chrome overlay (resolving to a `Full` frame) without adding any polling timeout to the event loop.

#### Scenario: Thin client pairs and attaches

- **WHEN** a thin client redeems a valid, unexpired pairing URL
- **THEN** it receives a scoped token and attaches to the session over the control API

#### Scenario: Revoked pairing is refused

- **WHEN** a client attempts to pair with a revoked or expired pairing token
- **THEN** the attach is refused and no session access is granted

### Requirement: Persistent relay keeps remote sessions alive across disconnect

When the last client detaches, the daemon SHALL open a grace-period lease (recorded in `session_leases`) that keeps the remote PTY and emulator state warm instead of tearing it down; a client reconnecting within the lease MUST resume the same session state, and lease expiry MUST reap the PTY — all lease bookkeeping happens off-loop and MUST NOT add a polling timeout to the event loop.

#### Scenario: Reconnect within the grace period resumes warm

- **WHEN** a remote client disconnects and reconnects before its session lease expires
- **THEN** it resumes the same warm emulator state without restarting the process

#### Scenario: Lease expiry reaps the session

- **WHEN** no client reconnects before the lease expires
- **THEN** the daemon reaps the PTY and releases the lease

### Requirement: Mobile companion monitors and lightly controls a paired instance

The mobile companion SHALL be a read-mostly client over the control API and AK event feed that can monitor agents and activity (including push notifications), stage and commit changes via the GitBackend seam, and switch accounts/scopes; it MUST operate entirely through the scoped control API with no hard dependency on any AI layer, and read-only views MUST require only a read scope.

#### Scenario: Monitor and receive activity

- **WHEN** a paired mobile client holds a read scope
- **THEN** it can view agent activity and receive push notifications without any write or AI capability

#### Scenario: Stage and commit from mobile

- **WHEN** a paired client with the appropriate scope stages and commits changes
- **THEN** the operation routes through the GitBackend seam against the worktree, with git remaining the source of truth

#### Scenario: Switch account or scope

- **WHEN** the user switches account or scope in the companion
- **THEN** subsequent control-API calls are authorized under the newly selected scope
