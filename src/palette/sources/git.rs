//! Git mode: PR/diff actions (wrapping the existing `pr`/`diff` commands) plus a
//! fuzzy list of local + remote branches to switch to. Branch listing shells out
//! to `git` in the focused worktree — cheap, and only when `g ` is typed.

use crate::palette::item::{Action, Row, RowKind};
use crate::theme;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// PR/diff action rows + branch rows for the focused `worktree`.
pub fn rows(worktree: &Path) -> Vec<Row> {
    let mut out = vec![
        action("±", theme::AMBER, "git diff", Action::Tool("diff".into())),
        action("⬡", theme::GREEN, "PR — status / checks", Action::PrStatus),
        action("⬡", theme::GREEN, "PR — open in browser", Action::PrOpen),
        action("⬡", theme::GREEN, "PR — create (web)", Action::PrCreate),
        action("⬡", theme::MAGENTA, "PR — approve", Action::PrApprove),
        action("⬡", theme::MAGENTA, "PR — merge (squash)", Action::PrMerge),
        action(
            "⬡",
            theme::MAGENTA,
            "PR — re-run failed checks",
            Action::PrRerun,
        ),
    ];

    for branch in branches(worktree) {
        out.push(Row {
            glyph: "⎇".into(),
            hue: theme::TEAL,
            label: branch.clone(),
            detail: "switch".into(),
            haystack: branch.clone(),
            kind: RowKind::Branch,
            action: Action::Checkout(branch),
            frecency_key: None,
            preview_path: None,
        });
    }
    out
}

fn action(glyph: &str, hue: &'static str, label: &str, action: Action) -> Row {
    Row {
        glyph: glyph.into(),
        hue,
        label: label.into(),
        detail: "git".into(),
        haystack: label.into(),
        kind: RowKind::Pr,
        action,
        frecency_key: Some(format!("git:{label}")),
        preview_path: None,
    }
}

/// Local + remote branch short-names (remotes de-prefixed and de-duped), with
/// HEAD pointers dropped.
fn branches(worktree: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args([
            "-C",
            &worktree.to_string_lossy(),
            "branch",
            "--all",
            "--format=%(refname:short)",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let mut seen = BTreeSet::new();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.contains("HEAD"))
        .map(|l| l.strip_prefix("origin/").unwrap_or(l).to_string())
        .filter(|b| seen.insert(b.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::testutil;

    #[test]
    fn rows_include_pr_actions_and_branches() {
        let repo = testutil::temp_git_repo("git-src");
        let rows = rows(&repo);
        // The fixed PR/diff actions are always present.
        assert!(rows.iter().any(|r| r.label == "git diff"));
        assert!(rows.iter().any(|r| r.label.starts_with("PR — ")));
        // The repo's single branch shows as a checkout row.
        let main = rows
            .iter()
            .find(|r| r.kind == RowKind::Branch && r.label == "main")
            .expect("main branch row");
        assert!(matches!(main.action, Action::Checkout(ref b) if b == "main"));
    }

    #[test]
    fn branches_empty_outside_a_repo() {
        let dir = testutil::sandbox().join("not-a-repo");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(branches(&dir).is_empty());
    }
}
