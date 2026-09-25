# Primary review + greenlight — THE-189

Reviewing row 542's investigation (`.thegn/pipeline/THE-189/maintenance-investigate/542.md`).

**Verdict: APPROVED to implement, points 1-6 exactly as written.** This is the
strongest plan in the batch. Implement it as specified; the notes below are
emphasis, not changes.

## What the primary is specifically endorsing

- **Point 1** — sibling `hydrate_schedule.rs`, nothing added to `run.rs`. This
  was the primary's hard constraint and the plan honours it.
- **Point 2** — one shared worker with one owned handle, **not** a timer per
  feature. This is what protects the 0%-idle contract; do not "simplify" it
  into per-class tasks.
- **Point 3** — replaceable command boundary with a monotonic generation, and
  re-arm only changed slots **at their next normal boundary**. No immediate
  refresh on rebuild.
- **Point 5** — the `Err` reload path does not call `reconfigure`, so an
  invalid reload keeps the last effective schedule and its existing status
  message. Exactly right; never fall back to defaults.
- **Point 6** — reuse the existing `hydrate_refresh_ticker.rs` I/O adapter and
  THE-483's reviewed arithmetic rather than duplicating it, and reuse the
  existing `TickerIo` fake-clock fixtures. Reusing the reviewed fixtures is why
  this lane is tractable — the primary's brief said to stop and report if a
  fake-time seam had to be built from scratch, and it does not.

## Point 4 is the best judgement in the plan — hold that line

> "preserve untagged event-driven/user-forced refreshes so this change does not
> disable legitimate on-demand work"

This is the failure mode a naive generation fence would introduce: fencing
_every_ refresh by schedule generation would silently break manual refresh and
event-driven updates, turning a scheduling fix into a worse bug. Only
**scheduler-originated** requests and deliveries carry the generation and are
discardable. Add an explicit test that a user-forced refresh still lands
immediately after a reload — that is the regression guard for this exact
mistake.

## Restated invariants (CLAUDE.md, and they bite here)

- **0% idle.** The loop blocks on `poll_input(None)`. The rebuildable schedule
  must not become a new wake source at rest. The idle-loop poll ratchet runs in
  `just test`; design for it.
- **Pulse the `TerminalWaker`** on every send from the off-loop worker — and
  only for actual refresh messages, as point 3 says.
- **No blocking I/O on the loop**, including during `reconfigure`.
- **`render_plan::plan` stays pure** — it must not consult scheduler state.
- New long-lived threads declare a QoS class (`platform::qos`); housekeeping is
  `Background`, not the `Interactive` default.

## Tests

The ordered plan is approved. Required coverage: every ticker class named in
the issue (clock, CI, PR, usage, weather, calendar, LOC); rapid successive
reloads; disable while work is in flight; a stale-generation result arriving
after reload; shutdown joins the handle; **no restart when a projection is
unchanged**; and the user-forced-refresh guard above. Fake clock, not sleeps.

## Validation

Do not run cargo/nextest/clippy. The primary runs the batch gate centrally.
Because this touches the loop wiring, list every `run.rs` line you changed so
the primary can review the wiring diff specifically.
