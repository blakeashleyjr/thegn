# Control Plane — make-daemon-default deltas

## MODIFIED Requirements

### Requirement: Headless daemon owns PTYs

A long-lived daemon SHALL own the `portable-pty` panes and their emulator state, registering itself in the `daemons` table so that panes survive a UI client detaching, and this MUST NOT alter the event loop's ~0%-idle contract: the loop still blocks on `poll_input(None)` with no polling timeout, and all daemon I/O reaches it off-loop via the tokio mpsc channel plus a `TerminalWaker` pulse.

Daemon routing SHALL be the default for local center-tree panes (`[daemon] enabled = true` by default), with `enabled = false` restoring in-process PTYs. Ephemeral panes — pins, the tool drawer, and the corner overlay — MUST bypass the daemon and use in-process PTYs, since they are not part of any tab's persisted center tree. Explicitly closing a pane or tab MUST kill its daemon session (not detach it into a lease), while quitting the compositor MUST detach center-tree daemon panes so they keep running. When the daemon is unreachable, a spawn MUST degrade to an in-process PTY.

#### Scenario: Pane survives client detach

- **WHEN** the only attached UI client detaches while an agent process keeps writing to a pane
- **THEN** the daemon keeps the PTY and its emulator state alive (the process is not killed) and continues recording its output

#### Scenario: Daemon work stays off the render loop

- **WHEN** the daemon is attached but no pane output, chrome, or geometry change occurs
- **THEN** the UI event loop receives zero wakes from the daemon and the render plan is `Skip`

#### Scenario: Explicit close kills the session

- **WHEN** the user closes a daemon-backed pane or its tab in the compositor
- **THEN** the daemon session and its child process are killed, and no relay lease is opened

#### Scenario: Quit detaches center-tree panes

- **WHEN** the user quits the compositor while daemon-backed center-tree panes are running
- **THEN** those sessions detach and keep running, and relaunching `thegn` warm-reattaches to them

#### Scenario: Ephemeral panes stay in-process

- **WHEN** a pin, tool-drawer, or corner-overlay pane is spawned with the daemon enabled
- **THEN** it runs as an in-process PTY and dies with the compositor, leaving no daemon session behind

#### Scenario: Daemon unreachable degrades in-process

- **WHEN** a pane spawn cannot reach or start the daemon
- **THEN** the pane opens as an in-process PTY and the failure is logged, not surfaced as a dead pane

### Requirement: Warm-reattach to a running session

A client SHALL be able to warm-reattach to a daemon-owned session and MUST receive the current emulator screen as an initial snapshot followed by a live delta stream, and an inbound pane delta MUST mark only the affected pane dirty so the render plan is `Panes` (a bounded pane diff) rather than a chrome recompose.

When an initial reattach fails because the session is gone (expired, reaped, or the daemon restarted — e.g. after a reboot), the pane MUST degrade to a freshly spawned shell showing the persisted scrollback tail and, when a foreground command was recorded, the relaunch overlay — never an error husk or dead pane. To support this, the daemon SHALL report each session's child pid so the compositor's cwd/foreground-command capture works for daemon panes, and scrollback snapshots SHALL be persisted for daemon panes like host panes.

#### Scenario: Reattach restores live screen

- **WHEN** a client reattaches to a session whose agent has been running while detached
- **THEN** the client first renders the daemon's current emulator snapshot and then applies subsequent live deltas

#### Scenario: Streaming output is a pane-only frame

- **WHEN** a pane delta arrives from the daemon with no chrome/geometry change
- **THEN** the render plan resolves to `Panes` and does not recompose chrome

#### Scenario: Expired session degrades gracefully

- **WHEN** the compositor reattaches a persisted daemon session that no longer exists
- **THEN** the pane opens a fresh shell in the persisted cwd, repaints the persisted scrollback tail, and arms the relaunch overlay for the recorded foreground command

#### Scenario: Daemon pane state is captured for resurrection

- **WHEN** session state is persisted while daemon-backed panes are running
- **THEN** their cwd, foreground command, and scrollback tail are captured just like in-process panes

### Requirement: Persistent relay keeps remote sessions alive across disconnect

When the last client detaches, the daemon SHALL open a lease (recorded in `session_leases`) that keeps the PTY and emulator state warm instead of tearing it down; a client reconnecting within the lease MUST resume the same session state. The default lease policy SHALL be never-reap (`lease_grace_secs = 0` means an infinite lease), so a detached session lives until explicitly killed or the machine restarts; a non-zero `lease_grace_secs` restores the grace-period behavior where expiry reaps the PTY. All lease bookkeeping happens off-loop and MUST NOT add a polling timeout to the event loop.

#### Scenario: Reconnect resumes warm

- **WHEN** a remote client disconnects and reconnects while its session lease is open
- **THEN** it resumes the same warm emulator state without restarting the process

#### Scenario: Default lease never expires

- **WHEN** all clients detach from a session under the default configuration
- **THEN** the session stays warm indefinitely and is listed by `thegn session list` until explicitly killed or reattached

#### Scenario: Configured lease expiry reaps the session

- **WHEN** `lease_grace_secs` is set to a non-zero value and no client reconnects before it elapses
- **THEN** the daemon reaps the PTY and releases the lease

## ADDED Requirements

### Requirement: Default persistence is visible and controllable

The compositor SHALL surface the persistent lifecycle: a statusbar chip MUST indicate when the focused pane is daemon-backed (glyph-degraded per terminal capabilities), the palette SHALL offer a **Detach** action (quit, keep panes running) and a **Quit and kill sessions** action (best-effort kill of daemon sessions, then quit), and on exit with detached sessions the process MUST print how many sessions were kept and how to reattach. Kill dispatch runs off-loop; the chip is chrome and renders on the existing `Full` damage path.

#### Scenario: Statusbar shows persistence

- **WHEN** the focused pane is daemon-backed
- **THEN** the statusbar shows a persistent-session chip (ASCII-degraded when Unicode glyphs are unavailable)

#### Scenario: Quit and kill leaves nothing behind

- **WHEN** the user invokes "Quit and kill sessions"
- **THEN** all daemon-backed sessions belonging to the UI session are killed best-effort and the compositor exits

#### Scenario: Exit reports kept sessions

- **WHEN** the compositor exits leaving N > 0 detached daemon sessions
- **THEN** it prints a message stating N sessions were kept running and that `thegn` reattaches / `thegn session list` inspects them
