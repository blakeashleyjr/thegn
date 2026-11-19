//! Sprite usage for `thegn fleet cost`: CPU seconds and reserved GB-hours from
//! `GET /v1/usage/{name}?start=<created_at>`. Runs only from the explicit verb.

use anyhow::{Context, Result, anyhow};

use crate::provider::{CONTROL_TIMEOUT, response_json};

/// One sprite's usage since a start time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpriteUsage {
    pub cpu_seconds: f64,
    pub reserved_gb_hours: f64,
    /// The API reported an incomplete window.
    pub partial: bool,
}

/// One usage reply with its observation metadata.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageReading {
    pub usage: SpriteUsage,
    /// The newest non-null `last_recorded_at` among the buckets (sanitized:
    /// [0-9A-Za-z:.+-], at most 40 chars).
    pub last_recorded_at: Option<String>,
    /// True when any bucket has a non-null `last_recorded_at`, or carries
    /// nonzero CPU or memory (legacy shape).
    pub reported: bool,
}

/// Reserved GB-hours from memory byte-seconds (`byte_sec / 2^30 / 3600`).
/// Non-finite or negative input yields 0 so no NaN ever reaches the output.
pub fn gb_hours_from_byte_seconds(byte_sec: f64) -> f64 {
    if !byte_sec.is_finite() || byte_sec < 0.0 {
        return 0.0;
    }
    byte_sec / (1u64 << 30) as f64 / 3600.0
}

fn num(bucket: &serde_json::Value, keys: &[&str]) -> f64 {
    keys.iter()
        .find_map(|k| bucket.get(*k).and_then(serde_json::Value::as_f64))
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0)
}

fn recorded_at(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(|value| {
        value
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '+' | '-'))
            .take(40)
            .collect()
    })
}

fn has_nonzero(bucket: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| {
        bucket
            .get(*key)
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|value| value.is_finite() && value != 0.0)
    })
}

/// Parse a usage response: `{"usage":[{cpu_time_sec,
/// reserved_memory_byte_sec}…], "partial": bool}`. Buckets are summed.
pub fn parse_usage(v: &serde_json::Value) -> Result<SpriteUsage> {
    parse_usage_reading(v).map(|reading| reading.usage)
}

/// Parse a usage response together with the API's observation metadata.
pub fn parse_usage_reading(v: &serde_json::Value) -> Result<UsageReading> {
    let buckets = v
        .get("usage")
        .or_else(|| v.get("buckets"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("sprites usage response has no usage or buckets array"))?;
    let mut cpu = 0.0;
    let mut byte_sec = 0.0;
    let mut last_recorded_at = None;
    let mut reported = false;
    for b in buckets {
        cpu += num(b, &["cpu_time_sec", "cpu_seconds", "cpu_sec"]);
        byte_sec += num(
            b,
            &[
                "reserved_memory_byte_sec",
                "memory_byte_seconds",
                "reserved_byte_seconds",
                "byte_sec",
                "byte_seconds",
            ],
        );
        if let Some(value) = b.get("last_recorded_at").filter(|value| !value.is_null()) {
            reported = true;
            if let Some(value) = recorded_at(value)
                && last_recorded_at
                    .as_ref()
                    .is_none_or(|old: &String| value.as_str() > old.as_str())
            {
                last_recorded_at = Some(value);
            }
        }
        reported |= has_nonzero(b, &["cpu_time_sec", "cpu_seconds", "cpu_sec"])
            || has_nonzero(
                b,
                &[
                    "reserved_memory_byte_sec",
                    "memory_byte_seconds",
                    "reserved_byte_seconds",
                    "byte_sec",
                    "byte_seconds",
                ],
            );
    }
    Ok(UsageReading {
        usage: SpriteUsage {
            cpu_seconds: if cpu.is_finite() { cpu } else { 0.0 },
            reserved_gb_hours: gb_hours_from_byte_seconds(byte_sec),
            partial: v
                .get("partial")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        },
        last_recorded_at,
        reported,
    })
}

/// The usage window start for a sprite created at `created_at_secs`, floored
/// to its UTC hour.
pub fn usage_window_start(created_at_secs: i64) -> i64 {
    created_at_secs - created_at_secs.rem_euclid(3600)
}

impl crate::provider::SpritesProvider {
    /// Usage and observation metadata for sprite `name` since `start_rfc3339`.
    pub async fn usage_reading_since(
        &self,
        name: &str,
        start_rfc3339: &str,
    ) -> Result<Option<UsageReading>> {
        let http = self.sprites_http()?;
        anyhow::ensure!(!name.is_empty(), "sprites: empty sprite name");
        let mut url = reqwest::Url::parse(http.base)?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("sprites: invalid usage URL"))?
            .pop_if_empty()
            .push("usage")
            .push(name);
        url.query_pairs_mut().append_pair("start", start_rfc3339);
        let resp = http
            .client
            .get(url)
            .bearer_auth(http.token)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .context("sprites: usage request")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        anyhow::ensure!(
            resp.status().is_success(),
            "sprites usage failed ({})",
            resp.status()
        );
        let body = response_json(resp, "sprites", "usage").await?;
        parse_usage_reading(&body).map(Some)
    }

    /// Usage for sprite `name` since `start_rfc3339` (`None` = the sprite is
    /// unknown to the API, a 404).
    pub async fn usage_since(
        &self,
        name: &str,
        start_rfc3339: &str,
    ) -> Result<Option<SpriteUsage>> {
        self.usage_reading_since(name, start_rfc3339)
            .await
            .map(|reading| reading.map(|reading| reading.usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_buckets_and_partial() {
        let v = serde_json::json!({
            "start": "2026-01-01T00:00:00Z",
            "end": "2026-01-01T02:00:00Z",
            "interval": "hour",
            "usage": [
                {
                    "start": "2026-01-01T00:00:00Z",
                    "end": "2026-01-01T01:00:00Z",
                    "last_recorded_at": "2026-01-01T01:00:00Z",
                    "cpu_time_sec": 3600.0,
                    "reserved_memory_byte_sec": 1073741824.0 * 3600.0,
                    "sprite_ref": "sprite-a"
                },
                {
                    "start": "2026-01-01T01:00:00Z",
                    "end": "2026-01-01T02:00:00Z",
                    "last_recorded_at": "2026-01-01T02:00:00Z",
                    "cpu_time_sec": 1800.0,
                    "reserved_memory_byte_sec": 1073741824.0 * 7200.0,
                    "sprite_ref": "sprite-a"
                }
            ],
            "partial": true
        });
        let u = parse_usage(&v).unwrap();
        assert_eq!(u.cpu_seconds, 5400.0);
        assert!((u.reserved_gb_hours - 3.0).abs() < 1e-9);
        assert!(u.partial);
    }

    #[test]
    fn empty_usage_parses_as_zero() {
        let u = parse_usage(&serde_json::json!({"usage": []})).unwrap();

        assert_eq!(u.cpu_seconds, 0.0);
        assert_eq!(u.reserved_gb_hours, 0.0);
    }

    #[test]
    fn legacy_buckets_shape_still_parses() {
        let old = serde_json::json!({
            "buckets": [{"cpu_seconds": 10.0, "memory_byte_seconds": 1073741824.0 * 3600.0}]
        });
        let u = parse_usage(&old).unwrap();
        assert_eq!(u.cpu_seconds, 10.0);
        assert!((u.reserved_gb_hours - 1.0).abs() < 1e-9);
    }

    #[test]
    fn byte_seconds_convert_and_never_nan() {
        assert_eq!(
            gb_hours_from_byte_seconds((1u64 << 30) as f64 * 3600.0),
            1.0
        );
        assert_eq!(gb_hours_from_byte_seconds(f64::NAN), 0.0);
        assert_eq!(gb_hours_from_byte_seconds(-5.0), 0.0);
    }

    #[test]
    fn missing_usage_and_buckets_is_an_error() {
        let error = parse_usage(&serde_json::json!({})).unwrap_err().to_string();

        assert!(error.contains("usage"));
        assert!(error.contains("buckets"));
    }

    #[test]
    fn reading_without_observations_is_not_reported() {
        let reading = parse_usage_reading(&serde_json::json!({
            "usage": [
                {"last_recorded_at": null, "cpu_time_sec": 0.0,
                 "reserved_memory_byte_sec": 0.0}
            ]
        }))
        .unwrap();

        assert!(!reading.reported);
        assert_eq!(reading.last_recorded_at, None);
    }

    #[test]
    fn reading_picks_newest_last_recorded_at_and_sanitizes() {
        let reading = parse_usage_reading(&serde_json::json!({
            "usage": [
                {"last_recorded_at": "2026-01-01T01:00:00Z\u{0007}"},
                {"last_recorded_at": "2026-01-01T02:00:00Z"},
                {"last_recorded_at": "2025/12/31"}
            ]
        }))
        .unwrap();

        assert_eq!(
            reading.last_recorded_at.as_deref(),
            Some("2026-01-01T02:00:00Z")
        );
        assert!(reading.reported);
    }

    #[test]
    fn legacy_buckets_count_as_reported_when_nonzero() {
        let reading = parse_usage_reading(&serde_json::json!({
            "buckets": [{"cpu_seconds": 10.0, "memory_byte_seconds": 0.0}]
        }))
        .unwrap();

        assert!(reading.reported);
    }

    #[test]
    fn window_start_floors_to_the_hour() {
        assert_eq!(usage_window_start(3599), 0);
        assert_eq!(usage_window_start(3600), 3600);
        assert_eq!(usage_window_start(7322), 7200);
    }
}
