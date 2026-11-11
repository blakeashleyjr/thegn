//! `thegn doctor` section: per-worktree sandbox profile pins.

use std::path::{Path, PathBuf};

use thegn_core::config::{Config, SandboxProfile};
use thegn_core::models::WorktreeRow;
use thegn_core::outln;
use thegn_core::sandbox_pin::{EnvSelection, explain, parse_pin};

/// One pinned worktree. `invalid` marks a stored pin that no longer parses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct PinRow {
    pub worktree: String,
    pub pinned: String,
    pub default: String,
    pub differs: bool,
    pub invalid: bool,
}

/// Build the report rows from stored worktree records. Unpinned worktrees are
/// skipped; an unparseable pin stays visible. Sorted by worktree path.
pub(super) fn rows(
    records: &[WorktreeRow],
    default_for: impl Fn(&WorktreeRow) -> SandboxProfile,
) -> Vec<PinRow> {
    let mut out: Vec<PinRow> = records
        .iter()
        .filter_map(|row| {
            let raw = row.sandbox_profile.as_deref()?;
            Some(match parse_pin(raw) {
                Ok(pinned) => {
                    let default = default_for(row);
                    PinRow {
                        worktree: row.worktree.clone(),
                        pinned: pinned.as_str().to_string(),
                        default: default.as_str().to_string(),
                        differs: pinned != default,
                        invalid: false,
                    }
                }
                Err(_) => PinRow {
                    worktree: row.worktree.clone(),
                    pinned: raw.to_string(),
                    default: "unknown".to_string(),
                    differs: true,
                    invalid: true,
                },
            })
        })
        .collect();
    out.sort_by(|a, b| a.worktree.cmp(&b.worktree));
    out
}

/// Read the pins from the DB. Never fails doctor: an unreadable DB yields none.
pub(super) fn collect(cfg: &Config) -> Vec<PinRow> {
    let records = match thegn_core::db::Db::open().and_then(|db| db.worktrees()) {
        Ok(records) => records,
        Err(e) => {
            tracing::debug!(error = %e, "doctor: worktree pins unavailable");
            return Vec::new();
        }
    };
    rows(&records, |row| {
        let location = (!row.location.is_empty()).then_some(row.location.as_str());
        let loc = thegn_core::remote::GitLoc::from_db(&row.worktree, location);
        let selection = EnvSelection {
            name: row.env_name.clone(),
            profile: None,
        };
        cfg.resolve_env(
            &PathBuf::from(&row.repo_root),
            &loc,
            Path::new(&row.worktree),
            &selection,
        )
        .sandbox
        .profile
    })
}

pub(super) fn json(rows: &[PinRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap_or_else(|_| serde_json::json!([]))
}

fn title(value: &str) -> String {
    match SandboxProfile::from_str_validated(value) {
        Ok(profile) => explain(profile).title.to_string(),
        Err(_) => value.to_string(),
    }
}

pub(super) fn lines(rows: &[PinRow]) -> Vec<String> {
    let mut out = vec!["Worktree sandbox pins".to_string()];
    if rows.is_empty() {
        out.push("  none (worktrees inherit the configured profile)".to_string());
        return out;
    }
    for row in rows {
        let verdict = if row.differs { "differs" } else { "same" };
        out.push(format!(
            "  {}: {} (default {}; {verdict})",
            row.worktree,
            title(&row.pinned),
            title(&row.default),
        ));
    }
    out
}

pub(super) fn print(rows: &[PinRow]) {
    for line in lines(rows) {
        outln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(worktree: &str, pin: Option<&str>) -> WorktreeRow {
        WorktreeRow {
            worktree: worktree.to_string(),
            branch: "b".to_string(),
            agent: String::new(),
            created_at: 0,
            repo_root: "/repo".to_string(),
            tab_name: String::new(),
            session_name: String::new(),
            location: String::new(),
            position: 0,
            sandbox_backend: None,
            observed_backend: None,
            folder_id: None,
            env_name: None,
            sandbox_profile: pin.map(str::to_string),
        }
    }

    #[test]
    fn rows_skip_unpinned_and_flag_a_pin_that_differs_from_the_default() {
        let records = [
            record("/wt/c", Some("sealed")),
            record("/wt/a", None),
            record("/wt/b", Some("sealed")),
        ];
        let out = rows(&records, |row| {
            if row.worktree == "/wt/b" {
                SandboxProfile::Sealed
            } else {
                SandboxProfile::Hardened
            }
        });
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].worktree, "/wt/b");
        assert_eq!(out[0].pinned, "sealed");
        assert_eq!(out[0].default, SandboxProfile::Sealed.as_str());
        assert!(!out[0].differs);
        assert_eq!(out[1].worktree, "/wt/c");
        assert_eq!(out[1].default, SandboxProfile::Hardened.as_str());
        assert!(out[1].differs);
    }

    #[test]
    fn rows_keep_an_unparseable_stored_pin_visible() {
        let out = rows(&[record("/wt/x", Some("bogus"))], |_| {
            SandboxProfile::Hardened
        });
        assert_eq!(out.len(), 1);
        assert!(out[0].invalid);
        assert!(out[0].differs);
        assert_eq!(out[0].pinned, "bogus");
    }

    #[test]
    fn json_is_a_stable_array_with_the_documented_keys() {
        let out = rows(&[record("/wt/a", Some("sealed"))], |_| {
            SandboxProfile::Hardened
        });
        let v = json(&out);
        let mut keys: Vec<&str> = v[0]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["default", "differs", "invalid", "pinned", "worktree"]
        );
        assert_eq!(json(&[]), serde_json::json!([]));
    }

    #[test]
    fn lines_say_none_when_there_are_no_pins_and_name_each_pin() {
        let none = lines(&[]);
        assert_eq!(none[0], "Worktree sandbox pins");
        assert!(none[1].contains("none ("));
        let out = rows(&[record("/wt/a", Some("sealed"))], |_| {
            SandboxProfile::Hardened
        });
        let text = lines(&out).join("\n");
        assert!(
            text.contains("Sealed (default Standard; differs)"),
            "{text}"
        );
    }
}
