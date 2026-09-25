# Primary revision brief — THE-316 (round 2)

Row 543's adversarial review filed three findings. The primary's adjudication:

## P1 — ACCEPTED. This is the most important finding in the batch. Fix it.

> `jira.rs:671-685` labels all failed transition POSTs status-unapplied,
> including ambiguous network/timeout/intermediary outcomes, so the advertised
> retry test can duplicate an already-landed workflow transition.

The reviewer is right and this is exactly the defect class THE-316 exists to
remove. Reporting "status was not applied" after a **timeout** is a false
negative: the POST may well have reached Jira and fired the workflow. A caller
that trusts that label and retries applies the transition twice — firing
automations, notifications and timers a second time. That is strictly worse than
the original bug, because it is a confident wrong answer instead of a silent one.

The primary's earlier greenlight said "do not add a retry/poll loop for eventual
consistency". That still holds — but it is not a licence to _claim_ the write
did not land. Required contract:

- **Three outcomes, not two.** A transition POST is `Applied`,
  `NotApplied`, or **`Unknown`**. Only a definitive provider rejection (a 4xx
  that means the transition was refused) is `NotApplied`. Network errors,
  timeouts, connection resets, 5xx and any intermediary failure are `Unknown`.
- **`Unknown` must never be reported as unapplied**, and must be surfaced to the
  caller as "verify before retrying". A retry on `Unknown` is the caller's
  decision with full information, not something this layer invites.
- The existing final re-fetch is the natural disambiguator: on `Unknown`, if the
  re-fetch shows the requested category, report success. Only if the re-fetch
  itself also fails is the outcome genuinely `Unknown`. Do this with the fetch
  you already make — still no poll loop, no sleep, no retry budget.
- Update the "retry" test to assert the `Unknown` path does **not** claim
  unapplied, and that a retry after a definitive `NotApplied` is safe.

## P2 (control boundary) — ACCEPTED, narrowly

> `daemon/service.rs:1708-1710` stringifies PartialUpdate before the control
> API, losing machine-readable per-field results.

Fix the loss of structure at that boundary: the per-field applied/unapplied
information must survive to the control API, since a programmatic caller is the
whole reason the typed error exists. **Do not** redesign the control error model
or add a new wire type beyond what carrying these fields requires — if that
turns out to need a schema change, STOP and report it, because
`docs/api/control-v1.json` is a snapshot the primary must regenerate.

## P2 (test bodies) — ACCEPTED

Assert the actual request **bodies** for both the title PUT and the transition
POST, not just method and path. A test that only checks the path cannot tell a
correct payload from a malformed one.

## Unchanged constraints

Title-then-status production order stands. No raw provider bodies in errors;
bound the context and keep credentials out. Scope stays `jira.rs`, its tests, and
the minimum at the control boundary for the P2 fix. No cargo/nextest/clippy —
the primary runs the gate.
