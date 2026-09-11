# Design — observer event contract

## Vocabulary split

Core exposes two explicit sets:

- observer kinds: `activity`, `lease`, `pairing`, `sessions`, and `exit`, all
  produced on the generic daemon broadcast;
- attach kinds: `hello`, `snapshot`, `delta`, and `exit`, produced by a specific
  session attachment.

Transport parsers use the observer set for `/v1/events`, SSE, gRPC observer
subscriptions, and `events tail`. The attach protocol keeps its own decoder and
schema. Unknown or attach-only observer filters return the shared bad-request
taxonomy with the supported set.

`hello` is mandatory transport bootstrap rather than a filterable broadcast
kind, and `lagged` is a transport-generated loss signal controlled by the
subscriber's `signal_lag` option. WebSocket, SSE, and gRPC all send `hello`
before reading the generic broadcast receiver.

## Bootstrap and recovery

There is no atomic `State` feed frame. An observer lists required resources,
subscribes, and treats coarse session/worktree change events and `Lagged` as a
signal to re-list. Documentation must state the possible list/subscribe race
and require idempotent reconciliation by stable ids. This is honest about the
delivered protocol and avoids inventing an unimplemented consistency point.

## Compatibility

Unfiltered subscribers receive the same generic event stream. Only filters that
could never match become errors. Pane attach clients retain initial snapshot
followed by delta behavior.
