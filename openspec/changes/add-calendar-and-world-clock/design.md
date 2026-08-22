# Design — calendar and world clocks

## Layering

```
thegn-core (pure, no tokio/termwiz, 95% coverage gate)
  calendar/{mod,grid,cursor,recur,ics,tz,locale,reminders}.rs
  config_calendar.rs · store/calendar.rs · db_calendar.rs
  plugin_api.rs                          ← the dormant contract, now consumed

thegn-svc (service seams)
  calendar/{mod,ics,ics_url,caldav,command}.rs
  plugin/proc.rs                         ← NDJSON subprocess runner (generic)
  control/http.rs                        ← /v1/calendar/*

thegn-host (compositor)
  detail/calendar/{mod,layout,render,keys}.rs
  calendar_docs.rs · hydrate_calendar.rs · handlers/calendar.rs
```

Everything decidable is pushed into core, because core is where the coverage
gate is and where purity makes exhaustive table tests cheap. The service layer
is thin I/O shells; the host owns only rendering and loop wiring.

## Decisions

### `chrono-tz`, pinned at 0.9

`chrono_tz::Tz` implements `chrono::TimeZone`, so it composes with the strftime
strings `[bars]` already uses and `%Z` yields real DST-aware abbreviations.
Crucially the database is **generated into the binary at build time**, which is
what makes world clocks work on Windows and inside a container or static Nix
build with no system `tzdata`. `jiff` reads `/usr/share/zoneinfo` on unix by
default, and forcing its bundle feature would change the tzdb source for
`gix-date` and `rusty-s3` too through feature unification.

Pinned at the 0.9 already in the lock file (via `tera`). **If a dependency ever
forces 0.10, the binary ends up with two complete copies of the IANA database** —
unify deliberately rather than letting that happen quietly. `filter-by-regex` is
deliberately not enabled: it would silently break "any IANA zone".

### Three-valued event times

```rust
enum EventTime {
    Date  { date },                 // floating; Christmas is Dec 25 everywhere
    Zoned { local, zone: TzRef },   // wall time; what recurrence stores
    Instant { at },                 // a fixed point
}
```

Collapsing these into one timestamp is _the_ classic calendar bug. `Zoned` is
what keeps a weekly 09:00 at 09:00 across a daylight-saving change; `Date` is
what stops an all-day event shifting by a day when viewed from another zone.

`TzRef` holds the zone _name_, not a resolved `Tz`, so a zone this build's
database does not know round-trips through the cache and the plugin wire instead
of failing deserialization and poisoning the whole payload.

### Recurrence: full model, hand-rolled expander

Every `BY*` part is parsed and stored even where the expander is not the thing
acting on it, so a rule round-trips losslessly. Serialization is the **RRULE
string**, not a struct — the spelling every calendar tool already speaks, and one
a shell plugin can emit by hand.

The load-bearing algorithm rule: **iterate in local wall time in the event's own
zone, convert to an instant only at the end.** Never add fixed durations.

Three consequences that are each a real-world bug if missed:

- A day-of-month a month lacks is **skipped**, not clamped — clamping invents
  meetings on Feb 28.
- A nonexistent local time (spring forward) shifts by the transition's _width_,
  so 02:15/02:30/02:45 stay three distinct occurrences instead of collapsing
  onto 03:00.
- An **excluded** occurrence still consumes a rule's `COUNT`.

### Extending `detail.rs`, not adding an overlay

The calendar is a new `DetailContent` variant plus one new `Section::MonthGrid`.
That inherits the whole `DetailOverlay` lifecycle. Two things are new:

- The content holds _live navigation state_, so its sections are rebuilt each
  frame from the cursor rather than snapshotted once. The state is then the only
  model, and a key press repaints correctly with no invalidation bookkeeping.
- Clicks inside the popup now do something. `handlers::overlay::pre_dispatch`
  gains an out-parameter for an action the popup cannot run itself, drained at
  the call site through the same dispatch the key path uses.

`detail/calendar/layout.rs` is pure and is the **single** source of geometry:
`render` draws into the rects it produces and `hit` tests against the same ones,
so painted and clickable cells cannot drift — a stronger guarantee than the
masthead's, where a layout function and a span function each compute the same
thing separately.

### Clock ticking

The clock previously repainted only when the stats sampler ticked, and stayed
live only because `StatsSnapshot::uptime_secs` happened to change every sample.
It now has its own slot in the _existing_ ticker thread, firing when
`now / period` changes — once a minute normally, once a second only if a
configured format actually renders seconds. That is one extra idle wake per
minute, an order of magnitude fewer than the stats wakes already happening, and
it sets only `bars_dirty`, so the frame is a two-row recompose.

Separately: `chrono`'s `format()` is lazy and panics at `Display`, which for
thegn is inside `masthead_widget` on the render path — a one-character typo in
`clock_format` was a compositor crash. Both formats are now validated at load.

### Sync

Modelled on `hydrate_tracker`: the background lane, a current-thread runtime
inside the blocking task, wake only on change. Three rules differ from the
issue/PR refreshers and each is easy to get wrong:

1. **Offline is per account, not per pass.** A local file or a subprocess plugin
   is not network-backed and must keep syncing offline — unlike `Issues`/`Pr`,
   which are gated at drain time.
2. **An empty full fetch is only believed when nothing is cached.** Otherwise a
   200-with-empty-body from a flaky proxy erases a month of meetings, and unlike
   an error nothing would warn.
3. **A failure touches nothing.** Events and the resume cursor both survive, so
   a blip degrades to stale data rather than no data.

The refresh floor lives in `CalendarAccount::refresh_secs`, not at the ticker, so
every caller inherits it — the `[pr_queue] poll_secs` lesson moved one layer up.

### Reminders

They ride the existing ticker on a coarse slot rather than getting a timer
thread: a "sleep until T" thread is a second always-on thread and one refactor
from a spin. The due window is half-open, so a reminder fires on the one tick
that straddles its trigger; a clock jump is clamped to an hour of catch-up so a
resume cannot replay a backlog.

Restart idempotency needs no new schema — `(event, occurrence, lead time)` is
encoded into the notification's existing `source_ref` and deduped with a
`SELECT 1`.

Delivery is the ordinary notification path, so reminders inherit the entire
`[[notifications.rules]]` engine — routing, priority, quiet hours, desktop
toasts — with **zero new config keys**. That reuse is the highest-leverage part
of the design.

### The plugin transport

`plugin_api.rs` already had the vocabulary. What did not exist anywhere in the
tree is reading _structured data_ back from a subprocess: `agent_run` caps and
discards stdout and treats the exit code as advisory.

NDJSON, not a JSON array and not `Content-Length` framing: one contract serves
both a poll and a watcher, memory stays bounded, and a shell script remains a
viable plugin. **Environment in, JSON out** — a plugin has to print JSON but
never parse it.

Two subtleties worth stating because getting them wrong is silent:

- Past the output cap the reader **keeps draining and discards**. Closing the
  pipe instead hands the plugin a SIGPIPE mid-write, turning an over-chatty
  plugin into "killed by signal" with no usable output — when what is wanted is
  its first N messages and an honest `truncated`.
- A timeout kills the **process group**, so a script whose child hangs cannot
  outlive the fetch.

`plugin/proc.rs` is deliberately outside `calendar/`: it is the reusable
primitive the "broad API surface" claim actually rests on.

### CalDAV

The one provider with real deltas — `sync-collection` returns tombstones, which
is why `EventPage::deleted` exists at all. The XML handling is narrow by design:
rather than a DAV/XML stack, it extracts the handful of elements the two reports
define, matching tags on their **local name** because prefixes vary by server. A
rejected sync token falls back to a full fetch, per RFC 6578 — without that the
account would be stuck forever.

## Rejected

- **A calendar panel section** — duplicates the popup for the cost of a
  `SECTION_ORDER` entry, a help context and a vocabulary entry.
- **A `CalendarPush` streaming trait** — the one place this design tempts an
  idle-CPU violation. A 15-minute poll is adequate for a calendar.
- **Google/Microsoft API providers** — OAuth registration, consent, refresh
  loop; and both publish a secret `.ics` URL that `ics_url` already reads.
- **The `rrule` crate** — not in the lock, and pulls its own `chrono-tz`.
- **The `icalendar` crate** — not in the lock, lossy model, and the content-line
  grammar is needed in-tree anyway to round-trip provider data.
- **A second in-flight fetch slot to prefetch neighbouring months** — buys
  ~150ms for real state complexity.
