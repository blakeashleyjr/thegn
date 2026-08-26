//! Response classification: what should routing do with a backend's reply.
//!
//! Port of `failKind` / `classifyResponse` / `errorBodyMessage` /
//! `isAuthExhaustionReason` / `errorBodySnippet` from `main.go`.
//!
//! Cooldown ([`FailKind::Exhausted`]) is reserved for genuine availability
//! problems — rate limits (429/402), auth/credit/billing errors, and upstream
//! outages (5xx) — so the backend is skipped until it recovers. Everything else
//! that isn't a usable 2xx is [`FailKind::Soft`]: a request-specific problem
//! where the backend is healthy and only *this* request failed, so it falls
//! through now but stays eligible for the next request.

/// What routing should do with a backend's HTTP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// Usable 2xx — return it to the client.
    Serve,
    /// Request-specific failure — fall through, NO cooldown (retry next request).
    Soft,
    /// Availability failure — fall through AND cool the backend down.
    Exhausted,
}

/// Extracts an OpenAI-style error (`message` + `code` + `type`) from a response
/// body, lowercased and joined for keyword matching. Returns `None` when the
/// body carries no error object. Mirrors `errorBodyMessage`.
pub fn error_body_message(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let err = v.get("error")?;
    if err.is_null() {
        return None;
    }
    let field = |k: &str| err.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let joined = format!("{} {} {}", field("message"), field("code"), field("type"));
    Some(joined.trim().to_lowercase())
}

/// Renders a short single-line excerpt of an upstream error body (whitespace
/// collapsed, truncated to 200 chars) for fall-through logs. Mirrors
/// `errorBodySnippet`.
pub fn error_body_snippet(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_log_message(&collapsed, 200)
}

/// Truncates a log message to `max` chars, appending an ellipsis when cut.
fn truncate_log_message(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

/// Decides how routing should treat a backend's HTTP response. Mirrors
/// `classifyResponse`. Empty completions are classified [`FailKind::Soft`] by the
/// caller (detection needs the streaming flag and lives in the I/O layer).
pub fn classify_response(status_code: u16, body: &[u8]) -> (FailKind, String) {
    let msg = error_body_message(body);

    match status_code {
        429 | 402 => {
            return match &msg {
                Some(m) => (
                    FailKind::Exhausted,
                    format!("HTTP {status_code} (rate limit): {m}"),
                ),
                None => (
                    FailKind::Exhausted,
                    format!("HTTP {status_code} (rate limit)"),
                ),
            };
        }
        401 | 403 => {
            return match &msg {
                Some(m) => (
                    FailKind::Exhausted,
                    format!("HTTP {status_code} (auth): {m}"),
                ),
                None => (FailKind::Exhausted, format!("HTTP {status_code} (auth)")),
            };
        }
        _ => {}
    }

    if let Some(m) = &msg {
        const AVAILABILITY_KEYWORDS: &[&str] = &[
            "insufficient_quota",
            "out of credits",
            "credit",
            "billing",
            "payment required",
            "rate limit",
            "rate_limit",
            "quota",
            "exhausted",
            "invalid api key",
            "unauthorized",
            "forbidden",
            "model is not supported",
            "not supported when using",
        ];
        if AVAILABILITY_KEYWORDS.iter().any(|kw| m.contains(kw)) {
            return (FailKind::Exhausted, m.clone());
        }
        // An error body on a 2xx that isn't account/availability related is
        // request-specific — fall through without a cooldown.
        if (200..300).contains(&status_code) {
            return (FailKind::Soft, m.clone());
        }
    }

    if status_code >= 500 {
        let snippet = error_body_snippet(body);
        return if snippet.is_empty() {
            (
                FailKind::Exhausted,
                format!("HTTP {status_code} (upstream outage)"),
            )
        } else {
            (
                FailKind::Exhausted,
                format!("HTTP {status_code} (upstream outage): {snippet}"),
            )
        };
    }

    // 400-499 (other than auth/rate-limit handled above): a request-specific
    // client error on an otherwise-healthy backend. Fall through without cooldown.
    if status_code >= 400 {
        let snippet = error_body_snippet(body);
        return if snippet.is_empty() {
            (FailKind::Soft, format!("HTTP {status_code}"))
        } else {
            (FailKind::Soft, format!("HTTP {status_code}: {snippet}"))
        };
    }

    (FailKind::Serve, String::new())
}

/// Whether an exhaustion reason is auth-class (401/403 or an auth/credential
/// message) rather than a transient rate limit/outage or payment problem.
/// Mirrors `isAuthExhaustionReason`.
pub fn is_auth_exhaustion_reason(reason: &str) -> bool {
    let r = reason.to_lowercase();
    r.contains("(auth)")
        || r.contains("401")
        || r.contains("403")
        || r.contains("unauthorized")
        || r.contains("forbidden")
        || r.contains("invalid api key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_clean_2xx() {
        let (k, reason) = classify_response(200, br#"{"choices":[]}"#);
        assert_eq!(k, FailKind::Serve);
        assert!(reason.is_empty());
    }

    #[test]
    fn rate_limit_codes_exhaust() {
        let (k, r) = classify_response(429, br#"{"error":{"message":"slow down"}}"#);
        assert_eq!(k, FailKind::Exhausted);
        assert!(r.contains("rate limit"));
        assert!(r.contains("slow down"));
        let (k, _) = classify_response(402, b"");
        assert_eq!(k, FailKind::Exhausted);
    }

    #[test]
    fn auth_codes_exhaust() {
        let (k, r) = classify_response(401, b"");
        assert_eq!(k, FailKind::Exhausted);
        assert!(r.contains("auth"));
        assert_eq!(classify_response(403, b"").0, FailKind::Exhausted);
    }

    #[test]
    fn availability_keyword_in_body_exhausts() {
        let (k, _) = classify_response(200, br#"{"error":{"message":"insufficient_quota"}}"#);
        assert_eq!(k, FailKind::Exhausted);
    }

    #[test]
    fn unrelated_2xx_error_is_soft() {
        let (k, m) = classify_response(200, br#"{"error":{"message":"weird thing"}}"#);
        assert_eq!(k, FailKind::Soft);
        assert_eq!(m, "weird thing");
    }

    #[test]
    fn server_error_exhausts_with_snippet() {
        let (k, r) = classify_response(503, b"upstream  is   down");
        assert_eq!(k, FailKind::Exhausted);
        assert!(r.contains("upstream is down")); // whitespace collapsed
    }

    #[test]
    fn other_4xx_is_soft() {
        let (k, r) = classify_response(400, br#"bad request body"#);
        assert_eq!(k, FailKind::Soft);
        assert!(r.contains("400"));
    }

    #[test]
    fn auth_reason_detection() {
        assert!(is_auth_exhaustion_reason("HTTP 401 (auth)"));
        assert!(is_auth_exhaustion_reason("invalid api key"));
        assert!(is_auth_exhaustion_reason("Forbidden"));
        assert!(!is_auth_exhaustion_reason("HTTP 429 (rate limit)"));
    }

    #[test]
    fn error_body_message_none_for_null_or_missing() {
        assert!(error_body_message(br#"{"error": null}"#).is_none());
        assert!(error_body_message(br#"{"foo": 1}"#).is_none());
        assert!(error_body_message(b"not json").is_none());
    }

    #[test]
    fn server_error_without_body_has_no_snippet() {
        let (k, r) = classify_response(500, b"");
        assert_eq!(k, FailKind::Exhausted);
        assert_eq!(r, "HTTP 500 (upstream outage)");
    }

    #[test]
    fn other_4xx_without_body() {
        let (k, r) = classify_response(404, b"");
        assert_eq!(k, FailKind::Soft);
        assert_eq!(r, "HTTP 404");
    }

    #[test]
    fn snippet_truncates() {
        let long = "x".repeat(500);
        let s = error_body_snippet(long.as_bytes());
        assert!(s.chars().count() <= 201); // 200 + ellipsis
        assert!(s.ends_with('…'));
    }
}
