# Chunk 2 — `[weather]` config, validation, documentation (`thegn-core`)

THE-46. Read `.thegn/pipeline/architect/design.md` §4.2, §6.3, §6.5, §7 first.
Iterate with `just quick thegn-core`; heavy gates only at the end.

## Scope

The `[weather]` config family and everything that makes it a first-class
config section: the struct, two `config_enum!`s, the accessors that carry the
refresh floor, `thegn config validate` coverage, the documented example block,
and the env-overlay bookkeeping.

Code against chunk 1's frozen types (`thegn_core::weather::Units`,
`resolve_units`) — design §4.1 gives their exact signatures.

## Files

| File                                            | Action                                                          |
| ----------------------------------------------- | --------------------------------------------------------------- |
| `crates/thegn-core/src/config_weather.rs`       | new                                                             |
| `crates/thegn-core/src/config_weather_tests.rs` | new (`#[path]`-included)                                        |
| `crates/thegn-core/src/config.rs`               | edit — `pub weather: WeatherConfig` field + default + re-export |
| `crates/thegn-core/src/config_validate.rs`      | edit — call `validate_weather`, bump the pinned enum count      |
| `crates/thegn-core/src/lib.rs`                  | edit — `pub mod config_weather;` (one line)                     |
| `config/config.toml.example`                    | edit — a documented `[weather]` block                           |
| `test/env-overlay-ratchet.txt`                  | edit — pin the keys that get no env knob                        |

**Shared file:** `lib.rs` is also touched by chunk 1 (`pub mod weather;`).
Add only your own line.

## Approach

### 1. `config_weather.rs`

Model it directly on `config_calendar.rs` — same module-doc register, same
"kept out of the god-file `config.rs`, which re-exports" note, same
`MIN_REFRESH_SECS`-in-the-accessor rule with the same reasoning recorded in a
doc comment.

Implement exactly the shape in design §4.2. Points that carry a decision:

- **`MIN_REFRESH_SECS = 600`.** Floored in `refresh_secs()`, not at the
  ticker, so every caller inherits it. Document _why_ (wttr.in caches
  ~10–15 min itself; and a stray `0` must never spin a poll loop — the
  `[pr_queue] poll_secs` lesson).
- **Enum default `None`, struct default `WttrIn`.** This is load-bearing and
  needs a comment: `#[serde(default)]` on the container fills a _missing_
  `provider` key from `WeatherConfig::default()` (⇒ `wttr_in`, the sane
  default), while a _present-but-invalid or reserved_ value goes through the
  enum's infallible `Deserialize`, which warns and yields the **enum**
  default (⇒ `none` ⇒ inert). Without this split, writing
  `provider = "open_meteo"` would silently produce unexpected egress to
  wttr.in. See design §6.3.
- **`poll_secs()` returns `None`** when `!enabled`, `provider == None`, or
  `provider.is_reserved()`. This is what makes the ticker emit no slot at all
  and keeps the 0%-idle contract for a user who never enables weather.
- **`is_active()`** is `poll_secs().is_some()`; the host uses it as the single
  gate.
- **`timeout()`** clamps `timeout_secs` to `3..=60` (`ics_url.rs` clamps the
  same way).
- **`resolved_units(locale)`** delegates to `thegn_core::weather::resolve_units`
  — do not reimplement the locale parse.

### 2. `validate_weather`

Return one message per problem. Only checks that can actually fire:

- `units` / `provider` spelling is already strict-checked by the schema walker
  (that is what `config_enum!` buys) — **do not** re-check them here.
- `refresh_interval_secs` below `MIN_REFRESH_SECS` and non-zero ⇒ an
  informational message saying it will be raised to 600 (not an error: the
  accessor already floors it).
- `stale_after_secs == 0` ⇒ "a snapshot would be stale the instant it lands".
- `hard_expiry_secs != 0 && hard_expiry_secs <= stale_after_secs` ⇒ the widget
  would hide before it ever renders stale.
- `forecast_days > 5` ⇒ clamp note (wttr.in gives at most 3).
- `location` longer than 128 chars, or containing a newline ⇒ reject (it goes
  into a URL path segment).
- `api_key` non-empty and not `env:`/`file:`-prefixed ⇒ **reject**, naming the
  SecretRef forms. Credentials never live raw in `config.toml`. (No provider
  reads it yet; the rule is fixed before anything depends on it.)

Wire it into `config_validate.rs` right beside the existing
`config_calendar::validate_calendar(&cfg.calendar)` call (~line 63), with the
same one-line rationale comment.

### 3. The pinned `config_enum!` count

`config_validate.rs`'s `marked_definition_count_is_pinned` asserts `88`. Two
new enums ⇒ **90**. Add a line to the running history comment above the
assert, in the established style:

```
// 88 → 90 (THE-46): `[weather] provider` (WeatherProviderKind — `wttr_in`
// implemented, `open_meteo`/`openweathermap` reserved) and `[weather] units`
// (WeatherUnits).
```

### 4. `config.rs`

- Add `pub weather: WeatherConfig,` to `Config` beside `pub calendar:` (line
  ~5153) with a one-line doc.
- Add its `Default` entry.
- Extend the `config_calendar` re-export block (~line 3206) with a sibling
  re-export of `WeatherConfig, WeatherProviderKind, WeatherUnits`.

### 5. `config/config.toml.example`

A `[weather]` block placed **immediately after** the `[calendar]` family
(after the `[[calendar.accounts]]` examples), because that is where the
date/time surfaces are documented. Every key documented, in the house voice.
Two things must be spelled out in prose:

- `enabled = false` is the default and **the consent step** — with it off
  there is zero network activity and zero background work;
- an empty `location` means the provider infers a **city-level** location from
  your request IP; a non-empty one is sent verbatim. thegn never reads an OS
  geolocation API and sends nothing else.

Sketch:

```toml
# Weather beside the clock (off by default). Enabling it is the consent step:
# with `enabled = false` nothing here is read, no thread runs, and no request
# is ever made. When on, exactly one host is contacted and the only thing sent
# is `location` — or nothing at all, in which case the provider infers a
# city-level location from your request IP. thegn never reads an OS location
# service. Shows as the `weather` widget in [bars] top_right and as a block in
# the calendar popup (click the date/clock, or Alt-d).
[weather]
enabled = false
provider = "wttr_in"        # "wttr_in" (keyless) | "open_meteo" (reserved) |
                            # "openweathermap" (reserved). A reserved value
                            # warns and disables weather rather than falling
                            # back to a provider you did not choose.
location = ""               # "Berlin", "94110", "48.85,2.35" — or "" to let
                            # the provider infer a city from your request IP
units = "auto"              # "auto" (from $LC_MEASUREMENT/$LANG) | "metric" | "imperial"
# Seconds between refreshes. Floored at 600 no matter what is written here —
# wttr.in caches ~10–15 minutes itself, and a stray 0 must never spin a poll
# loop against a free community service.
refresh_interval_secs = 1800
stale_after_secs = 10800    # past this the reading is dimmed and dated
hard_expiry_secs = 86400    # past this it hides entirely; 0 = never hide
show_forecast = true        # the day strip in the calendar popup
forecast_days = 3
timeout_secs = 10           # clamped to 3..=60
# api_key = "env:OWM_API_KEY"  # reserved keyed providers only; a raw key here
                               # is a validation ERROR — use env:/file:
```

### 6. Env overlay

Give `[weather] enabled` a real override (`THEGN_WEATHER_ENABLED`) in
`Config::env_overlay` beside the existing boolean knobs — it is exactly the
kind of flag a muse spec or a quick try wants to flip. Then run
`THEGN_RATCHET_UPDATE=1 just ratchet-update` (or add the lines by hand,
sorted) so the remaining `weather.*` keys are pinned in
`test/env-overlay-ratchet.txt`. Add a short reason line in the file's style if
the format allows it; otherwise the sorted keys are enough.

## Tests (`config_weather_tests.rs`)

Mirror `config_calendar_tests.rs`:

1. `defaults_are_inert` — `WeatherConfig::default().enabled == false`,
   `poll_secs() == None`, `is_active() == false`.
2. `a_missing_provider_key_defaults_to_wttr_in_but_a_reserved_one_disables` —
   two `toml::from_str` cases; this is the §6.3 decision, assert it directly.
3. `an_unknown_provider_or_unit_warns_and_falls_back` — `toml::from_str` of
   `provider = "nope"` yields `None`, not an error.
4. `refresh_is_floored_in_the_accessor` — `0`, `1`, `599` ⇒ `600`; `1800`
   passes through.
5. `poll_secs_is_none_unless_everything_is_on` — the disabled, `none`, and
   reserved cases.
6. `units_resolve_through_the_core_helper` — explicit beats auto; auto follows
   the locale.
7. `validate_flags_each_problem_once` — one case per rule in §2, including a
   raw `api_key` being rejected and an `env:` one accepted, and a location
   with a newline being rejected.
8. `timeout_is_clamped`.
9. A `Config`-level test that `toml::from_str("")` gives an inert `[weather]`
   (guards the field's `Default` wiring).

## Done criteria

- `just quick thegn-core` clean.
- `cargo nextest run -p thegn-core config_weather` green, and
  `cargo nextest run -p thegn-core config_validate` green **including** the
  re-pinned count of 90.
- `cargo run -p thegn-host -- config validate` (or the smoke path) reports no
  new findings on a default config.
- `config/config.toml.example` parses (it is loaded by the config-reference
  help page — a broken block breaks `just test`).
- Nothing outside the files listed above is modified.
