# Control Plane — observer contract delta

## ADDED Requirements

### Requirement: Accepted observer kinds are actually observable

The generic WebSocket, SSE, gRPC, and CLI observer subscriptions SHALL validate
kind filters against one canonical set containing only frames the observer
broadcast can emit. Pane-attach-only `snapshot` and `delta` kinds MUST NOT be
accepted by generic observer filters. Rejection SHALL use the stable bad-request
taxonomy and identify the supported observer kinds.

#### Scenario: Attach frame requested from observer feed

- **WHEN** a client requests `kinds=snapshot` from the generic event endpoint
- **THEN** subscription fails explicitly instead of opening a stream that can
  never match

#### Scenario: Every accepted kind has a producer

- **WHEN** the contract test enumerates the observer kind vocabulary
- **THEN** every name maps to a generic broadcaster producer and every public
  observer transport accepts the same set

### Requirement: Observer bootstrap uses lists and reconciliation

An observer client SHALL bootstrap resource state with the existing list
operations and SHALL re-list after a relevant coarse state-change event or an
opted-in `Lagged` frame. Public documentation MUST state that list and subscribe
are not an atomic snapshot and that clients reconcile idempotently by stable
ids. The generic observer protocol MUST NOT advertise a synthetic state frame
unless one is implemented with defined consistency semantics.

#### Scenario: Client starts observing sessions

- **WHEN** a client needs current sessions plus future changes
- **THEN** it lists sessions, subscribes to applicable observer kinds, and
  reconciles a subsequent re-list when change or lag is signaled

### Requirement: Pane attach retains snapshot and delta semantics

The authenticated session-attach path SHALL continue to deliver an initial pane
snapshot followed by live pane deltas using its attach-specific vocabulary;
clarifying the generic observer contract MUST NOT remove or rename those frames.

#### Scenario: Session reattach

- **WHEN** a client attaches to a live daemon-owned session
- **THEN** it receives the current emulator snapshot and then pane deltas on the
  attach path, independent of generic observer filters
