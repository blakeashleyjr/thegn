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
    /// One or more ERROR lines were detected in the thegn log.
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
    /// A queued pull request merged (by thegn or by the forge's auto-merge).
    PrQueueMerged,
    /// A queued pull request is green and mergeable, awaiting a manual merge.
    PrQueueReady,
    /// The PR queue could not unblock a pull request — human intervention needed.
    PrQueueNeedsHuman,
    /// A watched PR produced a new or revised per-thread review task.
    PrReviewTaskQueued,
    /// A watched PR review thread was successfully resolved.
    PrReviewThreadResolved,
    /// A calendar event is about to start.
    CalendarReminder,
    /// A calendar event moved or was cancelled since the last sync.
    CalendarChanged,
    /// A background fetch found new upstream commits on a worktree's branch —
    /// the branch is now behind its remote and can be pulled.
    UpstreamBehind,
    /// A system metric crossed a configured threshold (`[stats.alerts]`).
    ResourceAlert,
    /// An AI account is approaching (or past) a rate-limit threshold
    /// (`[usage.alerts]`).
    UsageLimit,
    /// A trusted automation action completed or emitted an informational result.
    Automation,
    /// A trusted automation action failed or timed out.
    AutomationFailed,
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
    /// The config spelling (`"info"` / `"notice"` / `"alert"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Notice => "notice",
            Self::Alert => "alert",
        }
    }

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

    /// The accent hue for this priority — the single source that colors the
    /// transient toast projection of a routed notification. `None` is the
    /// neutral (default text) color: an `Info` toast (a plain acknowledgement
    /// like "copied") must not pull a hue. This mirrors the flag/count colors:
    /// `Alert` is the red ⚑ urgency, `Notice` the neutral-blue unread accent.
    pub fn hue(self) -> Option<crate::theme::Hue> {
        match self {
            Self::Info => None,
            Self::Notice => Some(crate::theme::Hue::Blue),
            Self::Alert => Some(crate::theme::Hue::Red),
        }
    }
}

impl NotificationKind {
    /// Every notification kind, for exhaustive iteration (config classification,
    /// SQL `IN` set construction, tests). Kept in sync with the enum by the
    /// `notification_kind_*` tests, which loop over this.
    pub const ALL: [NotificationKind; 30] = [
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
        Self::PrQueueMerged,
        Self::PrQueueReady,
        Self::PrQueueNeedsHuman,
        Self::PrReviewTaskQueued,
        Self::PrReviewThreadResolved,
        Self::CalendarReminder,
        Self::CalendarChanged,
        Self::UpstreamBehind,
        Self::ResourceAlert,
        Self::UsageLimit,
        Self::Automation,
        Self::AutomationFailed,
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
            Self::PrQueueMerged => "pr_queue_merged",
            Self::PrQueueReady => "pr_queue_ready",
            Self::PrQueueNeedsHuman => "pr_queue_needs_human",
            Self::PrReviewTaskQueued => "pr_review_task_queued",
            Self::PrReviewThreadResolved => "pr_review_thread_resolved",
            Self::CalendarReminder => "calendar_reminder",
            Self::CalendarChanged => "calendar_changed",
            Self::UpstreamBehind => "upstream_behind",
            Self::ResourceAlert => "resource_alert",
            Self::UsageLimit => "usage_limit",
            Self::Automation => "automation",
            Self::AutomationFailed => "automation_failed",
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
            | Self::ProcessFailed
            | Self::QueueNeedsHuman
            | Self::PrQueueNeedsHuman
            // A sustained threshold breach is worth the red flag: the whole
            // point of the sustain/hysteresis machinery is that reaching here
            // already means it is real and ongoing.
            | Self::ResourceAlert
            // Same reasoning: the sustain/hysteresis machinery means an
            // exhausted quota reaching here is real, ongoing, and about to
            // stop the user working.
            | Self::UsageLimit => Priority::Alert,
            Self::AutomationFailed => Priority::Alert,
            // LogError is thegn's own diagnostics — informational, never a red
            // alert (and off by default, see `surface_self_log_errors`). It shows
            // in the Logs group as a quiet entry point, not the Alerts group.
            Self::LogError
            | Self::WorktreeCreated
            | Self::ProcessExited
            | Self::QueueLanded
            // A meeting that moved is worth recording, but it is not something
            // to interrupt the user over — only the reminder is.
            | Self::CalendarChanged
            | Self::PrQueueMerged
            | Self::PrReviewThreadResolved => Priority::Info,
            Self::Assigned
            | Self::Mentioned
            | Self::StatusChanged
            | Self::BlockerResolved
            | Self::PrLinked
            | Self::Overdue
            | Self::PrStateChanged
            | Self::AgentDone
            | Self::QueueReady
            | Self::PrQueueReady
            | Self::PrReviewTaskQueued
            // Time-critical, but not a failure: `Notice` reaches the desktop at
            // the default threshold without claiming the red Alert group.
            | Self::CalendarReminder
            | Self::UpstreamBehind => Priority::Notice,
            Self::Automation => Priority::Notice,
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
            Self::ResourceAlert => "▲",
            Self::UsageLimit => "▲",
            Self::Automation => "◆",
            Self::AutomationFailed => "✗",
            Self::AgentDone => "◉",
            Self::AgentFailed => "◎",
            Self::AgentAttention => "⚠",
            Self::TestFailed => "✗",
            Self::WorktreeCreated => "+",
            Self::LogError => "✗",
            Self::ProcessExited => "◇",
            Self::ProcessFailed => "✗",
            Self::QueueLanded => "✓",
            Self::PrQueueMerged => "✓",
            Self::PrQueueReady => "⑂",
            Self::PrQueueNeedsHuman => "✋",
            Self::PrReviewTaskQueued => "③",
            Self::PrReviewThreadResolved => "✓",
            Self::CalendarReminder => "◷",
            Self::CalendarChanged => "◷",
            Self::QueueReady => "◆",
            Self::QueueNeedsHuman => "✋",
            Self::UpstreamBehind => "↓",
        }
    }

    /// The one caps-aware hued-glyph vocabulary for notifications — the single
    /// source every surface renders through (the unified inbox overlay, the
    /// panel inbox section, and the transient toast). Mirrors
    /// [`crate::attention::MqStatus::glyph`]: glyphs come from the caller's
    /// capability-resolved [`GlyphSet`](crate::termcaps::GlyphSet) where a slot
    /// exists (so failure/queue markers degrade to ASCII with the rest of the
    /// chrome), and the hue is per-kind so the popup and panel can never diverge.
    /// Kinds without a dedicated `GlyphSet` slot keep their fixed BMP glyph
    /// (identical to [`Self::glyph`]).
    pub fn hued_glyph(self, gl: &crate::termcaps::GlyphSet) -> (&'static str, crate::theme::Hue) {
        use crate::theme::Hue;
        match self {
            Self::Assigned => ("→", Hue::Blue),
            Self::Mentioned => ("@", Hue::Blue),
            Self::StatusChanged => ("⟳", Hue::Amber),
            Self::BlockerResolved => (gl.check, Hue::Green),
            Self::PrLinked => ("⎇", Hue::Amber),
            Self::Overdue => ("!", Hue::Red),
            Self::PrStateChanged => ("⑂", Hue::Amber),
            Self::AgentDone => ("◉", Hue::Green),
            Self::AgentFailed => (gl.cross, Hue::Red),
            Self::AgentAttention => (gl.warn, Hue::Amber),
            Self::TestFailed => (gl.cross, Hue::Red),
            Self::WorktreeCreated => ("+", Hue::Green),
            Self::LogError => (gl.cross, Hue::Red),
            Self::ProcessExited => (gl.diamond_hollow, Hue::Green),
            Self::ProcessFailed => (gl.cross, Hue::Red),
            Self::QueueLanded => (gl.check, Hue::Green),
            Self::QueueReady => (gl.diamond_filled, Hue::Green),
            Self::QueueNeedsHuman => (gl.attention, Hue::Red),
            Self::PrQueueMerged => (gl.check, Hue::Green),
            Self::PrQueueReady => ("⑂", Hue::Blue),
            Self::PrQueueNeedsHuman => (gl.attention, Hue::Red),
            Self::PrReviewTaskQueued => (gl.diamond_hollow, Hue::Blue),
            Self::PrReviewThreadResolved => (gl.check, Hue::Green),
            // A reminder is time-critical but not a failure: amber, not red.
            Self::CalendarReminder => (gl.dot_filled, Hue::Amber),
            Self::CalendarChanged => (gl.dot_hollow, Hue::Blue),
            Self::UpstreamBehind => (gl.arrow_down, Hue::Blue),
            Self::ResourceAlert => (gl.warn, Hue::Amber),
            Self::UsageLimit => (gl.warn, Hue::Amber),
            Self::Automation => (gl.diamond_filled, Hue::Blue),
            Self::AutomationFailed => (gl.cross, Hue::Red),
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
            Self::PrQueueMerged => "pull request merged",
            Self::PrQueueReady => "pull request ready to merge",
            Self::PrQueueNeedsHuman => "pr queue needs you",
            Self::PrReviewTaskQueued => "review task queued",
            Self::PrReviewThreadResolved => "review thread resolved",
            Self::CalendarReminder => "event starting soon",
            Self::CalendarChanged => "event changed",
            Self::UpstreamBehind => "upstream updates",
            Self::ResourceAlert => "resource alert",
            Self::UsageLimit => "ai usage limit",
            Self::Automation => "automation",
            Self::AutomationFailed => "automation failed",
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
        assert_eq!(seen.len(), 28, "ALL is missing kinds");
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
                    | NotificationKind::ProcessFailed
                    | NotificationKind::QueueNeedsHuman
                    | NotificationKind::PrQueueNeedsHuman
                    // A threshold breach only reaches here after sustain +
                    // hysteresis, so it is real and ongoing by construction.
                    | NotificationKind::ResourceAlert
                    | NotificationKind::UsageLimit
                    | NotificationKind::AutomationFailed
            );
            let expect_info = matches!(
                kind,
                NotificationKind::LogError
                    | NotificationKind::WorktreeCreated
                    | NotificationKind::ProcessExited
                    | NotificationKind::QueueLanded
                    // A merged PR is a completed lifecycle event, not something
                    // that needs you — Info, like a landed merge-queue branch.
                    | NotificationKind::PrQueueMerged
                    | NotificationKind::PrReviewThreadResolved
                    // A meeting that moved is a record, not an interruption;
                    // only the reminder itself is worth surfacing.
                    | NotificationKind::CalendarChanged
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
    fn hued_glyph_is_total_caps_aware_and_nonempty() {
        use crate::termcaps::{ASCII, UNICODE};
        for kind in NotificationKind::ALL {
            let (gu, _) = kind.hued_glyph(&UNICODE);
            let (ga, _) = kind.hued_glyph(&ASCII);
            assert!(!gu.is_empty(), "{kind:?} unicode glyph empty");
            assert!(!ga.is_empty(), "{kind:?} ascii glyph empty");
        }
        // Failure/queue kinds degrade with the chrome (they route through a
        // GlyphSet slot), so the ASCII glyph differs from the Unicode one.
        let (u, _) = NotificationKind::TestFailed.hued_glyph(&UNICODE);
        let (a, _) = NotificationKind::TestFailed.hued_glyph(&ASCII);
        assert_eq!(u, UNICODE.cross);
        assert_eq!(a, ASCII.cross);
    }

    #[test]
    fn hued_glyph_failures_are_red() {
        use crate::termcaps::UNICODE;
        use crate::theme::Hue;
        // The hard-failure kinds all render the red cross; AgentAttention is a
        // deliberate amber ⚠ (attention, not failure), so it is excluded.
        for kind in [
            NotificationKind::AgentFailed,
            NotificationKind::TestFailed,
            NotificationKind::ProcessFailed,
            NotificationKind::LogError,
            NotificationKind::AutomationFailed,
        ] {
            let (glyph, hue) = kind.hued_glyph(&UNICODE);
            assert_eq!(glyph, UNICODE.cross, "{kind:?}");
            assert_eq!(hue, Hue::Red, "{kind:?}");
        }
    }

    #[test]
    fn priority_hue_neutral_info_colored_rest() {
        use crate::theme::Hue;
        assert_eq!(Priority::Info.hue(), None);
        assert_eq!(Priority::Notice.hue(), Some(Hue::Blue));
        assert_eq!(Priority::Alert.hue(), Some(Hue::Red));
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

    /// `config.toml.example`'s `[notifications.priority]` prose is the user's
    /// only list of kinds; it must name every one (the enum is the single
    /// source — `thegn notify push --help` is generated from it).
    #[test]
    fn example_config_prose_names_every_kind() {
        let example = include_str!("../../../config/config.toml.example");
        let start = example
            .find("# [notifications.priority]")
            .expect("[notifications.priority] prose block");
        // The prose immediately precedes the commented table header; scan a
        // window of the surrounding section.
        let window = &example[start.saturating_sub(2500)..(start + 1500).min(example.len())];
        let missing: Vec<&str> = NotificationKind::ALL
            .iter()
            .map(|k| k.as_str())
            .filter(|k| !window.contains(k))
            .collect();
        assert!(
            missing.is_empty(),
            "config.toml.example [notifications.priority] prose does not mention: {missing:?}"
        );
    }

    #[test]
    fn priority_as_str_round_trips() {
        for p in [Priority::Info, Priority::Notice, Priority::Alert] {
            assert_eq!(Priority::parse(p.as_str()), Some(p));
        }
    }
}

pub const MAX_REVIEW_NOTIFICATION_CHARS: usize = 1024;

/// Bounded audit text for a newly queued or revised review task.
pub fn review_task_queued_message(
    event: &crate::pr_review_tasks::ReviewTaskEvent,
    revised: bool,
) -> String {
    let action = if revised { "revised" } else { "queued" };
    let location = match event.line {
        Some(line) if !event.path.is_empty() => format!("{}:{line}", event.path),
        _ if event.path.is_empty() => "unanchored".to_string(),
        _ => event.path.clone(),
    };
    bounded_notification(&format!(
        "PR #{} review task {action}: {} [{} @ {}; rev {}]",
        event.pr_number, location, event.thread_id, event.head_oid, event.source_revision
    ))
}

/// Bounded audit text for a successfully resolved review thread.
pub fn review_thread_resolved_message(
    transition: &crate::pr_review_tasks::ReviewTaskResolution,
) -> String {
    let location = match transition.line {
        Some(line) if !transition.path.is_empty() => format!("{}:{line}", transition.path),
        _ if transition.path.is_empty() => "unanchored".to_string(),
        _ => transition.path.clone(),
    };
    bounded_notification(&format!(
        "PR #{} review thread resolved: {} [{} @ {}; rev {}]",
        transition.pr_number,
        location,
        transition.thread_id,
        transition.head_oid,
        transition.source_revision
    ))
}

fn bounded_notification(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_REVIEW_NOTIFICATION_CHARS)
        .collect()
}
