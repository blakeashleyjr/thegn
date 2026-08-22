# Calendar and world clocks

## Summary

thegn's entire date/time system is four calls to `chrono::Local::now()`: two
masthead widgets and a two-row "Date & time" popup behind them. There is no
timezone support past the OS local zone, no calendar, and nothing can contribute
an event.

This turns the date/clock widgets into a real calendar: a month grid with today
highlighted and fast month paging, a per-day agenda, and configurable world
clocks with accurate IANA data — sitting on an extension surface broad enough
that plugins can supply events and sync with calendar providers.

Two existing pieces shape the design and are why this is smaller than it sounds:

1. **The popup already exists and is already anchored top-right.** `detail.rs`
   is a mature overlay system — an `Option<DetailOverlay>` in the loop, snapshot
   at open, in-place drilling, `Placement::near` opening downward from a
   masthead item. Extending it inherits the loop wiring, hit-testing, bar
   navigation and `render_plan::Overlays` integration rather than reimplementing
   them.
2. **A complete, tested Plugin API v0 has been sitting dormant since the WASM
   strip.** `thegn-core/src/plugin_api.rs` carries `ExtensionPoint::DataSource`,
   `CadenceHint`, `Capability` grants, `HostContract::negotiate`, and an audit
   log — all unit-tested, with no loader and exactly one production consumer
   (a config field nothing reads). The calendar is the right first consumer, and
   waking it delivers the plugin surface as reusable infrastructure instead of a
   one-off.

## Impact

- Roadmap: **AF 155** (next calendar event widget) gets its data layer; **AM
  473** (calendar tile) is superseded by a first-class surface rather than a
  wrapped TUI. **AX 226** (scheduled tasks with RRULE + IANA timezones) can
  reuse `calendar::recur` and `calendar::tz` wholesale — a real argument for
  putting recurrence in core rather than in a provider.
- Spec: new `calendar` and `plugin-data-sources` capabilities. `state-db` —
  ADDED `calendar_events` and `calendar_sync`.
- Code: new `thegn-core/src/{calendar/,config_calendar.rs,db_calendar.rs,
store/calendar.rs}`, `thegn-svc/src/{calendar/,plugin/}`,
  `thegn-host/src/{calendar_docs.rs,hydrate_calendar.rs,detail/calendar/,
handlers/calendar.rs}`.
- **DB schema change: `user_version` 51 → 52** for the two cache tables.
- New dependencies: `chrono-tz` and `iana-time-zone`, both already in the lock
  file transitively. `chrono` gains its `serde` feature.
- One new action id (`open-calendar`), one new chord (`Alt-d`), and two new
  `NotificationKind`s — so a new `docs/help/calendar.md` claims them (the help
  ratchet enforces this).

## Rationale

**Extending the existing popup, not adding an overlay.** Every other approach
duplicates the anchoring, dismissal and damage-tracking that `detail.rs` already
gets right, and `render_plan::Overlays` would need a new field to avoid a
half-erased popup on the incremental-repaint path.

**Waking `plugin_api` rather than inventing a protocol.** The vocabulary
(manifest, capability grant, extension point, negotiate, audit) is already
written and tested. What is genuinely new is the _transport_: nothing in the
tree can read structured data back from a subprocess — `agent_run` caps and
discards stdout and treats the exit code as advisory. That runner is built here
as `thegn-svc/src/plugin/proc.rs`, deliberately outside `calendar/`, because the
next data-source tile needs exactly the same thing.

**Recurrence in core, hand-rolled.** The `rrule` crate is not in the lock and
pulls its own `chrono-tz`, which would put two complete copies of the IANA
database in the binary. Expansion is pure and window-bounded, so it tests
cheaply against the core coverage gate — and AX 226 gets it for free.

## Non-goals

- **Writing to a provider.** Every backend is read-only; the trait's write
  methods exist so `EditScope` and the per-account `sync_token` are fixed before
  anything depends on them, since retrofitting either would break the plugin
  wire format.
- **Google Calendar and Microsoft Graph as first-class providers.** Both need an
  OAuth client registration, a consent flow and a token-refresh loop — an entire
  subsystem — and both publish a secret `.ics` URL that `ics_url` already reads,
  as do Fastmail, Nextcloud and Proton. This is the single largest scope saver
  in the change.
- **A calendar panel section.** It would duplicate the popup for the real cost
  of a `SECTION_ORDER` entry, a help context, and a vocabulary entry.
- **Push/streaming providers.** A `CalendarPush` trait is the one place "broad
  API surface" tempts an idle-CPU violation; a 15-minute poll is entirely
  adequate for a calendar.
- **The `View`/`StyleRole` half of the plugin API.** That is the rendering side;
  a data source needs none of it, and `chrome::draw_plugin_view` stays dead.
