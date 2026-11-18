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

/// Parse a usage response: `{"buckets":[{cpu_seconds, memory_byte_seconds}…],
/// "partial": bool}`. Buckets are summed.
pub fn parse_usage(v: &serde_json::Value) -> Result<SpriteUsage> {
    let buckets = v
        .get("buckets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("sprites usage response has no buckets array"))?;
    let mut cpu = 0.0;
    let mut byte_sec = 0.0;
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
    }
    Ok(SpriteUsage {
        cpu_seconds: if cpu.is_finite() { cpu } else { 0.0 },
        reserved_gb_hours: gb_hours_from_byte_seconds(byte_sec),
        partial: v
            .get("partial")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

impl crate::provider::SpritesProvider {
    /// Usage for sprite `name` since `start_rfc3339` (`None` = the sprite is
    /// unknown to the API, a 404).
    pub async fn usage_since(
        &self,
        name: &str,
        start_rfc3339: &str,
    ) -> Result<Option<SpriteUsage>> {
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
        parse_usage(&body).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_buckets_and_partial() {
        let v = serde_json::json!({
            "buckets": [
                {"cpu_time_sec": 3600.0, "reserved_memory_byte_sec": 1073741824.0 * 3600.0},
                {"cpu_time_sec": 1800.0, "reserved_memory_byte_sec": 1073741824.0 * 7200.0}
            ],
            "partial": true
        });
        let u = parse_usage(&v).unwrap();
        assert_eq!(u.cpu_seconds, 5400.0);
        assert!((u.reserved_gb_hours - 3.0).abs() < 1e-9);
        assert!(u.partial);
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
    fn missing_buckets_is_an_error() {
        assert!(parse_usage(&serde_json::json!({})).is_err());
    }
}
