# Primary review + greenlight — THE-477

Reviewing row 521's investigation (`.thegn/pipeline/THE-477/maintenance-investigate/521.md`).

**Verdict: APPROVED to implement.** The investigation is thorough and correct.
Two findings in it are especially valuable and are adopted:

- `strftime_needs_seconds` **already exists** at
  `crates/thegn-core/src/config.rs:54-78` with tests at
  `config_tests.rs:3582-3596`, recognizing `%S %T %r %X %s`, nanoseconds and
  escaped `%%S`. **Reuse it. Do not write a second seconds detector.**
- THE-471's bounded `DisplayText` / `Field::ClockLabel` projection is already on
  this branch at `crates/thegn-core/src/calendar/display.rs`. **Reuse it** for
  formatted time/date text, exactly as labels and abbreviations already do.

## Answers to the three open contract questions

### 1. Full-date spelling when `show_date = true` — APPROVED as `YYYY-MM-DD`

Locale-independent, deterministic, fixed-width (10 cells), unambiguous, and it
does not re-introduce the locale dependency THE-476 owns. Use it.

### 2. Does `+1d`/`-1d` stay alongside the full date? — NO, not on the same row

When a row shows its full local date, the relative delta is redundant: the date
already answers "which day". Showing both spends width in a popup that must stay
narrow and says the same thing twice.

Contract:

- `show_date = true` → that row's date cell shows `YYYY-MM-DD`; no `±1d` for
  that row.
- `show_date = false` → **exactly today's behaviour, unchanged**: weekday cell
  plus the conditional amber `±1d`.

The column itself remains (rows are mixed), it is simply empty of a delta for
date-showing rows. Assert both shapes in tests.

### 3. Lenient runtime load vs strict validate — YES, add the normalization

This is the best catch in the investigation. Rejecting seconds-bearing formats
only in strict `config validate` would leave a lenient load able to carry a bad
format to the render path. Add calendar-format normalization in
`Config::post_process`, **mirroring the existing bar-format normalization at
`config.rs:6341-6362`** so the two behave the same way. A bad row format
normalizes to the inherited default and warns; it must never panic a draw site
and must never silently render garbage.

## Binding constraints (restated)

- No cadence/ticker work. Do not touch `hydrate_refresh_ticker.rs`, account
  polling, or refresh ownership. Seconds are rejected/normalized, not scheduled.
- `read_clocks` stays a **pure instant-based calculation**: it carries policy,
  it never consults config or environment.
- Preserve unknown-zone warn-and-skip, and give the synthesized home row (both
  at `calendar_docs.rs` and the fallback in `detail/calendar/mod.rs:224-237`)
  the resolved global default rather than an empty format.
- `show_date` must reach the renderer via the typed `ResolvedClock` /
  `ClockReading` contract. No second config lookup at the draw site.

## Ratchets

The renderer is a **draw site**: any color or glyph must come from
`caps::active_glyphs()` / the `wire.rs` chokepoint. The amber `±1d` already
exists — do not inline a new literal for the date cell. Changing a validation
message or a schema doc may move a config snapshot; flag it in your report and
the primary regenerates centrally.

## Validation

Do not run cargo/nextest/clippy. Record the focused filters you want. Core is
gated at 95% lines; cover every new branch, including the normalization path.

---

## Adversarial finding ACCEPTED and fixed by the primary (row 532)

Row 532 filed a High finding: chrono's no-dot fractional directives `%3f`,
`%6f`, `%9f` were not recognized, so both strict validation and the tolerant
`post_process` admitted sub-minute output on a minute-resolution tick.

**Confirmed and fixed.** The primary verified the mechanism directly in the
vendored chrono source (0.4.45,
`src/format/strftime.rs:615` → `'f' => internal_fixed(Nanosecond3NoDot)`): those
forms become `Item::Fixed(Fixed::Internal(InternalFixed))`, whose payload is a
**private** field. No syntactic match can name them or even distinguish them, so
extending the match arm was not an option.

Note this gap **predates** THE-477: the original `strftime_needs_seconds` had
the same blind spot, so `[bars] clock_format = "%H:%M%3f"` was also mis-tiered
onto the minute tick. This fix closes both.

Fix: the named walk is now a _naming_ pass only, and the decision is
behavioural — render one instant twice differing solely in seconds/nanoseconds
and compare. Anything whose output moves is sub-minute-bearing, including
directives chrono adds later. A malformed format short-circuits to `None` before
formatting, since `Display` on a bad format panics and `validate_strftime` owns
that error path. Runs at config admission only, never on the render path.

Verified by the primary (not claimed, run):
`cargo test -p thegn-core --lib strftime` → 3 passed;
`cargo test -p thegn-core --lib clock` → 16 passed;
`cargo test -p thegn-core --lib seconds_and_invalid_row_formats_are_rejected_or_normalized`
→ passed, covering `%S %T %r %X %s %f %.3f %.6f %.9f %3f %6f %9f`.
