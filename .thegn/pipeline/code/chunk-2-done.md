# Chunk 2 — done: `[weather]` config, validation, documentation (`thegn-core`)

THE-46, stage `code`, chunk 2. Branch `tg/the-46-weather`, commit `07124e23`.

## What landed

| File                                             | Action                                                   |
| ------------------------------------------------ | -------------------------------------------------------- |
| `crates/thegn-core/src/config_weather.rs`        | new — the family, accessors, `validate_weather` (~240 l) |
| `crates/thegn-core/src/config_weather_tests.rs`  | new — 9 tests, `#[path]`-included                        |
| `crates/thegn-core/src/config.rs`                | edit — field + default + re-export + env knob            |
| `crates/thegn-core/src/config_validate.rs`       | edit — `validate_weather` call, pin 88 → 90              |
| `crates/thegn-core/src/lib.rs`                   | edit — `pub mod config_weather;` (one line)              |
| `config/config.toml.example`                     | edit — documented `[weather]` block after `[calendar]`   |
| `test/env-overlay-ratchet.txt`                   | edit — 10 `weather.*` keys pinned                        |
| `crates/thegn-core/src/config_tests.rs`          | edit — **not in the chunk spec**; see below              |
| `crates/thegn-core/src/config_tests_coverage.rs` | edit — **not in the chunk spec**; see below              |

### The two files outside the listed set

Both are mechanically forced by the `THEGN_WEATHER_ENABLED` knob the spec
asked for, and neither could be skipped without a red build:

- `config_tests_coverage.rs::config_overlay_apply_sets_every_field` builds
  `ConfigOverlay` as an **exhaustive struct literal** (no `..Default::default()`,
  deliberately — that is how the test forces a new overlay field to be
  exercised). Adding `weather_enabled` is a compile error until the literal
  gains it. Added the field + its `assert!(cfg.weather.enabled)`.
- `config_tests.rs::env_overlay_covers_every_knob` is guarded by
  `tests/env_overlay_coverage.rs::every_env_knob_is_exercised_by_the_coverage_test`,
  which scans `env_overlay` for `THEGN_*` literals and fails on any not driven
  by that test. Added the `("THEGN_WEATHER_ENABLED", "yes")` row + its assertion.

Nothing else is modified — `git show --stat` is exactly these nine paths.

## Public surface — design §4.2 verbatim

`MIN_REFRESH_SECS` (600) · `WeatherConfig` (11 keys) · `refresh_secs` ·
`poll_secs` · `is_active` · `units_pref` · `resolved_units` · `timeout` ·
`WeatherProviderKind` · `WeatherUnits` · `validate_weather`. Re-exported from
`config.rs` beside the `config_calendar` block; `Config::weather` sits directly
after `Config::calendar`.

One **addition** inside the module (permitted; changes no frozen signature):
`pub const WTTR_IN_BASE: &str = "https://wttr.in/"` — §6.5 says the rule
survives "as a constant and as a code comment", so the constant is here with
the _there is no user-configurable provider URL_ reasoning attached, and
`validate_weather`'s doc points at it to explain why no URL check exists.

## Decisions inside the chunk's latitude

- **The `default = None` / `WttrIn` split is documented on the enum, not the
  struct field**, because the enum's `Deserialize` is what makes it load-bearing.
  The struct's `Default` carries a back-pointer comment so a future reader who
  "tidies" it to `WeatherProviderKind::default()` sees what that would cost.
- **`is_reserved()` reaches `poll_secs` through `crate::seam::Kind`**, so the
  reserved gate is the same predicate `thegn doctor` and the schema walker use
  rather than a hand-written `matches!` that would rot when a kind graduates.
- **`refresh_interval_secs == 0` is quiet in `validate_weather`.** The spec says
  "below the floor _and non-zero_"; `0` reads as "unset" rather than as an
  attempt at a rate (and `refresh_secs()` floors it either way). Comment records
  the reasoning; a test pins the silence.
- **`api_key` uses `SecretRef::parse(.., BareAs::Literal).is_literal()`** — the
  `[[model_proxy.providers]]` precedent — rather than a hand-rolled
  `starts_with("env:")`. Same rejection for a raw key, but `keyring:` is also
  accepted, which is a real SecretRef form the spec's prose predates. The
  message still names `env:VAR` / `file:PATH`, which is what the example shows.
- **`location` rejects `\r` as well as `\n`.** Both split a request line; the
  test drives the realistic `"Berlin\nHost: evil"` shape.
- **`forecast_days > 5` is worded "clamped by what the provider returns"**
  rather than promising a clamp accessor. No `forecast_days()` exists (design
  §4.2 froze no such accessor), so the message says what is actually true:
  chunk 5 renders `min(forecast_days, snapshot.forecast.len())`.

## Verification

- `cargo nextest run -p thegn-core config_weather` — **9/9 pass**.
- `cargo nextest run -p thegn-core` (whole crate, lib + all integration
  binaries) — **3382/3382 pass, 2 skipped**. That includes every gate this chunk
  moves: `config_validate::tests::marked_definition_count_is_pinned` (the
  re-pinned **90**), both `config_example` drift tests, both
  `env_overlay_coverage` tests, and both `hm_module_drift` tests.
- `cargo clippy -p thegn-core --all-targets` — clean for every file in this
  chunk (lib **and** test targets).
- `nix fmt` applied; the pre-commit treefmt hook passed on the commit.
- Live check with the built binary, `XDG_STATE_HOME`/`XDG_CONFIG_HOME` isolated:
  - default config ⇒ `config validate` exit 0, no findings;
  - `config/config.toml.example` copied in as the config ⇒ `ok`;
  - a deliberately broken `[weather]` ⇒ all six expected findings, each exactly
    once, with the schema walker independently catching the two enum spellings
    (no duplication between the walker and `validate_weather`).

## Two notes for whoever runs the final gate

1. **`just quick thegn-core` is still red on the same pre-existing lint chunk 1
   flagged** — `clippy::manual_ok_err` at `sandbox_cpucap.rs:297`. Confirmed
   untouched by this branch (`git diff --name-only main...HEAD` lists no
   `sandbox_cpucap.rs`). One-line fix (`v.parse().ok()`), left alone here for
   the same reason chunk 1 left it: it is outside the chunk's file set.
2. **`validate_weather`'s "informational" messages still exit non-zero.**
   `thegn config validate` renders the whole `Vec<String>` at ERROR and counts
   every entry as a problem — there is no severity channel in the return type
   (`validate_calendar` has the same shape). So the refresh-floor and
   forecast-days notes are _worded_ as informational and are not load failures,
   but they do make `config validate` report a problem. If the change wants a
   real advisory tier, that is a `config_validate` signature change and belongs
   in its own change, not here.

## Handoff notes for chunks 3–5

- Chunk 3: `cfg.weather.timeout()` is the clamped `Duration`;
  `cfg.weather.resolved_units(locale)` gives the `weather::Units` to request;
  `WTTR_IN_BASE` is the constant to build the URL from. `location` is already
  length- and newline-checked by `config validate`, but the fetch path must
  still percent-encode it — validation is advisory, not a load-time reject.
- Chunk 4: gate the whole task on `cfg.weather.is_active()` and the ticker slot
  on `cfg.weather.poll_secs()`. Both already fold in the disabled / `none` /
  reserved cases, so don't re-derive the condition at the call site.
- Chunk 5: `[weather] enabled` has an env knob, so `e2e_freeze` can force it off
  either way; the design's §6.4 route (`cfg.weather.enabled = false` in
  `apply_to_config`) is still the one to take.
- Chunk 5's openspec sync: §6.3's observable behaviour is now pinned by
  `a_missing_provider_key_defaults_to_wttr_in_but_a_reserved_one_disables` —
  the spec scenario should be rewritten against that, not the other way round.
