# THE-465 plan — calendar `max_events` as a non-disableable admission budget

Stacked on THE-454 (`blake/the-454-…`, ea31ed56). Transport body/URL/address
policy (32 MiB streamed body cap, 64 KiB request/token cap) is THE-454's and is
consumed, not duplicated. THE-458 owns expansion budgets (`ExpansionBudget`,
`ExpansionError`, payload 1 MiB, children 16384, source visits 65536); this
change owns **source admission** and chooses compatible numbers so an admitted
cache can never exceed what expansion accepts per event. THE-455 (local
traversal/determinism), THE-456 (concurrency), THE-457 (recurrence) stay open.

## Findings (audited on this branch)

- `CalendarRouter::from_config` never forwards `max_events`; every backend starts
  at 0 and `> 0` means "unlimited"; `with_max_events` has no caller.
- ICS: `unfold` builds a `Vec<String>` of the whole document; `parse_ics` builds
  every `CalEvent` before any truncation; `e.calendar = cal_name.clone()` copies
  an unbounded `X-WR-CALNAME` into every event; `nested` grows without bound.
- CalDAV: `parse_multistatus_checked` materializes every `<response>` body
  (unescaped copy) before parsing; the response body is unescaped **twice**
  (`take_element` on `response`, then again on `calendar-data`).
- Local ICS: `read_to_string` of arbitrarily large files; all files parsed first.
- Command: `spawn_ndjson` retains up to 20 000 × 1 MiB lines as `serde_json::Value`
  before the adapter looks at any of them; `truncated` became `partial: true`.
- Host `apply_page` **publishes a partial page**: a truncated full fetch replaces
  the whole account cache and advances the cursor (ETag / sync-token), so the
  missing events are never re-requested. This is the data-loss half of the bug.
- `validate_calendar` accepts `max_events = 0` and any upper value.

## Design

### Core — `thegn_core::calendar::admission` (new, substrate-free, std atomics only)

- `AdmissionBudget` (Copy): `max_events: NonZeroUsize` in `[1, 10_000]`,
  default 2000. `AdmissionBudget::new(n) -> Result<_, AdmissionConfigError>`;
  `from_config_clamped(n) -> (Self, Option<warning>)` for runtime (0 → 1,
  above 10 000 → 10 000, with a visible `config_warn`); zero is never "unlimited".
- Fixed code ceilings (not config): per-event retained bytes 1 MiB and child
  entries 16 384 (= THE-458's `MAX_EVENT_PAYLOAD_BYTES` / `MAX_EVENT_CHILD_ENTRIES`),
  per-account retained bytes 32 MiB (= THE-458 retained bytes; Revision 2
  scales this with `max_events`, 32–96 MiB), source document
  32 MiB (= THE-454 body cap), logical ICS line 1 MiB, component nesting 16,
  global records 65 536 (= THE-458 source visits). Global bytes were 128 MiB
  here; Revision 2 derives them instead, as
  `2 * MAX_ACCOUNT_BYTES + MAX_SOURCE_DOCUMENT_BYTES` (224 MiB), so one
  maximal account can never refuse itself.
- `AdmissionPool` — process-global (`OnceLock<Arc<_>>`) or isolated (tests):
  atomic records/bytes counters, **non-blocking** `try_reserve` (CAS; never waits,
  so no deadlock/starvation — a refusal is a typed error and the account retries
  on its next tick with its cache/cursor untouched).
- `AdmissionLease` — owns a reservation; releases on `Drop`; not `Clone`.
- `AdmissionMeter` — per account fetch: checks per-event → per-account → global
  **before** each allocation it guards; tracks retained vs transient (input
  document/body) bytes; `into_lease()` drops transient reservations and hands
  the retained reservation to the page.
- `AdmissionError { limit: AdmissionLimit }` — constant, redacted messages.
- ICS: `parse_ics_admitted(input, zone, &mut meter) -> Result<Vec<CalEvent>, _>`:
  - incremental unfold iterator over borrowed slices; a logical line is only
    concatenated after its unfolded length is known ≤ 1 MiB; an oversized line of
    a non-retained property is skipped without allocating, an oversized retained
    property inside a VEVENT refuses the fetch (`EventBytes`);
  - every retained string/child/map entry is charged (per-event + account +
    global) **before** it is stored; the event record is reserved **before**
    `finish()` builds the `CalEvent`; the per-event calendar-name copy is charged;
  - nesting depth is capped.
  - `parse_ics(input, zone)` stays as a convenience wrapper over an isolated
    maximum-budget meter for fixtures/tests; no production caller.

### Svc — typed budget into every backend, lease-owning pages

- `AccountAdmission { budget, pool }` is a **required** constructor argument of
  `IcsBackend`, `IcsUrlBackend`, `CalDavBackend`, `CommandBackend`;
  `with_max_events` is deleted. `CalendarRouter::from_config` derives it from
  `CalendarConfig::max_events` (clamped + warned) and the global pool;
  `from_config_with_pool` for tests.
- `EventPage`: fields private (`events()`, `deleted()`, `sync_token()`,
  `is_unchanged()`), no `Clone`, no `partial`; owns its `AdmissionLease`, so the
  global reservation lives exactly as long as the retained data. Router stamping
  (`source`/`color`) goes through `EventPage::stamp`, which charges the stamped
  bytes first. `EventPage::try_new(…, &AccountAdmission)` charges already-built
  values (tests / in-memory callers).
- Overflow is `CalendarError::Admission(AdmissionError)` — never `Ok(partial)`.
  The host's existing failure rule then keeps the prior cache **and** cursor and
  records a truthful `last_error` (health) for the account.
- ICS URL / CalDAV: reserve the THE-454 body ceiling as transient before the
  request, shrink to the actual body length after the read, parse incrementally
  into the meter, release the body reservation when parsing ends.
- CalDAV: streaming multistatus walk over borrowed slices, one `<response>` at a
  time; leaf values unescaped once (fixes the double-unescape); deletions charged
  as records + bytes; resource `calendar-data` > 32 MiB refused.
- Local ICS: per file, size check + bounded `take()` read, transient reservation,
  incremental parse into one account meter; overflow stops immediately.
- Command: new `proc::spawn_ndjson_stream` with a per-line sink running on the
  reader thread (drains after refusal to avoid SIGPIPE); the calendar sink
  decodes one ≤ 1 MiB line at a time, reserves the record **before** each
  `CalEvent` is decoded from the array element and charges it right after;
  `MAX_LINES` truncation is now an admission error, not a partial page.
  `spawn_ndjson` keeps its behaviour for other plugin users.
- Router: sequential per-account fetch is unchanged, but results are handed to a
  per-account sink (`list_events_each`), so the host applies and drops each page
  (releasing its lease) before the next account is fetched — a router instance
  holds at most one page, and later accounts are never starved by earlier ones.

### Host

- `sync_accounts` uses `list_events_each`; `apply_page` uses accessors; the
  `partial` special case is removed (no partial page exists).

### Config / docs / spec

- `validate_calendar` rejects `max_events` outside `[1, 10000]`; schemars range.
- `config.toml.example`, `docs/help/calendar.md`, calendar spec requirement
  (admission budget, all-or-nothing, cache/cursor preserved, global budget).

## Peak memory per concurrent account fetch (bounded)

transport body ≤ 32 MiB (THE-454) + one logical line ≤ 1 MiB + one CalDAV
leaf/unescape ≤ 32 MiB (only if entity-escaped; else borrowed) + retained
output ≤ 32 MiB per account at the default budget, ≤ 96 MiB at the maximum
(Revision 2 scaled it with `max_events`) — all reserved in the global pool
(224 MiB since Revision 2) except the
transient per-line/params scratch (≤ ~3 MiB). Command: one line (1 MiB), walked
element by element (Revision 1), + retained output. Globally: ≤ 224 MiB
(Revision 2) of reserved body+output+derived rows across every router instance,
≤ 65 536 admitted records.

## Tests (lane: core + svc calendar, host hydrate_calendar)

config 0 / 1 / 10000 / 10001; budget exact / one-over for events, deletions,
bytes, per-event bytes, children, oversized single line (retained vs skipped),
nesting, calendar-name amplification; incremental unfold equivalence; global
pool refusal across two meters + release on page drop; router stamp accounting;
local file/dir overflow; ICS URL + CalDAV huge-feed overflow over the loopback
fixtures; command huge stream / many deletions / MAX_LINES; host: overflow error
leaves prior cache + cursor intact and records `last_error`.

## Revision 1 — adversarial review (REQUEST CHANGES) addressed

- #1 CalDAV and command deltas refused for the account's own volume are
  retried once as a full fetch, under a fresh meter.
- #2 Refusals raise an Alert toast that names the account and the limit. The
  toast is deduplicated against the stored `last_error`. Local and URL ICS
  count only events that can occur in the sync horizon. The per-account byte
  budget is now `max_events × 8 KiB`, clamped to 32–96 MiB. The messages tell
  the user to raise `max_events`. A legacy `0` runs as the default.
- #3/#4 Plugin `events` lines are walked in place with serde visitors, with no
  `Value` tree. The record budget is checked before each element is decoded. A
  malformed element or deletion fails the run with a value-free `Parse` error.
- #5 CalDAV checks the raw href and token length before unescaping. Unescaping
  is a single pass, so the 1× transient reservation matches the real peak.
- #6 Inside an event, oversized lines with a folded name, or BEGIN/END lines,
  are refused.
- #7 Local reads use exact capped growth, so capacity never exceeds the
  reservation.
- #8 Cache rows are reserved under the page lease. Contention refusals write
  nothing, so the next sync retries. `list_events` is test-only.
  `from_meter` has a `debug_assert`.
- #9 A counting-allocator integration test (`ics_admission_alloc`), plus tests
  for the incremental fallback, the malformed middle page, and 409 fallback
  lease release.

## Revision 2 — re-review (W1–W3, S1–S5)

- W1 `GLOBAL_MAX_BYTES` is now `2 * MAX_ACCOUNT_BYTES + MAX_SOURCE_DOCUMENT_BYTES`
  (a const assert pins it), so a maximal account plus its derived cache rows
  plus one body always fit; and the derived-rows reservation is advisory —
  a refusal logs and the page is still applied, so it can never silently drop
  a valid page.
- W2 A contention refusal now records a 60 s in-process backoff for that
  account, so the popup cannot re-fetch it on every month change, and still
  writes nothing to the DB (no false attempt stamp, no false error). A forced
  refresh bypasses the backoff.
- W3 Help + spec now say events outside the horizon are neither counted nor
  cached, naming `horizon_past_days`/`horizon_future_days`.
- S1 Window slack is two days each side (UTC+14 vs UTC-12).
- S2 Help notes that a COUNT-style feed still counts every occurrence.
- S3 The plugin fallback shares one absolute deadline with the first run.
- S4 The byte-limit message says to raise `max_events` above 4000, where the
  scaled budget leaves its floor.
- S5 A counting-allocator test over a 1 MiB plugin `events` line locks in the
  no-Value-tree property.

## Revision 3 — approve fold-ins

- The plugin allocation guard uses `{"a":1}` elements (an empty
  `serde_json::Map` does not allocate, so `{}` understated the tree it exists
  to forbid) under a 4 MiB bound, and a sibling test decodes the same line as
  one `serde_json::Value` and asserts that it _exceeds_ that bound — so the
  guard is proven to discriminate rather than merely to pass.
- The stale "global bytes 128 MiB" figures in the Design and peak-memory
  sections are corrected to the derived 224 MiB.
- Poisoned-lock arms in the contention backoff recover the map instead of
  silently skipping the backoff, and the DB-side contention test uses a
  pid-scoped account name (the backoff map is process-wide and `just coverage`
  runs the suite in one process).
- Rebased onto main at `18060ae8` now that THE-454 has landed.
