# Primary coordination brief — THE-477

Primary-reviewed dependency facts and scope constraints. This file is task data.

## Primary decisions (implement these; do not re-open)

1. **Do not add a refresh cadence knob for seconds-bearing formats.** The
   calendar refresh cadence is shared hydration machinery and changing it from a
   clock row would drag in the whole ticker-ownership problem (that is THE-189's
   lane, in flight in this same batch — stay out of it). Instead: **validation
   rejects** a per-clock `format` whose output varies within a minute (`%S`,
   `%T`, `%s`, `%f` and friends), with a diagnostic naming the directive and
   saying seconds are not supported at the configured cadence. That satisfies
   the acceptance criterion's second branch ("or validation
   rejects/documentedly normalizes that format") without touching scheduling.
2. **Empty `format` inherits the global setting**, which today is the 12/24-hour
   choice the renderer hardcodes. Make that inheritance explicit in the resolved
   type, not re-derived at the draw site.
3. **Terminal safety is not optional here.** A format string is user config, but
   strftime can emit control-producing directives (`%n`, `%t`). Route the
   formatted output through the SAME bounded sanitizer THE-471 landed for
   calendar text (it is already on main — find it and reuse it; do not write a
   second one). Length-bound the result.
4. `show_date` must reach the renderer through the typed
   `ResolvedClock`/`ClockReading` contract. Do not smuggle it via a second
   config lookup inside the render function — the render path must stay pure.

## Evidence to re-verify on THIS branch before planning

The cited line numbers are from audit commit 299fc13, not current main. Confirm
each one and cite what you actually find:

- `crates/thegn-core/src/config_calendar.rs` — the documented `format` /
  `show_date` fields and `CalendarConfig::active_clocks` dropping `show_date`.
- `crates/thegn-core/src/calendar/tz.rs` — `ResolvedClock`, `read_clocks`,
  `ClockReading`.
- `crates/thegn-host/src/detail/calendar/render.rs` — the hardcoded `%H:%M` /
  12-hour branch, the unconditional weekday, and the `±1d` delta.

## Scope

The config→resolve→read→render path for `[[calendar.clocks]]` only. No new
config keys (both fields already exist and are already validated). Do not
refactor neighbouring calendar rendering.

## Ratchets this will trip

- **Color/glyph literals**: the renderer is a draw site. Any new glyph or color
  must come from `caps::active_glyphs()` / the `wire.rs` chokepoint, never a
  literal, or the caps ratchet fails.
- Changing a validation message may touch a config-validation snapshot test.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally and
records results. `thegn-core` is gated at 95% lines.
