# Primary review + greenlight — THE-482

Reviewing row 519's investigation (`.thegn/pipeline/THE-482/maintenance-investigate/519.md`).

**Verdict: APPROVED to implement**, with the open question answered and two
amendments.

The investigation is accurate. The primary independently checked the proposed
week arithmetic against the generalized rule ("week 1 is the configured-start
week containing January 4") and confirms the plan's expected vectors:

- Sunday-first 2021: week 1 is the Sun 2021-01-03 row, and the preceding
  Sun 2020-12-27 row is week 53 (it is 52 whole weeks after the Sunday-anchored
  week 1 of 2020, which begins Sun 2019-12-29). Vector `[53, 1, 2, 3, 4, 5]` ✓
- Saturday-first 2021: week 1 is the Sat 2021-01-02 row; the preceding
  Sat 2020-12-26 row is week 52 (Saturday-anchored week 1 of 2020 begins
  Sat 2020-01-04). Vector `[52, 1, 2, 3, 4, 5]` ✓

## Answer to the plan's open question — YES, fix both doc residues

The plan asks whether the two documentation-only "ISO" residues stay outside the
lane. **Bring them in.** Fix the wording at `crates/thegn-core/src/calendar/mod.rs:10`
and `crates/thegn-host/src/detail/calendar/layout.rs:18`.

Rationale: this issue _is_ a truthfulness defect about the word "ISO". Landing a
fix that leaves two comments still asserting unconditional ISO reproduces the
defect in the next reader. These are comment lines only — no rendering or layout
logic changes, so the "do not touch rendering layout" constraint in the original
brief is not violated. That constraint was about behaviour, not comments.

## Amendment 1 (REQUIRED) — checked arithmetic, no panics

`thegn-core` is substrate-free and must not panic on adversarial input. Use
checked date arithmetic throughout the week-year selection (the
previous/current/next year probe crosses `NaiveDate` boundaries). A month at
chrono's representable extrema must return a defined result, not an unwrap
panic. `MonthGrid::build` already returns `Option` for an out-of-range month —
stay consistent with that contract rather than introducing a panic path.

## Amendment 2 (REQUIRED) — keep the ISO guard test honest

Test 1 must assert the Monday-first output is **unchanged from today's
behaviour**, not merely "equal to a vector I computed". Pin it against the
existing per-cell `iso_week` values for the same grid so the test would fail if
the generalization silently drifted Monday output. The existing
`iso_week_numbers_are_not_hand_rolled_across_the_new_year` test at
`calendar/tests.rs:86-94` stays untouched.

## Scope lock

`crates/thegn-core/src/calendar/grid.rs` (the helper + `week_numbers()`),
`crates/thegn-core/src/calendar/tests.rs`, the `show_week_numbers` doc in
`crates/thegn-core/src/config_calendar.rs`, `config/config.toml.example:347`,
and the two comment lines named above. Nothing else. No new config key — if you
conclude one is needed, STOP and report a blocker.

`DayCell.iso_week` keeps its exact current meaning and value. Do not repurpose
it.

## Validation

Do not run cargo/nextest/clippy/coverage. The primary runs the batch gate.
Record the focused filter you want run. Note that changing the
`show_week_numbers` schema description may move a generated config/schema
snapshot — flag it in your report so the primary regenerates it centrally.
