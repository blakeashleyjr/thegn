# Primary review + greenlight — THE-459

Reviewing row 661. **APPROVED. Implement steps 2–4 as written, with the API
question decided below.**

This is a precise investigation. You identified the right canonical shape — a
**half-open `[start_ms, end_exclusive_ms)`** range built by converting midnight at
`from` and midnight of `to + 1 local day` — reused the existing
`calendar::tz::resolve_local` and its `GapPolicy::ShiftForward` rather than
inventing boundary handling, and chose to **error on an unrepresentable window
instead of substituting UTC or adding a fixed duration**. That last one is the
decision that matters: silently falling back to UTC is how the current bug reads as
working.

You also correctly refused to build a CalDAV range from the backend's empty `zone`
field. Leave it empty.

## DECISION — one core `CalendarWindow` value

You preferred a single value over a parallel window-aware method, and you are
right. Take the `CalendarWindow`.

The reason is not tidiness. Parallel date-and-instant arguments are precisely how
two representations of one fact drift apart, and this session has now fixed that
exact failure three times — two MIME guards that disagreed, two host predicates
that disagreed, and a timestamp cursor that could not express what a sequence
cursor could. One value carrying both the local inclusive dates and the canonical
half-open instants means a caller cannot supply a window whose halves disagree.

Keep date-consuming providers on their local-date fields, as you propose; only
CalDAV query generation moves to UTC instants with the exclusive end.

## Confirmed as written

- `db_calendar.rs` likely needs **no production change** — its half-open overlap
  predicate already matches. Add a focused test only if existing coverage cannot
  assert the exact boundary, and say which you chose.
- Preserve the current SQL strict inequalities, recurring-master inclusion, and
  all-day/recurrence expansion. This change is about the window, not the semantics
  inside it.
- Remove `day_ms` **after** callers migrate, not before.
- The same window feeds provider fetch, cache overlap, **and** `calendar_sync`
  horizon metadata. Metadata that disagrees with what was fetched is the bug in a
  different costume — assert they agree.
- Token-recovery full fetch uses the same formatting. Easy to miss; you caught it.

## Test table — this is the deliverable

Your list is right and I am holding you to all of it: UTC, an east offset, a west
offset, **+05:30 and +05:45**, spring-forward, fall-back, and an invalid window.
Assert **exact endpoints and durations**, not just "contains the event".

**Every test names its zone explicitly.** A test that passes only under the
machine's `TZ` is worse than no test. No `sleep`, and no dependence on today's
date.

## File coordination

`hydrate_calendar.rs` is shared with **THE-455**, running as a sibling lane. You
own the `day_ms` removal and the window threading; THE-455 owns ingestion bounds.
**Land order is THE-459 first, THE-455 second** — you are the one deleting
`day_ms`, so going second would make its diff conflict. List the functions you
touched in that file in your report.

On THE-451/THE-452: your own scope note says date-oriented providers keep their
existing endpoints, which makes this self-contained. Proceed. If you hit a genuine
dependency on either, stop and report it rather than reaching into them.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
