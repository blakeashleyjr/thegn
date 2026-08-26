//! Extracts an upstream-reported quota-reset deadline from a 429 body. Several
//! providers report the reset in the body (not a header), so a header-only parse
//! misses it and the backend gets re-probed every cooldown against a window that
//! won't reopen for minutes-to-hours. Port of `parseResetFromBody` (ratelimit.go).
//!
//! Patterns match raw bytes and tolerate escaped quotes, because sidecars
//! stringify the upstream error into `.error.message` — nesting the field inside
//! a JSON string a typed parse can't reach.

use std::sync::LazyLock;

use regex::bytes::Regex;

static QUOTA_RESET_TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"quotaResetTimeStamp\\?"\s*:\s*\\?"([^"\\]+)"#).unwrap());
static RESETS_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"resets_at\\?"\s*:\s*([0-9]+)"#).unwrap());
static RESETS_IN_SECONDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"resets_in_seconds\\?"\s*:\s*([0-9]+(?:\.[0-9]+)?)"#).unwrap());
static QUOTA_RESET_DELAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"quotaResetDelay\\?"\s*:\s*\\?"([^"\\]+)"#).unwrap());
static RETRY_DELAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"retryDelay\\?"\s*:\s*\\?"([^"\\]+)"#).unwrap());

/// Returns an absolute reset deadline in epoch millis, given `now_ms`. Absolute
/// deadlines (RFC3339 timestamp, Unix epoch) are preferred over relative
/// durations.
pub fn parse_reset_from_body(body: &[u8], now_ms: i64) -> Option<i64> {
    if body.is_empty() {
        return None;
    }
    if let Some(c) = QUOTA_RESET_TIMESTAMP.captures(body)
        && let Ok(s) = std::str::from_utf8(&c[1])
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s)
    {
        return Some(dt.timestamp_millis());
    }
    if let Some(c) = RESETS_AT.captures(body)
        && let Ok(ts) = std::str::from_utf8(&c[1]).unwrap_or("").parse::<i64>()
        && ts > 0
    {
        // 13-digit values are already epoch millis; else seconds.
        return Some(if ts > 1_000_000_000_000 {
            ts
        } else {
            ts * 1000
        });
    }
    if let Some(c) = RESETS_IN_SECONDS.captures(body)
        && let Ok(secs) = std::str::from_utf8(&c[1]).unwrap_or("").parse::<f64>()
        && secs > 0.0
    {
        return Some(now_ms + (secs * 1000.0) as i64);
    }
    for re in [&*QUOTA_RESET_DELAY, &*RETRY_DELAY] {
        if let Some(c) = re.captures(body)
            && let Ok(s) = std::str::from_utf8(&c[1])
            && let Some(ms) = parse_go_duration_ms(s)
            && ms > 0
        {
            return Some(now_ms + ms);
        }
    }
    None
}

/// Parses a Go-style duration string (`"4h7m38.8s"`, `"2510.146s"`) into millis.
fn parse_go_duration_ms(s: &str) -> Option<i64> {
    let mut total_ms = 0f64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            chars.next();
            continue;
        }
        // Unit char(s): h, m, s, ms, us/µs, ns.
        let mut unit = String::new();
        while let Some(&u) = chars.peek() {
            if u.is_ascii_alphabetic() || u == 'µ' {
                unit.push(u);
                chars.next();
            } else {
                break;
            }
        }
        let v: f64 = num.parse().ok()?;
        num.clear();
        total_ms += match unit.as_str() {
            "h" => v * 3_600_000.0,
            "m" => v * 60_000.0,
            "s" => v * 1_000.0,
            "ms" => v,
            "us" | "µs" => v / 1_000.0,
            "ns" => v / 1_000_000.0,
            _ => return None,
        };
    }
    if !num.is_empty() {
        // trailing number with no unit is invalid in Go durations
        return None;
    }
    Some(total_ms as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_timestamp() {
        let body = br#"{"error":{"quotaResetTimeStamp":"2030-01-01T00:00:00Z"}}"#;
        let got = parse_reset_from_body(body, 0).unwrap();
        assert_eq!(
            got,
            chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
                .unwrap()
                .timestamp_millis()
        );
    }

    #[test]
    fn epoch_seconds_and_millis() {
        assert_eq!(
            parse_reset_from_body(br#"{"resets_at":1700000000}"#, 0),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_reset_from_body(br#"{"resets_at":1700000000000}"#, 0),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn relative_seconds() {
        assert_eq!(
            parse_reset_from_body(br#"{"resets_in_seconds":30}"#, 1000),
            Some(31_000)
        );
    }

    #[test]
    fn go_duration_delay() {
        // 1h = 3_600_000ms, offset from now.
        assert_eq!(
            parse_reset_from_body(br#"{"retryDelay":"1h"}"#, 0),
            Some(3_600_000)
        );
        assert_eq!(
            parse_go_duration_ms("4h7m38.8s"),
            Some(4 * 3_600_000 + 7 * 60_000 + 38_800)
        );
    }

    #[test]
    fn escaped_quotes_in_stringified_error() {
        let body = br#"{"error":{"message":"{\"resets_in_seconds\":60}"}}"#;
        assert_eq!(parse_reset_from_body(body, 0), Some(60_000));
    }

    #[test]
    fn none_when_absent() {
        assert!(parse_reset_from_body(b"{}", 0).is_none());
        assert!(parse_reset_from_body(b"", 0).is_none());
    }
}
