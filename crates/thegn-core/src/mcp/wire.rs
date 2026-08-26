//! Pure agent-settings merge for `thegn mcp wire`.
//!
//! Writing the single secret-free proxy entry into an agent CLI's MCP settings
//! is a JSON merge with three non-negotiables, all enforced here (host adapters
//! only supply the file path + the container's JSON path):
//!
//! 1. **Secret-free.** The entry is argv only — no `env` block. Upstream
//!    secrets resolve at spawn in the hub, never in an agent settings file.
//! 2. **Marker-tagged & idempotent.** thegn's entry carries [`MARKER_KEY`];
//!    wiring twice yields exactly one entry.
//! 3. **Never clobber the user.** thegn refuses to overwrite or remove an entry
//!    it did not mark — a user's own `thegn`-named entry is left untouched.

use serde_json::{Map, Value, json};

/// The `mcpServers` key thegn's proxy entry lives under.
pub const ENTRY_KEY: &str = "thegn";

/// The marker field thegn stamps on its own entry, so it is distinguishable
/// from a user-authored entry of the same name. `x-` prefixed (conventionally
/// ignored by MCP clients).
pub const MARKER_KEY: &str = "x-thegn-managed";

/// The outcome of applying a wire operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireOutcome {
    /// A new thegn entry was inserted.
    Added,
    /// An existing thegn-marked entry was updated (argv changed).
    Updated,
    /// The thegn entry was already exactly what we would write.
    Unchanged,
    /// A thegn-marked entry was removed.
    Removed,
    /// `--remove` ran but there was no thegn entry to remove.
    NothingToRemove,
}

impl WireOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            WireOutcome::Added => "added",
            WireOutcome::Updated => "updated",
            WireOutcome::Unchanged => "unchanged",
            WireOutcome::Removed => "removed",
            WireOutcome::NothingToRemove => "nothing to remove",
        }
    }
}

/// Build the single secret-free proxy entry: `thegn mcp proxy` argv, marked.
/// **No `env` block, ever** — this is the credential-custody guarantee.
pub fn proxy_entry(command: &str) -> Value {
    json!({
        "command": command,
        "args": ["mcp", "proxy"],
        MARKER_KEY: true,
    })
}

/// Whether a settings-entry value is thegn-managed (carries [`MARKER_KEY`]).
pub fn is_thegn_managed(entry: &Value) -> bool {
    entry
        .get(MARKER_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Apply a wire (or `--remove`) operation to an agent's parsed settings tree.
///
/// `container_path` is the JSON path from the document root to the object that
/// holds the server entries (e.g. `["mcpServers"]`, or `["mcp", "servers"]`);
/// it is created if absent. `entry` is [`proxy_entry`]. On `remove`, `entry` is
/// ignored.
///
/// Returns the outcome, or an error when the operation would touch a
/// user-authored entry thegn does not own.
pub fn apply(
    settings: &mut Value,
    container_path: &[&str],
    entry: Value,
    remove: bool,
) -> Result<WireOutcome, String> {
    let container = ensure_object_path(settings, container_path)?;

    let existing = container.get(ENTRY_KEY);
    let existing_is_ours = existing.map(is_thegn_managed).unwrap_or(false);
    let existing_is_foreign = existing.is_some() && !existing_is_ours;

    if remove {
        if existing_is_foreign {
            return Err(format!(
                "an entry named `{ENTRY_KEY}` exists but was not created by thegn \
                 (no `{MARKER_KEY}` marker) — refusing to remove it"
            ));
        }
        return if existing_is_ours {
            container.remove(ENTRY_KEY);
            Ok(WireOutcome::Removed)
        } else {
            Ok(WireOutcome::NothingToRemove)
        };
    }

    if existing_is_foreign {
        return Err(format!(
            "an entry named `{ENTRY_KEY}` exists but was not created by thegn \
             (no `{MARKER_KEY}` marker) — refusing to overwrite it"
        ));
    }
    if existing == Some(&entry) {
        return Ok(WireOutcome::Unchanged);
    }
    let outcome = if existing_is_ours {
        WireOutcome::Updated
    } else {
        WireOutcome::Added
    };
    container.insert(ENTRY_KEY.to_string(), entry);
    Ok(outcome)
}

/// Navigate to (creating as needed) the object at `path`, returning a mutable
/// handle to it. Errors if a path segment exists but is not an object.
fn ensure_object_path<'a>(
    root: &'a mut Value,
    path: &[&str],
) -> Result<&'a mut Map<String, Value>, String> {
    if !root.is_object() {
        // An empty/absent settings file parses to null → start a fresh object.
        if root.is_null() {
            *root = Value::Object(Map::new());
        } else {
            return Err("settings root is not a JSON object".to_string());
        }
    }
    let mut cur = root;
    for seg in path {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| format!("settings path segment `{seg}` is not a JSON object"))?;
        cur = obj
            .entry(seg.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    cur.as_object_mut()
        .ok_or_else(|| "settings container is not a JSON object".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Value {
        proxy_entry("thegn")
    }

    #[test]
    fn proxy_entry_is_secret_free() {
        let e = entry();
        assert_eq!(e["command"], "thegn");
        assert_eq!(e["args"], json!(["mcp", "proxy"]));
        assert_eq!(e[MARKER_KEY], true);
        // The credential-custody invariant: never an env block.
        assert!(e.get("env").is_none(), "wired entry must carry no env");
    }

    #[test]
    fn adds_into_empty_settings() {
        let mut s = json!({});
        let out = apply(&mut s, &["mcpServers"], entry(), false).unwrap();
        assert_eq!(out, WireOutcome::Added);
        assert_eq!(s["mcpServers"]["thegn"]["command"], "thegn");
        assert!(s["mcpServers"]["thegn"].get("env").is_none());
    }

    #[test]
    fn creates_missing_container_path() {
        let mut s = json!({ "other": 1 });
        apply(&mut s, &["mcp", "servers"], entry(), false).unwrap();
        assert_eq!(s["mcp"]["servers"]["thegn"]["command"], "thegn");
        assert_eq!(s["other"], 1, "unrelated keys survive");
    }

    #[test]
    fn null_root_becomes_object() {
        let mut s = Value::Null;
        apply(&mut s, &["mcpServers"], entry(), false).unwrap();
        assert!(s["mcpServers"]["thegn"].is_object());
    }

    #[test]
    fn wire_twice_is_one_entry_and_idempotent() {
        let mut s = json!({});
        assert_eq!(
            apply(&mut s, &["mcpServers"], entry(), false).unwrap(),
            WireOutcome::Added
        );
        assert_eq!(
            apply(&mut s, &["mcpServers"], entry(), false).unwrap(),
            WireOutcome::Unchanged
        );
        assert_eq!(s["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn updates_when_argv_changes() {
        let mut s = json!({});
        apply(&mut s, &["mcpServers"], proxy_entry("thegn"), false).unwrap();
        let out = apply(&mut s, &["mcpServers"], proxy_entry("tg"), false).unwrap();
        assert_eq!(out, WireOutcome::Updated);
        assert_eq!(s["mcpServers"]["thegn"]["command"], "tg");
    }

    #[test]
    fn preserves_user_entries() {
        let mut s = json!({ "mcpServers": { "git": { "command": "git-mcp" } } });
        apply(&mut s, &["mcpServers"], entry(), false).unwrap();
        assert_eq!(s["mcpServers"]["git"]["command"], "git-mcp");
        assert_eq!(s["mcpServers"]["thegn"]["command"], "thegn");
    }

    #[test]
    fn refuses_to_overwrite_a_foreign_thegn_entry() {
        let mut s = json!({ "mcpServers": { "thegn": { "command": "not-ours" } } });
        let err = apply(&mut s, &["mcpServers"], entry(), false).unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert_eq!(s["mcpServers"]["thegn"]["command"], "not-ours");
    }

    #[test]
    fn remove_only_touches_marked_entries() {
        let mut s = json!({
            "mcpServers": {
                "git": { "command": "git-mcp" },
                "thegn": proxy_entry("thegn"),
            }
        });
        let out = apply(&mut s, &["mcpServers"], Value::Null, true).unwrap();
        assert_eq!(out, WireOutcome::Removed);
        assert!(s["mcpServers"].get("thegn").is_none());
        // The user's own entry is untouched.
        assert_eq!(s["mcpServers"]["git"]["command"], "git-mcp");
    }

    #[test]
    fn remove_refuses_foreign_thegn_entry() {
        let mut s = json!({ "mcpServers": { "thegn": { "command": "not-ours" } } });
        let err = apply(&mut s, &["mcpServers"], Value::Null, true).unwrap_err();
        assert!(err.contains("refusing to remove"), "{err}");
        assert_eq!(s["mcpServers"]["thegn"]["command"], "not-ours");
    }

    #[test]
    fn remove_when_absent_is_nothing_to_remove() {
        let mut s = json!({ "mcpServers": {} });
        assert_eq!(
            apply(&mut s, &["mcpServers"], Value::Null, true).unwrap(),
            WireOutcome::NothingToRemove
        );
    }

    #[test]
    fn is_thegn_managed_reads_marker() {
        assert!(is_thegn_managed(&proxy_entry("thegn")));
        assert!(!is_thegn_managed(&json!({ "command": "x" })));
    }

    #[test]
    fn outcome_labels() {
        assert_eq!(WireOutcome::Added.as_str(), "added");
        assert_eq!(WireOutcome::NothingToRemove.as_str(), "nothing to remove");
    }
}
