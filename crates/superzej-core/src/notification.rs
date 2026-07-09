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
    /// Creation time, in Unix **seconds** (populated from [`crate::util::now`],
    /// which returns seconds). The `_ms` suffix is a legacy misnomer kept to
    /// avoid a DB-column rename; feed it to [`crate::util::age`] for display,
    /// never to a millisecond clock.
    pub created_at_ms: i64,
    /// True once the user has seen/acknowledged this entry.
    pub read: bool,
    /// The worktree this notification is most relevant to (may be empty).
    pub worktree_path: String,
}

/// What triggered the notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// An agent explicitly asked for human attention/input (MCP `request_human`).
    AgentAttention,
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
    /// A merge-queue branch landed on the target (fold-actor).
    QueueLanded,
    /// A merge-queue branch gated green and awaits a manual land.
    QueueReady,
    /// The merge-queue agent gave up on a branch — human intervention needed.
    QueueNeedsHuman,
}

/// Attention priority of a notification — the single source of truth that drives
/// the inbox flag badge, the neutral unread count, and (mapped to urgency) desktop
/// toasts. Derived from [`NotificationKind::default_priority`], overridable per
/// kind in `[notifications.priority]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    /// Informational lifecycle events (worktree created, process exited). Shown in
    /// the inbox list as history but never counted toward any badge.
    Info,
    /// Normal awareness (mentions, assignments, agent done, PR/status changes).
    /// Counts toward the neutral unread badge but never raises the red flag.
    Notice,
    /// Needs attention (failures). Raises the red ⚑ flag and a desktop toast.
    Alert,
}

impl Priority {
    /// Numeric rank for ordering/threshold comparison (higher = more urgent).
    pub fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Notice => 1,
            Self::Alert => 2,
        }
    }

    /// Parse a priority from a config string (`"info"`, `"notice"`, `"alert"`).
    /// Returns `None` for unknown values so the caller can fall back to the kind's
    /// default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "info" => Some(Self::Info),
            "notice" => Some(Self::Notice),
            "alert" => Some(Self::Alert),
            _ => None,
        }
    }
}

impl NotificationKind {
    /// Every notification kind, for exhaustive iteration (config classification,
    /// SQL `IN` set construction, tests). Kept in sync with the enum by the
    /// `notification_kind_*` tests, which loop over this.
    pub const ALL: [NotificationKind; 18] = [
        Self::Assigned,
        Self::Mentioned,
        Self::StatusChanged,
        Self::BlockerResolved,
        Self::PrLinked,
        Self::Overdue,
        Self::PrStateChanged,
        Self::AgentDone,
        Self::AgentFailed,
        Self::AgentAttention,
        Self::TestFailed,
        Self::WorktreeCreated,
        Self::LogError,
        Self::ProcessExited,
        Self::ProcessFailed,
        Self::QueueLanded,
        Self::QueueReady,
        Self::QueueNeedsHuman,
    ];

    /// The snake_case identifier for this kind — matches both the serde
    /// representation and the `kind` strings persisted in the DB, so it is the key
    /// for config overrides and SQL `kind IN (...)` filters.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::Mentioned => "mentioned",
            Self::StatusChanged => "status_changed",
            Self::BlockerResolved => "blocker_resolved",
            Self::PrLinked => "pr_linked",
            Self::Overdue => "overdue",
            Self::PrStateChanged => "pr_state_changed",
            Self::AgentDone => "agent_done",
            Self::AgentFailed => "agent_failed",
            Self::AgentAttention => "agent_attention",
            Self::TestFailed => "test_failed",
            Self::WorktreeCreated => "worktree_created",
            Self::LogError => "log_error",
            Self::ProcessExited => "process_exited",
            Self::ProcessFailed => "process_failed",
            Self::QueueLanded => "queue_landed",
            Self::QueueReady => "queue_ready",
            Self::QueueNeedsHuman => "queue_needs_human",
        }
    }

    /// The built-in attention priority for this kind, before any config override.
    /// Failures are `Alert`; lifecycle/info events (`WorktreeCreated`,
    /// `ProcessExited`) are `Info`; everything else is `Notice`.
    pub fn default_priority(self) -> Priority {
        match self {
            Self::AgentFailed
            | Self::AgentAttention
            | Self::TestFailed
            | Self::LogError
            | Self::ProcessFailed
            | Self::QueueNeedsHuman => Priority::Alert,
            Self::WorktreeCreated | Self::ProcessExited | Self::QueueLanded => Priority::Info,
            Self::Assigned
            | Self::Mentioned
            | Self::StatusChanged
            | Self::BlockerResolved
            | Self::PrLinked
            | Self::Overdue
            | Self::PrStateChanged
            | Self::AgentDone
            | Self::QueueReady => Priority::Notice,
        }
    }

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
            Self::AgentAttention => "⚠",
            Self::TestFailed => "✗",
            Self::WorktreeCreated => "+",
            Self::LogError => "✗",
            Self::ProcessExited => "◇",
            Self::ProcessFailed => "✗",
            Self::QueueLanded => "✓",
            Self::QueueReady => "◆",
            Self::QueueNeedsHuman => "✋",
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
            Self::AgentAttention => "agent needs attention",
            Self::TestFailed => "tests failed",
            Self::WorktreeCreated => "worktree created",
            Self::LogError => "log error",
            Self::ProcessExited => "process exited",
            Self::ProcessFailed => "process failed",
            Self::QueueLanded => "merge queue landed",
            Self::QueueReady => "merge queue ready to land",
            Self::QueueNeedsHuman => "merge queue needs you",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_kind_roundtrips() {
        for kind in NotificationKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            let back: NotificationKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn notification_kind_glyphs_and_labels_are_non_empty() {
        for kind in NotificationKind::ALL {
            assert!(!kind.glyph().is_empty(), "{kind:?} glyph is empty");
            assert!(!kind.label().is_empty(), "{kind:?} label is empty");
        }
    }

    #[test]
    fn as_str_matches_serde_snake_case() {
        // as_str must equal the serde representation so config keys and DB `kind`
        // values line up. ALL must also be complete + free of duplicates.
        let mut seen = std::collections::HashSet::new();
        for kind in NotificationKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            let serde_name = json.trim_matches('"');
            assert_eq!(kind.as_str(), serde_name, "{kind:?}");
            assert!(seen.insert(kind), "{kind:?} duplicated in ALL");
        }
        assert_eq!(seen.len(), 18, "ALL is missing kinds");
    }

    #[test]
    fn default_priority_is_total_and_correct() {
        use Priority::*;
        for kind in NotificationKind::ALL {
            // Total: every kind classifies (the match is exhaustive, so this just
            // exercises it) and the failure/info sets are exactly as designed.
            let p = kind.default_priority();
            let expect_alert = matches!(
                kind,
                NotificationKind::AgentFailed
                    | NotificationKind::AgentAttention
                    | NotificationKind::TestFailed
                    | NotificationKind::LogError
                    | NotificationKind::ProcessFailed
                    | NotificationKind::QueueNeedsHuman
            );
            let expect_info = matches!(
                kind,
                NotificationKind::WorktreeCreated
                    | NotificationKind::ProcessExited
                    | NotificationKind::QueueLanded
            );
            let expected = if expect_alert {
                Alert
            } else if expect_info {
                Info
            } else {
                Notice
            };
            assert_eq!(p, expected, "{kind:?}");
        }
    }

    #[test]
    fn priority_parse_and_rank() {
        assert_eq!(Priority::parse("alert"), Some(Priority::Alert));
        assert_eq!(Priority::parse(" Notice "), Some(Priority::Notice));
        assert_eq!(Priority::parse("INFO"), Some(Priority::Info));
        assert_eq!(Priority::parse("bogus"), None);
        assert!(Priority::Alert.rank() > Priority::Notice.rank());
        assert!(Priority::Notice.rank() > Priority::Info.rank());
        // Ord matches rank (used for >= threshold comparisons).
        assert!(Priority::Alert > Priority::Notice && Priority::Notice > Priority::Info);
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
