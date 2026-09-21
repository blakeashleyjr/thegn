# THE-458 code handoff (resumed row after row 516)

## In-flight review findings, resolved

1. Long-running overlap: recurring lookback is the event's full nominal span
   (+2 days of zone slack), saturating at NaiveDate::MIN (exact: nothing
   starts earlier). No `min(max_window_days)` clamp. COUNT=1 / RDATE
   instances from 20 years ago are found with <100 work units.
2. Budget before clone: overlap is decided from borrowed start/end
   (`span_dates`); occurrence + retained bytes are reserved before
   `materialize` (which never clones the recurrence). `source_cost` runs on
   every row (in or out of window): O(1) child counts checked before any
   child walk; bytes cover strings, zone names (start/end/RDATE/EXDATE),
   reminders, extra map entries with node overhead, every BY\* list.
   Bucket handles and new date keys are charged too.
3. Same-day inversion refused (order checked in the span's own frame);
   sub-second midnight is not midnight; zero-length is a point; a DST-gap
   start after a valid wall-time end is a point, not an error.
   Yearly interval*12 / weekly *7 checked; sub-daily steps via try_seconds.
   The old empty-period valve (silent truncation) is gone: uncounted rules
   fast-forward and stop one week past `to`; COUNT walks are work-bounded
   and refuse with Budget(RecurrenceWork). Sub-daily cap -> typed refusal.
   Fixed a latent month-arithmetic bug in the earlier fast-forward draft
   (saturating_sub on months skipped occurrences); differential test
   `fast_forward_matches_the_full_walk` pins it.
4. Reminder window: the worker evaluates exactly the dispatched
   `(from_ms, to_ms]`. `ReminderCursor` (hydrate_calendar.rs) fences acks by
   generation+window; stale/replayed acks ignored; cursor never regresses.
   Permit reserved at dispatch (spawn_bg's silent skip would strand the
   in-flight window); a drop guard acks Failed if the worker panics.
5. Cache errors: DB open/query failure -> CalendarError::CacheUnavailable
   (month "unavailable"/"stale"; reminders Failed, cursor kept).
   Vec-returning wrappers (`dates_in`, `occurrences`, `expand_local`,
   `recur::occurrences`, host `due_reminders` compat) are removed or
   `#[cfg(test)]`.

## Per-row vs whole-call failure (adversarial review F1-F4)

A defect in ONE row never fails the call. `ExpansionError::is_row_local()`
(InvalidSpan, Budget(EventPayload), Budget(EventChildren)) makes
`expand_calendar_with_budget` skip that row and count it in
`ExpandedCalendar::skipped`; the host adds it to `Cached::skipped` and reports
MalformedCache / "incomplete". The same reasoning as for undecodable rows: the
row stays in the SQLite cache, every tick re-reads the same bytes, so
escalating would blank the month and stop reminders for the life of the
process. Only the shared dimensions (WindowDays, SourceVisits, RecurrenceWork,
MaterializedOccurrences, BucketEntries, RetainedBytes), InvalidWindow and
Arithmetic are whole-call failures.

Ceilings (F3) are raised and their arithmetic is recorded above the constants
and pinned by `default_ceilings_admit_a_heavy_but_legitimate_month`:
occurrences 8,192 -> 131,072, bucket entries 32,768 -> 524,288, recurrence work
262,144 -> 4,194,304, retained bytes 32 -> 64 MiB. Sized so `max_events`
(2,000/account) worth of daily recurrences over the widened (~49-day) window
fits, and so memory — not an arbitrary count — is what binds first.

F4: the grid header carries the state (`MonthGridSection::status`, drawn where
the today chip goes), because `show_agenda = false` never builds the agenda
note.

## Not in scope / open

- Reminder lookahead is still today±1 day (THE-460).
- max_events admission (THE-465); transport (THE-454).
- Month merge never clears a date bucket that became empty, and a failed month
  can still show markers delivered by a neighbouring month's widened payload
  (both pre-existing).
- `load_cached` deserializes every matching cache row BEFORE the expansion
  budget sees them, so the 64 MiB ceiling bounds the expansion, NOT the peak
  memory of the read. Source-row admission is THE-465 / THE-455.
- `expand_by_date` and `reminders::due` remain pub with no production caller.
