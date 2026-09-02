//! Bounded compatibility for the project/program vocabulary transition.
//!
//! TOML aliases are normalized before serde sees the document.  Doing this at
//! the raw-document boundary lets us diagnose duplicate canonical/legacy keys
//! while keeping the internal `Workspace*` names stable.

use std::collections::BTreeMap;

/// Legacy spellings remain accepted for three stable releases.
pub const LEGACY_RELEASE_WINDOW: u8 = 3;
/// Named removal policy shown to users alongside the compatibility window.
pub const LEGACY_REMOVAL_RELEASE: &str = "the fourth stable release after introduction";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedToml {
    pub body: String,
    pub diagnostics: Vec<String>,
}

/// Normalize legacy project/workspace config spellings and report every
/// compatibility use.  Canonical values/tables always win on duplicates.
pub fn normalize(body: &str) -> Result<NormalizedToml, String> {
    let mut value: toml::Value = body.parse().map_err(|e| format!("{e}"))?;
    let mut diagnostics = Vec::new();
    let root = value
        .as_table_mut()
        .ok_or_else(|| "config document must be a TOML table".to_string())?;

    rename_scalar(
        root,
        "projects_dir",
        "workspaces_dir",
        None,
        &mut diagnostics,
    );

    if let Some(ui) = root.get_mut("ui").and_then(toml::Value::as_table_mut) {
        rename_scalar(
            ui,
            "confirm_delete_project",
            "confirm_delete_workspace",
            Some("ui"),
            &mut diagnostics,
        );
        rename_scalar(
            ui,
            "sidebar_project_sort",
            "sidebar_workspace_sort",
            Some("ui"),
            &mut diagnostics,
        );
    }

    normalize_project_tables(root, &mut diagnostics);

    Ok(NormalizedToml {
        body: toml::to_string(&value).map_err(|e| format!("cannot normalize config: {e}"))?,
        diagnostics,
    })
}

/// Convert a dotted key used by `config get/set` or `--set` to its canonical
/// spelling.  Legacy keys are still accepted by callers during the window.
pub fn canonical_key(key: &str) -> String {
    match key {
        "workspaces_dir" => "projects_dir".to_string(),
        "ui.confirm_delete_workspace" => "ui.confirm_delete_project".to_string(),
        "ui.sidebar_workspace_sort" => "ui.sidebar_project_sort".to_string(),
        _ if key == "workspace" || key.starts_with("workspace.") => {
            format!("project{}", &key[9..])
        }
        _ => key.to_string(),
    }
}

fn rename_scalar(
    table: &mut toml::map::Map<String, toml::Value>,
    canonical: &str,
    legacy: &str,
    section: Option<&str>,
    diagnostics: &mut Vec<String>,
) {
    let legacy_path = section.map_or_else(|| legacy.to_string(), |s| format!("{s}.{legacy}"));
    let canonical_path =
        section.map_or_else(|| canonical.to_string(), |s| format!("{s}.{canonical}"));
    if table.contains_key(legacy) {
        if table.contains_key(canonical) {
            table.remove(legacy);
            diagnostics.push(format!(
                "duplicate config keys `{canonical_path}` and `{legacy_path}`; using canonical `{canonical_path}` (legacy accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
            ));
        } else if let Some(value) = table.remove(legacy) {
            table.insert(canonical.to_string(), value);
            diagnostics.push(format!(
                "deprecated config key `{legacy_path}`; use `{canonical_path}` (accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
            ));
        }
    }
}

fn normalize_project_tables(
    root: &mut toml::map::Map<String, toml::Value>,
    diagnostics: &mut Vec<String>,
) {
    let Some(legacy) = root.remove("workspace") else {
        return;
    };
    let Some(legacy_table) = legacy.as_table() else {
        // Leave malformed legacy values visible to strict validation rather
        // than changing the error into a compatibility diagnostic.
        root.insert("workspace".to_string(), legacy);
        return;
    };

    let canonical = root
        .entry("project")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let Some(canonical_table) = canonical.as_table_mut() else {
        root.insert(
            "workspace".to_string(),
            toml::Value::Table(legacy_table.clone()),
        );
        return;
    };

    // BTreeMap makes diagnostics deterministic while retaining TOML values.
    let entries: BTreeMap<_, _> = legacy_table.clone().into_iter().collect();
    if entries.is_empty() {
        diagnostics.push(format!(
            "deprecated config table `workspace`; use `project` (accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
        ));
    }
    for (slug, item) in entries {
        let legacy_path = format!("workspace.{slug}");
        let canonical_path = format!("project.{slug}");
        if canonical_table.contains_key(&slug) {
            diagnostics.push(format!(
                "duplicate config tables `{canonical_path}` and `{legacy_path}`; using canonical `{canonical_path}` (legacy accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
            ));
        } else {
            canonical_table.insert(slug, item);
            diagnostics.push(format!(
                "deprecated config table `{legacy_path}`; use `{canonical_path}` (accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_values_win_and_diagnose_exact_paths() {
        let out = normalize(
            r#"projects_dir = "canonical"
workspaces_dir = "legacy"
[ui]
confirm_delete_project = false
confirm_delete_workspace = true
[project.alpha]
base_branch = "main"
[workspace.alpha]
base_branch = "legacy"
[workspace.beta]
base_branch = "develop"
"#,
        )
        .unwrap();
        let cfg: toml::Value = out.body.parse().unwrap();
        assert_eq!(cfg["projects_dir"].as_str(), Some("canonical"));
        assert!(cfg.get("workspaces_dir").is_none());
        assert_eq!(cfg["ui"]["confirm_delete_project"].as_bool(), Some(false));
        assert!(cfg["project"].get("alpha").is_some());
        assert!(cfg["project"].get("beta").is_some());
        let loaded: crate::config::Config = toml::from_str(&out.body).unwrap();
        assert_eq!(loaded.workspaces_dir, "canonical");
        assert!(loaded.workspace.contains_key("beta"));
        let written = toml::to_string(&loaded).unwrap();
        assert!(written.contains("projects_dir ="));
        assert!(!written.contains("workspaces_dir ="));
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.contains("projects_dir") && d.contains("workspaces_dir"))
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.contains("project.alpha") && d.contains("workspace.alpha"))
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.contains("workspace.beta") && d.contains("project.beta"))
        );
    }

    #[test]
    fn canonical_key_accepts_legacy_dotted_names() {
        assert_eq!(canonical_key("workspaces_dir"), "projects_dir");
        assert_eq!(
            canonical_key("ui.confirm_delete_workspace"),
            "ui.confirm_delete_project"
        );
        assert_eq!(canonical_key("workspace.alpha.git"), "project.alpha.git");
        assert_eq!(
            canonical_key("tracker.workspace_id"),
            "tracker.workspace_id"
        );
    }
}
