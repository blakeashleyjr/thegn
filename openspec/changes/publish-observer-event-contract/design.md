# Design — observer event contract

## Vocabulary split

Core exposes two explicit sets:

- observer kinds: frames the generic event broadcaster can produce;
- attach kinds: pane `snapshot`/`delta` frames produced by a specific session
  attachment.

Transport parsers use the observer set for `/v1/events`, SSE, gRPC observer
subscriptions, and `events tail`. The attach protocol keeps its own decoder and
schema. Unknown or attach-only observer filters return the shared bad-request
taxonomy with the supported set.

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
