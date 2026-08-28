//! Provider-agnostic issue tracker domain types.
//!
//! All concrete provider logic lives in `thegn-svc`; this module holds only
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
    /// Due date in unix milliseconds (midnight UTC for date-only providers).
    /// `None` when the provider has no due-date concept (GitHub) or none set.
    #[serde(default)]
    pub due_at_ms: Option<i64>,
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IssueFilter {
    /// Only return issues assigned to the authenticated user.
    pub assignee_me: bool,
    /// Restrict to specific statuses; empty means all active statuses.
    pub statuses: Vec<IssueStatus>,
    /// Optional project / team scope (provider-specific id).
    pub project_id: Option<String>,
    /// Optional repository scope as `"owner/repo"` — used by the GitHub Issues
    /// backend to restrict to one repo (the repo-scoped "My Work" feed). Other
    /// providers ignore it (they scope via `project_id` / config team/project).
    pub repo: Option<String>,
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IssueDraft {
    pub title: String,
    pub body: Option<String>,
    pub priority: IssuePriority,
    /// Provider-specific project/team id to create under (uses provider default when None).
    pub project_id: Option<String>,
}

/// Partial update applied to an existing issue.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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

/// The branch name a worktree for this issue should take, **before**
/// de-duplication against existing branches: the provider's `branch_hint` when
/// it has one, else a slug of the issue number. The single rule both the TUI
/// `D`-key dispatch and the headless `worktrees.create` door use, so the two
/// can never derive a different branch for the same issue.
pub fn issue_branch_seed(branch_hint: Option<&str>, number: &str) -> String {
    branch_hint
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::util::slugify(number))
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
    /// Pipeline stage this row belongs to (a `[[pipeline.stages]]` name, e.g.
    /// `"architect"` / `"code"` / `"review"`). Free-form by design: thegn stores
    /// and groups by it, and never advances it — stage transitions are the
    /// supervising agent's judgment (the roster gains columns, never
    /// transitions). `None` on every row written before v56 and on any dispatch
    /// made outside a pipeline.
    #[serde(default)]
    pub stage: Option<String>,
    /// The row this one was chunked out of — an Architect's row is the parent of
    /// each coder row it fanned out. `None` for a root dispatch. A plain
    /// self-referential id, deliberately not a foreign key: the roster is a
    /// cache-side ledger and a pruned parent must never make a child unreadable.
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// The daemon session id running this dispatch, when it was launched through
    /// `sessions.open`. This is the row's *identity* for pane-exit attribution
    /// (see `dispatch_for_exit`): several stages can share one worktree, so the
    /// worktree path alone cannot say which row a dying pane belonged to.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Path to the handoff artifact this row produced or consumes (a file
    /// committed in the worktree, e.g. `.thegn/pipeline/architect/42.md`). A
    /// POINTER, never the payload: git stays the source of truth, so the roster
    /// never becomes a document store.
    #[serde(default)]
    pub artifact_path: Option<String>,
    /// Free-text note on the row, written (append-style) by the daemon's
    /// transport-retry observer (THE-86): why a headless worker died, which
    /// retry attempt it reached, or why a relaunch failed. Written ONLY by the
    /// stamper — a human or the supervising agent reading it via `dispatch
    /// list` can trust it as the daemon's ledger of what it did. `None` on
    /// every row written before v59 and on any row the observer never touched.
    #[serde(default)]
    pub note: Option<String>,
}

/// Unix-epoch cutoff separating a SECONDS stamp from a MILLISECONDS one.
/// As milliseconds this is 1973-03-03; as seconds it is the year 5138 — so
/// anything below it is unambiguously seconds for any timestamp this program
/// can legitimately hold.
pub const MS_EPOCH_FLOOR: i64 = 100_000_000_000;

/// Coerce a `dispatched_at_ms` value to milliseconds.
///
/// Rows written before the `util::now()` → `util::now_ms()` fix stored
/// SECONDS, which rendered as an age of ~20 671 days. The DB migration at
/// schema v58 rewrites those in place; this guard is the second defence, for
/// values that never pass through it — a roster deserialized over the control
/// API, a DB a newer build wrote, a hand-edited row.
///
/// `<= 0` is passed through unchanged: an unstamped row must read as
/// unstamped, never as 1970 multiplied by a thousand.
///
/// Idempotent over its domain — real wall-clock stamps — which is what makes
/// it safe to apply on top of the v58 migration: a scaled seconds value lands
/// above the floor and is left alone on a second pass. (The single-digit
/// positives that would scale twice are not timestamps anything writes.)
pub fn normalize_dispatch_ms(v: i64) -> i64 {
    if v > 0 && v < MS_EPOCH_FLOOR {
        v.saturating_mul(1_000)
    } else {
        v
    }
}

/// The writable fields of a new roster row — everything
/// [`put_agent_dispatch`](crate::store::NotificationStore::put_agent_dispatch)
/// inserts.
///
/// A struct rather than positional arguments: the row went from three strings
/// to seven fields in one change, and a seven-argument insert is exactly the
/// call site where a caller silently swaps two same-typed strings. Borrowed
/// (`&str`) because every caller already holds its parts; [`Self::new`] fills
/// the pipeline columns with `None` so a non-pipeline dispatch reads as one
/// line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewDispatch<'a> {
    pub issue_id: &'a str,
    pub worktree_path: &'a str,
    pub agent_name: &'a str,
    pub stage: Option<&'a str>,
    pub parent_id: Option<i64>,
    pub session_id: Option<&'a str>,
    pub artifact_path: Option<&'a str>,
}

impl<'a> NewDispatch<'a> {
    /// A plain (non-pipeline) dispatch: issue, worktree, agent; every pipeline
    /// column `None`.
    pub fn new(issue_id: &'a str, worktree_path: &'a str, agent_name: &'a str) -> Self {
        Self {
            issue_id,
            worktree_path,
            agent_name,
            stage: None,
            parent_id: None,
            session_id: None,
            artifact_path: None,
        }
    }
}

/// Lifecycle of an agent dispatch — a **closed, parseable** set. A supervisor
/// resumes from the durable roster by reading these back, so the terminal
/// outcomes (`Done`/`Failed`) must be distinguishable from the in-flight and
/// human-parked states, and every writer must go through the typed status (see
/// [`AgentDispatchStatus::parse`] / [`AgentDispatchStatus::as_str`]) rather than
/// a free string. `Unknown` is not a writable state — it is only what a *read*
/// coerces a legacy or corrupt stored string to, so listing the roster never
/// fails on one bad row (the never-reset-user-data contract).
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
    /// The worker finished cleanly (its pane exited 0). Written by the pane-exit
    /// handler.
    Done,
    /// The worker's pane exited non-zero (or crashed). Written by the pane-exit
    /// handler.
    Failed,
    /// A stored status string this build does not recognize — a read-only
    /// coercion so the roster stays listable. Never written back.
    Unknown,
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
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored status string into the closed set. Every canonical
    /// [`as_str`](Self::as_str) form round-trips; the legacy lowercase strings
    /// the pane-exit handler wrote before `Done`/`Failed` existed (`"done"` /
    /// `"failed"`) map onto the new variants (they share the canonical form);
    /// anything else coerces to [`Unknown`](Self::Unknown) so a read never
    /// errors. **Total by construction** — this is the single reader every
    /// roster load goes through.
    pub fn parse(s: &str) -> AgentDispatchStatus {
        match s.trim() {
            "queued" => Self::Queued,
            "spawning" => Self::Spawning,
            "running" => Self::Running,
            "waiting_human" => Self::WaitingHuman,
            "pr_open" => Self::PrOpen,
            "merged" => Self::Merged,
            "abandoned" => Self::Abandoned,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    /// Whether this is a terminal outcome — the worker is finished and its row
    /// should not be re-dispatched. `Merged`/`Abandoned` are the human-driven
    /// terminals; `Done`/`Failed` are the pane-exit terminals.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Merged | Self::Abandoned | Self::Done | Self::Failed
        )
    }

    /// Whether a worker for this row is (or should be) live — so a resuming
    /// supervisor never dispatches a second agent onto a `Running` row.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Spawning | Self::Running | Self::WaitingHuman | Self::PrOpen
        )
    }

    /// The glyph token for this status, resolved against the live glyph set at
    /// the DRAW site — so the board degrades with `[theme] glyphs` / a non-UTF-8
    /// locale instead of mojibaking. (A `&'static str` baked in here could not
    /// follow a caps reload, which is why the board used to.)
    ///
    /// One token per PHASE, not per variant: `Merged`/`Done` are both "finished
    /// cleanly" and `Abandoned`/`Failed` both "ended badly". What must never
    /// collide is the five ACTIVE states — they are what a supervisor scans a
    /// board for, and `Queued`/`Spawning`/`Running` all rendered the same glyph
    /// before this.
    pub fn glyph_token(self) -> crate::termcaps::Glyph {
        use crate::termcaps::Glyph as G;
        match self {
            Self::Queued => G::DiamondHollow,
            Self::Spawning => G::Refresh,
            Self::Running => G::DotFilled,
            Self::WaitingHuman => G::Attention,
            Self::PrOpen => G::Hex,
            Self::Merged | Self::Done => G::Check,
            Self::Abandoned | Self::Failed => G::Cross,
            Self::Unknown => G::DotHollow,
        }
    }

    /// The full-Unicode glyph — what `thegn dispatch list` prints. Defined as
    /// [`Self::glyph_token`] resolved at the top rung so the CLI and the board
    /// can never disagree about what a row is doing.
    pub fn glyph(self) -> &'static str {
        self.glyph_token().resolve(&crate::termcaps::UNICODE)
    }

    /// The one hued-glyph vocabulary for roster status, shared by every chrome
    /// surface that renders it (the pipeline board, the sidebar's stage chip) —
    /// a single source so the surfaces can never diverge. Glyphs come from the
    /// caller's capability-resolved [`GlyphSet`](crate::termcaps::GlyphSet), so
    /// the vocabulary degrades to ASCII with the rest of the chrome; the hues
    /// match the board's `dispatch_tone`, so the board, the sidebar and the CLI
    /// never tell different stories about one row.
    ///
    /// Unlike [`glyph`](Self::glyph) — which `thegn dispatch list` keeps for its
    /// fixed plain-text output — `Queued`, `Spawning` and `Running` are
    /// **distinct** here: three states that all rendered as ⚙ made a whole
    /// pipeline look identically busy whatever it was actually doing. `Unknown`
    /// keeps [`Glyph::DotHollow`] — the same token [`Self::glyph_token`] gives
    /// it — so the CLI and the chrome stay one vocabulary; it does not read as
    /// a queued row.
    pub fn glyph_set(self, gl: &crate::termcaps::GlyphSet) -> (&'static str, crate::theme::Hue) {
        use crate::theme::Hue;
        match self {
            // `Hue` has no dim/grey member; Blue is what the palette already
            // uses for inert-but-fine state (see `MqStatus::Queued`). A surface
            // with a dim slot (the board maps these to `S::Dim`) may override.
            Self::Queued => (gl.diamond_hollow, Hue::Blue),
            Self::Unknown => (gl.dot_hollow, Hue::Blue),
            Self::Spawning => (gl.refresh, Hue::Teal),
            Self::Running => (gl.dot_filled, Hue::Teal),
            Self::WaitingHuman => (gl.attention, Hue::Amber),
            Self::PrOpen => (gl.hex, Hue::Blue),
            Self::Merged | Self::Done => (gl.check, Hue::Green),
            Self::Abandoned | Self::Failed => (gl.cross, Hue::Red),
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

    #[test]
    fn status_string_representations() {
        let cases = [
            (IssueStatus::Backlog, "backlog", "Backlog", '○'),
            (IssueStatus::Todo, "todo", "Todo", '◌'),
            (IssueStatus::InProgress, "in_progress", "In Progress", '◑'),
            (IssueStatus::Done, "done", "Done", '●'),
            (IssueStatus::Cancelled, "cancelled", "Cancelled", '⊘'),
        ];
        for (s, as_str, label, glyph) in cases {
            assert_eq!(s.as_str(), as_str);
            assert_eq!(s.label(), label);
            assert_eq!(s.glyph(), glyph);
        }
    }

    #[test]
    fn priority_string_representations() {
        let cases = [
            (IssuePriority::Urgent, "urgent", "URGENT"),
            (IssuePriority::High, "high", "HIGH"),
            (IssuePriority::Medium, "medium", "MED"),
            (IssuePriority::Low, "low", "LOW"),
            (IssuePriority::None, "none", "—"),
        ];
        for (p, as_str, label) in cases {
            assert_eq!(p.as_str(), as_str);
            assert_eq!(p.label(), label);
        }
    }

    #[test]
    fn defaults_for_drafts_and_patches() {
        let draft = IssueDraft::default();
        assert!(draft.title.is_empty());
        assert!(draft.body.is_none());
        assert_eq!(draft.priority, IssuePriority::Low);
        assert!(draft.project_id.is_none());

        let patch = IssuePatch::default();
        assert!(patch.status.is_none());
        assert!(patch.title.is_none());
        assert!(patch.assignee_me.is_none());
        assert!(patch.priority.is_none());

        let comment = IssueComment::default();
        assert!(comment.author.is_empty());
        assert!(comment.body.is_empty());
        assert_eq!(comment.created_at_ms, 0);

        let filter = IssueFilter::default();
        assert!(!filter.assignee_me);
        assert!(filter.statuses.is_empty());
        assert!(filter.project_id.is_none());
        assert!(filter.query.is_none());
        assert_eq!(filter.limit, 0);
    }

    #[test]
    fn draft_and_patch_can_be_populated() {
        let draft = IssueDraft {
            title: "New issue".into(),
            body: Some("details".into()),
            priority: IssuePriority::Urgent,
            project_id: Some("team-1".into()),
        };
        assert_eq!(draft.priority, IssuePriority::Urgent);
        assert_eq!(draft.project_id.as_deref(), Some("team-1"));

        let patch = IssuePatch {
            status: Some(IssueStatus::Done),
            title: Some("renamed".into()),
            assignee_me: Some(true),
            priority: Some(IssuePriority::Low),
        };
        assert_eq!(patch.status, Some(IssueStatus::Done));
        assert_eq!(patch.assignee_me, Some(true));
    }

    #[test]
    fn issue_branch_seed_prefers_the_hint_then_slugs_the_number() {
        // A provider hint wins verbatim (trimmed).
        assert_eq!(
            issue_branch_seed(Some("  abc-42-fix-foo  "), "ABC-42"),
            "abc-42-fix-foo"
        );
        // No hint (or an empty/whitespace one) ⇒ slug of the number.
        assert_eq!(
            issue_branch_seed(None, "ABC-42"),
            crate::util::slugify("ABC-42")
        );
        assert_eq!(
            issue_branch_seed(Some("   "), "PROJ-7"),
            crate::util::slugify("PROJ-7")
        );
    }

    #[test]
    fn agent_dispatch_status_default_is_queued() {
        assert_eq!(AgentDispatchStatus::default(), AgentDispatchStatus::Queued);
    }

    /// Every variant that a writer can produce (i.e. every one except the
    /// read-only `Unknown` coercion). Kept here so the round-trip and
    /// representation tests stay exhaustive as variants are added.
    const WRITABLE_STATUSES: &[AgentDispatchStatus] = &[
        AgentDispatchStatus::Queued,
        AgentDispatchStatus::Spawning,
        AgentDispatchStatus::Running,
        AgentDispatchStatus::WaitingHuman,
        AgentDispatchStatus::PrOpen,
        AgentDispatchStatus::Merged,
        AgentDispatchStatus::Abandoned,
        AgentDispatchStatus::Done,
        AgentDispatchStatus::Failed,
    ];

    /// Every variant, its wire string and its glyph TOKEN — the token rather
    /// than a resolved literal, so this pins the mapping without baking a
    /// glyph into a source file that can never follow the caps ladder.
    const STATUS_TOKENS: &[(AgentDispatchStatus, &str, crate::termcaps::Glyph)] = {
        use crate::termcaps::Glyph as G;
        &[
            (AgentDispatchStatus::Queued, "queued", G::DiamondHollow),
            (AgentDispatchStatus::Spawning, "spawning", G::Refresh),
            (AgentDispatchStatus::Running, "running", G::DotFilled),
            (
                AgentDispatchStatus::WaitingHuman,
                "waiting_human",
                G::Attention,
            ),
            (AgentDispatchStatus::PrOpen, "pr_open", G::Hex),
            (AgentDispatchStatus::Merged, "merged", G::Check),
            (AgentDispatchStatus::Abandoned, "abandoned", G::Cross),
            (AgentDispatchStatus::Done, "done", G::Check),
            (AgentDispatchStatus::Failed, "failed", G::Cross),
            (AgentDispatchStatus::Unknown, "unknown", G::DotHollow),
        ]
    };

    #[test]
    fn agent_dispatch_status_string_representations() {
        for &(s, as_str, token) in STATUS_TOKENS {
            assert_eq!(s.as_str(), as_str);
            assert_eq!(s.glyph_token(), token, "{s:?}");
        }
    }

    #[test]
    fn glyph_agrees_with_its_token() {
        // `glyph()` is the CLI's shape; it must be nothing more than the token
        // resolved at the top rung, or the board and `dispatch list` drift.
        for &(s, _, _) in STATUS_TOKENS {
            assert_eq!(
                s.glyph(),
                s.glyph_token().resolve(&crate::termcaps::UNICODE)
            );
        }
    }

    #[test]
    fn every_active_status_has_a_distinct_glyph_at_every_rung() {
        // The five states a supervisor scans the board for. Queued, Spawning
        // and Running used to share one glyph, which is the whole point.
        let active: Vec<AgentDispatchStatus> = STATUS_TOKENS
            .iter()
            .map(|&(s, _, _)| s)
            .filter(|s| s.is_active())
            .collect();
        assert_eq!(active.len(), 5);
        for set in [
            &crate::termcaps::UNICODE,
            crate::termcaps::glyphs(crate::termcaps::UnicodeLevel::Ascii),
        ] {
            let mut seen: Vec<&'static str> = Vec::new();
            for &s in &active {
                let g = s.glyph_token().resolve(set);
                assert!(
                    !seen.contains(&g),
                    "{s:?} collides with an earlier active status at this rung"
                );
                seen.push(g);
            }
        }
    }

    #[test]
    fn agent_dispatch_status_roundtrip() {
        for &s in WRITABLE_STATUSES {
            let json = serde_json::to_string(&s).unwrap();
            let back: AgentDispatchStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back, "roundtrip failed for {:?}", s);
        }
    }

    #[test]
    fn agent_dispatch_status_parse_round_trips_every_writable_status() {
        // The pane-exit fix's core contract: what a writer stores parses back
        // to exactly what it wrote — including the two terminals (`done` /
        // `failed`) the roster used to corrupt on.
        for &s in WRITABLE_STATUSES {
            assert_eq!(AgentDispatchStatus::parse(s.as_str()), s);
        }
    }

    #[test]
    fn agent_dispatch_status_parse_is_total_and_tolerates_legacy_and_junk() {
        // Whitespace-padded and mixed junk never panic and never error.
        assert_eq!(
            AgentDispatchStatus::parse("  done  "),
            AgentDispatchStatus::Done
        );
        assert_eq!(
            AgentDispatchStatus::parse("failed"),
            AgentDispatchStatus::Failed
        );
        // Anything outside the closed set coerces to Unknown, so a legacy or
        // corrupt row is listable rather than a read failure.
        for junk in ["", "in_flight", "DONE", "weird-legacy-value", "42"] {
            assert_eq!(
                AgentDispatchStatus::parse(junk),
                AgentDispatchStatus::Unknown,
                "{junk:?} should coerce to Unknown"
            );
        }
    }

    #[test]
    fn agent_dispatch_status_terminal_and_active_partition_the_writable_set() {
        // A resuming supervisor asks exactly these two questions; every
        // writable status answers one of them, and never both.
        for &s in WRITABLE_STATUSES {
            assert_ne!(
                s.is_terminal(),
                s.is_active(),
                "{s:?} must be either terminal or active, not both/neither"
            );
        }
        assert!(AgentDispatchStatus::Running.is_active());
        assert!(AgentDispatchStatus::Done.is_terminal());
        assert!(AgentDispatchStatus::Failed.is_terminal());
        assert!(AgentDispatchStatus::Abandoned.is_terminal());
        // Unknown is neither writable-active nor terminal — a read artifact.
        assert!(!AgentDispatchStatus::Unknown.is_active());
        assert!(!AgentDispatchStatus::Unknown.is_terminal());
    }

    #[test]
    fn agent_dispatch_roundtrip() {
        let orig = AgentDispatch {
            id: 7,
            issue_id: "linear:ABC-1".into(),
            worktree_path: "/tmp/wt".into(),
            agent_name: "claude".into(),
            dispatched_at_ms: 1_700_000_000_000,
            status: AgentDispatchStatus::Running,
            stage: Some("code".into()),
            parent_id: Some(3),
            session_id: Some("sess-1".into()),
            artifact_path: Some(".thegn/pipeline/architect/3.md".into()),
            note: Some("transport: connection error. (attempt 1/3)".into()),
        };
        let json = serde_json::to_string(&orig).unwrap();
        let back: AgentDispatch = serde_json::from_str(&json).unwrap();
        assert_eq!(orig, back);

        // A pre-v56 payload (no pipeline keys at all) still deserializes — the
        // fields are `#[serde(default)]`, so an older client's JSON is valid.
        let legacy = r#"{"id":7,"issue_id":"linear:ABC-1","worktree_path":"/tmp/wt",
            "agent_name":"claude","dispatched_at_ms":1700000000000,"status":"running"}"#;
        let back: AgentDispatch = serde_json::from_str(legacy).unwrap();
        assert_eq!(back.stage, None);
        assert_eq!(back.parent_id, None);
        assert_eq!(back.session_id, None);
        assert_eq!(back.artifact_path, None);
        assert_eq!(back.note, None);
    }

    #[test]
    fn new_dispatch_defaults_the_pipeline_columns_to_none() {
        let n = NewDispatch::new("linear:A-1", "/wt/a", "claude");
        assert_eq!(n.issue_id, "linear:A-1");
        assert_eq!(n.worktree_path, "/wt/a");
        assert_eq!(n.agent_name, "claude");
        assert_eq!(n.stage, None);
        assert_eq!(n.parent_id, None);
        assert_eq!(n.session_id, None);
        assert_eq!(n.artifact_path, None);
        // Struct-update keeps the constructor usable for a pipeline row.
        let chunk = NewDispatch {
            stage: Some("code"),
            parent_id: Some(1),
            ..n
        };
        assert_eq!(chunk.agent_name, "claude");
        assert_eq!(chunk.stage, Some("code"));
    }

    /// Every variant, including the read-only `Unknown` coercion — the glyph
    /// vocabulary must be total, because it renders whatever the roster holds.
    const ALL_STATUSES: &[AgentDispatchStatus] = &[
        AgentDispatchStatus::Queued,
        AgentDispatchStatus::Spawning,
        AgentDispatchStatus::Running,
        AgentDispatchStatus::WaitingHuman,
        AgentDispatchStatus::PrOpen,
        AgentDispatchStatus::Merged,
        AgentDispatchStatus::Abandoned,
        AgentDispatchStatus::Done,
        AgentDispatchStatus::Failed,
        AgentDispatchStatus::Unknown,
    ];

    #[test]
    fn normalize_dispatch_ms_scales_seconds_and_leaves_milliseconds_alone() {
        // A legacy SECONDS stamp (2023-11-14) scales to the same instant in ms.
        assert_eq!(normalize_dispatch_ms(1_700_000_000), 1_700_000_000_000);
        // An already-millisecond stamp is untouched.
        assert_eq!(normalize_dispatch_ms(1_700_000_000_000), 1_700_000_000_000);
        // Unstamped / nonsense values pass through: an unstamped row must read
        // as unstamped, never as 1970 multiplied by a thousand.
        assert_eq!(normalize_dispatch_ms(0), 0);
        assert_eq!(normalize_dispatch_ms(-1), -1);
        assert_eq!(normalize_dispatch_ms(i64::MIN), i64::MIN);
    }

    #[test]
    fn normalize_dispatch_ms_boundary_is_exclusive_below_the_floor() {
        // The floor itself is already milliseconds (1973-03-03); one below it
        // is still unambiguously seconds.
        assert_eq!(normalize_dispatch_ms(MS_EPOCH_FLOOR), MS_EPOCH_FLOOR);
        assert_eq!(
            normalize_dispatch_ms(MS_EPOCH_FLOOR - 1),
            (MS_EPOCH_FLOOR - 1) * 1_000
        );
    }

    #[test]
    fn normalize_dispatch_ms_is_idempotent() {
        // Idempotence is what makes the read-side guard safe to apply on top of
        // the v58 migration: a row the migration already scaled must not scale
        // a second time.
        for v in [
            0,
            -1,
            i64::MIN,
            i64::MAX,
            1_700_000_000,
            1_700_000_000_000,
            MS_EPOCH_FLOOR,
            MS_EPOCH_FLOOR - 1,
        ] {
            let once = normalize_dispatch_ms(v);
            assert_eq!(
                normalize_dispatch_ms(once),
                once,
                "normalize_dispatch_ms not idempotent for {v}"
            );
        }
    }

    #[test]
    fn dispatch_status_glyph_set_agrees_with_glyph_token_at_every_rung() {
        // The two vocabularies were born on different branches: `glyph_token`
        // (the CLI's `glyph()`) and `glyph_set` (the chrome surfaces) each map
        // the statuses to tokens. If they ever disagree, the CLI and the board
        // tell different stories about the SAME row — the exact drift both
        // exist to prevent. (They disagreed on `Unknown` once: DotHollow vs
        // DiamondHollow.) Pin every variant, both rungs.
        for &s in ALL_STATUSES {
            for gl in [&crate::termcaps::UNICODE, &crate::termcaps::ASCII] {
                let (set_glyph, _) = s.glyph_set(gl);
                let token_glyph = s.glyph_token().resolve(gl);
                assert_eq!(
                    set_glyph, token_glyph,
                    "{s:?}: glyph_set and glyph_token disagree"
                );
            }
        }
    }

    #[test]
    fn dispatch_status_glyph_set_is_total_across_the_caps_ladder() {
        for &s in ALL_STATUSES {
            for gl in [&crate::termcaps::UNICODE, &crate::termcaps::ASCII] {
                let (g, _hue) = s.glyph_set(gl);
                assert!(!g.is_empty(), "{s:?} resolves to an empty glyph");
            }
            // The ASCII rung must be 7-bit — it is what a terminal with no
            // Unicode gets, so a stray multibyte glyph here is a mojibake bug.
            let (ascii, _) = s.glyph_set(&crate::termcaps::ASCII);
            assert!(
                ascii.is_ascii(),
                "{s:?} ASCII glyph is not 7-bit: {ascii:?}"
            );
        }
    }

    #[test]
    fn dispatch_status_glyph_set_separates_queued_spawning_running() {
        // The collision this exists to fix: `glyph()` renders all three as ⚙,
        // so a whole pipeline looked identically busy whatever it was doing.
        use AgentDispatchStatus as S;
        let gl = &crate::termcaps::UNICODE;
        let (q, _) = S::Queued.glyph_set(gl);
        let (sp, _) = S::Spawning.glyph_set(gl);
        let (r, _) = S::Running.glyph_set(gl);
        assert_ne!(q, sp);
        assert_ne!(sp, r);
        assert_ne!(q, r);
    }
}
