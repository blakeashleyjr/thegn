# Weather

## ADDED Requirements

### Requirement: Weather is an optional provider seam with zero default cost

Weather SHALL be governed by a `[weather]` config table with `enabled = false`
by default: while disabled, no network request is ever made, no background
work runs, and no widget or popup row renders. The backend SHALL be a
provider seam kind — `wttr_in` implemented (keyless), `open_meteo` and
`openweathermap` reserved — probed by `thegn doctor` as
implemented/unreachable/reserved. Vendor specifics (URLs, payload field
names) MUST stay inside the provider implementation file, and the payload
decode to the normalized snapshot model MUST be pure core logic under the
coverage gate.

#### Scenario: Disabled means silent

- **WHEN** `[weather]` is unconfigured
- **THEN** no weather network request, thread, or UI element exists

#### Scenario: Reserved provider kind

- **WHEN** `provider = "open_meteo"` is configured
- **THEN** config loads, no fetch occurs, and `thegn doctor` reports the
  weather seam as reserved for that kind

### Requirement: Weather renders in the date/time surfaces

When enabled, weather SHALL render as a masthead bars widget (`weather`,
placeable like `date`/`clock`) showing a condition glyph and temperature, and
as a row in the calendar popup adjacent to the world clocks showing
condition, temperature, high/low, and a compact forecast strip when the
provider supplied one. Clicking the widget SHALL open the calendar popup via
the existing `open-calendar` action — no new action id or chord. Condition
glyphs MUST resolve through the capability chokepoint (Unicode tier with
ASCII fallback); the surfaces MUST remain legible on an ASCII-only,
non-colour terminal, and MUST be pinned to deterministic content under
`THEGN_E2E=1`.

#### Scenario: Widget click opens the calendar popup

- **WHEN** the user clicks the weather widget
- **THEN** the calendar popup opens with the weather row visible

#### Scenario: ASCII terminal

- **WHEN** the terminal reports no Unicode support
- **THEN** the widget and popup row render with ASCII fallback glyphs and
  plain-text temperatures

### Requirement: Fetching is off-loop, cached, and quiet on failure

Weather fetches SHALL run on a background lane — never on the event loop and
never before the first frame — delivering snapshots over a channel with a
waker pulse, on a refresh interval floored at 600 seconds regardless of
configuration. The last-good snapshot SHALL be cached in the state DB and
shown at launch with no network access; a snapshot older than
`stale_after_secs` MUST render with a staleness indication, and one older
than a hard expiry MUST hide rather than mislead. A fetch failure MUST keep
the last-good snapshot and produce no toast, statusbar error, or status-line
noise — provider reachability is reported by `thegn doctor` only.

#### Scenario: Launch offline

- **WHEN** thegn starts with a cached snapshot and no network
- **THEN** the widget shows the cached reading immediately, marked stale once
  past the threshold, and no error surfaces

#### Scenario: A stray zero interval

- **WHEN** `refresh_interval_secs = 0` is configured
- **THEN** the effective interval is the 600-second floor

#### Scenario: Long-offline hard expiry

- **WHEN** the cached snapshot is older than the hard-expiry age
- **THEN** the widget and popup row hide instead of showing obsolete weather

### Requirement: Location and units are explicit, bounded user data

The `location` key SHALL be a free-form place string sent to the provider as
given; when empty, the provider infers a city-level location from the request
IP, and the config documentation MUST state this plainly. thegn MUST NOT read
any OS geolocation API or transmit any data beyond the configured location
and units. `units` SHALL support `auto` | `metric` | `imperial`. Reserved
keyed providers MUST take credentials only as SecretRef (`env:` / `file:`);
provider URLs MUST be HTTPS. `thegn config validate` SHALL warn on unknown
units and reject a raw credential value.

#### Scenario: Empty location

- **WHEN** `enabled = true` with `location = ""`
- **THEN** the request carries no location string and the provider's IP-based
  inference is used, as documented in `config.toml.example`

#### Scenario: No OS geolocation

- **WHEN** weather is enabled on any platform
- **THEN** no OS location service is queried; only the configured string (or
  nothing) leaves the machine
