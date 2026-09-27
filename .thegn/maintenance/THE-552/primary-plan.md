# Primary review + greenlight — THE-552

Reviewing row 622. **APPROVED. Implement the three-part plan, with the escalated
search-direction question decided below.**

Good plan, and the part I most wanted is right: you identified that a millisecond
timestamp **cannot** express a position within a same-millisecond group, and went
to an event-sequence cursor rather than trying to make the timestamp carry it.
That is the fix; everything else follows from it.

## DECISION — preserve the current search direction semantics

You asked whether acceptance means forward search from zero including the current
frame. **No. Keep `>` forward and `<` reverse exactly as they are.**

The issue's acceptance is _search and reconstruct must agree_, not _search must
change direction_. Widening `>` to `>=` would alter the result of every existing
search call to make one boundary case reachable forward — a behaviour change to a
working API in service of a bug that is really about reconstruction. Zero being
found by reverse search from a later position is sufficient and is the existing
contract.

## Confirmed as written

- **Inclusive reconstruction**: apply every retained event with
  `at_ms <= requested_at_ms`, including all events at zero. Timestamp zero is a
  valid time, not a sentinel.
- **Sequence cursor**, initialised to the first event after the inclusive
  reconstruction; forward advances, backward seek reconstructs and resets.
- **Clamp/rebuild when eviction overtakes the cursor.** This is the one that will
  bite if it is left implicit — a stale scratch cursor surviving a changed
  retained front is a silent wrong-output bug, not a crash. Test it directly.
- **Sample after each complete timestamp group, not each event.** That is what
  actually makes search and reconstruct agree; sampling per event is why they
  disagree today.

## On the missing THE-548

You reported no THE-548 branch, commit, issue packet or review artifact in any
reachable ref. The primary confirms: treat it as **not existing**. Its only
relevance in the issue body was a warning that its future coverage must not hide
this defect with delays — which is advice to a later author, not a dependency on
you. Do not wait for it and do not reconstruct it.

## Scope

`replay.rs` and `replay_overlay.rs` only, as you scoped. No export-format change,
no migration — the ring is in-memory. Do not touch `pane.rs`, `panes.rs` or
`run.rs`.

**No `sleep` anywhere in the tests.** Shifting events off zero with a delay is the
bug concealing itself, and the issue says so explicitly.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in the previous chain shipped something that did not
build and the primary caught each — that division of labour is expected, an
optimistic report is not.
