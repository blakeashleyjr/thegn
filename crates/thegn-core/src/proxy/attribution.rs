//! Virtual attribution keys (V 287).
//!
//! The host mints a per-worktree token at agent spawn and injects it with
//! `route_via_proxy`; the proxy verifies it to attribute spend. The signed token
//! carries the caller-scope chain, so the proxy needs no virtual-keys table or DB
//! lookup to resolve a caller. Legacy `mpk_<hex(json)>` helpers remain for data
//! compatibility, but are never accepted for proxy authorization.

use hmac::Mac;
use serde::{Deserialize, Serialize};

/// Token prefix for the proxy's virtual attribution keys.
pub const KEY_PREFIX: &str = "mpk_";
const SIGNED_PREFIX: &str = "mpk2_";
/// Protocol version expected by the host identity probe.
pub const IDENTITY_PROTOCOL_VERSION: u32 = 1;
const ATTR_DOMAIN: &[u8] = b"thegn-attribution-v2:";
const IDENTITY_DOMAIN: &[u8] = b"tgproxy-identity-v1:";

/// HMAC proof for the local daemon identity challenge.
pub fn identity_proof(secret: &[u8], challenge: &str) -> String {
    let mut mac =
        <HmacSha256 as hmac::Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(IDENTITY_DOMAIN);
    mac.update(challenge.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

/// Verifies an identity proof in constant time.
pub fn verify_identity_proof(secret: &[u8], challenge: &str, proof: &str) -> bool {
    let Some(bytes) = hex_decode(proof) else {
        return false;
    };
    let mut mac =
        <HmacSha256 as hmac::Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(IDENTITY_DOMAIN);
    mac.update(challenge.as_bytes());
    mac.verify_slice(&bytes).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrError {
    Malformed,
    BadSignature,
    Expired,
}

/// The caller scope a virtual key carries. The proxy rolls spend up `scope →
/// agent → workspace → zone → global`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionScope {
    /// The most-specific scope, e.g. `worktree:/repo/wt`, `agent:reviewer`.
    pub scope: String,
    /// Agent identity bound by the trusted host when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The enclosing `workspace:<repo>` scope, when derivable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The worktree's `zone:<name>` scope, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// Optional upstream-account pin: routing prefers this provider's lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Issuer-supplied discriminator carried inside the authenticated payload.
    #[serde(default)]
    pub nonce: String,
    /// Expiration time in Unix milliseconds for signed keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp_ms: Option<i64>,
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// Authenticates a scope with the host-owned runtime secret and an expiry.
pub fn encode_signed(scope: &AttributionScope, secret: &[u8], expires_ms: i64) -> String {
    let mut payload = scope.clone();
    payload.exp_ms = Some(expires_ms);
    let json = serde_json::to_vec(&payload).unwrap_or_default();
    let body = hex_encode(&json);
    let mut mac =
        <HmacSha256 as hmac::Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(ATTR_DOMAIN);
    mac.update(body.as_bytes());
    format!(
        "{SIGNED_PREFIX}{body}.{}",
        hex_encode(&mac.finalize().into_bytes())
    )
}

/// Verifies an authenticated, unexpired host-issued attribution key.
pub fn verify(token: &str, secret: &[u8], now_ms: i64) -> Result<AttributionScope, AttrError> {
    let token = token
        .strip_prefix(SIGNED_PREFIX)
        .ok_or(AttrError::Malformed)?;
    let (body, signature) = token.split_once('.').ok_or(AttrError::Malformed)?;
    let signature = hex_decode(signature).ok_or(AttrError::Malformed)?;
    let mut mac =
        <HmacSha256 as hmac::Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(ATTR_DOMAIN);
    mac.update(body.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| AttrError::BadSignature)?;
    let payload: AttributionScope =
        serde_json::from_slice(&hex_decode(body).ok_or(AttrError::Malformed)?)
            .map_err(|_| AttrError::Malformed)?;
    match payload.exp_ms {
        Some(exp) if now_ms < exp => Ok(payload),
        Some(_) => Err(AttrError::Expired),
        None => Err(AttrError::Malformed),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 15) as u32, 16).unwrap());
    }
    out
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.bytes();
    while let (Some(h), Some(l)) = (chars.next(), chars.next()) {
        out.push(((h as char).to_digit(16)? * 16 + (l as char).to_digit(16)?) as u8);
    }
    Some(out)
}

/// Parses a 32-byte host secret represented as exactly 64 hexadecimal digits.
pub fn parse_secret_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    hex_decode(value)
}

/// Encodes an attribution scope into a `mpk_<hex>` token.
pub fn encode(scope: &AttributionScope) -> String {
    let json = serde_json::to_vec(scope).unwrap_or_default();
    let mut out = String::with_capacity(KEY_PREFIX.len() + json.len() * 2);
    out.push_str(KEY_PREFIX);
    for b in json {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

/// Decodes a `mpk_<hex>` token back into its attribution scope. Returns `None`
/// for anything not minted by [`encode`] (wrong prefix, bad hex, bad JSON).
pub fn decode(token: &str) -> Option<AttributionScope> {
    let hex = token.strip_prefix(KEY_PREFIX)?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.bytes();
    while let (Some(h), Some(l)) = (chars.next(), chars.next()) {
        let hi = (h as char).to_digit(16)?;
        let lo = (l as char).to_digit(16)?;
        bytes.push(((hi << 4) | lo) as u8);
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_full_scope() {
        let s = AttributionScope {
            scope: "worktree:/repo/wt".into(),
            agent: Some("reviewer".into()),
            workspace: Some("workspace:/repo".into()),
            zone: Some("zone:clientA".into()),
            upstream: Some("anthropic".into()),
            nonce: "abc123".into(),
            exp_ms: None,
        };
        let token = encode(&s);
        assert!(token.starts_with(KEY_PREFIX));
        assert!(
            token
                .bytes()
                .skip(KEY_PREFIX.len())
                .all(|b| b.is_ascii_hexdigit())
        );
        assert_eq!(decode(&token), Some(s));
    }

    #[test]
    fn round_trips_minimal_scope() {
        let s = AttributionScope {
            scope: "global".into(),
            nonce: "n".into(),
            ..Default::default()
        };
        assert_eq!(decode(&encode(&s)), Some(s));
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert_eq!(decode("nope"), None);
        assert_eq!(decode("mpk_"), None);
        assert_eq!(decode("mpk_zz"), None); // bad hex
        assert_eq!(decode("mpk_7b"), None); // valid hex "{" but not valid json object
        assert_eq!(decode("mpk_abc"), None); // odd length
    }

    #[test]
    fn parses_host_secret_hex() {
        let parsed = parse_secret_hex("0123456789abcdef".repeat(4).as_str()).unwrap();
        assert_eq!(parsed.len(), 32);
        assert_eq!(parsed[0], 0x01);
        assert_eq!(parsed[31], 0xef);
        assert_eq!(parse_secret_hex(&"a".repeat(63)), None);
        assert_eq!(parse_secret_hex(&"g".repeat(64)), None);
    }

    #[test]
    fn signed_keys_authenticate_scope_and_expiry() {
        let secret = b"host secret";
        let scope = AttributionScope {
            scope: "worktree:/repo".into(),
            ..Default::default()
        };
        let token = encode_signed(&scope, secret, 500);
        assert_eq!(verify(&token, secret, 499).unwrap().scope, scope.scope);
        assert_eq!(verify(&token, secret, 500), Err(AttrError::Expired));
        assert_eq!(verify(&token, b"wrong", 1), Err(AttrError::BadSignature));
        let edited = token.replacen("mpk2_", "mpk2_f", 1);
        assert!(verify(&edited, secret, 1).is_err());
        assert_eq!(
            verify(&encode(&scope), secret, 1),
            Err(AttrError::Malformed)
        );
        assert!(!verify_identity_proof(secret, "challenge", "zz"));
        let proof = identity_proof(secret, "challenge");
        assert!(verify_identity_proof(secret, "challenge", &proof));
        assert!(!verify_identity_proof(secret, "other", &proof));

        let body = hex_encode(br#"{"scope":"scope"}"#);
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).unwrap();
        mac.update(ATTR_DOMAIN);
        mac.update(body.as_bytes());
        let no_expiry = format!(
            "{SIGNED_PREFIX}{body}.{}",
            hex_encode(&mac.finalize().into_bytes())
        );
        assert_eq!(verify(&no_expiry, secret, 1), Err(AttrError::Malformed));
        assert_eq!(
            verify("mpk2_not-hex.sig", secret, 1),
            Err(AttrError::Malformed)
        );
        assert_eq!(verify("mpk2_ab", secret, 1), Err(AttrError::Malformed));
        let invalid_body = "zz";
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).unwrap();
        mac.update(ATTR_DOMAIN);
        mac.update(invalid_body.as_bytes());
        let invalid_body_token = format!(
            "{SIGNED_PREFIX}{invalid_body}.{}",
            hex_encode(&mac.finalize().into_bytes())
        );
        assert_eq!(
            verify(&invalid_body_token, secret, 1),
            Err(AttrError::Malformed)
        );
        let invalid_json = hex_encode(b"not json");
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).unwrap();
        mac.update(ATTR_DOMAIN);
        mac.update(invalid_json.as_bytes());
        let invalid_json_token = format!(
            "{SIGNED_PREFIX}{invalid_json}.{}",
            hex_encode(&mac.finalize().into_bytes())
        );
        assert_eq!(
            verify(&invalid_json_token, secret, 1),
            Err(AttrError::Malformed)
        );
    }
}
