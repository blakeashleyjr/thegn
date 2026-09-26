# Primary review + greenlight — THE-188

Reviewing row 578's investigation. **APPROVED to implement.**

The plan is small and engine-centred, which is right. Note your row could not
file a `dispatch report` — that was the primary rebuilding the release binary
mid-run and is not your fault; the committed artifact was read instead.

## Your question — THE-184 is unlanded and is NOT a prerequisite

Confirmed: THE-184 (concurrent panel fetching with bounded queues/deadlines) is
unlanded, and window advancement does not depend on it. Do **not** implement
bounded queues, deadlines, or concurrency here. One `now` per refresh cycle is
independent of how many panels fetch in parallel.

You _directly block_ THE-186, so keep the resulting window type clean enough for
a timestamp-ordered renderer to consume — but do not build THE-186 either.

## Restated decisions

- **Relative and absolute windows are different types, not a flag.** An absolute
  range must be structurally incapable of drifting; a relative one structurally
  incapable of freezing. That turns both acceptance criteria into type
  properties rather than conditionals someone can later get wrong.
- **One `now` per refresh, captured once and passed down.** No
  `SystemTime::now()` inside the query builder — it must be injectable so tests
  drive a fake clock.
- Define pause/resume (paused freezes the relative window; resume advances to
  current) and make a backwards system clock unable to produce an inverted
  range.

## Tests

Fake clock only, no sleeps: successive refreshes give advancing equal-duration
ranges; an absolute range is byte-identical across refreshes; every panel in one
cycle shares start/end; a backwards clock does not invert.

## Scope

`crates/gtui-app/src/engine.rs` and its tests.

## Validation

You may run `cargo check -p gtui-app --all-targets` and a focused
`cargo test -p gtui-app --lib <filter>`. Your row is not done until both are
clean. Nothing wider — the primary runs the batch gate.
