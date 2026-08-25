# control-plane — deltas

## ADDED Requirements

### Requirement: The event feed accepts narrowing subscriptions

The `/v1/events` feed (WebSocket and SSE) and its gRPC mirror SHALL accept
optional subscription filters — `kinds` (a subset of the frame-kind
vocabulary) and `session` (limit session-keyed frames to one session) —
applied per connection in the transport pump, never in the daemon's
broadcast. Filters MUST only narrow: an unfiltered subscription receives
exactly today's full feed, and a filter can never expose anything the
token's `read` scope would not already see. An unknown kind name MUST be
rejected as a bad request rather than silently matching nothing. SSE frames
SHALL set the `event:` field to the frame kind.

#### Scenario: A monitor watches one session's activity only

- **WHEN** a client subscribes with `kinds=activity,exit&session=s1`
- **THEN** it receives `Hello`, then only activity and exit frames for `s1`,
  and no pane, lease, pairing or session-list traffic

#### Scenario: A legacy subscriber is untouched

- **WHEN** a client connects to `/v1/events` with no filter parameters
- **THEN** the stream is byte-identical to the pre-filter behavior

#### Scenario: A typo cannot silently filter everything out

- **WHEN** a client subscribes with `kinds=activty`
- **THEN** the request is rejected as a bad request naming the unknown kind

### Requirement: Subscribers can bootstrap from a state snapshot

A feed subscription MAY request `snapshot=1`; the server then sends,
immediately after `Hello`, one `State` frame carrying the current session
and worktree lists (the same wire types as `sessions.list` /
`worktrees.list`), so a client starts from consistent state and applies
subsequent events instead of racing a re-list. The `State` frame SHALL be
sent only to connections that requested it.

#### Scenario: A fresh client needs no racing re-list

- **WHEN** a client subscribes with `snapshot=1`
- **THEN** it receives `Hello`, then one `State` frame listing every current
  session and worktree, then live events from that point on

### Requirement: Feed loss is signaled to opted-in subscribers

For a subscription that opted into lag signaling, a lagged broadcast
consumer SHALL receive a `Lagged` frame carrying the missed-event count
instead of a silent skip — the client's cue to re-snapshot; connections that
did not opt in keep the existing silent-skip behavior. New frame tags
(`State`, `Lagged`) MUST be sent only to connections that requested the
corresponding feature, `Hello` SHALL advertise the server's feed features in
an additive field, and the wire protocol version stays unchanged for these
additive frames.

#### Scenario: A slow consumer learns it lost events

- **WHEN** an opted-in subscriber lags the broadcast by `n` events
- **THEN** it receives `Lagged` with `missed = n` and can re-request a
  snapshot, and the stream continues

#### Scenario: An old client never sees an unknown tag

- **WHEN** a client that requested no feed features subscribes and the feed
  lags or state changes
- **THEN** the server sends it only pre-existing frame kinds, so its decoder
  never tears down on an unknown tag

### Requirement: Control errors carry stable machine-readable codes

HTTP error bodies SHALL carry a `code` field beside the existing `error`
message, drawn from a closed vocabulary projected from the control error
taxonomy (`not_found`, `no_scope`, `conflict`, `unimplemented`, `internal`,
`unauthorized`, `bad_request`); gRPC status codes and plugin RPC error codes
remain projections of the same taxonomy. The addition MUST be
backward-compatible (the `error` string is unchanged) and the control wire
schema snapshot MUST be regenerated to include it.

#### Scenario: A client branches on the code, not prose

- **WHEN** a client calls a control verb with a token lacking the required
  scope
- **THEN** the 403 body contains `code: "no_scope"` alongside the
  human-readable `error` message

### Requirement: The CLI can tail the event feed

`thegn events tail` SHALL stream the event feed over the control socket with
the same narrowing filters (`--kinds`, `--session`, `--snapshot`), printing
human-readable lines by default and NDJSON with `--json` through the CLI's
one emitter, and degrading with a clear message when no daemon is running.
The `events.subscribe` catalog row SHALL list the CLI surface, implemented in
the same change so no coverage gap is introduced.

#### Scenario: An operator watches agent activity from a shell

- **WHEN** `thegn events tail --kinds activity --json` runs against a live
  daemon
- **THEN** it emits one JSON line per activity event as they happen, without
  polling

#### Scenario: No daemon degrades gracefully

- **WHEN** `thegn events tail` runs with no daemon
- **THEN** the command exits with a clear message rather than crashing
