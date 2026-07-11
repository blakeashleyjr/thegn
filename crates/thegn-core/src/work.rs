//! Unified "My Work" domain types — the cross-repo, cross-tool actionable feed.
//!
//! A `WorkRow` flattens an assigned issue (any provider), a pull request
//! awaiting the user's attention, or a high-priority notification into one
//! sortable record. The aggregator (host side) builds these from the issue
//! router + `gh search` + the notification inbox; they round-trip through the
//! `my_work_cache` DB row and feed the `Mine` panel section.

use serde::{Deserialize, Serialize};

/// Cache-scope sentinel for the cross-repo "all repos" feed (the toggle view),
/// distinct from any real repo-root path used for the default repo-scoped feed.
pub const ALL_SCOPE: &str = "*";

/// Which actionable bucket a row belongs to — the section's grouped headers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkGroup {
    /// An issue assigned to me, across any tracker.
    #[default]
    Assigned,
    /// A pull request requesting my review.
    ReviewRequested,
    /// My open PRs, plus mentions / blocker-resolved notifications.
    NeedsAttention,
}

impl WorkGroup {
    pub fn label(self) -> &'static str {
        match self {
            WorkGroup::Assigned => "Assigned to me",
            WorkGroup::ReviewRequested => "Review requested",
            WorkGroup::NeedsAttention => "Needs attention",
        }
    }

    /// Display order (lower sorts first).
    pub fn order(self) -> u8 {
        match self {
            WorkGroup::ReviewRequested => 0, // reviewing others unblocks them — first
            WorkGroup::NeedsAttention => 1,
            WorkGroup::Assigned => 2,
        }
    }
}

/// The underlying entity a row points at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    #[default]
    Issue,
    Pr,
    Notification,
}

/// One actionable row in the unified "My Work" feed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRow {
    pub group: WorkGroup,
    pub kind: WorkKind,
    /// Provider slug for the sigil: `"linear" | "github" | "jira"` (`""` = none).
    pub provider: String,
    /// Human-readable key/number, e.g. `"ABC-12"`, `"#42"`.
    pub number: String,
    pub title: String,
    /// `owner/repo` (PRs) or tracker scope; `""` when not applicable.
    #[serde(default)]
    pub repo: String,
    pub url: String,
    /// Sort weight within a group (higher = more urgent / more recent).
    #[serde(default)]
    pub urgency: u8,
    /// `"<provider>:<key>"` when this row maps to a tracked issue — enables
    /// branch-from-issue and worktree linkage.
    #[serde(default)]
    pub issue_id: Option<String>,
    /// Provider-suggested branch name, when known.
    #[serde(default)]
    pub branch_hint: Option<String>,
    /// The worktree this row is already linked to, if any (jump target).
    #[serde(default)]
    pub worktree_path: Option<String>,
}

impl WorkRow {
    /// Whether this row already has a worktree to jump into (vs. needing one
    /// created via branch-from-issue).
    pub fn is_linked(&self) -> bool {
        self.worktree_path.as_deref().is_some_and(|p| !p.is_empty())
    }
}

/// Sort rows into their display order: by group, then urgency (desc), then
/// number for stability.
pub fn sort_rows(rows: &mut [WorkRow]) {
    rows.sort_by(|a, b| {
        a.group
            .order()
            .cmp(&b.group.order())
            .then(b.urgency.cmp(&a.urgency))
            .then(a.number.cmp(&b.number))
    });
}

#[cfg(test)]
mod spec {
    use super::*;

    #[test]
    fn group_order_puts_review_first_assigned_last() {
        assert!(WorkGroup::ReviewRequested.order() < WorkGroup::NeedsAttention.order());
        assert!(WorkGroup::NeedsAttention.order() < WorkGroup::Assigned.order());
    }

    #[test]
    fn is_linked_reflects_worktree_path() {
        let mut row = WorkRow::default();
        assert!(!row.is_linked());
        row.worktree_path = Some(String::new());
        assert!(!row.is_linked());
        row.worktree_path = Some("/tmp/wt".into());
        assert!(row.is_linked());
    }

    #[test]
    fn sort_groups_then_urgency_then_number() {
        let mut rows = vec![
            WorkRow {
                group: WorkGroup::Assigned,
                number: "ABC-2".into(),
                urgency: 5,
                ..Default::default()
            },
            WorkRow {
                group: WorkGroup::ReviewRequested,
                number: "#9".into(),
                urgency: 1,
                ..Default::default()
            },
            WorkRow {
                group: WorkGroup::Assigned,
                number: "ABC-1".into(),
                urgency: 9,
                ..Default::default()
            },
        ];
        sort_rows(&mut rows);
        // Review-requested group first, then assigned sorted by urgency desc.
        assert_eq!(rows[0].group, WorkGroup::ReviewRequested);
        assert_eq!(rows[1].number, "ABC-1"); // urgency 9 before 5
        assert_eq!(rows[2].number, "ABC-2");
    }

    #[test]
    fn work_row_roundtrips_through_serde() {
        let row = WorkRow {
            group: WorkGroup::ReviewRequested,
            kind: WorkKind::Pr,
            provider: "github".into(),
            number: "#42".into(),
            title: "Fix the bug".into(),
            repo: "acme/widget".into(),
            url: "https://github.com/acme/widget/pull/42".into(),
            urgency: 3,
            issue_id: None,
            branch_hint: None,
            worktree_path: Some("/tmp/wt".into()),
        };
        let json = serde_json::to_string(&row).unwrap();
        let back: WorkRow = serde_json::from_str(&json).unwrap();
        assert_eq!(row, back);
    }

    #[test]
    fn legacy_rows_without_extension_fields_deserialize() {
        // A minimal row (older cache shape) still loads via serde defaults.
        let json = r#"{"group":"assigned","kind":"issue","provider":"linear",
            "number":"ABC-1","title":"t","url":"u"}"#;
        let row: WorkRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.group, WorkGroup::Assigned);
        assert_eq!(row.urgency, 0);
        assert!(row.issue_id.is_none());
        assert!(row.repo.is_empty());
    }
}
