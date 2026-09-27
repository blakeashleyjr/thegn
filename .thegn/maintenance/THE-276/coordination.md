# Coordination brief — THE-276 (primary)

## Scope

`crates/thegn-core/src/control_wire.rs` and `crates/thegn-svc/src/control/client.rs`,
plus their private test modules. **Do not** touch the daemon service, the pane
actor, or any other control-API surface.

The issue names THE-273 (handshake/body deadlines) and THE-184 (bounded stream
consumers) as related. **Neither is a prerequisite and neither is in scope.** If
you find work that belongs to one of them, report it as a finding — do not do it.

## What the primary most wants verified

The 257-frame deadlock claim is the load-bearing one: `start_attach` forwarding
every frame from the first message into a fresh 256-slot channel _before_
returning the receiver. **Confirm the channel capacity and the ordering from the
source before designing around it**, and say what you found either way. If the
capacity or the ordering is not what the issue states, that changes the fix and
the primary needs to know before greenlighting.

Note the two halves are independent and should not be conflated:

- **Correctness**: one frame per binary message; reject anything else; bootstrap
  cannot queue behind a missing receiver.
- **Complexity**: the quadratic shift. Linear decode with at most one bounded
  compaction.

A fix for one is not a fix for the other, and a report should not let the cheaper
one stand in for both.

## Constraints

- `thegn-core` is **substrate-free** — no tokio, no tungstenite, no HTTP inside
  it. The wire format and its decoder are pure; the transport limits belong on
  the `thegn-svc` side. Do not import a transport type into core to express a cap.
- `thegn-core` is gated at **95% lines**. New decoder logic needs unit tests in
  the same change.
- Tests must be **deterministic and self-contained**: no network, no real
  WebSocket server, no sleeps. The 257-frame and oversized-message cases are
  constructed byte inputs, not timing experiments.
- Rejecting a malformed message must produce a **typed, specific** error naming
  the observation (e.g. "binary message contained 2 frames; exactly 1 required"),
  not a category. A refusal on a transport path that says only "invalid frame" is
  what turns a one-line diagnosis into an investigation.

## Deliverable

A plan the primary reviews and greenlights before any implementation. No
production edits in this stage.
