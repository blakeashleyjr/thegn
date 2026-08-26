//! The canonical secret-redaction seam.
//!
//! One list of sensitive config-key names, one `is_sensitive` predicate, one
//! JSON-tree masker — shared by every surface that must not leak a secret:
//! the MCP docs router's `get_config` ([`crate::mcp::docs`]), the crash /
//! diagnostics reporter, `thegn doctor`, and the typed [`crate::secretref`]
//! vocabulary (whose redacted `Debug` prints [`PLACEHOLDER`]).
//!
//! Before this module the predicate lived in `mcp/docs.rs` and was being
//! re-derived, subtly differently, at each new leak surface (a crash reporter
//! with its own list would mask `token` but miss `_key`, or vice versa). The
//! credential-broker change (THE-66) promotes it here so there is exactly one
//! answer to "is this key a secret?" and one placeholder string. New leak
//! surfaces MUST import from here rather than grow a local copy.

use serde_json::{Value, json};

/// The string a redacted scalar becomes. Stable so tests and diff-review can
/// match on it, and so a value that legitimately equals it is indistinguishable
/// from a masked one (acceptable — it carries no secret).
pub const PLACEHOLDER: &str = "***redacted***";

/// Config-key substrings whose scalar values are secrets and must never be
/// served, logged, or reported. Matched case-insensitively as a substring of
/// the key name; [`is_sensitive`] additionally treats any `*_key` suffix as
/// sensitive (so `monitor_key`, `signing_key`, `private_key` all match without
/// enumerating each).
///
/// This is the single source of truth: sibling leak surfaces (crash reporter,
/// doctor, MCP docs) reconcile onto THIS list rather than maintaining their own.
pub const SENSITIVE: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "credential",
    "private_key",
];

/// Whether a config key names a secret scalar. Case-insensitive substring match
/// against [`SENSITIVE`], plus the `*_key` suffix rule.
pub fn is_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|s| k.contains(s)) || k.ends_with("_key")
}

/// Mask secret scalar values in a resolved-config JSON tree in place, so a
/// surface can serve/emit config without leaking tokens or credentials. A
/// scalar (string/number) directly under a [sensitive](is_sensitive) key
/// becomes [`PLACEHOLDER`]; objects/arrays are always recursed (so nested
/// secrets are caught, and non-secret subtrees survive).
pub fn redact_json(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_sensitive(k) && matches!(val, Value::String(_) | Value::Number(_)) {
                    *val = json!(PLACEHOLDER);
                } else {
                    redact_json(val);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(redact_json),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_sensitive_covers_the_list_and_key_suffix() {
        for &s in SENSITIVE {
            assert!(is_sensitive(s), "{s} should be sensitive");
            assert!(is_sensitive(&format!("github_{s}")), "substring {s}");
            assert!(is_sensitive(&s.to_ascii_uppercase()), "case-insensitive");
        }
        // The `*_key` suffix rule catches keys not literally in the list.
        assert!(is_sensitive("monitor_key"));
        assert!(is_sensitive("signing_key"));
        // Non-secret keys are left alone.
        assert!(!is_sensitive("backend"));
        assert!(!is_sensitive("name"));
        assert!(!is_sensitive("keymap")); // contains "key" but not "_key" suffix
    }

    #[test]
    fn redact_json_masks_secrets_and_keeps_the_rest() {
        let mut v = json!({
            "github_token": "ghp_realsecret",
            "sandbox": { "backend": "podman" },
            "accounts": [ { "name": "work", "api_key": "sk-123" } ],
            "monitor_key": "F5",
            "keybinds": { "quit": "ctrl-q" },
        });
        redact_json(&mut v);
        assert_eq!(v["github_token"], PLACEHOLDER);
        assert_eq!(v["accounts"][0]["api_key"], PLACEHOLDER);
        assert_eq!(v["monitor_key"], PLACEHOLDER); // ends_with _key
        // Non-secrets survive, including the name alongside a redacted key.
        assert_eq!(v["sandbox"]["backend"], "podman");
        assert_eq!(v["accounts"][0]["name"], "work");
        assert_eq!(v["keybinds"]["quit"], "ctrl-q");
    }

    #[test]
    fn redact_json_masks_numbers_too() {
        let mut v = json!({ "secret": 12345, "port": 8080 });
        redact_json(&mut v);
        assert_eq!(v["secret"], PLACEHOLDER);
        assert_eq!(v["port"], 8080);
    }
}
