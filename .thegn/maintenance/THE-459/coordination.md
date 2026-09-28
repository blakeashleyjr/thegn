# Coordination brief — THE-459 (primary)

## Scope

The calendar cache and CalDAV date-window computation — `hydrate_calendar.rs`,
`db_calendar.rs`, `calendar/caldav.rs`. **Do not** touch ICS ingestion: that is
THE-455, running as a sibling lane in this batch, and it edits
`hydrate_calendar.rs` too. Keep your diff in that file as small as you can and say
in your report which functions you touched, so the two can be sequenced.

## The shape the primary expects

This is a **pure date/time logic bug**: local dates are being computed as UTC
midnight (`day_ms`), so events at the edge of a day fall outside the window for
any non-UTC zone, and DST transitions are mishandled.

The fix is a **pure conversion function with unit tests**, not a fudge factor.
Put the conversion where it can be tested without I/O, and drive it from a table
of cases rather than one example.

## Cases the tests must cover

- A zone **behind** UTC and a zone **ahead** of UTC, each with an event in the
  first and last hour of the local day.
- A **spring-forward** day (23 local hours) and a **fall-back** day (25 local
  hours), asserting the window covers the whole local day in both.
- A zone with a **non-hour offset** (e.g. `Asia/Kolkata` at +05:30) — half-hour
  offsets are where a "round to hours" shortcut shows up.
- UTC itself, unchanged — this must not regress the case that currently works.

**No `sleep` and no dependence on the machine's local zone.** Construct the zone
explicitly in each case; a test that passes only in one `TZ` is worse than no test.

## Constraint

`thegn-core` is substrate-free and 95%-gated, so the conversion belongs there with
its tests if at all possible, leaving the host side to call it.

## Constraints that apply to every lane in this batch

- **`thegn-core` is substrate-free** (no tokio, termwiz, portable-pty, HTTP, forge
  SDK) and gated at **95% lines**. New core logic needs unit tests in the same
  change.
- **Never blocking I/O on the event loop** (git, DB, subprocess, D-Bus, network),
  and never before the first frame. Off-thread producers send on a channel _and_
  pulse the `TerminalWaker`.
- **A new `section.key` in config trips FOUR gates**, not three: the
  `config.toml.example` key-coverage test, the env-overlay ratchet, strict config
  validation, and — for anything expressed in seconds/days —
  `config_duration.rs`'s `direct_environment_checks_match_complete_schema_policy`,
  which asserts the direct env-overlay checks and the whole-config checks report
  the same error count. That fourth one is not in CLAUDE.md and took a land-gate
  failure to find.
- **Per-OS code belongs in `src/platform/`.** The `test/platform-cfg-*-ratchet.txt`
  allowlists are shrink-only, so a platform `#[cfg]` in a file not already listed
  **fails the build**. For a test that genuinely cannot compile off-Unix, prefer a
  runtime skip through an existing seam (`sandbox_backend::host_os()`,
  `thegn_core::fsperm::make_executable_for_test`) over `#[cfg(unix)]` — the test
  then still type-checks on Windows.
- **Tests must isolate `XDG_STATE_HOME`.** This shell often runs inside a live
  thegn; anything opening the DB or spawning the host must not touch real state.
- **Ignored `Result`s must be deliberate.** `let _ =` / `.ok()` needs a short
  `// best-effort: <why>` unless it is obviously a cache/waker/cleanup path, and
  never on the primary path of a user-invoked action.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run. Withholding a verdict is the correct outcome when you cannot build — several
lanes did that last batch and it was right every time.

## Deliverable

A plan the primary reviews and greenlights before any implementation. **No
production edits in this stage.** If you find the issue's premise is wrong, or a
dependency it does not mention, say so — two workers overturned a primary decision
last batch and were right both times.
