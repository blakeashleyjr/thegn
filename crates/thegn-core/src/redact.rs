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
//! surfaces MUST import from here rather than grow a local copy. Structural
//! calendar-account secrets are recognized by path: URLs, usernames, env
//! values, and command arguments after argv[0].

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
    // OpenVPN `user\npass` credentials (`sandbox.vpn.openvpn`); the only
    // `*pass*` credential field that neither "password" nor `_key` catches.
    "auth_user_pass",
];

/// Exact key names that the substring / `*_key` rules match but which hold
/// NON-secret values. A reviewed, explicit list: add an entry only after
/// checking the config field really is public data.
///
/// - `project_key`: a tracker project identifier (Jira `PROJ`), not a credential.
/// - `binary_cache_key`: a Nix binary-cache PUBLIC signing key
///   (`cache.example.org-1:...`), published by design.
///
/// Keys ending `_env` are exempt separately (see [`is_sensitive`]): they hold
/// the NAME of an environment variable (`api_key_env = "LINEAR_API_KEY"`), which
/// is the whole point of not putting the secret in the file.
const NOT_SECRET: &[&str] = &["project_key", "binary_cache_key"];

/// Whether a config key names a secret scalar. Case-insensitive substring match
/// against [`SENSITIVE`], plus the `*_key` suffix rule, minus `*_env` keys and
/// the explicit [`NOT_SECRET`] list.
pub fn is_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    if k.ends_with("_env") || NOT_SECRET.contains(&k.as_str()) {
        return false;
    }
    SENSITIVE.iter().any(|s| k.contains(s)) || k.ends_with("_key")
}

/// Whether a path names a structural calendar-account secret: fields whose
/// VALUE is credential-bearing whatever its key is called; mirrors
/// `CalendarAccount`'s Debug impl (`crates/thegn-core/src/config_calendar.rs`)
/// and must stay in step with it.
pub fn is_sensitive_path<S: AsRef<str>>(segments: &[S]) -> bool {
    let segments: Vec<&str> = segments.iter().map(AsRef::as_ref).collect();
    if segments.len() < 4
        || segments[0] != "calendar"
        || segments[1] != "accounts"
        || segments[2].is_empty()
        || !segments[2].bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    match segments[3..] {
        ["url"] | ["username"] => true,
        ["env", _] => true,
        ["command", index] => {
            !index.is_empty()
                && index.bytes().all(|byte| byte.is_ascii_digit())
                && index.bytes().any(|byte| byte != b'0')
        }
        _ => false,
    }
}

/// Mask secret scalar values in a resolved-config JSON tree in place, so a
/// surface can serve/emit config without leaking tokens or credentials. A
/// STRING directly under a [sensitive](is_sensitive) key becomes
/// [`PLACEHOLDER`] (numbers and bools keep their type: `auto_max_tokens` is a
/// count, and a machine-readable consumer must still get a number); an array of
/// strings under one has each element masked; objects are always recursed (so
/// nested secrets are caught, and non-secret subtrees survive).
pub fn redact_json(v: &mut Value) {
    redact_json_at::<&str>(&[], v);
}

/// Redact a JSON subtree while carrying its path from the config root.
pub fn redact_json_at<S: AsRef<str>>(prefix: &[S], v: &mut Value) {
    let mut path: Vec<String> = prefix
        .iter()
        .map(|segment| segment.as_ref().to_owned())
        .collect();
    redact_json_at_inner(&mut path, v, false);
}

fn redact_json_at_inner(path: &mut Vec<String>, v: &mut Value, masked: bool) {
    match v {
        Value::String(_) if masked || is_sensitive_path(path) => *v = json!(PLACEHOLDER),
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                path.push(key.clone());
                // A sensitive key whose value is an object follows the
                // existing rule: recurse by child keys rather than masking
                // every string in the object.
                redact_json_at_inner(path, value, is_sensitive(key));
                path.pop();
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter_mut().enumerate() {
                path.push(index.to_string());
                redact_json_at_inner(path, value, masked);
                path.pop();
            }
        }
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
    fn redact_json_keeps_numbers_and_bools_typed() {
        let mut v = json!({ "auto_max_tokens": 4096, "token_rollups": true, "port": 8080 });
        redact_json(&mut v);
        assert_eq!(v["auto_max_tokens"], 4096);
        assert_eq!(v["token_rollups"], true);
        assert_eq!(v["port"], 8080);
    }

    #[test]
    fn redact_json_masks_arrays_of_scalars_under_a_sensitive_key() {
        let mut v = json!({
            "api_keys": ["CANARY-1", "CANARY-2"],
            "nested": { "tokens": [ "CANARY-3", { "password": "CANARY-4" } ] },
            "hosts": ["a", "b"],
        });
        redact_json(&mut v);
        assert_eq!(v["api_keys"], json!([PLACEHOLDER, PLACEHOLDER]));
        assert_eq!(v["nested"]["tokens"][0], PLACEHOLDER);
        assert_eq!(v["nested"]["tokens"][1]["password"], PLACEHOLDER);
        assert_eq!(v["hosts"], json!(["a", "b"]));
        assert!(!v.to_string().contains("CANARY"));
    }

    #[test]
    fn non_secret_lookalikes_are_exempt() {
        for k in [
            "project_key",
            "binary_cache_key",
            "api_key_env",
            "token_ENV",
        ] {
            assert!(!is_sensitive(k), "{k}");
        }
        let mut v =
            json!({ "project_key": "PROJ", "api_key_env": "LINEAR_API_KEY", "api_key": "x" });
        redact_json(&mut v);
        assert_eq!(v["project_key"], "PROJ");
        assert_eq!(v["api_key_env"], "LINEAR_API_KEY");
        assert_eq!(v["api_key"], PLACEHOLDER);
    }

    fn calendar_sentinels() -> Value {
        json!({"calendar":{"accounts":[{
            "name":"work",
            "url":"https://cal.example.test/feed.ics?sig=cal-url-sentinel-7a1",
            "username":"cal-user-sentinel-3c2",
            "command":["khal-thegn","--auth","cal-argv-sentinel-9d4"],
            "env":{"KHAL_AUTH":"cal-env-sentinel-5e8"},
            "cwd":"/srv/khal"
        }]}})
    }

    fn assert_calendar_credentials_redacted(value: &Value) {
        let output = value.to_string();
        for sentinel in [
            "cal-url-sentinel-7a1",
            "cal-user-sentinel-3c2",
            "cal-argv-sentinel-9d4",
            "cal-env-sentinel-5e8",
        ] {
            assert!(!output.contains(sentinel), "leaked {sentinel}: {output}");
        }
        assert_eq!(value["calendar"]["accounts"][0]["url"], PLACEHOLDER);
        assert_eq!(value["calendar"]["accounts"][0]["username"], PLACEHOLDER);
        assert_eq!(value["calendar"]["accounts"][0]["command"][0], "khal-thegn");
        assert_eq!(value["calendar"]["accounts"][0]["command"][1], PLACEHOLDER);
        assert_eq!(
            value["calendar"]["accounts"][0]["env"]["KHAL_AUTH"],
            PLACEHOLDER
        );
        assert_eq!(value["calendar"]["accounts"][0]["name"], "work");
        assert_eq!(value["calendar"]["accounts"][0]["cwd"], "/srv/khal");
    }

    #[test]
    fn calendar_account_credentials_are_masked_at_the_root() {
        let mut value = calendar_sentinels();
        redact_json(&mut value);
        assert_calendar_credentials_redacted(&value);
    }

    #[test]
    fn calendar_account_credentials_are_masked_under_a_prefix() {
        let mut account = calendar_sentinels()["calendar"]["accounts"][0].clone();
        let mut under_accounts = json!({"0": account});
        redact_json_at(&["calendar", "accounts"], &mut under_accounts);
        assert!(!under_accounts.to_string().contains("cal-url-sentinel-7a1"));
        assert!(!under_accounts.to_string().contains("cal-user-sentinel-3c2"));
        assert!(!under_accounts.to_string().contains("cal-argv-sentinel-9d4"));
        assert!(!under_accounts.to_string().contains("cal-env-sentinel-5e8"));

        account = calendar_sentinels()["calendar"]["accounts"][0].clone();
        let mut under_calendar = json!({"accounts": [account]});
        redact_json_at(&["calendar"], &mut under_calendar);
        for sentinel in [
            "cal-url-sentinel-7a1",
            "cal-user-sentinel-3c2",
            "cal-argv-sentinel-9d4",
            "cal-env-sentinel-5e8",
        ] {
            assert!(!under_calendar.to_string().contains(sentinel));
        }
    }

    #[test]
    fn calendar_account_sensitive_paths_are_exact() {
        for path in [
            ["calendar", "accounts", "0", "url"].as_slice(),
            ["calendar", "accounts", "12", "username"].as_slice(),
            ["calendar", "accounts", "0", "env", "KHAL_AUTH"].as_slice(),
            ["calendar", "accounts", "0", "command", "1"].as_slice(),
            ["calendar", "accounts", "0", "command", "2"].as_slice(),
        ] {
            assert!(is_sensitive_path(path), "{path:?}");
        }
        for path in [
            ["calendar", "accounts", "0", "command", "0"].as_slice(),
            ["calendar", "accounts", "0", "command"].as_slice(),
            ["calendar", "accounts", "0", "env"].as_slice(),
            ["calendar", "accounts", "0", "name"].as_slice(),
            ["calendar", "accounts", "0", "cwd"].as_slice(),
            ["calendar", "accounts", "0", "path"].as_slice(),
            ["calendar", "accounts", "x", "url"].as_slice(),
            ["issues", "issue_accounts", "0", "url"].as_slice(),
            ["url"].as_slice(),
        ] {
            assert!(!is_sensitive_path(path), "{path:?}");
        }
    }
}
