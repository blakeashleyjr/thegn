//! Provider-agnostic issue tracker domain types.
//!
//! All concrete provider logic lives in `superzej-svc`; this module holds only
//! the pure data types, filters, and serializable records that flow through the
//! DB cache and panel rendering layers.

use serde::{Deserialize, Serialize};

/// One tracked issue from any configured provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Stable opaque id in `"<provider>:<key>"` form, e.g. `"linear:ABC-123"`.
    pub id: String,
    /// Human-readable issue number/key, e.g. `"ABC-123"`, `"42"`, `"PROJ-5"`.
    pub number: String,
    /// Provider slug: `"linear"` | `"github"` | `"jira"`.
    pub provider: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub url: String,
    /// Provider-suggested branch name for this issue (e.g. `"abc-123-fix-foo"`).
    #[serde(default)]
    pub branch_hint: Option<String>,
    /// Unix milliseconds of last update (for sort + age display).
    pub updated_at_ms: i64,
    /// Project/sprint/milestone IDs this issue belongs to.
    #[serde(default)]
    pub project_ids: Vec<String>,
    /// Issue IDs that this issue is blocked by.
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

/// Workflow state of an issue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    #[default]
    Backlog,
    Todo,
    InProgress,
    Done,
    Cancelled,
}

impl IssueStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueStatus::Backlog => "backlog",
            IssueStatus::Todo => "todo",
            IssueStatus::InProgress => "in_progress",
            IssueStatus::Done => "done",
            IssueStatus::Cancelled => "cancelled",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IssueStatus::Backlog => "Backlog",
            IssueStatus::Todo => "Todo",
            IssueStatus::InProgress => "In Progress",
            IssueStatus::Done => "Done",
            IssueStatus::Cancelled => "Cancelled",
        }
    }

    /// Single-character glyph for compact display.
    pub fn glyph(self) -> char {
        match self {
            IssueStatus::Backlog => '○',
            IssueStatus::Todo => '◌',
            IssueStatus::InProgress => '◑',
            IssueStatus::Done => '●',
            IssueStatus::Cancelled => '⊘',
        }
    }

    /// Whether this state counts as "active" (not done/cancelled).
    pub fn is_active(self) -> bool {
        matches!(
            self,
            IssueStatus::Backlog | IssueStatus::Todo | IssueStatus::InProgress
        )
    }
}

/// Triage priority of an issue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssuePriority {
    Urgent,
    High,
    Medium,
    #[default]
    Low,
    None,
}

impl IssuePriority {
    pub fn as_str(self) -> &'static str {
        match self {
            IssuePriority::Urgent => "urgent",
            IssuePriority::High => "high",
            IssuePriority::Medium => "medium",
            IssuePriority::Low => "low",
            IssuePriority::None => "none",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IssuePriority::Urgent => "URGENT",
            IssuePriority::High => "HIGH",
            IssuePriority::Medium => "MED",
            IssuePriority::Low => "LOW",
            IssuePriority::None => "—",
        }
    }
}

/// Filter applied when fetching issues from a provider.
#[derive(Debug, Clone, Default)]
pub struct IssueFilter {
    /// Only return issues assigned to the authenticated user.
    pub assignee_me: bool,
    /// Restrict to specific statuses; empty means all active statuses.
    pub statuses: Vec<IssueStatus>,
    /// Optional project / team scope (provider-specific id).
    pub project_id: Option<String>,
    /// Free-text search query.
    pub query: Option<String>,
    /// Maximum number of issues to return (provider may impose lower cap).
    pub limit: usize,
}

impl IssueFilter {
    pub fn my_open(limit: usize) -> Self {
        IssueFilter {
            assignee_me: true,
            statuses: vec![
                IssueStatus::Backlog,
                IssueStatus::Todo,
                IssueStatus::InProgress,
            ],
            limit,
            ..Default::default()
        }
    }
}

/// Minimal issue payload for creating a new issue.
#[derive(Debug, Clone, Default)]
pub struct IssueDraft {
    pub title: String,
    pub body: Option<String>,
    pub priority: IssuePriority,
    /// Provider-specific project/team id to create under (uses provider default when None).
    pub project_id: Option<String>,
}

/// Partial update applied to an existing issue.
#[derive(Debug, Clone, Default)]
pub struct IssuePatch {
    pub status: Option<IssueStatus>,
    pub title: Option<String>,
    /// `true` = assign self, `false` = unassign self.
    pub assignee_me: Option<bool>,
    pub priority: Option<IssuePriority>,
}

/// Extended detail record fetched for a single issue (includes comments).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDetail {
    #[serde(flatten)]
    pub issue: Issue,
    #[serde(default)]
    pub comments: Vec<IssueComment>,
}

/// One comment on an issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub author: String,
    pub body: String,
    /// Unix milliseconds.
    pub created_at_ms: i64,
}

/// An agent dispatch record: one AI coding agent working on one issue
/// in a dedicated worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDispatch {
    /// DB row id (0 for unsaved).
    pub id: i64,
    pub issue_id: String,
    pub worktree_path: String,
    /// Matches an `[[agents]]` name in config.
    pub agent_name: String,
    pub dispatched_at_ms: i64,
    pub status: AgentDispatchStatus,
}

/// Lifecycle of an agent dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentDispatchStatus {
    #[default]
    Queued,
    Spawning,
    Running,
    WaitingHuman,
    PrOpen,
    Merged,
    Abandoned,
}

impl AgentDispatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::WaitingHuman => "waiting_human",
            Self::PrOpen => "pr_open",
            Self::Merged => "merged",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Queued | Self::Spawning => "⚙",
            Self::Running => "⚙",
            Self::WaitingHuman => "⏸",
            Self::PrOpen => "⎇",
            Self::Merged => "✓",
            Self::Abandoned => "✗",
        }
    }
}

#[cfg(test)]
mod spec {
    use super::*;

    #[test]
    fn issue_default_is_valid() {
        let i = Issue::default();
        assert_eq!(i.status, IssueStatus::Backlog);
        assert_eq!(i.priority, IssuePriority::Low);
        assert!(i.assignees.is_empty());
    }

    #[test]
    fn issue_status_roundtrip() {
        for s in [
            IssueStatus::Backlog,
            IssueStatus::Todo,
            IssueStatus::InProgress,
            IssueStatus::Done,
            IssueStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: IssueStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back, "roundtrip failed for {:?}", s);
        }
    }

    #[test]
    fn issue_priority_roundtrip() {
        for p in [
            IssuePriority::Urgent,
            IssuePriority::High,
            IssuePriority::Medium,
            IssuePriority::Low,
            IssuePriority::None,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: IssuePriority = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back, "roundtrip failed for {:?}", p);
        }
    }

    #[test]
    fn priority_ordering() {
        assert!(IssuePriority::Urgent < IssuePriority::High);
        assert!(IssuePriority::High < IssuePriority::Medium);
        assert!(IssuePriority::Medium < IssuePriority::Low);
        assert!(IssuePriority::Low < IssuePriority::None);
    }

    #[test]
    fn status_active_flags() {
        assert!(IssueStatus::Backlog.is_active());
        assert!(IssueStatus::Todo.is_active());
        assert!(IssueStatus::InProgress.is_active());
        assert!(!IssueStatus::Done.is_active());
        assert!(!IssueStatus::Cancelled.is_active());
    }

    #[test]
    fn my_open_filter_defaults() {
        let f = IssueFilter::my_open(50);
        assert!(f.assignee_me);
        assert_eq!(f.limit, 50);
        assert!(f.statuses.iter().all(|s| s.is_active()));
    }

    #[test]
    fn issue_full_roundtrip() {
        let orig = Issue {
            id: "linear:ABC-42".into(),
            number: "ABC-42".into(),
            provider: "linear".into(),
            title: "Fix the thing".into(),
            body: Some("Description here.".into()),
            status: IssueStatus::InProgress,
            priority: IssuePriority::High,
            assignees: vec!["Blake".into()],
            labels: vec!["bug".into()],
            url: "https://linear.app/team/issue/ABC-42".into(),
            branch_hint: Some("abc-42-fix-the-thing".into()),
            updated_at_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(orig, back);
    }

    #[test]
    fn issue_detail_serializes_comments() {
        let d = IssueDetail {
            issue: Issue {
                id: "github:7".into(),
                ..Default::default()
            },
            comments: vec![IssueComment {
                author: "alice".into(),
                body: "LGTM".into(),
                created_at_ms: 1_000,
            }],
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: IssueDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
