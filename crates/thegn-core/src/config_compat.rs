//! Bounded compatibility for the project/program vocabulary transition.
//!
//! TOML aliases are normalized before serde sees the document.  Doing this at
//! the raw-document boundary lets us diagnose duplicate canonical/legacy keys
//! while keeping the internal `Workspace*` names stable.

use serde::Serialize;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    Parse(String),
    Budget(crate::config_budget::BudgetError),
    Serialize(String),
}

impl std::fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(error) => f.write_str(error),
            Self::Budget(error) => write!(f, "{error}"),
            Self::Serialize(error) => f.write_str(error),
        }
    }
}

/// Normalize legacy project/workspace config spellings and report every
/// compatibility use.  Canonical values/tables always win on duplicates.
pub fn normalize(body: &str) -> Result<NormalizedToml, String> {
    normalize_admission(body).map_err(|error| error.to_string())
}

pub fn normalize_admission(body: &str) -> Result<NormalizedToml, NormalizeError> {
    crate::config_budget::scan(body.as_bytes()).map_err(NormalizeError::Budget)?;
    let mut value: toml::Value = body
        .parse()
        .map_err(|error| NormalizeError::Parse(format!("{error}")))?;
    crate::config_budget::check_toml_value(&value).map_err(NormalizeError::Budget)?;
    let mut diagnostics = Vec::new();
    let root = value
        .as_table_mut()
        .ok_or_else(|| NormalizeError::Parse("config document must be a TOML table".to_string()))?;

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

    // Bound serialized output before handing the String to TOML's serializer.
    // Header paths repeat for every descendant table, so account for the full
    // inherited path at each node rather than a fixed depth multiplier.
    let encoded_bound = encoded_upper_bound(&value).map_err(NormalizeError::Budget)?;
    let mut body = String::with_capacity(encoded_bound);
    let initial_capacity = body.capacity();
    value
        .serialize(toml::ser::Serializer::new(&mut body))
        .map_err(|error| NormalizeError::Serialize(format!("cannot normalize config: {error}")))?;
    if body.capacity() > initial_capacity {
        return Err(NormalizeError::Budget(
            crate::config_budget::BudgetError::AggregateBytes,
        ));
    }
    if body.len() > crate::config_budget::MAX_NORMALIZED_BYTES {
        return Err(NormalizeError::Budget(
            crate::config_budget::BudgetError::AggregateBytes,
        ));
    }
    Ok(NormalizedToml { body, diagnostics })
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
            push_diagnostic(
                diagnostics,
                format_args!(
                    "duplicate config keys `{canonical_path}` and `{legacy_path}`; using canonical `{canonical_path}` (legacy accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
                ),
            );
        } else if let Some(value) = table.remove(legacy) {
            table.insert(canonical.to_string(), value);
            push_diagnostic(
                diagnostics,
                format_args!(
                    "deprecated config key `{legacy_path}`; use `{canonical_path}` (accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
                ),
            );
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
        push_diagnostic(
            diagnostics,
            format_args!(
                "deprecated config table `workspace`; use `project` (accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
            ),
        );
    }
    for (slug, item) in entries {
        if canonical_table.contains_key(&slug) {
            push_diagnostic(
                diagnostics,
                format_args!(
                    "duplicate config tables `project.{slug}` and `workspace.{slug}`; using canonical `project.{slug}` (legacy accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
                ),
            );
        } else {
            push_diagnostic(
                diagnostics,
                format_args!(
                    "deprecated config table `workspace.{slug}`; use `project.{slug}` (accepted for {LEGACY_RELEASE_WINDOW} stable releases; removal: {LEGACY_REMOVAL_RELEASE})"
                ),
            );
            canonical_table.insert(slug, item);
        }
    }
}

fn encoded_upper_bound(value: &toml::Value) -> Result<usize, crate::config_budget::BudgetError> {
    fn add(total: &mut usize, amount: usize) -> Result<(), crate::config_budget::BudgetError> {
        *total = total
            .checked_add(amount)
            .filter(|value| *value <= crate::config_budget::MAX_NORMALIZED_BYTES)
            .ok_or(crate::config_budget::BudgetError::AggregateBytes)?;
        Ok(())
    }

    fn string_bytes(value: &str) -> Result<usize, crate::config_budget::BudgetError> {
        value
            .len()
            .checked_mul(6)
            .and_then(|value| value.checked_add(2))
            .filter(|value| *value <= crate::config_budget::MAX_NORMALIZED_BYTES)
            .ok_or(crate::config_budget::BudgetError::AggregateBytes)
    }

    fn walk(
        value: &toml::Value,
        path_bytes: usize,
    ) -> Result<usize, crate::config_budget::BudgetError> {
        let mut total = 0;
        match value {
            toml::Value::String(value) => add(&mut total, string_bytes(value)?.saturating_add(8))?,
            toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Datetime(_) => {
                add(&mut total, 64)?;
            }
            toml::Value::Boolean(_) => add(&mut total, 8)?,
            toml::Value::Array(values) => {
                add(&mut total, 8)?;
                for value in values {
                    // Arrays of tables emit a full [[ancestor.path]] for each
                    // element. Charging all elements also bounds mixed/nested
                    // arrays without depending on serializer layout choices.
                    add(&mut total, path_bytes.saturating_add(16))?;
                    add(&mut total, walk(value, path_bytes)?)?;
                }
            }
            toml::Value::Table(values) => {
                add(&mut total, 32)?;
                for (key, value) in values {
                    let mut child_path = path_bytes;
                    add(&mut child_path, string_bytes(key)?.saturating_add(1))?;
                    // Charge the full path even for scalar assignments, whose
                    // serializer only needs the leaf. Empty tables still pay
                    // their header, and wide trees pay for every repetition.
                    add(&mut total, child_path.saturating_add(16))?;
                    add(&mut total, walk(value, child_path)?)?;
                }
            }
        }
        Ok(total)
    }

    walk(value, 0)
}

fn push_diagnostic(diagnostics: &mut Vec<String>, diagnostic: std::fmt::Arguments<'_>) {
    use std::fmt::Write;
    if diagnostics.len() >= crate::config_budget::MAX_DIAGNOSTICS {
        return;
    }
    struct BoundedMessage(String);
    impl Write for BoundedMessage {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let remaining = crate::config_budget::MAX_DIAGNOSTIC_BYTES - self.0.len();
            let mut end = text.len().min(remaining);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&text[..end]);
            if end < text.len() {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut message = BoundedMessage(String::with_capacity(
        crate::config_budget::MAX_DIAGNOSTIC_BYTES,
    ));
    // Best-effort diagnostic projection: formatting stops at the byte cap,
    // which surfaces as a fmt::Error that is expected and carries nothing.
    message.write_fmt(diagnostic).unwrap_or_default();
    diagnostics.push(message.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_encoding_bound_counts_repeated_ancestor_paths() {
        let parent = "p".repeat(5_500);
        let mut source = format!("[{parent}]\n");
        for index in 0..1_500 {
            source.push_str(&format!("k{index} = {{}}\n"));
        }
        assert!(crate::config_budget::scan(source.as_bytes()).is_ok());
        assert!(matches!(
            normalize_admission(&source),
            Err(NormalizeError::Budget(
                crate::config_budget::BudgetError::AggregateBytes
            ))
        ));
    }

    #[test]
    fn admission_encoding_bound_covers_wide_deep_and_escaped_documents() {
        let sources = [
            "x = \"a\\n\\t\\\"b\"\n",
            "[a]\nx = {}\ny = {}\n[a.b.c]\nz = [1, 2, 3]\n",
            "[[a.b]]\nx = 1\n[[a.b]]\ny = { z = {} }\n",
            "values = [[1,2], [3,4]]\n",
            "[\"quoted.key\"]\n\"\\t\" = \"\\u0000\"\n",
        ];
        for source in sources {
            let value: toml::Value = source.parse().unwrap();
            let bound = encoded_upper_bound(&value).unwrap();
            let serialized = toml::to_string(&value).unwrap();
            assert!(serialized.len() <= bound, "source={source:?} bound={bound}");
            assert!(normalize_admission(source).is_ok());
        }
    }

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
