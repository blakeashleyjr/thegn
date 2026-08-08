# Control Plane

## ADDED Requirements

### Requirement: Daemon owns the session model

The daemon SHALL own the session model — the pane-tree layout, focus, and
active group/tab — for each state dir, so the model survives with zero clients
attached. The model MUST be a semantic structure (not a composed framebuffer):
clients render their own chrome from it at their own geometry. All model
mutation MUST stay off the render loop, preserving the ~0%-idle contract.

#### Scenario: The session model outlives every client

- **WHEN** every UI client of a running daemon detaches
- **THEN** the daemon retains the session model (layout, focus, active tab) and a
  later client that attaches sees the same layout, not a fresh one

#### Scenario: The daemon streams a model, not pixels

- **WHEN** a client attaches to the session
- **THEN** the daemon sends a semantic `SessionModel` (worktree groups, tabs, each
  tab's pane tree + focused pane + per-leaf session id), and the client composes
  its chrome locally against its own row/column geometry

### Requirement: Clients apply layout operations over the control API

The control API SHALL expose layout operations — `session_model` (snapshot),
`apply_layout` (one structural mutation), and `subscribe_layout` (a delta
stream) — so a client drives the shared layout without owning it. An
`apply_layout` from an interactive client MUST be authoritative and broadcast to
every attached client as a `LayoutDelta`/`FocusChanged` frame; observer clients
MUST NOT mutate layout. A layout change MUST map to a full chrome repaint on each
client (the sanctioned repaint path), and an idle layout stream MUST NOT wake an
idle client.

#### Scenario: A split in one client appears in another

- **WHEN** an interactive client calls `apply_layout` to split a pane
- **THEN** the daemon applies it, broadcasts a `LayoutDelta`, and every attached
  client repaints its chrome to show the new split

#### Scenario: An observer cannot mutate layout

- **WHEN** an observer client attempts `apply_layout`
- **THEN** the daemon rejects it and the layout is unchanged

#### Scenario: An idle layout stream does not wake an idle client

- **WHEN** a client is attached but no layout or pane activity occurs
- **THEN** no frame is delivered that wakes the client's render loop (it stays at
  the 0%-idle Skip decision)

### Requirement: Whole-session attach and detach as a unit

The control API SHALL let a client attach to, and detach from, an entire session
as one unit — all panes, layout, and focus — via `attach_session` and
`detach_session`. `attach_session` MUST return the `SessionModel` and establish
the per-leaf pane streams in one call. `detach_session` MUST keep the whole
session warm under a per-session-group relay lease, so the work continues with no
client attached and any later client reattaches the same unit.

#### Scenario: Reattaching restores the whole session

- **WHEN** a client detaches a session and later a client calls `attach_session`
- **THEN** the returned model reproduces the prior layout, focus, and active tab,
  and every pane reattaches with its scrollback

#### Scenario: Focus is server-authoritative across clients

- **WHEN** two interactive clients are attached to one session and one moves focus
- **THEN** the daemon broadcasts `FocusChanged` and both clients agree on the
  focused pane, while each client's own cursor/selection stays local
