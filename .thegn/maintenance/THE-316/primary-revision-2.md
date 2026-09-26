# Primary revision brief — THE-316 (round 3)

Row 553 filed two findings against round 2. **Both accepted.** This round is
narrow; do not re-open anything else.

## P1 — ACCEPTED. "4xx" is not a synonym for "definitively refused".

Round 2 treats _every_ 4xx transition response as safely unapplied. That is too
broad, and the primary's round-2 wording ("a 4xx that means the transition was
refused") invited it. Making it precise:

**Classify as `NotApplied` (safe to retry) only this allowlist**, which are the
codes Jira itself returns when it rejects a transition without performing it:

- `400` Bad Request
- `401` Unauthorized
- `403` Forbidden
- `404` Not Found
- `409` Conflict (transition not available in the current workflow state)
- `422` Unprocessable Entity

**Everything else is `Unknown`**, explicitly including:

- `408` Request Timeout — the server may have begun processing; this is the
  reviewer's example and it is a real one.
- `425`, `429`, and **any 4xx not in the allowlist above**. An unrecognized code
  must fall to `Unknown`, not to the safe-looking default. Default-to-ambiguous
  is the fail-safe direction here, because the cost of a wrong `NotApplied` is a
  duplicated workflow transition while the cost of a wrong `Unknown` is only
  that the caller verifies first.
- All 5xx, network errors, timeouts, connection resets and intermediary
  failures (already correct).

Keep round 2's `Unknown` verification semantics exactly as they are: the
existing final re-fetch disambiguates, a confirmed category reports success, and
only a failed verification stays `Unknown` with the bounded "verify before
retrying" message. No poll loop, no sleep, no retry budget.

The reviewer added an HTTP-408 regression that is expected to fail against the
current code — make it pass. Add one more asserting an **unrecognized** 4xx
(e.g. `418`) also lands in `Unknown`, so the default direction is pinned.

## P2 — ACCEPTED

> `jira.rs:718-725` replaces typed ambiguous POST/fetch errors with `Api` text
> and loses transient classification.

Accepted. Flattening a typed transport error into `Api(String)` destroys
`IssueError::is_transient`, which is what callers use to decide whether a retry
or backoff is even appropriate. Preserve the typed source error through the
ambiguous path so transient classification survives to the caller. Keep the
bounded, credential-free message requirement — preserving the _type_ does not
mean copying a raw provider body.

## Out of scope for this round

The control-wire `PartialUpdate` item stays **deferred** (see the round-2
addendum). Title-then-status order stands. Scope remains `jira.rs` and its
tests. No cargo/nextest/clippy — the primary runs the gate.
