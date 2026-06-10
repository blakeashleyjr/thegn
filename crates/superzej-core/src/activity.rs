//! Read-only view of the host-side activity state machine.
//!
//! The full `none/active/quiet/acked` finite state machine and its `/proc`
//! scan live in the CLI (`superzej activity`), which persists a snapshot to
//! `~/.superzej/activity.json`. The native host's sidebar only needs to *read*
//! the latest states to paint its dots, so this module deserializes the subset
//! it cares about without duplicating the FSM. A missing/garbled file yields an
//! empty map (the sidebar simply shows no dots).

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    worktrees: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
struct Entry {
    tab: String,
    state: String,
}

/// Path to the activity snapshot the CLI writes.
fn state_path() -> std::path::PathBuf {
    crate::util::superzej_dir().join("activity.json")
}

/// Read the latest activity states as `tab_name -> state` (`"active"`,
/// `"quiet"`, `"none"`, `"acked"`). Empty on any read/parse failure.
pub fn read_states() -> BTreeMap<String, String> {
    read_states_at(&state_path())
}

/// [`read_states`] against an explicit snapshot path (testable, no global env).
pub fn read_states_at(path: &std::path::Path) -> BTreeMap<String, String> {
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(snap) = serde_json::from_slice::<Snapshot>(&bytes) else {
        return BTreeMap::new();
    };
    snap.worktrees
        .into_values()
        .map(|e| (e.tab, e.state))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty() {
        let path = std::env::temp_dir().join(format!("sz-act-missing-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(read_states_at(&path).is_empty());
    }

    #[test]
    fn parses_tab_states_from_disk() {
        let path = std::env::temp_dir().join(format!("sz-act-parse-{}.json", std::process::id()));
        let json = r#"{"worktrees":{"/wt/a":{"tab":"app/home","state":"active","cpu_jiffies":0},
                        "/wt/b":{"tab":"app/feat","state":"quiet","cpu_jiffies":0}}}"#;
        std::fs::write(&path, json).unwrap();
        let m = read_states_at(&path);
        assert_eq!(m.get("app/home").map(String::as_str), Some("active"));
        assert_eq!(m.get("app/feat").map(String::as_str), Some("quiet"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn garbled_file_is_empty() {
        let path = std::env::temp_dir().join(format!("sz-act-bad-{}.json", std::process::id()));
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(read_states_at(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_states_uses_default_path_without_panicking() {
        // Exercises state_path()/read_states(); the result depends on the host's
        // ~/.superzej/activity.json (may be empty) — we only assert it returns.
        let _ = read_states();
    }
}
