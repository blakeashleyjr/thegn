//! The `<worktree>/.thegn/maintenance/issue.json` snapshot a maintenance stage
//! worker (and its reviewers) reads as the captured task data. Pure: this
//! module decides the path and the bytes; the host does the filesystem work.

use std::path::{Path, PathBuf};

use crate::issue::Issue;

/// Directory (relative to the worktree root) holding maintenance task data.
pub const DIR: &str = ".thegn/maintenance";
/// File name of the snapshot inside [`DIR`].
pub const FILE: &str = "issue.json";

/// The snapshot path for a worktree root.
pub fn path_for(worktree: &Path) -> PathBuf {
    worktree.join(DIR).join(FILE)
}

/// Pretty JSON (trailing newline) of the issue: id, number, provider, title,
/// description, url, status, priority, labels, assignees, blocked_by.
pub fn render(issue: &Issue) -> String {
    let value = serde_json::json!({
        "id": issue.id,
        "number": issue.number,
        "provider": issue.provider,
        "title": issue.title,
        "description": issue.body.clone().unwrap_or_default(),
        "url": issue.url,
        "status": issue.status.as_str(),
        "priority": issue.priority.as_str(),
        "labels": issue.labels,
        "assignees": issue.assignees,
        "blocked_by": issue.blocked_by,
    });
    let mut out = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
    out.push('\n');
    out
}

/// Coarse, body-free classification of a tracker fetch failure.
pub fn error_kind(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("no issue provider") || m.contains("provider none") {
        "provider_none"
    } else {
        "unavailable"
    }
}

/// Snapshot for a stage whose tracker fetch failed but whose prompt needs no
/// issue fields: only the reference, `"fetched": false` and the error kind.
pub fn render_unfetched(issue_ref: &str, error_kind: &str) -> String {
    let value = serde_json::json!({
        "id": issue_ref,
        "fetched": false,
        "error_kind": error_kind,
    });
    let mut out = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::{IssuePriority, IssueStatus};

    fn issue(body: Option<&str>) -> Issue {
        Issue {
            id: "linear:THE-1".into(),
            number: "THE-1".into(),
            provider: "linear".into(),
            title: "A title".into(),
            body: body.map(str::to_string),
            status: IssueStatus::InProgress,
            priority: IssuePriority::High,
            assignees: vec!["me".into()],
            labels: vec!["Bug".into()],
            url: "https://example.test/THE-1".into(),
            branch_hint: None,
            updated_at_ms: 0,
            project_ids: vec![],
            blocked_by: vec!["linear:THE-0".into()],
            due_at_ms: None,
            account: None,
        }
    }

    #[test]
    fn path_is_under_worktree() {
        assert_eq!(
            path_for(Path::new("/w/x")),
            PathBuf::from("/w/x/.thegn/maintenance/issue.json")
        );
    }

    #[test]
    fn render_carries_fields_and_trailing_newline() {
        let s = render(&issue(Some("body text")));
        assert!(s.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], "linear:THE-1");
        assert_eq!(v["number"], "THE-1");
        assert_eq!(v["provider"], "linear");
        assert_eq!(v["title"], "A title");
        assert_eq!(v["description"], "body text");
        assert_eq!(v["url"], "https://example.test/THE-1");
        assert_eq!(v["status"], "in_progress");
        assert_eq!(v["priority"], "high");
        assert_eq!(v["labels"][0], "Bug");
        assert_eq!(v["assignees"][0], "me");
        assert_eq!(v["blocked_by"][0], "linear:THE-0");
    }

    #[test]
    fn render_missing_body_is_empty_description() {
        let v: serde_json::Value = serde_json::from_str(&render(&issue(None))).unwrap();
        assert_eq!(v["description"], "");
    }

    #[test]
    fn unfetched_has_ref_flag_and_kind_only() {
        let v: serde_json::Value =
            serde_json::from_str(&render_unfetched("linear:THE-1", "provider_none")).unwrap();
        assert_eq!(v["id"], "linear:THE-1");
        assert_eq!(v["fetched"], false);
        assert_eq!(v["error_kind"], "provider_none");
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn error_kind_classifies() {
        assert_eq!(
            error_kind("issues.get x: no issue provider configured"),
            "provider_none"
        );
        assert_eq!(error_kind("connection refused"), "unavailable");
    }
}
