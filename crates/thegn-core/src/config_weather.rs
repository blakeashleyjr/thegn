//! The `[weather]` config family — the reading shown beside the clock in the
//! masthead and as a block in the calendar popup. Kept in a sibling module
//! (rather than the god-file `config.rs`) per the keep-god-files-flat guidance;
//! `config.rs` re-exports everything here.
//!
//! The whole family is **off by default**: `enabled = false` is the consent
//! step. With it off nothing here is read, no thread runs, and no request is
//! ever made — [`WeatherConfig::poll_secs`] returns `None`, so the ticker emits
//! no weather slot at all and the 0%-idle contract is untouched for the user
//! who never turns it on.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::{config_enum, config_warn};
use crate::seam::Kind;
use crate::secretref::{BareAs, SecretRef};
use crate::weather::{Units, resolve_units};

/// The floor on the weather refresh interval, in seconds.
///
/// Applied in [`WeatherConfig::refresh_secs`] rather than at the ticker, so
/// *every* caller inherits it. Two reasons for the value: wttr.in caches each
/// location for roughly 10–15 minutes itself, so a shorter interval buys
/// nothing but egress; and a stray `0` must never spin a poll loop against a
/// free community service. (The `[pr_queue] poll_secs` lesson, moved one layer
/// up — the same rule `config_calendar::MIN_REFRESH_SECS` states.)
pub const MIN_REFRESH_SECS: u64 = 600;

/// The one host v1 ever contacts. There is deliberately no user-configurable
/// provider URL — nothing to typo, nothing to validate, and no way to point the
/// feature at an arbitrary endpoint. The constant lives here as documentation;
/// the request is built in `thegn_svc::weather`.
pub const WTTR_IN_BASE: &str = "https://wttr.in/";

/// `[weather]` — the reading beside the clock, and the popup's weather block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WeatherConfig {
    /// Master switch, and the consent step. `false` (the default) means no
    /// network activity and no background work whatsoever.
    pub enabled: bool,
    /// Which backend to ask. See the note on [`WeatherProviderKind`] for why
    /// this defaults to `wttr_in` *here* while the enum itself defaults to
    /// `none`.
    pub provider: WeatherProviderKind,
    /// What to ask about: `"Berlin"`, `"94110"`, `"48.85,2.35"`. Empty means
    /// the provider infers a city-level location from the request IP — thegn
    /// never reads an OS geolocation API, and `location` is the only thing it
    /// ever sends.
    pub location: String,
    /// `auto` (from the locale), `metric`, or `imperial`.
    pub units: WeatherUnits,
    /// Seconds between refreshes. Always floored at [`MIN_REFRESH_SECS`].
    pub refresh_interval_secs: u64,
    /// Past this age the reading is dimmed and dated rather than shown as
    /// current.
    pub stale_after_secs: u64,
    /// Past this age the reading is hidden entirely; `0` disables expiry.
    pub hard_expiry_secs: u64,
    /// Show the day strip in the calendar popup.
    pub show_forecast: bool,
    /// Days in that strip. Capped by what the provider actually returns
    /// (wttr.in gives at most 3).
    pub forecast_days: usize,
    /// Seconds before a fetch is abandoned. Clamped to `3..=60` in
    /// [`WeatherConfig::timeout`].
    pub timeout_secs: u64,
    /// Secret ref (`"env:VAR"` / `"file:PATH"`) for the reserved keyed
    /// providers. A raw key here is a validation **error** — credentials never
    /// live in `config.toml`. Nothing reads this yet; the custody rule is fixed
    /// before anything depends on it.
    pub api_key: String,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        WeatherConfig {
            enabled: false,
            // NOT `WeatherProviderKind::default()` — see the type's doc. A
            // *missing* `provider` key is filled from here (⇒ the sane keyless
            // default), while a present-but-invalid one falls back to the
            // enum's default (⇒ `none` ⇒ inert).
            provider: WeatherProviderKind::WttrIn,
            location: String::new(),
            units: WeatherUnits::Auto,
            refresh_interval_secs: 1800,
            stale_after_secs: 10_800,
            hard_expiry_secs: 86_400,
            show_forecast: true,
            forecast_days: 3,
            timeout_secs: 10,
            api_key: String::new(),
        }
    }
}

impl WeatherConfig {
    /// Effective refresh interval, floored at [`MIN_REFRESH_SECS`].
    ///
    /// Floored *here*, not at the ticker, so every caller inherits it rather
    /// than each remembering to clamp (the `CalendarAccount::refresh_secs`
    /// rule).
    pub fn refresh_secs(&self) -> u64 {
        self.refresh_interval_secs.max(MIN_REFRESH_SECS)
    }

    /// Seconds between polls, or `None` when weather is inert — disabled,
    /// `provider = "none"`, or a reserved kind.
    ///
    /// `None` means the ticker emits no weather slot at all, so a user who
    /// never enables weather pays nothing for the feature existing.
    pub fn poll_secs(&self) -> Option<u64> {
        if !self.enabled
            || self.provider == WeatherProviderKind::None
            || self.provider.is_reserved()
        {
            return None;
        }
        Some(self.refresh_secs())
    }

    /// True when a fetch should even be attempted. The single gate the host
    /// asks; nothing else re-derives the condition.
    pub fn is_active(&self) -> bool {
        self.poll_secs().is_some()
    }

    /// The configured unit preference, or `None` for `auto`.
    pub fn units_pref(&self) -> Option<Units> {
        match self.units {
            WeatherUnits::Auto => None,
            WeatherUnits::Metric => Some(Units::Metric),
            WeatherUnits::Imperial => Some(Units::Imperial),
        }
    }

    /// The units to request, resolving `auto` against the locale string.
    pub fn resolved_units(&self, locale: Option<&str>) -> Units {
        resolve_units(self.units_pref(), locale)
    }

    /// Fetch timeout, clamped to `3..=60` seconds — the same shape the ICS-URL
    /// account clamp uses. A `0` would mean "no timeout" to most HTTP clients,
    /// which is how a background task becomes a permanent one.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.clamp(3, 60))
    }
}

config_enum! {
    /// Where the reading comes from.
    ///
    /// **The enum default is `None` while [`WeatherConfig`]'s field default is
    /// `WttrIn`, deliberately.** `#[serde(default)]` on the container fills a
    /// *missing* `provider` key from `WeatherConfig::default()` (⇒ `wttr_in`,
    /// the sane keyless default), whereas a *present-but-invalid or reserved*
    /// value goes through this enum's infallible `Deserialize`, which warns and
    /// yields the **enum** default (⇒ `none` ⇒ inert). Without the split,
    /// writing `provider = "open_meteo"` would warn and then silently produce
    /// unexpected egress to wttr.in — a provider the user did not choose.
    pub enum WeatherProviderKind : "weather provider" {
        None           = "none",
        WttrIn         = "wttr_in" | "wttr",
        OpenMeteo      = "open_meteo" reserved,
        OpenWeatherMap = "openweathermap" | "owm" reserved,
    } default = None;
}

config_enum! {
    /// Measurement system for the reading.
    pub enum WeatherUnits : "weather units" {
        Auto     = "auto",
        Metric   = "metric" | "si" | "celsius",
        Imperial = "imperial" | "us" | "fahrenheit",
    } default = Auto;
}

/// Validate `[weather]`, returning one message per problem.
///
/// Only checks that can actually fire live here. `units` and `provider`
/// spelling is already strict-checked by the schema walker — that is what
/// `config_enum!` buys — so it is deliberately not re-checked, and there is no
/// provider-URL key to validate (see [`WTTR_IN_BASE`]).
pub fn validate_weather(cfg: &WeatherConfig) -> Vec<String> {
    let mut out = Vec::new();

    // Informational, not an error: `refresh_secs` already floors it. `0` is
    // quiet because it reads as "unset" rather than as an attempt at a rate.
    if cfg.refresh_interval_secs > 0 && cfg.refresh_interval_secs < MIN_REFRESH_SECS {
        out.push(format!(
            "weather.refresh_interval_secs: {} is below the {}s floor and will be raised to {} \
             (wttr.in caches ~10–15 minutes itself)",
            cfg.refresh_interval_secs, MIN_REFRESH_SECS, MIN_REFRESH_SECS
        ));
    }
    if cfg.stale_after_secs == 0 {
        out.push(
            "weather.stale_after_secs: 0 means a snapshot would be stale the instant it lands"
                .to_string(),
        );
    }
    if cfg.hard_expiry_secs != 0 && cfg.hard_expiry_secs <= cfg.stale_after_secs {
        out.push(format!(
            "weather.hard_expiry_secs: {} must be greater than stale_after_secs ({}) — otherwise \
             the widget hides before it ever renders stale (0 disables expiry)",
            cfg.hard_expiry_secs, cfg.stale_after_secs
        ));
    }
    if cfg.forecast_days > 5 {
        out.push(format!(
            "weather.forecast_days: {} is clamped by what the provider returns (wttr.in gives at \
             most 3 days)",
            cfg.forecast_days
        ));
    }
    // `location` is interpolated into a URL path segment, so a newline is a
    // request-splitting shape and an unbounded string is an unbounded URL.
    if cfg.location.chars().count() > 128 {
        out.push(format!(
            "weather.location: {} characters is too long (max 128) — it is sent as a URL path \
             segment",
            cfg.location.chars().count()
        ));
    }
    if cfg.location.contains(['\n', '\r']) {
        out.push(
            "weather.location: must not contain a newline — it is sent as a URL path segment"
                .to_string(),
        );
    }
    // SecretRef-only key: a raw literal is refused. No provider reads it yet;
    // the custody rule is fixed before anything depends on it.
    let key = cfg.api_key.trim();
    if !key.is_empty() && SecretRef::parse(key, BareAs::Literal).is_literal() {
        out.push(
            "weather.api_key: must be a secret reference (env:VAR or file:PATH), not a raw key"
                .to_string(),
        );
    }
    out
}

#[cfg(test)]
#[path = "config_weather_tests.rs"]
mod tests;
