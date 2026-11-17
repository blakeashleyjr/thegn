//! Local classification of the Vercel API token (access token vs OIDC). No
//! verification and no refresh: thegn has nothing to refresh an OIDC token
//! with, so a near-expiry OIDC token is refused with a pointer to an access
//! token. Nothing here returns or logs token text.

/// What kind of token the configured Vercel credential is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Oidc { exp_s: Option<i64> },
}

/// Classify a token by shape (JWT payload `exp`, unverified).
pub fn classify(token: &str) -> TokenKind {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return TokenKind::Access;
    }
    let header_ok = b64url_decode(parts[0])
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v.get("alg").is_some());
    if !header_ok {
        return TokenKind::Access;
    }
    let exp_s = b64url_decode(parts[1])
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("exp").and_then(|e| e.as_i64()));
    TokenKind::Oidc { exp_s }
}

/// base64url (no padding required; trailing `=` tolerated). `None` on any
/// invalid character or impossible length.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// An OIDC token with less than this left is refused.
pub const OIDC_MIN_REMAINING_S: i64 = 300;

/// `Some(msg)` when the token is OIDC and its expiry is unknown or within
/// [`OIDC_MIN_REMAINING_S`] of `now_s`.
pub fn oidc_refusal(kind: TokenKind, now_s: i64) -> Option<String> {
    match kind {
        TokenKind::Access => None,
        TokenKind::Oidc { exp_s } => match exp_s {
            Some(exp) if exp - now_s >= OIDC_MIN_REMAINING_S => None,
            _ => Some(
                "the Vercel OIDC token is expired or expiring; thegn does not refresh OIDC \
                 tokens; use a Vercel access token"
                    .to_string(),
            ),
        },
    }
}

/// A short description for doctor (`access token`, `OIDC, expires in 3h`).
pub fn describe(kind: TokenKind, now_s: i64) -> String {
    match kind {
        TokenKind::Access => "access token".to_string(),
        TokenKind::Oidc { exp_s: None } => "OIDC, expiry unknown".to_string(),
        TokenKind::Oidc { exp_s: Some(exp) } => {
            let left = exp.saturating_sub(now_s);
            if left <= 0 {
                "OIDC, expired".to_string()
            } else if left >= 3600 {
                format!("OIDC, expires in {}h", left / 3600)
            } else {
                format!("OIDC, expires in {}m", left / 60)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(b: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for ch in b.chunks(3) {
            let n = ch.iter().fold(0u32, |a, x| (a << 8) | u32::from(*x)) << (8 * (3 - ch.len()));
            for i in 0..=ch.len() {
                s.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        s
    }

    fn jwt(payload: &str) -> String {
        format!(
            "{}.{}.sig",
            enc(br#"{"alg":"RS256"}"#),
            enc(payload.as_bytes())
        )
    }

    #[test]
    fn jwt_with_exp() {
        assert_eq!(
            classify(&jwt(r#"{"exp":1234}"#)),
            TokenKind::Oidc { exp_s: Some(1234) }
        );
    }

    #[test]
    fn jwt_without_exp_or_bad_exp() {
        assert_eq!(
            classify(&jwt(r#"{"sub":"x"}"#)),
            TokenKind::Oidc { exp_s: None }
        );
        assert_eq!(
            classify(&jwt(r#"{"exp":"soon"}"#)),
            TokenKind::Oidc { exp_s: None }
        );
        assert_eq!(
            classify(&jwt(r#"{"exp":99999999999999999999}"#)),
            TokenKind::Oidc { exp_s: None }
        );
        assert_eq!(classify(&jwt("not json")), TokenKind::Oidc { exp_s: None });
    }

    #[test]
    fn non_jwt_is_access() {
        assert_eq!(classify("abcDEF123"), TokenKind::Access);
        assert_eq!(classify("a.b.c.d"), TokenKind::Access);
        assert_eq!(classify("a..c"), TokenKind::Access);
        assert_eq!(classify("a$b.c.d"), TokenKind::Access);
        assert_eq!(classify("a.b"), TokenKind::Access);
        // header without alg
        let t = format!("{}.{}.s", enc(br#"{"typ":"JWT"}"#), enc(br#"{"exp":1}"#));
        assert_eq!(classify(&t), TokenKind::Access);
        // malformed base64 header (length % 4 == 1)
        assert_eq!(classify("abcde.abcd.abcd"), TokenKind::Access);
        // header not JSON
        let t = format!("{}.{}.s", enc(b"hello"), enc(br#"{"exp":1}"#));
        assert_eq!(classify(&t), TokenKind::Access);
    }

    #[test]
    fn refusal_rules() {
        assert!(oidc_refusal(TokenKind::Access, 0).is_none());
        let k = |e| TokenKind::Oidc { exp_s: e };
        assert!(oidc_refusal(k(Some(10_000)), 0).is_none());
        assert!(oidc_refusal(k(Some(OIDC_MIN_REMAINING_S)), 0).is_none());
        assert!(oidc_refusal(k(Some(OIDC_MIN_REMAINING_S - 1)), 0).is_some());
        assert!(oidc_refusal(k(Some(-5)), 0).is_some());
        assert!(oidc_refusal(k(None), 0).is_some());
    }

    #[test]
    fn describe_text() {
        assert_eq!(describe(TokenKind::Access, 0), "access token");
        let k = |e| TokenKind::Oidc { exp_s: e };
        assert_eq!(describe(k(Some(3 * 3600 + 5)), 0), "OIDC, expires in 3h");
        assert_eq!(describe(k(Some(600)), 0), "OIDC, expires in 10m");
        assert_eq!(describe(k(Some(5)), 10), "OIDC, expired");
        assert_eq!(describe(k(None), 0), "OIDC, expiry unknown");
        assert_eq!(describe(k(Some(i64::MIN)), 1), "OIDC, expired");
    }
}
