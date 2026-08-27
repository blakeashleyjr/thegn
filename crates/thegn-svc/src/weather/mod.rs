//! Weather sources.
//!
//! House seam pattern (`thegn_core::seam`): an object-safe trait whose async
//! op returns a [`BoxFuture`] (never `async fn` — `test/async-trait-ratchet.txt`),
//! an error type implementing [`SeamError`], and a factory that returns `None`
//! for a deactivated or reserved kind. Read-only by construction: there is
//! nothing to write to a weather service.
//!
//! One backend is implemented, [`wttr_in`], and it is the **only** file that
//! knows wttr.in exists — the base URL, the `j1` query parameter and the
//! User-Agent all live there, so retargeting the seam touches one file.

pub mod wttr_in;

use thegn_core::config_weather::{WeatherConfig, WeatherProviderKind};
use thegn_core::seam::{BoxFuture, ErrorClass, SeamError};
use thegn_core::weather::{Units, WeatherSnapshot};

/// Why a weather fetch failed.
///
/// **Nothing here ever embeds the configured location.** It is the one piece of
/// user data this feature handles, so an error carries a status code or a
/// transport description and never the URL that was requested (see
/// `wttr_in::network_error`, which strips the URL a `reqwest::Error` embeds).
#[derive(Debug, Clone)]
pub enum WeatherError {
    /// Nothing to ask — the seam was built without a usable configuration.
    NotConfigured,
    /// Connect / timeout / transport failure. Retrying later may work.
    Network(String),
    /// The service answered, but not with a reading (non-2xx, oversized body,
    /// rate limit).
    Api(String),
    /// The body could not be understood.
    Parse(String),
    /// The seam has no implementation for this op.
    Unsupported(&'static str),
}

impl std::fmt::Display for WeatherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WeatherError::NotConfigured => write!(f, "weather is not configured"),
            WeatherError::Network(e) => write!(f, "weather network error: {e}"),
            WeatherError::Api(e) => write!(f, "weather provider error: {e}"),
            WeatherError::Parse(e) => write!(f, "could not read the weather response: {e}"),
            WeatherError::Unsupported(op) => {
                write!(f, "{op} is not supported by this weather provider")
            }
        }
    }
}

impl std::error::Error for WeatherError {}

impl SeamError for WeatherError {
    fn class(&self) -> ErrorClass {
        match self {
            WeatherError::NotConfigured => ErrorClass::NotConfigured,
            WeatherError::Network(_) => ErrorClass::Transient,
            WeatherError::Unsupported(_) => ErrorClass::Unsupported,
            // `Api` and `Parse` are both `Other`, and `Parse` is deliberately
            // NOT transient: a payload we cannot read is a provider change, not
            // a blip, so reporting it as transient would wrongly flip the whole
            // app to "offline" (the same argument `CalendarError::is_transient`
            // makes about a missing `.ics`).
            WeatherError::Api(_) | WeatherError::Parse(_) => ErrorClass::Other,
        }
    }
    fn unsupported(op: &'static str) -> Self {
        WeatherError::Unsupported(op)
    }
}

/// A source of weather readings.
///
/// Object-safe: [`fetch`](WeatherProvider::fetch) returns a [`BoxFuture`] so the
/// host can hold a `Box<dyn WeatherProvider>`.
///
/// **There is deliberately no caps struct.** The seam rule is "an optional
/// operation exists iff it has a caps bit"; this seam has no optional
/// operations at all — one round trip returns current conditions and the short
/// forecast together, and there is nothing to write — so a caps type would be
/// an empty struct that every probe serialized as `{}`. The omission is a
/// decision, not an oversight; adding a second op means adding caps with it.
pub trait WeatherProvider: Send + Sync {
    /// The stable provider token, also stamped onto
    /// [`WeatherSnapshot::provider`] and used in the cache key.
    fn provider_id(&self) -> &'static str;

    /// Current conditions + a short forecast, in ONE round trip.
    fn fetch<'a>(&'a self) -> BoxFuture<'a, Result<WeatherSnapshot, WeatherError>>;
}

/// The backend for a configuration, or `None` when weather is inert.
///
/// Mirrors `calendar::backend_from_account`: the gate is the config's own
/// [`WeatherConfig::is_active`], so "disabled", `provider = "none"` and every
/// reserved kind all fold into one predicate rather than being re-derived here.
pub fn provider_for(cfg: &WeatherConfig, units: Units) -> Option<Box<dyn WeatherProvider>> {
    if !cfg.is_active() {
        return None;
    }
    match cfg.provider {
        WeatherProviderKind::WttrIn => Some(Box::new(wttr_in::WttrInBackend::new(cfg, units))),
        // `is_active()` already excluded `none` and every reserved kind. Listed
        // exhaustively so a kind that graduates is a compile error here rather
        // than a silent `None`.
        WeatherProviderKind::None
        | WeatherProviderKind::OpenMeteo
        | WeatherProviderKind::OpenWeatherMap => None,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
