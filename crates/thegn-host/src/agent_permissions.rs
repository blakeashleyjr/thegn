//! Seed a headless agent's tool allow-list into its worktree.
//!
//! A headless harness auto-denies tools it has no standing permission for; the
//! repo grants none, so every pipeline worker used to need the Lead to
//! hand-write `.claude/settings.local.json` first. `[[agents]].permissions`
//! (and a stage's override) makes that declarative: at launch the list is
//! written into the harness's per-worktree settings file, preserving every
//! other key the file already holds.
//!
//! Only the `claude` harness has a per-worktree permissions file thegn knows.
//! Other harnesses are a no-op with a warning (never an error: a permission
//! list is a convenience for the worker, not a gate on the launch).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where `harness` reads a per-worktree allow-list from, relative to the
/// worktree root. `None` for a harness without one.
pub fn settings_path(harness: &str) -> Option<&'static str> {
    match harness {
        "claude" => Some(".claude/settings.local.json"),
        _ => None,
    }
}

/// The settings file content after `allow` replaces `permissions.allow`.
/// `existing` is the current file (or `None`); every other key survives, and
/// a file that is not a JSON object is an error (never silently clobbered).
pub fn merged_settings(existing: Option<&str>, allow: &[String]) -> Result<String> {
    let mut root: serde_json::Value = match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => serde_json::from_str(s).context("existing settings file is not valid JSON")?,
        None => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .context("existing settings file is not a JSON object")?;
    let perms = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let perms = perms
        .as_object_mut()
        .context("`permissions` in the existing settings file is not an object")?;
    perms.insert(
        "allow".to_string(),
        serde_json::Value::Array(
            allow
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
    );
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    Ok(out)
}

/// Write `allow` into the harness's settings file under `worktree`. Returns the
/// path written, or `None` when the harness has no such file (warned once per
/// call — the operator configured a list that cannot land anywhere).
pub fn seed(worktree: &Path, harness: &str, allow: &[String]) -> Result<Option<PathBuf>> {
    let Some(rel) = settings_path(harness) else {
        tracing::warn!(
            target: "thegn::agent",
            %harness,
            "`permissions` configured but this harness has no per-worktree allow-list file; ignored"
        );
        return Ok(None);
    };
    let path = worktree.join(rel);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let body = merged_settings(existing.as_deref(), allow)?;
    if existing.as_deref() == Some(body.as_str()) {
        return Ok(Some(path));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merged_settings_creates_the_allow_list_from_nothing() {
        let out = merged_settings(None, &allow(&["Read", "Bash"])).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["permissions"]["allow"],
            serde_json::json!(["Read", "Bash"])
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn merged_settings_replaces_allow_and_keeps_every_other_key() {
        let existing = r#"{"permissions":{"allow":["Old"],"deny":["Write"]},"model":"x"}"#;
        let out = merged_settings(Some(existing), &allow(&["Read"])).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["allow"], serde_json::json!(["Read"]));
        assert_eq!(v["permissions"]["deny"], serde_json::json!(["Write"]));
        assert_eq!(v["model"], "x");
    }

    #[test]
    fn merged_settings_refuses_a_non_object_file() {
        assert!(merged_settings(Some("[1,2]"), &allow(&["Read"])).is_err());
        assert!(merged_settings(Some("not json"), &allow(&["Read"])).is_err());
        assert!(merged_settings(Some(r#"{"permissions": 3}"#), &allow(&["Read"])).is_err());
    }

    #[test]
    fn seed_writes_only_for_a_harness_with_a_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(seed(dir.path(), "pi", &allow(&["Read"])).unwrap(), None);
        assert!(!dir.path().join(".claude").exists());
        let p = seed(dir.path(), "claude", &allow(&["Read"]))
            .unwrap()
            .unwrap();
        assert_eq!(p, dir.path().join(".claude/settings.local.json"));
        let first = std::fs::read_to_string(&p).unwrap();
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        seed(dir.path(), "claude", &allow(&["Read"])).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first);
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime,
            "unchanged content is not rewritten"
        );
    }
}
