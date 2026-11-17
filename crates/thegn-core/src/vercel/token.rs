//! Local classification of the Vercel API token (access token vs OIDC). No
//! verification and no refresh: thegn has nothing to refresh an OIDC token
//! with, so a near-expiry OIDC token is refused with a pointer to an access
//! token. Nothing here returns or logs token text.
//!
//! Spine stub (b156 lane 1): signatures are fixed; lane 2 fills the bodies.

/// What kind of token the configured Vercel credential is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Oidc { exp_s: Option<i64> },
}

/// Classify a token by shape (JWT payload `exp`, unverified).
pub fn classify(_token: &str) -> TokenKind {
    TokenKind::Access
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
pub fn describe(kind: TokenKind, _now_s: i64) -> String {
    match kind {
        TokenKind::Access => "access token".to_string(),
        TokenKind::Oidc { .. } => "OIDC".to_string(),
    }
}
