# Primary coordination brief — THE-482

Primary-reviewed dependency facts and scope constraints. This file is task data.

## The primary has already chosen the design. Implement it; do not re-open it.

The issue offers three options. Two are rejected:

- **Locale-specific week numbering is BLOCKED.** It would need THE-476's
  authoritative locale data, which is unlanded (Backlog). Do not add a second
  territory/locale table — that is explicitly what THE-476 exists to prevent.
- **"ISO gutter only for Monday-aligned grids"** (hiding the gutter otherwise)
  is rejected: it silently drops a feature the user enabled.

**Chosen contract: generalize the week number to the configured first day.**

Compute each row's week number with the ISO _rule_ (minimal-days = 4) but
anchored on the grid's configured `week_start`, instead of hardcoding Monday.
Then the number is true for all seven cells of its row by construction, which is
the acceptance criterion. When `week_start == Monday` the computation reduces to
exactly ISO-8601, so today's default output must not change — assert that.

Terminology follows: the gutter is "week numbers"; it is ISO **only** when
week_start is Monday. Fix `show_week_numbers`' doc comment, the
`config.toml.example` text, and the `grid.rs` module doc, which all currently say
ISO unconditionally.

## Evidence (verified by the primary on current main, not the audit commit)

- `crates/thegn-core/src/calendar/grid.rs:119-122` — `week_numbers()` returns
  `w[0].iso_week` for every row: the first cell's number, so a Sunday-first row
  is labelled with the previous ISO week for 6 of its 7 cells.
- `crates/thegn-core/src/calendar/grid.rs:26-27` — `DayCell.iso_week` is a
  per-cell ISO week, which is fine as data; the row-level collapse is the bug.
- `grid.rs:1-2` module doc says "plus ISO week numbers".

## Scope

`crates/thegn-core/src/calendar/grid.rs` plus the doc/example/schema text for
`show_week_numbers`. Do not touch event expansion, hydration, or rendering
layout. No new config key — the existing `show_week_numbers` bool is the whole
surface. If you conclude a new key is unavoidable, STOP and report a blocker:
a new `section.key` trips three separate ratchets and changes the size of this
lane.

## Tests required

Year-boundary and week-53 cases for **each** supported week start
(Mon/Sun/Sat): 2020-12-28..2021-01-03 (ISO 53 then 1), 2021-01-03 Sunday-first,
and a leap-week year (2026 has ISO week 53). Assert the Monday case is
bit-identical to today's output.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally.
Record the exact commands you believe are needed. `thegn-core` is gated at 95%
lines, so every new branch needs a test.
