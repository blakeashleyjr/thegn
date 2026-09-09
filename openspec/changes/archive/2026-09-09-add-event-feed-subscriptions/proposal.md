# Event-feed subscriptions (accepted scope)

Linear: THE-34 (Done)

## Why

Observer clients needed to narrow the existing event stream and handle lag and
errors programmatically without adding a polling path or breaking legacy feed
consumers.

## What Changed

- Added optional event-kind and session filters to WebSocket, SSE, and gRPC
  event streams.
- Added opt-in lag notifications while retaining legacy silent-skip behavior.
- Added stable machine-readable control error codes.
- Added `thegn events tail` with human and NDJSON output through the CLI emitter.

A synthetic state snapshot frame was intentionally removed from the accepted
design. Clients bootstrap with list operations and treat session-change events
as a prompt to re-list. The remaining mismatch between advertised generic
filter kinds and what observer streams can actually emit is tracked by THE-105
(`publish-observer-event-contract`).

## Impact

- Specs: `control-plane` feed filtering, lag, errors, and CLI tailing.
- Compatibility: unfiltered/legacy subscriptions keep their established frame
  behavior; additive frames are opt-in.
- Runtime: filtering is per connection after daemon broadcast; no idle poll.

## Archive status

The bounded THE-34 delivery is complete. Snapshot/bootstrap truthfulness is
separated into THE-105 rather than represented as delivered here.
