//! Pure projection of resident plugin row-field values onto sidebar rows
//! (THE-2003).
//!
//! The loop owns the plugin row store (`handlers::plugins::RowFieldSnapshot`,
//! keyed by `(plugin, field)` then by the worktree key the plugin sent). A
//! plugin may key a value by absolute worktree path or by branch name
//! (`docs/extending/row-field.md`); this projection resolves both to the row's
//! worktree path, so the result is keyed by path only. `sidebar::worktree_row`
//! relies on that and looks values up by path alone.

use std::collections::BTreeMap;

use thegn_core::plugin_api::RowFieldSlot;

use crate::handlers::plugins::RowFieldSnapshot;
use crate::sidebar::{PluginFieldValue, SidebarRow};

/// `(plugin, field)` to the slot its `WorktreeField` contribution declared.
pub(crate) type FieldSlots = BTreeMap<(String, String), RowFieldSlot>;

/// The row-store keys a worktree's values may be stored under, in lookup
/// order: its absolute path first, then its branch name (absent for a
/// detached HEAD or an empty name). Shared by the sidebar projection and the
/// `until_scan` drop so the two cannot disagree about which values belong to
/// a worktree.
pub(crate) fn row_store_keys<'a>(path: &'a str, branch: Option<&'a str>) -> [Option<&'a str>; 2] {
    [Some(path), branch.filter(|branch| !branch.is_empty())]
}

/// Resolve every worktree row's plugin values, write them onto
/// `row.plugin_fields` (clearing stale values on rows with none), and return
/// the path-keyed map stored as `SidebarStatus::plugin_fields`.
///
/// A value whose `(plugin, field)` has no declared slot is skipped. Rows with
/// no worktree path receive nothing. Value order follows the snapshot's
/// `(plugin, field)` order, so repeated calls produce identical vectors.
pub(crate) fn project_plugin_fields(
    rows: &mut [SidebarRow],
    snapshot: &RowFieldSnapshot,
    slots: &FieldSlots,
) -> BTreeMap<String, Vec<PluginFieldValue>> {
    let mut projected = BTreeMap::new();
    for row in rows.iter().filter(|row| row.kind.is_worktree()) {
        let Some(path) = row.worktree_path.as_deref() else {
            continue;
        };
        let branch = row
            .branch
            .as_deref()
            .filter(|branch| !branch.is_empty())
            .unwrap_or(&row.label);
        let keys = row_store_keys(path, Some(branch));
        let mut values = Vec::new();
        for ((plugin, field), worktrees) in snapshot {
            let Some(value) = keys.iter().flatten().find_map(|key| worktrees.get(*key)) else {
                continue;
            };
            let Some(slot) = slots.get(&(plugin.clone(), field.clone())).copied() else {
                continue;
            };
            values.push(PluginFieldValue {
                plugin: plugin.clone(),
                field: field.clone(),
                text: value.text.clone(),
                role: value.role,
                slot,
            });
        }
        if !values.is_empty() {
            projected.insert(path.to_string(), values);
        }
    }
    for row in rows.iter_mut() {
        row.plugin_fields = row
            .worktree_path
            .as_deref()
            .and_then(|path| projected.get(path))
            .cloned()
            .unwrap_or_default();
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::plugins::RowValue;
    use crate::sidebar::RowKind;
    use thegn_core::plugin_api::RowRole;

    fn row(path: Option<&str>, branch: &str) -> SidebarRow {
        let mut row = SidebarRow::base(RowKind::Worktree, 1, "tab-label", "app");
        row.worktree_path = path.map(str::to_string);
        row.branch = Some(branch.to_string());
        row
    }

    fn value(text: &str) -> RowValue {
        RowValue {
            text: text.to_string(),
            role: RowRole::Good,
            expires_at: None,
            until_scan: false,
        }
    }

    fn snapshot(entries: &[(&str, &str, &str, &str)]) -> RowFieldSnapshot {
        let mut snapshot = RowFieldSnapshot::new();
        for (plugin, field, key, text) in entries {
            snapshot
                .entry((plugin.to_string(), field.to_string()))
                .or_default()
                .insert(key.to_string(), value(text));
        }
        snapshot
    }

    fn slots(entries: &[(&str, &str, RowFieldSlot)]) -> FieldSlots {
        entries
            .iter()
            .map(|(plugin, field, slot)| ((plugin.to_string(), field.to_string()), *slot))
            .collect()
    }

    fn texts(values: &[PluginFieldValue]) -> Vec<&str> {
        values.iter().map(|value| value.text.as_str()).collect()
    }

    #[test]
    fn projects_path_keyed_values_onto_rows_and_status() {
        let mut rows = vec![row(Some("/wt/a"), "feat/a")];
        let snap = snapshot(&[("ci", "status", "/wt/a", "passing")]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Badge)]);
        let projected = project_plugin_fields(&mut rows, &snap, &slots);
        assert_eq!(
            projected.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["/wt/a"]
        );
        let expected = vec![PluginFieldValue {
            plugin: "ci".into(),
            field: "status".into(),
            text: "passing".into(),
            role: RowRole::Good,
            slot: RowFieldSlot::Badge,
        }];
        assert_eq!(projected["/wt/a"], expected);
        assert_eq!(rows[0].plugin_fields, expected);
    }

    #[test]
    fn projects_values_onto_pipeline_worktree_rows() {
        let mut pipeline = row(Some("/wt/pipeline"), "feat/pipeline");
        pipeline.kind = RowKind::PipelineWorktree;
        let mut rows = vec![pipeline];
        let snap = snapshot(&[("ci", "status", "/wt/pipeline", "passing")]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Badge)]);

        let projected = project_plugin_fields(&mut rows, &snap, &slots);

        assert_eq!(texts(&rows[0].plugin_fields), ["passing"]);
        assert_eq!(texts(&projected["/wt/pipeline"]), ["passing"]);
    }

    #[test]
    fn plugin_without_declared_slot_is_skipped() {
        let mut rows = vec![row(Some("/wt/a"), "feat/a")];
        let snap = snapshot(&[
            ("ci", "status", "/wt/a", "passing"),
            ("rogue", "field", "/wt/a", "nope"),
        ]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Detail)]);
        let projected = project_plugin_fields(&mut rows, &snap, &slots);
        assert_eq!(texts(&projected["/wt/a"]), vec!["passing"]);
        assert_eq!(texts(&rows[0].plugin_fields), vec!["passing"]);
    }

    #[test]
    fn branch_keyed_value_projects_under_the_path_never_the_branch() {
        // The plugin contract allows a branch-name key; the projection resolves
        // it to the row's path, so the status map (what `worktree_row` reads)
        // never carries a branch-name key.
        let mut rows = vec![row(Some("/wt/a"), "feat/a")];
        let snap = snapshot(&[("ci", "status", "feat/a", "passing")]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Detail)]);
        let projected = project_plugin_fields(&mut rows, &snap, &slots);
        assert!(!projected.contains_key("feat/a"));
        assert_eq!(texts(&projected["/wt/a"]), vec!["passing"]);
    }

    #[test]
    fn row_store_keys_are_path_then_nonempty_branch() {
        assert_eq!(
            row_store_keys("/wt/a", Some("feat/a")),
            [Some("/wt/a"), Some("feat/a")]
        );
        assert_eq!(row_store_keys("/wt/a", Some("")), [Some("/wt/a"), None]);
        assert_eq!(row_store_keys("/wt/a", None), [Some("/wt/a"), None]);
    }

    #[test]
    fn path_key_wins_over_branch_key() {
        let mut rows = vec![row(Some("/wt/a"), "feat/a")];
        let snap = snapshot(&[
            ("ci", "status", "feat/a", "by-branch"),
            ("ci", "status", "/wt/a", "by-path"),
        ]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Detail)]);
        let projected = project_plugin_fields(&mut rows, &snap, &slots);
        assert_eq!(texts(&projected["/wt/a"]), vec!["by-path"]);
    }

    #[test]
    fn rows_without_worktree_path_are_skipped() {
        let mut rows = vec![row(None, "feat/a")];
        rows[0].plugin_fields = vec![PluginFieldValue {
            plugin: "ci".into(),
            field: "status".into(),
            text: "stale".into(),
            role: RowRole::Bad,
            slot: RowFieldSlot::Detail,
        }];
        let snap = snapshot(&[("ci", "status", "feat/a", "passing")]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Detail)]);
        let projected = project_plugin_fields(&mut rows, &snap, &slots);
        assert!(projected.is_empty());
        assert!(rows[0].plugin_fields.is_empty());
    }

    #[test]
    fn non_worktree_rows_are_not_projected() {
        let mut workspace = SidebarRow::base(RowKind::Workspace, 0, "app", "app");
        workspace.worktree_path = Some("/wt/a".into());
        let mut rows = vec![workspace];
        let snap = snapshot(&[("ci", "status", "/wt/a", "passing")]);
        let slots = slots(&[("ci", "status", RowFieldSlot::Detail)]);
        assert!(project_plugin_fields(&mut rows, &snap, &slots).is_empty());
    }

    #[test]
    fn empty_snapshot_clears_stale_row_values() {
        let mut rows = vec![row(Some("/wt/a"), "feat/a")];
        let slots = slots(&[("ci", "status", RowFieldSlot::Detail)]);
        let snap = snapshot(&[("ci", "status", "/wt/a", "passing")]);
        project_plugin_fields(&mut rows, &snap, &slots);
        assert_eq!(rows[0].plugin_fields.len(), 1);
        let projected = project_plugin_fields(&mut rows, &RowFieldSnapshot::new(), &slots);
        assert!(projected.is_empty());
        assert!(rows[0].plugin_fields.is_empty());
    }

    #[test]
    fn value_order_is_stable_across_calls() {
        let mut rows = vec![row(Some("/wt/a"), "feat/a"), row(Some("/wt/b"), "feat/b")];
        let snap = snapshot(&[
            ("zeta", "z", "/wt/a", "z"),
            ("alpha", "b", "/wt/a", "ab"),
            ("alpha", "a", "/wt/a", "aa"),
            ("alpha", "a", "feat/b", "ba"),
        ]);
        let slots = slots(&[
            ("zeta", "z", RowFieldSlot::Detail),
            ("alpha", "a", RowFieldSlot::Badge),
            ("alpha", "b", RowFieldSlot::Detail),
        ]);
        let first = project_plugin_fields(&mut rows, &snap, &slots);
        let second = project_plugin_fields(&mut rows, &snap, &slots);
        assert_eq!(first, second);
        assert_eq!(texts(&first["/wt/a"]), vec!["aa", "ab", "z"]);
        assert_eq!(texts(&first["/wt/b"]), vec!["ba"]);
        assert_eq!(texts(&rows[0].plugin_fields), vec!["aa", "ab", "z"]);
    }
}
