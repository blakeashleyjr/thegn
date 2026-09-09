# Control Plane

## Purpose

The control plane lets a headless daemon own the compositor's PTYs and emulator
state so panes survive UI clients detaching, and exposes a scoped-token control
API (HTTP/gRPC plus an SSE/WebSocket event feed) that CLI verbs, thin clients,
and a mobile companion use to drive, monitor, and reattach to a running
instance. All daemon and API work stays off the render loop, preserving the
~0%-idle contract, and pairing/serve mode plus persistent relay leases keep
remote sessions warm across disconnects.

## Requirements

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

### Requirement: Control API drives a running instance

The daemon SHALL expose a control API (HTTP/gRPC plus an SSE/WebSocket event
feed) gated by scoped tokens. Implemented `thegn` CLI control verbs (including
open worktree, send-to-terminal, and snapshot) MUST drive a running instance
through this API and degrade gracefully when no daemon is running. A catalog
stub such as `browser.drive` MUST return the stable typed `unimplemented`
result and remain visibly classified as a stub; it is not an implemented
control promise. The API transport runs entirely off the render loop and never
introduces a polling timeout. HTTP routes MUST be generated from the `ROUTES`
table of the capability catalog so every route names the capability, verb, and
scope it serves, and the API MUST include `GET /v1/worktrees`.

#### Scenario: CLI verb reaches the live instance

- **WHEN** the user runs a `thegn` send-to-terminal verb against a running daemon
- **THEN** the input is delivered to the live pane over the control API and reflected in the attached UI

#### Scenario: Scope is enforced

- **WHEN** a client calls a control verb with a token lacking the required scope
- **THEN** the request is rejected without performing the action

#### Scenario: No daemon present

- **WHEN** a `thegn` control verb runs and no daemon is running
- **THEN** the CLI degrades gracefully with a clear message rather than crashing

#### Scenario: Routes are catalog-driven

- **WHEN** the HTTP router is built
- **THEN** every registered route corresponds to exactly one `ROUTES` entry whose capability id exists in the catalog

### Requirement: Serve mode pairs thin clients over a pairing URL

`thegn serve` SHALL advertise a pairing URL that desktop, web, or mobile thin clients use to pair and attach over the control API, where each pairing issues a scoped token stored hashed in the `pairings` table, and the pairing/approval prompt MUST render as a chrome overlay (resolving to a `Full` frame) without adding any polling timeout to the event loop.

#### Scenario: Thin client pairs and attaches

- **WHEN** a thin client redeems a valid, unexpired pairing URL
- **THEN** it receives a scoped token and attaches to the session over the control API

#### Scenario: Revoked pairing is refused

- **WHEN** a client attempts to pair with a revoked or expired pairing token
- **THEN** the attach is refused and no session access is granted

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

#### Scenario: Reconnect within the grace period resumes warm

- **WHEN** `lease_grace_secs` is non-zero and a remote client reconnects before its session lease expires
- **THEN** it resumes the same warm emulator state without restarting the process

#### Scenario: Lease expiry reaps the session

- **WHEN** `lease_grace_secs` is non-zero and no client reconnects before the lease expires
- **THEN** the daemon reaps the PTY and releases the lease

### Requirement: Paired read-mostly clients can monitor and lightly control an instance

The control API and event feed SHALL support a read-mostly paired client that
can monitor sessions and activity, stage and commit through the GitBackend
seam, and select among separately issued scoped tokens. No first-party mobile
companion application is shipped or required; the supported phone paths are
ntfy command/push and SSH/mosh as documented. Any future thin client MUST use
the scoped control API with no hard dependency on an AI layer, and read-only
views MUST require only `read`.

#### Scenario: Monitor and receive activity

- **WHEN** a paired client holds a read scope
- **THEN** it can view available session/activity data without any write or AI capability

#### Scenario: Stage and commit from mobile

- **WHEN** a paired client with the appropriate scope stages and commits changes
- **THEN** the operation routes through the GitBackend seam against the worktree, with git remaining the source of truth

#### Scenario: Switch account or scope

- **WHEN** the user switches account or scope in the companion
- **THEN** subsequent control-API calls are authorized under the newly selected scope

### Requirement: PR status and notification push are control verbs

The control API SHALL expose `pr.status` (a read-only projection of the cached per-worktree PR state) and `notify.push` (insert a notification through the same store and routing rules as in-process producers) on HTTP, gRPC and the typed client, with wire types published in the control schema.

#### Scenario: A script pushes a notification

- **WHEN** an authorized client POSTs `/v1/notify` with a title and body
- **THEN** the note lands in the notification inbox subject to the user's routing rules, and the response carries its stored identity

### Requirement: MCP serves scope-gated state tools

`thegn mcp serve` SHALL expose state tools beside the docs tools, gated by
`--scopes`: each tool maps to one catalog capability claimed on the MCP
surface and is listed/callable only when its `required_scope` is within the
requested set; live data comes from the daemon, with a cache fallback where
honest, and a clean error naming the daemon when live data is required but
unreachable. `MCP_STATE_CAPS` SHALL list exactly the implemented
capabilities. A tool MAY declare an argument schema (name, type, required);
when it does, `tools/list` MUST publish that schema as the tool's
`inputSchema`, and a `tools/call` whose arguments do not satisfy it MUST be
rejected with a JSON-RPC "Invalid params" error before any daemon call is
made. Mutating state tools (`required_scope` above `Read`) MUST log an
audit event naming the capability and a redacted view of the call's
arguments; a tool's argument redaction MUST replace any value that can carry
a secret (terminal input bytes, launch environment variables) with a
non-reversible size descriptor rather than omitting the field, so the audit
trail still shows _that_ a value was present and roughly how large it was.
`thegn mcp serve` MAY require an additional, tool-specific opt-in beyond
scope for a capability whose blast radius exceeds its scope tier's other
members; such an opt-in MUST be an explicit flag or config key checked
alongside (never instead of) the scope check, and MUST still deny both
listing and calling the tool when scope is granted but the opt-in is not.

#### Scenario: A scope-excluded tool is refused

- **WHEN** the server runs with `--scopes` that exclude a tool's required
  scope and a client calls it
- **THEN** the reply is a JSON-RPC error naming the missing scope, and
  `tools/list` did not advertise it

#### Scenario: Malformed arguments are rejected before the daemon is called

- **WHEN** a client calls a state tool with arguments that do not satisfy its
  declared schema (missing a required field, or a field of the wrong type)
- **THEN** the reply is a JSON-RPC "Invalid params" error and no daemon call
  occurs

#### Scenario: A mutating tool call is audited

- **WHEN** a client successfully calls a state tool whose required scope is
  above `Read`
- **THEN** an audit event is logged naming the capability, the call's
  redacted arguments, and the outcome, with any argument value that can
  carry a secret replaced by a size descriptor rather than its content

#### Scenario: An opted-in-but-under-scoped tool stays refused

- **WHEN** a tool requires both a scope and an additional opt-in, and the
  server is launched with the opt-in but not the scope
- **THEN** the tool is neither listed nor callable — the opt-in narrows what
  the granted scope covers, it never substitutes for the scope

#### Scenario: A scoped-but-not-opted-in tool stays refused

- **WHEN** a tool requires both a scope and an additional opt-in, and the
  server is launched with the scope but not the opt-in
- **THEN** the tool is neither listed nor callable

### Requirement: The catalog is callable generically

`thegn api list` SHALL print the capability catalog (id, surfaces, required scope); `thegn api schema` SHALL emit the control wire schema; `thegn api call <cap>` SHALL resolve the capability's HTTP route from the route table and perform it over the control socket with JSON parameters and output — with no per-verb client code, so a newly routed verb is immediately callable.

#### Scenario: A newly routed verb needs no client change

- **WHEN** a verb gains a route in the `ROUTES` table
- **THEN** `thegn api call <its id>` performs it without any `thegn api` code change

### Requirement: The control wire schema is pinned

The control API's wire types SHALL derive a JSON schema emitted to `docs/api/control-v1.json`, and a snapshot test SHALL fail on any wire change that does not regenerate the file (`THEGN_UPDATE_SNAPSHOTS=1`).

#### Scenario: A silent wire change fails

- **WHEN** a control wire type changes shape without the snapshot being regenerated
- **THEN** the snapshot test fails naming the drift

### Requirement: gRPC coverage is explicit and shrink-ratcheted

Every implemented gRPC capability SHALL have a proto message and handler
adapting `ControlApi`, scope-checked through `required_scope` before dispatch
exactly like HTTP. `GRPC_CAPS` SHALL list the implemented set. Every intended
catalog capability without a gRPC method SHALL appear in `SURFACE_GAPS` with a
specific reason and in the shrink-only gap ratchet; no unclassified gap is
allowed. Capabilities deliberately excluded from gRPC policy SHALL be expressed
by catalog surface declarations rather than excuses.

#### Scenario: A mirrored verb behaves like its HTTP twin

- **WHEN** a client calls the gRPC `MergeAdd` with a token holding `git`
  scope
- **THEN** the worktree's branch is enqueued exactly as via
  `POST /v1/merge/add`, and an under-scoped call is rejected with
  `PermissionDenied` before any action

#### Scenario: An unimplemented intended gRPC method is visible debt

- **WHEN** an intended catalog capability has no gRPC method
- **THEN** coverage reports it as an excused gap with a written reason and the ratchet prevents the gap set from growing silently

### Requirement: Daemon shutdown is a routed, scoped verb

The control API SHALL expose `POST /v1/daemon/shutdown` (capability
`daemon.shutdown`, admin scope) performing a graceful daemon shutdown, and
`thegn daemon stop` SHALL drive it over the control socket, degrading with a
clear message when no daemon is running. Local owner-protected unix-socket
requests reach it through listener-level implicit admin (`[serve]
local_admin`); v1 does not independently verify Unix peer credentials. TCP
callers MUST present an admin-scoped token.

#### Scenario: An operator stops the daemon from the CLI

- **WHEN** `thegn daemon stop` runs against a live daemon
- **THEN** the daemon shuts down gracefully and the command reports it

#### Scenario: A non-admin token cannot stop the daemon

- **WHEN** a TCP client whose token lacks `admin` calls
  `POST /v1/daemon/shutdown`
- **THEN** the request is rejected before any shutdown begins

### Requirement: The pairing web-redeem page is served

`GET /pair` SHALL serve a static, fully self-contained HTML page (no external
assets, restrictive CSP) that reads the pairing code from the URL fragment —
so the code never appears in server request logs — redeems it via
`POST /v1/pair`, and shows the minted token exactly once without persisting
it. The page is unauthenticated like `/health`, and the pinned
unauthenticated-route list grows to exactly `/health`, `/pair`, `/v1/pair`.

#### Scenario: The advertised web form works

- **WHEN** a browser opens the `PairingUrl::web_form()` URL
  (`http://host:port/pair#t=tgp1_…`) for a valid unexpired code
- **THEN** the page redeems the code and displays the scoped `tgc1_` token
  once, and the code is absent from the server's request log

#### Scenario: A bad code fails on the page, burning nothing

- **WHEN** the page submits a malformed or already-redeemed code
- **THEN** the redeem is refused with a clear message and no pairing state
  changes

### Requirement: Browser-hosted clients are ordinary paired thin clients

A web client SHALL authenticate exactly like every thin client — redeem a
pairing code, hold a scoped bearer token, answer to `required_scope` — with
no cookie/session or second auth mechanism. Cross-origin access SHALL be off
by default and enabled only by an explicit `[serve] cors_origins` allowlist;
a wildcard origin MUST be rejected at config validation.

#### Scenario: A disallowed origin gets no CORS grant

- **WHEN** a browser script from an origin not in `cors_origins` preflights
  a `/v1` request
- **THEN** the response carries no CORS allowance and the browser blocks the
  call, while non-browser clients are unaffected

#### Scenario: An allowed origin drives the API with its token

- **WHEN** an operator lists a GUI's origin in `cors_origins` and the GUI
  presents a paired token
- **THEN** its `/v1` calls succeed under exactly the token's scopes — no new
  policy surface exists for browsers

### Requirement: Mutating control calls emit audit records

Every control invocation whose required scope is `write`, `git`, `exec` or `admin`,
and every authentication or scope rejection, SHALL emit one structured audit
record on the tracing target `thegn::control::audit` carrying the caller's
pairing id and label, the capability id, the target resource, and the
outcome. Records MUST never contain a token secret, and emission MUST be free
when no tracing subscriber is installed.

#### Scenario: A commit from a paired phone is attributable

- **WHEN** a paired client with `git` scope performs `git.commit` while a
  tracing subscriber is installed
- **THEN** one audit record is captured naming the pairing id, `git.commit`,
  the worktree, and outcome `ok`

#### Scenario: A refused call is recorded

- **WHEN** a client whose token lacks `write` calls `sessions.input`
- **THEN** the call is rejected and one audit record is captured with outcome
  `no_scope`, containing no secret material

### Requirement: Sessions record server-side as asciicast

The daemon SHALL record a session's PTY output as an asciicast v2 file on
request — capability `sessions.record` (`Verb::RecordSession`, scope via
`required_scope`, surfaces HTTP/gRPC/CLI; deliberately not MCP or plugin in
v1) with start, stop and status operations, and CLI
`thegn session record <id> [--stop]`. Recording is owned by the session
actor, so it MUST continue while no client is attached and MUST stop
(finalizing the file) when the session exits. When no recording is active
the tee MUST cost a single null check per output event — no allocation.
Resizes are recorded as asciicast resize events. Files live under the
per-profile recordings directory (directory 0700, files 0600), bounded by a
configured `[recording] max_bytes` cap that finalizes the file rather than
filling the disk, and the control API returns recording status and path —
never file content. A session being recorded MUST show a recording indicator
in any attached UI.

#### Scenario: Recording survives detach

- **WHEN** a recording is started on a daemon session and every client
  detaches while the process keeps writing
- **THEN** the daemon keeps appending output events to the cast file, and the
  file finalizes when recording is stopped or the session exits

#### Scenario: Off means free

- **WHEN** no recording is active on a session
- **THEN** output handling performs only a null check — no timestamping, no
  allocation, no I/O

#### Scenario: Under-scoped record is refused

- **WHEN** a client whose token lacks the required scope calls
  `sessions.record`
- **THEN** the request is rejected and no file is created

#### Scenario: The size cap finalizes, not truncates

- **WHEN** a recording reaches `[recording] max_bytes`
- **THEN** the writer finalizes a valid cast file, recording status reports
  the cap was hit, and the session itself is unaffected

#### Scenario: Recording is visible at the keyboard

- **WHEN** a session is being recorded and a client is attached
- **THEN** the attached UI shows a recording indicator for that session

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

### Requirement: Event feeds accept narrowing filters

WebSocket, SSE, and gRPC observer feeds SHALL accept optional event-kind and
session filters applied per connection after the daemon broadcast. Filters MUST
only narrow authorized data, and an unknown kind MUST fail as a bad request
rather than silently matching nothing.

#### Scenario: Observe one session's activity

- **WHEN** a client subscribes to activity for one session
- **THEN** it receives applicable activity for that session and no unrelated
  session-keyed frames

#### Scenario: Unknown kind is rejected

- **WHEN** a client supplies a kind outside the accepted filter vocabulary
- **THEN** subscription fails with a bad-request error naming that kind

### Requirement: Feed loss is signaled only by opt-in

A subscriber that explicitly opts into lag signaling SHALL receive a `Lagged`
frame carrying the missed-event count when its broadcast receiver falls behind.
A subscriber that did not opt in SHALL retain the legacy skip behavior and MUST
NOT receive the additive frame tag.

#### Scenario: Opted-in slow subscriber learns of loss

- **WHEN** its receiver misses events
- **THEN** it receives the missed count and the stream continues

### Requirement: Control errors have stable machine codes

HTTP error bodies SHALL include a stable code from the closed control-error
taxonomy beside the existing human-readable message. Other public transports
SHALL remain projections of that same taxonomy.

#### Scenario: Caller lacks scope

- **WHEN** a request fails authorization
- **THEN** its HTTP body includes `code: "no_scope"` without removing the
  existing error message

### Requirement: CLI tails the observer feed

`thegn events tail` SHALL stream the observer feed with kind and session
filters, use the CLI's standard emitter for human or NDJSON output, and fail
clearly when no daemon is reachable. It MUST wait on stream readiness rather
than poll.

#### Scenario: Tail activity as NDJSON

- **WHEN** an operator runs `thegn events tail --kinds activity --json`
- **THEN** one JSON value is emitted per received activity event

#### Scenario: Daemon is absent

- **WHEN** the command cannot connect to the daemon
- **THEN** it exits non-zero with a clear diagnostic instead of crashing

### Requirement: Sessions report an activity state and support conditional waits

The daemon SHALL track a per-session activity state — blocked, working, done,
idle — derived from output, attention signals, and process state, published
edge-triggered on the event feed. A `sessions.wait` capability SHALL block
until the session reaches a named condition (exited, idle, blocked, done, or
output matching a regex) or a caller-supplied timeout elapses, reporting which
fired. An idle wait MUST NOT fire on a just-spawned session that has never been
busy, an output-match wait MUST consider retained scrollback at registration
time, and no state transition may be lost between a waiter's registration and
its first level probe.

#### Scenario: Waiting for a worker to finish

- **WHEN** a caller waits on a session with a done condition and a timeout
- **THEN** the call returns when the session reports done, or returns
  unmatched when the timeout elapses first, and says which happened

#### Scenario: A fresh spawn does not satisfy an idle wait

- **WHEN** a caller waits for idle on a session that has produced no activity
  yet
- **THEN** the wait does not fire until the session has been busy and then
  gone quiet

#### Scenario: A blocked agent is observable

- **WHEN** a session's agent asks for input and the attention signal fires
- **THEN** the session's state reads blocked until user input clears it, and
  waiters on blocked are woken

### Requirement: Dead sessions leave readable tombstones

When a session's process exits, the daemon SHALL retain a tombstone — exit
code and final screen — so a late `sessions.wait` or `sessions.snapshot` still
answers instead of returning not-found. The tombstone MUST be recorded before
the session's exit is announced, and a waiter whose session dies mid-wait MUST
receive the exit code the tombstone holds.

#### Scenario: A late poller reads the corpse

- **WHEN** a caller snapshots a session that exited before the call
- **THEN** the response carries the final screen and exit code rather than a
  not-found error

#### Scenario: A mid-wait death reports its exit code

- **WHEN** a session dies while a caller is waiting on it
- **THEN** the wait resolves with the session's exit code, whichever condition
  was being waited on

### Requirement: Issue and dispatch orchestration are catalog capabilities

thegn SHALL expose orchestration operations as capability-catalog rows
projected across the control surfaces: listing and reading tracker issues
(read scope), updating and commenting on issues (write scope), listing the
agent-dispatch roster (read scope), recording and re-statusing dispatches
(write scope), and creating a worktree (git scope) — optionally from an issue
id, deriving the branch from the tracker's branch hint and linking the issue.
Each row MUST be gated by `required_scope`, implemented on each surface or
recorded as an explicit gap, and MUST work with no agent configured (the
operations are plain tracker, git, and roster reads/writes).

#### Scenario: A supervisor enumerates the board and the roster

- **WHEN** a caller with read scope lists issues filtered by status and lists
  dispatches
- **THEN** both return machine-readable rows from the tracker router and the
  durable roster, without spawning anything

#### Scenario: Creating a worktree from an issue

- **WHEN** a caller with git scope creates a worktree naming an issue id
- **THEN** the branch derives from the tracker's branch hint (with the naming
  fallback), the worktree is registered, and the issue is linked to it

#### Scenario: A write without write scope is refused

- **WHEN** a caller whose scope set lacks write invokes an issue update or a
  dispatch status change
- **THEN** the operation is refused naming the missing scope, on every surface
  that projects it

### Requirement: The dispatch put verb carries the pipeline columns

The `dispatches.put` payload SHALL accept the pipeline fields — stage, parent
row, session, artifact path — as optional, default-absent fields, and the created
row returned to the caller SHALL include them. No additional verb, capability row
or scope SHALL be introduced for them: one append-only writer carries the whole
row, so no mutable stage field exists on the wire for the system to advance.

#### Scenario: A client written before the fields exist

- **WHEN** a client posts a payload carrying only issue, worktree and agent
- **THEN** the call succeeds unchanged and the created row's pipeline fields are
  absent

#### Scenario: A pipeline dispatch over the control plane

- **WHEN** a client posts a payload including the pipeline fields
- **THEN** the created row is returned carrying every one of them

### Requirement: A guarded push command inbox maps signed messages to catalog capabilities

thegn SHALL optionally accept phone-initiated commands through a push command
inbox that is **off by default** and hosted by the daemon process (never the
UI event loop; unavailable — with a reason — when the daemon is disabled).
When `[notifications.push.inbox]` is enabled it MUST require a SecretRef
`inbox_secret` and a non-empty capability `allow` list (enabling without
either is a startup configuration error). The daemon subscribes to the
configured command topic; each message is a versioned JSON envelope
(`v, id, ts, cap, params, mac`) and SHALL be executed only when ALL hold: the
HMAC over the canonical envelope verifies against `inbox_secret`; `ts` is
within the freshness window and `id` has not been seen (replay protection);
`cap` is in the `allow` list; `required_scope(cap)` is within the configured
`scopes` ceiling; and the capability is not admin-scoped (admin capabilities
are refused unconditionally, regardless of config). Execution MUST route
through the same capability-catalog dispatch as the control API — the inbox
is a projection of the one catalog, never a second policy table and never a
shell command. Failed verification, replays, and refused capabilities SHALL
be dropped with counters visible to doctor/logs; replies to an optional reply
topic MUST be truncated to a fixed size cap.

#### Scenario: A signed, allowlisted read command executes

- **WHEN** the inbox is enabled with `allow = ["worktree.list"]` and a fresh,
  correctly signed envelope for `worktree.list` arrives on the command topic
- **THEN** the capability executes through the catalog dispatch under the
  configured scope ceiling and its truncated result is published to the reply
  topic (when one is configured)

#### Scenario: Tampered, replayed, or unlisted commands are refused

- **WHEN** an envelope arrives with a bad MAC, a stale timestamp, a
  previously seen id, a capability outside the allow list, or an admin-scoped
  capability
- **THEN** nothing executes; the message is dropped and the corresponding
  refusal counter increments

#### Scenario: Off by default means no subscription exists

- **WHEN** `[notifications.push.inbox]` is absent or `enabled = false`
- **THEN** thegn opens no subscription to any command topic and no
  phone-initiated command path exists

#### Scenario: Enabling without a secret is a configuration error

- **WHEN** the inbox is enabled with no `inbox_secret` (or an empty `allow`
  list)
- **THEN** startup surfaces a configuration error naming the missing key, and
  the inbox does not start

### Requirement: A live session forks into a new sibling session

The daemon SHALL fork a live session — capability `sessions.fork`
(`Verb::ForkSession`, same required scope as `sessions.open`, non-streaming
surfaces) — by opening a **new** session from the source's retained resolved
spawn recipe: same argv/cwd/env for raw-argv sessions, a freshly re-resolved
composition (command, sandbox, environment) for `agent:`-launched sessions,
with cwd/worktree overridable. The fork MUST be a new process with a new
session id and pid; the source session MUST be unaffected. The daemon SHALL
retain spawn recipes in memory only for the lifetime of the live session —
never persisted to the database or tombstones and never returned over the
API — so forking a dead session fails with a clear error naming
`sessions.open` as the alternative. Resource-cap wrapping MUST be re-applied
to the fork by the daemon regardless of how the source was capped, and the
forked PTY SHALL inherit the source's current rows and columns. When `harness`
is supplied, `session` is a native id from `agent.sessions`; the selected
harness's `FORK` operation is authoritative. If an `agent` is supplied, its
configured provider MUST match that harness, otherwise the request is refused.

#### Scenario: Fork re-runs the recipe

- **WHEN** a client forks a live session opened with argv `["npm","run","dev"]`
  in `/w/app`
- **THEN** a new session spawns running `npm run dev` in `/w/app` with a new
  session id and a different pid, and the source session keeps running
  untouched

#### Scenario: Agent launches re-resolve, never replay

- **WHEN** a session opened via an `agent:` launch is forked after the
  agent's credentials were rotated
- **THEN** the fork's environment is composed fresh at fork time (current
  config and credentials), not replayed from the source's spawn

#### Scenario: A dead session cannot fork

- **WHEN** a client forks a session that has exited
- **THEN** the daemon returns an error stating the session has exited and
  that `sessions.open` starts the command anew, and no process spawns

#### Scenario: Recipes never leak

- **WHEN** any control API response describes a session (listing, snapshot,
  fork result)
- **THEN** it never includes the retained env pairs, and the state database
  contains no spawn environment for any session

#### Scenario: Fork preserves a live resize

- **WHEN** a client resizes a live source to 41 rows by 137 columns and forks
  it
- **THEN** the new PTY starts at 41 rows by 137 columns and the source remains
  at its existing size

#### Scenario: Recorded harness selection is authoritative

- **WHEN** a client forks native id `native-1` with `harness=claude` and a
  configured agent whose provider is `codex`
- **THEN** the request is rejected without spawning; with a Claude-configured
  agent, the Claude harness fork command is used while the agent's current
  credentials and sandbox are composed

### Requirement: Forks carry lineage and optional scrollback context

A forked session's environment SHALL carry `THEGN_FORKED_FROM` (the source
session id) beside the standard identity variables, and `SessionInfo` SHALL
expose `forked_from` so listings and UIs can show lineage. When the fork
requests scrollback hand-off, the daemon SHALL write the source's retained
scrollback tail as plain text to an owner-only file under the per-profile
state dir, expose its path as `THEGN_FORK_SCROLLBACK` in the fork's
environment only, and best-effort delete it when the forked session exits.
The forked pane's screen MUST show only output the forked process itself
wrote — the source's output is never replayed into the new emulator.

#### Scenario: The fork can find its parent

- **WHEN** a forked session's process reads its environment
- **THEN** `THEGN_FORKED_FROM` names the source session, and
  `thegn session list --json` shows the fork's `forked_from`

#### Scenario: Scrollback rides a file, not the screen

- **WHEN** a fork is created with scrollback hand-off from a source with
  retained history
- **THEN** the fork's `THEGN_FORK_SCROLLBACK` names a 0600 file containing
  the source's scrollback tail, and the fork's terminal shows only the new
  process's own output

### Requirement: Fork placement and worktree fork compose existing flows

Forking from the CLI (`thegn session fork <id>`) or the UI (`fork-session`
action on the focused pane) SHALL place the fork through the existing adopt
intent — a running compositor grafts it as a split beside the source pane, or
a new tab on request; with no compositor attached the fork simply exists in
the daemon. A worktree fork SHALL first create a new worktree branched from
the source session's worktree via the existing worktree-creation path, then
fork the session with cwd and worktree remapped into it (relative cwd
preserved); a worktree-creation failure MUST leave the source session and
layout untouched, and a fork failure after worktree creation MUST report the
surviving worktree rather than deleting it. With the daemon disabled, fork
MUST degrade with a clear message that it requires the daemon.

#### Scenario: Fork lands beside its source

- **WHEN** the user invokes `fork-session` on a focused daemon-backed pane in
  a running compositor
- **THEN** the forked session is grafted as a sibling split beside the source
  pane

#### Scenario: Fork into a fresh worktree

- **WHEN** the user forks with worktree fork from a session whose cwd is
  `src/api` inside worktree `feat-x`
- **THEN** a new worktree is branched from `feat-x`, and the fork starts in
  the new worktree's `src/api`

#### Scenario: No daemon, clear answer

- **WHEN** `[daemon] enabled = false` and the user invokes fork
- **THEN** the action fails with a message naming the daemon requirement, and
  nothing spawns

### Requirement: Fork is projected by the complete control catalog

The `sessions.fork` capability SHALL be mapped to `Verb::ForkSession`, use the
same write scope as `sessions.open`, be non-streaming, and have
`SurfaceSet::ALL`. HTTP, gRPC, CLI, MCP, and plugin generic calls SHALL project
the same catalog row. MCP SHALL expose flat scope-checked arguments for
`session`, optional `harness`, `agent`, `cwd`, and `worktree`, and boolean
`scrollback`, `adopt`, and `tab`; it SHALL NOT accept raw argv or arbitrary env.
The wire request and response SHALL use those fields plus additive optional
`SessionInfo.forked_from`.

#### Scenario: MCP exposes the catalog fork operation

- **WHEN** a caller has the `sessions.open` write scope and lists or invokes
  the MCP state tools
- **THEN** `sessions_fork` is advertised and dispatches the same scope-checked
  operation as the control API
