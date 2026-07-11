//! Response-header extraction: upstream-reported cost and `Retry-After`.
//!
//! Port of the Go proxy's `costFromHeaders` (metrics.go) plus standard
//! `Retry-After` handling (the Go proxy only parsed reset hints from 429
//! bodies; the header is the RFC-standard signal and several providers send
//! it). Body hints still win when present — they tend to be more precise —
//! so the router consults [`crate::reset::parse_reset_from_body`] first.

use reqwest::header::HeaderMap;

/// Header names that carry a per-request cost in USD, in preference order.
const COST_HEADERS: &[&str] = &[
    "x-nanogpt-cost",
    "x_nanogpt_pricing",
    "x-upstream-cost-usd",
    "x_upstream_cost_usd",
    "x-openrouter-cost",
];

/// An upstream-reported request cost (USD), when a known cost header parses.
pub fn header_cost(headers: &HeaderMap) -> Option<f64> {
    for name in COST_HEADERS {
        if let Some(v) = headers.get(*name)
            && let Ok(s) = v.to_str()
            && let Ok(c) = s.trim().parse::<f64>()
            && c.is_finite()
            && c >= 0.0
        {
            return Some(c);
        }
    }
    None
}

/// The `Retry-After` deadline as epoch millis, given `now_ms`. Accepts the
/// delta-seconds form (integer or fractional) and the HTTP-date form.
pub fn retry_after_ms(headers: &HeaderMap, now_ms: i64) -> Option<i64> {
    let v = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(secs) = v.parse::<f64>() {
        if secs > 0.0 {
            return Some(now_ms + (secs * 1000.0) as i64);
        }
        return None;
    }
    let dt = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    let ts = dt.timestamp_millis();
    (ts > now_ms).then_some(ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn hm(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn cost_parses_known_headers() {
        assert_eq!(header_cost(&hm("x-nanogpt-cost", "0.00042")), Some(0.00042));
        assert_eq!(header_cost(&hm("x-upstream-cost-usd", " 1.5 ")), Some(1.5));
        assert_eq!(header_cost(&hm("x-openrouter-cost", "0")), Some(0.0));
    }

    #[test]
    fn cost_rejects_garbage_and_unknown() {
        assert_eq!(header_cost(&hm("x-nanogpt-cost", "not-a-number")), None);
        assert_eq!(header_cost(&hm("x-nanogpt-cost", "-1")), None);
        assert_eq!(header_cost(&hm("x-nanogpt-cost", "inf")), None);
        assert_eq!(header_cost(&hm("x-random-header", "0.5")), None);
        assert_eq!(header_cost(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_delta_seconds() {
        assert_eq!(retry_after_ms(&hm("retry-after", "30"), 1000), Some(31_000));
        assert_eq!(retry_after_ms(&hm("retry-after", "0.5"), 1000), Some(1500));
        assert_eq!(retry_after_ms(&hm("retry-after", "0"), 1000), None);
        assert_eq!(retry_after_ms(&hm("retry-after", "junk"), 1000), None);
        assert_eq!(retry_after_ms(&HeaderMap::new(), 1000), None);
    }

    #[test]
    fn retry_after_http_date() {
        // A date in the future relative to now_ms=0.
        let got = retry_after_ms(&hm("retry-after", "Wed, 21 Oct 2065 07:28:00 GMT"), 0).unwrap();
        assert!(got > 3_000_000_000_000); // past year 2065 in epoch ms
        // A past date yields nothing.
        assert_eq!(
            retry_after_ms(
                &hm("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT"),
                i64::MAX
            ),
            None
        );
    }
}
