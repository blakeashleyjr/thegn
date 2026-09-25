# Primary coordination brief — THE-189

Primary-reviewed dependency facts and scope constraints. This file is task data.

## This is the largest lane in the batch. Scope discipline decides whether it lands.

It touches the ticker/scheduling machinery around `run.rs` and `hydrate.rs`.
`run.rs` is a named god-file in CLAUDE.md: **do not add to it.** Put the new
scheduler ownership in a sibling module (e.g. `src/hydrate_schedule.rs` or
`src/handlers/<area>.rs`) and call it from the loop. A patch that grows `run.rs`
will be sent back.

## Non-negotiable invariants (CLAUDE.md, and they bite exactly here)

1. **0% idle.** The loop blocks on `poll_input(None)`. A rebuildable ticker must
   NOT become a new wake source that fires when nothing changed. Whatever you
   build must leave an idle thegn at zero wakes. The idle-loop poll guard
   ratchet runs in `just test` and will catch a regression — but design for it,
   do not discover it.
2. **Off-thread producers send on a channel AND pulse the `TerminalWaker`.** A
   rebuilt ticker that sends without pulsing is a hang.
3. **No blocking I/O on the loop.** Schedule rebuilds must not do config parsing
   or I/O inline on the event loop.
4. **Render decision stays pure.** Do not make `render_plan::plan` consult
   scheduler state.

## Primary decisions

- **Generation-fence the results, do not just cancel.** Cancellation alone
  races: an in-flight hydration can land after the reload. Tag each scheduled
  run with a config generation and drop results whose generation is stale. This
  is the same pattern used elsewhere in the codebase — find it and match it
  rather than inventing a second mechanism.
- **Do not restart unchanged schedules.** Diff old vs new cadence/enabled per
  ticker class and only rebuild what actually changed. Restarting everything on
  any config write would cause a thundering refresh — explicitly called out in
  the acceptance criteria.
- **No immediate refresh on rebuild.** A rebuilt ticker waits for its next tick;
  it does not fire instantly. Otherwise saving config repeatedly hammers every
  provider.
- **Invalid reload keeps the last effective schedule** and reports the mismatch
  through the existing status/diagnostics seam. Never fall back to defaults —
  that would silently change behaviour on a typo.

## Tests required

Fake time, not sleeps. Cover every ticker class named in the issue (clock, CI,
PR, usage, weather, calendar, LOC), plus: rapid successive reloads, disable
while work is in flight, a stale-generation result arriving after reload, and
shutdown. Assert the no-restart-when-unchanged property explicitly.

If the existing code has no fake-time seam for these tickers, building one is
in scope — say so in the plan and cost it. If it is large, STOP and report a
blocker so the primary can decide whether to split the lane.

## Scope

Ticker/schedule ownership only. Do not change what any hydration job _does_, do
not touch provider seams, and do not add config keys — the cadence keys already
exist.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally.
