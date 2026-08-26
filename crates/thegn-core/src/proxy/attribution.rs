//! Virtual attribution keys (V 287).
//!
//! The host mints a per-worktree token at agent spawn and injects it with
//! `route_via_proxy`; the proxy decodes it to attribute spend. The token is
//! **self-describing** — the caller-scope chain is baked in at mint time — so the
//! proxy needs no virtual-keys table and does no DB lookup to resolve a caller.
//!
//! An attribution key is NOT an upstream secret: leaking one leaks attribution
//! plus local spend capacity against the loopback proxy, never a provider key.
//! It carries a random nonce so it is unguessable and revocable (regenerated per
//! session). Encoding is `mpk_<hex(json)>` — dependency-free and header-safe.

use serde::{Deserialize, Serialize};

/// Token prefix for the proxy's virtual attribution keys.
pub const KEY_PREFIX: &str = "mpk_";

/// The caller scope a virtual key carries, most specific first. The proxy rolls
/// spend up `scope → workspace → zone → global`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionScope {
    /// The most-specific scope, e.g. `worktree:/repo/wt`, `agent:reviewer`.
    pub scope: String,
    /// The enclosing `workspace:<repo>` scope, when derivable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The worktree's `zone:<name>` scope, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// Optional upstream-account pin: routing prefers this provider's lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Random nonce — makes the key unguessable and per-session revocable.
    #[serde(default)]
    pub nonce: String,
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
            workspace: Some("workspace:/repo".into()),
            zone: Some("zone:clientA".into()),
            upstream: Some("anthropic".into()),
            nonce: "abc123".into(),
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
}
