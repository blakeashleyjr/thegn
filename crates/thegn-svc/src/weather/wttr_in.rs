//! wttr.in — the one implemented weather backend, and the only file in the
//! workspace that knows the service exists.
//!
//! `?format=j1` returns current conditions **and** a short forecast in a single
//! GET, keyless. The location is a path segment; omit it and the service infers
//! a city from the request IP. Decoding is pure and lives in
//! `thegn_core::weather::decode_wttr_j1` — this file is the transport.

use std::time::Duration;

use thegn_core::config_weather::WeatherConfig;
use thegn_core::seam::BoxFuture;
use thegn_core::weather::{Units, WeatherSnapshot};

use super::{WeatherError, WeatherProvider};

/// The provider token — [`WeatherSnapshot::provider`] and the cache key.
pub const PROVIDER_ID: &str = "wttr_in";

/// The service base. wttr.in is HTTPS-only and there is deliberately no config
/// key for this — a user-supplied provider URL is a different feature with a
/// different threat model. Aliased from the core constant rather than
/// re-spelled, so the string itself exists exactly once in the workspace.
const BASE: &str = thegn_core::config_weather::WTTR_IN_BASE;

/// Sent on every request. reqwest sends no User-Agent by default and some
/// fronting CDNs reject that outright.
const USER_AGENT: &str = concat!("thegn/", env!("CARGO_PKG_VERSION"));

/// Refuse to buffer a body larger than this (the `j1` payload is ~10 KiB).
const MAX_BODY: usize = 1 << 20;

/// A wttr.in reader for one configuration.
pub struct WttrInBackend {
    /// As configured. Empty ⇒ the service infers a city from the request IP.
    location: String,
    /// Which unit family to *select* from the payload. wttr.in returns both, so
    /// nothing here converts.
    units: Units,
    timeout: Duration,
}

impl WttrInBackend {
    pub fn new(cfg: &WeatherConfig, units: Units) -> Self {
        WttrInBackend {
            location: cfg.location.trim().to_string(),
            units,
            timeout: cfg.timeout(),
        }
    }
}

/// The request URL for `location` (empty ⇒ IP-inferred).
///
/// Split out so it is testable without a client — and so the encoding is done
/// by the URL type rather than by hand: `push` percent-encodes the segment, so
/// a space, non-ASCII text or a `../` traversal attempt all become data, never
/// syntax.
///
/// There is no `?m` / `?u` unit flag: the `j1` payload carries **both** unit
/// systems and `decode_wttr_j1` selects between them. That is why this feature
/// contains no conversion arithmetic anywhere.
pub(crate) fn url_for(location: &str) -> Result<String, WeatherError> {
    let mut u = reqwest::Url::parse(BASE).map_err(|e| WeatherError::Api(e.to_string()))?;
    let location = location.trim();
    if !location.is_empty() {
        u.path_segments_mut()
            .map_err(|_| WeatherError::Api("bad base url".into()))?
            // The base ends in `/`, i.e. one trailing empty segment; drop it so
            // the location lands at `/<loc>` and not at `//<loc>`.
            .pop_if_empty()
            .push(location);
    }
    u.query_pairs_mut().append_pair("format", "j1");
    Ok(u.to_string())
}

/// A reqwest failure as a seam error, **with the URL stripped**.
///
/// `reqwest::Error`'s `Display` embeds the request URL ("error sending request
/// for url (…)"), and that URL contains the user's location. `without_url`
/// removes it, so the message keeps the transport reason and loses the one
/// piece of user data this feature handles.
fn network_error(e: reqwest::Error) -> WeatherError {
    WeatherError::Network(e.without_url().to_string())
}

impl WeatherProvider for WttrInBackend {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn fetch<'a>(&'a self) -> BoxFuture<'a, Result<WeatherSnapshot, WeatherError>> {
        Box::pin(async move {
            let url = url_for(&self.location)?;
            let client = reqwest::Client::builder()
                .timeout(self.timeout)
                .user_agent(USER_AGENT)
                .build()
                .map_err(network_error)?;
            let resp = client.get(url).send().await.map_err(network_error)?;

            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                // Named specifically because the recovery is "wait", not "check
                // your config" — wttr.in throttles anonymous callers.
                return Err(WeatherError::Api(
                    "rate limited (HTTP 429): the service throttles anonymous callers — \
                     wait for the next refresh"
                        .into(),
                ));
            }
            if !status.is_success() {
                return Err(WeatherError::Api(format!("HTTP {status}")));
            }
            // Two-step size guard (the `ics_url` shape): refuse an advertised
            // oversize body before reading it, and re-check what actually
            // arrived, since `content_length` is absent on a chunked response.
            if let Some(len) = resp.content_length()
                && len as usize > MAX_BODY
            {
                return Err(WeatherError::Api(format!(
                    "weather response too large ({len} bytes)"
                )));
            }
            let body = resp.text().await.map_err(network_error)?;
            if body.len() > MAX_BODY {
                return Err(WeatherError::Api("weather response too large".into()));
            }

            let mut snapshot =
                thegn_core::weather::decode_wttr_j1(&body, self.units, thegn_core::util::now())
                    .map_err(|e| WeatherError::Parse(e.to_string()))?;
            // The pure decode does not know which provider produced the body.
            snapshot.provider = PROVIDER_ID.to_string();
            Ok(snapshot)
        })
    }
}
