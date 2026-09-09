# Control Plane — event subscription deltas

## ADDED Requirements

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
