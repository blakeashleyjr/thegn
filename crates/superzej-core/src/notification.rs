//! Notification domain types for ambient program-wide awareness.
//!
//! Notifications are written by background refresh diff engines and consumed
//! by the panel inbox.  They are lightweight — heavy data (issues, PRs) lives
//! in their own caches; notifications only store a kind, source reference, and
//! a pre-formatted message string.

use serde::{Deserialize, Serialize};

/// A single notification entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// DB row id (0 for unsaved).
    pub id: i64,
    pub kind: NotificationKind,
    /// The entity this notification references — an issue id like `"linear:ABC-42"`,
    /// a PR reference like `"pr:42"`, a worktree path, or any opaque reference
    /// string whose interpretation is determined by `kind`.
    #[serde(rename = "issue_id")]
    pub source_ref: String,
    /// Human-readable summary shown in the inbox.
    pub message: String,
    /// Unix milliseconds when the notification was created.
    pub created_at_ms: i64,
    /// True once the user has seen/acknowledged this entry.
    pub read: bool,
    /// The worktree this notification is most relevant to (may be empty).
    pub worktree_path: String,
}

/// What triggered the notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    // --- issue-tracker kinds ---
    /// An issue was (re-)assigned to me.
    Assigned,
    /// Someone @-mentioned me in a comment.
    Mentioned,
    /// An issue linked to the current worktree changed state.
    StatusChanged,
    /// A blocker of one of my issues was closed.
    BlockerResolved,
    /// A PR was opened whose branch matches a worktree linked to this issue.
    PrLinked,
    /// An issue is past its due date.
    Overdue,
    // --- program-wide kinds ---
    /// A PR's state changed (opened / merged / closed / checks failed).
    PrStateChanged,
    /// An agent dispatch finished successfully.
    AgentDone,
    /// An agent dispatch exited with a failure or crash.
    AgentFailed,
    /// A test run ended with one or more failures.
    TestFailed,
    /// A new worktree was created.
    WorktreeCreated,
    /// One or more ERROR lines were detected in the szhost log.
    LogError,
    /// A non-agent pane's process exited cleanly (a task-like command finished).
    ProcessExited,
    /// A non-agent pane's process crashed or exited non-zero.
    ProcessFailed,
}

impl NotificationKind {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Assigned => "→",
            Self::Mentioned => "@",
            Self::StatusChanged => "⟳",
            Self::BlockerResolved => "✓",
            Self::PrLinked => "⎇",
            Self::Overdue => "!",
            Self::PrStateChanged => "⑂",
            Self::AgentDone => "◉",
            Self::AgentFailed => "◎",
            Self::TestFailed => "✗",
            Self::WorktreeCreated => "+",
            Self::LogError => "✗",
            Self::ProcessExited => "◇",
            Self::ProcessFailed => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::Mentioned => "mentioned",
            Self::StatusChanged => "status changed",
            Self::BlockerResolved => "blocker resolved",
            Self::PrLinked => "pr linked",
            Self::Overdue => "overdue",
            Self::PrStateChanged => "pr state changed",
            Self::AgentDone => "agent done",
            Self::AgentFailed => "agent failed",
            Self::TestFailed => "tests failed",
            Self::WorktreeCreated => "worktree created",
            Self::LogError => "log error",
            Self::ProcessExited => "process exited",
            Self::ProcessFailed => "process failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_kind_roundtrips() {
        for kind in [
            NotificationKind::Assigned,
            NotificationKind::Mentioned,
            NotificationKind::StatusChanged,
            NotificationKind::BlockerResolved,
            NotificationKind::PrLinked,
            NotificationKind::Overdue,
            NotificationKind::PrStateChanged,
            NotificationKind::AgentDone,
            NotificationKind::AgentFailed,
            NotificationKind::TestFailed,
            NotificationKind::WorktreeCreated,
            NotificationKind::LogError,
            NotificationKind::ProcessExited,
            NotificationKind::ProcessFailed,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: NotificationKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn notification_kind_glyphs_and_labels_are_non_empty() {
        for kind in [
            NotificationKind::Assigned,
            NotificationKind::Mentioned,
            NotificationKind::StatusChanged,
            NotificationKind::BlockerResolved,
            NotificationKind::PrLinked,
            NotificationKind::Overdue,
            NotificationKind::PrStateChanged,
            NotificationKind::AgentDone,
            NotificationKind::AgentFailed,
            NotificationKind::TestFailed,
            NotificationKind::WorktreeCreated,
            NotificationKind::LogError,
            NotificationKind::ProcessExited,
            NotificationKind::ProcessFailed,
        ] {
            assert!(!kind.glyph().is_empty(), "{kind:?} glyph is empty");
            assert!(!kind.label().is_empty(), "{kind:?} label is empty");
        }
    }

    #[test]
    fn notification_serializes_with_source_ref() {
        let n = Notification {
            id: 0,
            kind: NotificationKind::Assigned,
            source_ref: "linear:ABC-1".into(),
            message: "ABC-1 assigned to you".into(),
            created_at_ms: 1_700_000_000_000,
            read: false,
            worktree_path: "/repo".into(),
        };
        let json = serde_json::to_string(&n).unwrap();
        // Serde rename: field serialises as "issue_id" for DB backward-compat.
        assert!(json.contains("\"issue_id\""));
        let back: Notification = serde_json::from_str(&json).unwrap();
        assert_eq!(n, back);
    }
}
