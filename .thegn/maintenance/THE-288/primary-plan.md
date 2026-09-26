# Primary review + greenlight — THE-288

Reviewing row 570's investigation. **APPROVED to implement.**

Evidence confirmed on this branch: a detached thread per copy, an unbounded
stdin write and `child.wait()`, and callers that report success optimistically
because "the thread started".

## Your question — the primary sets the budgets. Use these.

- **Payload cap: 1 MiB.** Above it, refuse the copy and report that to the
  caller rather than truncating. A silently truncated clipboard is worse than a
  refused one — the user pastes something that looks right and is not.
- **Queue depth: 1, latest-wins.** A clipboard holds one value; a superseded
  pending copy is worthless. Replacing a queued copy is not a failure and must
  not be reported as one.
- **Timing budget, per helper attempt:** 2s to spawn and finish writing stdin,
  5s total to exit. On expiry, kill the process group and move to the next
  candidate helper. **Whole-operation ceiling: 10s** across all candidates, so a
  machine with several hung helpers still settles.
- These are constants with a named rationale, not config keys. Do not add
  `[clipboard]` settings — that is new public surface this issue does not ask
  for, and every new key trips three ratchets.

## Restated decisions

- One bounded worker, not a thread per copy.
- Kill and reap the whole tree on timeout/shutdown/replacement; Unix uses the
  process group. **THE-274 is unlanded** — do not build Windows Job Object
  containment. Windows gets the deadlines and the existing spawn behaviour, with
  the containment gap recorded as an explicit follow-up naming THE-274.
- Clipboard content is **absent** from diagnostics — not truncated, not
  redacted in place. It carries passwords.
- Real async success/failure reaches the caller. Fix the optimistic reporting at
  the `run.rs` / `actions.rs` / `help` / `overlay.rs` sites the investigation
  found, but do not put the wait on the event loop.

## Tests

Copy bursts do not grow threads/processes. A helper that never reads stdin and
never exits is cancelled inside the budget. Shutdown leaves nothing alive.
An oversized payload is refused, not truncated. Failure reaches the caller with
no content in the message.

## Scope

`crates/thegn-host/src/clipboard.rs`, the call sites that report the outcome,
and the platform seam for process-group kill. Do not touch THE-152's terminal
work.
