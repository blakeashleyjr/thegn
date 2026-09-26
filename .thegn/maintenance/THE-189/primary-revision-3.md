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

# Primary revision brief — THE-189 (round 4)

Row 562 filed four findings. The primary accepts three in full and **narrows
R-001**. This is the last planned round for this lane: do exactly what is below.

## R-001 — ACCEPTED IN PART. Fix the omission; do NOT build an atomic commit boundary.

**Accepted:** the paths that perform a side effect with **no generation check at
all** are genuine omissions and must be fixed — specifically the CI error path
at `ci_refresh.rs:274-282` updating fetch health unfenced, the continuing CI log
writes at `:373-376`, and any sibling you find with the same shape. Check
immediately before the side effect, as the other classes already do.

**Declined:** making check-and-commit atomic across weather, CI, calendar, LOC
and usage. That means holding schedule state across every cache write in five
subsystems, which serializes those writes behind the scheduler and puts new
contention on paths the 0%-idle and latency invariants depend on. The residual
window is a few instructions wide, its worst outcome is one stale cache row, and
the next scheduled refresh corrects it — that is a far smaller cost than the
redesign.

Record the residual race explicitly in a comment at `generation_is_current`
("this is a check, not a commit barrier; a reload landing between the check and
the write can admit one stale row, which the next refresh corrects") and note it
as a follow-up finding in your artifact. Do not silently leave it undocumented.

## R-002 — ACCEPTED IN FULL. This is the real hole.

A scheduled parent spawning an **unfenced child** defeats the whole mechanism,
and both paths named are real:

- `on_ci_tick` starts `spawn_ci_detail` without the generation
  (`ci_refresh.rs:123-124`); it writes CI log cache entries
  (`actions.rs:393-398`) and sends an untagged `CiDetail` (`:411-418`).
- the scheduled PR path calls `refetch_pr_view` without the generation
  (`run.rs:12197-12205`); it writes the review cache and emits an untagged wake
  (`actions.rs:957-973`).

Carry an **optional** schedule fence through these scheduled child jobs and
check it at both cache commit and delivery. Optional is the operative word:
manual and user-forced detail/view refreshes stay untagged and unfenced, exactly
as F-003 requires. Add the reload-mid-detail regressions for CI and for the PR
review cache path.

## R-003 — ACCEPTED

Store and compare the **effective** cadence in `ScheduleConfig`, not the raw
TTL. `scan_sched::pump_slots` divides by four and floors, so LOC TTLs 1 and 2
both resolve to the same 60-second pump while the raw comparison reports a
change and needlessly re-arms — breaking "unchanged schedules are not
restarted". Audit the disk and auto-fetch fields at `hydrate_schedule.rs:51-55`
and `:72-75` for the same raw-versus-effective shape and fix them together. The
regression the reviewer added at `:372-389` should pass when you are done.

## R-004 — ACCEPTED

The coverage that claims to prove this lane works is mostly model tests: a local
`FakeSlot`, an `AtomicU64`, and one CI-only live replacement. Drive the real
`ScheduleOwner` through the production worker and the event-loop envelope, for
every ticker class — not just CI. Keep the reviewer's existing tests.

## Unchanged

Sibling module only. 0% idle — no new wake source at rest. Pulse the
`TerminalWaker`. No blocking I/O on the loop. `render_plan::plan` stays pure.
Fake time, not sleeps. Untagged refreshes always win coalescing.
