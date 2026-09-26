# PRIMARY AUTHORIZATION — you MAY run `cargo check` AND focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary is granting a narrow exception**, because lanes
kept reporting `implementation-ready` for code that did not compile or whose own
new tests failed, and each round-trip costs far more than the checks would.

You are authorized to run **exactly these**, as many times as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` on the check is required: the library frequently builds when the
**test** targets do not. Keep the test filter narrow (your module or your test
names) — it must not become a workspace run.

Still forbidden, and still the primary's job: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, `clippy`, `just lint`, `just test`,
`just ci`, and anything full-workspace. Do not run them.

**Your row is not finished until the check is clean AND the tests you added or
touched pass.** If you cannot get there inside your approved scope, report the
remaining failures verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build, or whose own tests fail,
is not.

Report what you actually ran. `rustfmt` passing is evidence of formatting only.

---

# Primary revision brief — THE-188 (round 2)

Row 593 filed three findings. The primary's adjudication:

## High — ACCEPTED. A variant nothing constructs is not a fix.

> real `ObserveApp` always routes `TimeRange` through relative conversion, so
> `QueryWindow::Absolute`/`SetWindow` are unreachable and historical callers can
> drift.

This is the batch's recurring pattern and it defeats the purpose: the type split
was supposed to make "absolute ranges cannot drift" **structural**, but an
unreachable variant proves nothing and the drift the issue reports is still
possible for any historical caller.

Resolve it one of two ways, and say which:

1. **Wire it** — find where a historical/absolute range legitimately enters and
   construct `Absolute` there, so the relative conversion is no longer the only
   path. Preferred if such a caller exists.
2. **Remove it** — if nothing in the product can supply an absolute range today,
   delete the variant and state plainly that only relative windows exist, with
   the drift guarantee resting on the single-`now`-per-refresh rule instead.

What is **not** acceptable is shipping a variant that exists only to satisfy an
acceptance criterion. That is the same untruthful-surface defect THE-463 and
THE-191 are about.

## Medium (tests bypass the real path) — ACCEPTED

> tests call private `query_all` and test-only pause/resume helpers, bypassing
> the spawned command/ticker path.

Drive the real path. A test that reaches past the ticker cannot show that
successive _refreshes_ advance — which is the entire claim. Keep the fake clock;
lose the private-helper shortcut.

## Medium (config reload of an existing Observe tile) — SCOPED OUT

> host config reload does not reconfigure an existing Observe tile, so reload
> behavior is undefined/unapplied.

Accurate, and **not this lane**. Reconfiguring a live tile needs a reload owner
with its own generation/cancellation story — that is THE-407's
(`app-tab configuration registry-aware and live-reloadable`) territory, and
THE-189 just did the equivalent work for hydration tickers at real cost.

Do this instead: **document** the current reload behaviour where the window type
is defined (a reload does not retarget a live tile; the new config applies to the
next tile), and record it as a follow-up finding naming THE-407. Do not build it.

## Unchanged

One `now` per refresh, captured once, injectable. THE-184 is unlanded and not a
prerequisite — no bounded queues or deadlines. Backwards clock must not invert a
range.
