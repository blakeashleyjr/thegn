# Primary revision brief — THE-574 (round 2)

Row 541's adversarial review filed four findings. The primary's adjudication:

## F3 — ACCEPTED, and it is the important one. Fix first.

> Raw `anyhow` error text is emitted into the always-on WARN diagnostic
> ring/debug bundle without sanitization.

Accepted. The primary's plan asked for a **bounded, actionable** refusal and the
implementation emits the raw error. This matters more than it looks: the WARN
ring is always on and is harvested into crash reports and the debug bundle, which
users attach to issues. A raw provider/sandbox error can carry absolute paths,
account names, hostnames, or command text.

Required: bound the length and emit a **classified** reason plus a sanitized
detail. You already classify into provider-ownership / sandbox / generic — lead
with that classification, and treat the underlying text as untrusted: truncate
it and strip control characters. There is existing bounded-display machinery in
the tree for exactly this (see `thegn_core::calendar::display::DisplayText` as
the pattern, or whatever the host already uses for bounded diagnostic text) —
reuse rather than writing a third one.

## F2 — ACCEPTED

> Global process-lifetime `HashSet` retains unbounded worktree/agent strings.

Accepted. Unbounded retention keyed by worktree is a slow leak in a process
designed to run for days across many worktrees. Bound it: a small capacity cap
with oldest-eviction is sufficient, and re-reporting a refusal after eviction is
acceptable behaviour (it is a diagnostic, not a ledger). State the cap and the
eviction policy in a comment.

## F1 — ACCEPTED in part

> Repeated prewarm resolver work is not deduplicated; refusal leaves the batch
> `Ok` and does not enter `prewarm_failed`.

The **deduplication of the report** is required and you have it. The
deduplication of the _resolver work_ is a performance concern, not a correctness
one, and a refused spec resolution is cheap relative to a launch.

Fix the part that is a real inconsistency: the refusal should be reflected in
the batch's own outcome bookkeeping rather than silently leaving `Ok`, so the
surrounding code is not told everything succeeded. **Do not** change the
fail-open behaviour — the tab still gets its shell. This is about the internal
status value, not the user-visible result.

If reflecting it in `prewarm_failed` would change retry or scheduling behaviour,
STOP and report that instead of changing it — the primary will not accept a
scheduling change smuggled in through a diagnostics lane.

## F4 — ACCEPTED

Tests must capture the emitted WARN and assert (a) the classification is the
expected one, (b) no agent command was executed, (c) the shell fallback still
happened, and (d) a repeat call does not re-emit. The reviewer already added a
typed WARN capture helper — use it.

## Unchanged constraints

`suppress_agent_record: true` stays; `worktrees.agent` is never rewritten. No
work moves onto the event loop. WARN level stays (it is what reaches the
always-on ring). Scope stays `worktree_launch.rs` plus the minimum diagnostics
seam. No cargo/nextest/clippy — the primary runs the gate.
