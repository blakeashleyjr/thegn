# Publish a truthful observer event contract

Linear: THE-105 (Active, Client API & Remote Access)

## Why

Generic observer subscriptions accept kind filters from a shared vocabulary
that includes `snapshot` and `delta`, but those pane frames are emitted only by
the attach path. A remote monitor can therefore request valid-looking kinds it
will never receive. Bootstrap behavior is also implicit: THE-34 did not ship a
synthetic state frame.

## What Changes

- Split the generic observer-filter vocabulary from pane-attach frame kinds.
- Reject attach-only kinds on observer WebSocket/SSE/gRPC and CLI subscriptions.
- Publish list-then-subscribe/re-list-on-session-change as the supported
  observer bootstrap and recovery contract.
- Preserve snapshot-then-delta semantics on the authenticated pane attach path.
- Regenerate wire/client documentation and add transport parity tests.

## Impact

- Specs: `control-plane`.
- Client API: turns currently misleading accepted filters into explicit input
  errors and documents race/recovery semantics.
- No new frame type, polling loop, database table, or authorization surface.
- Origin: remaining truth gap separated from completed THE-34.
