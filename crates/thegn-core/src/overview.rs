//! The Lead overview (`overview.get`, THE-2171): the pure assembly of one
//! cross-workspace snapshot of worktrees, agents, queues and attention.
//!
//! The host gathers plain rows from the DB and caches ([`OverviewInputs`]);
//! [`assemble`] scores, filters, orders and bounds them. Nothing here touches
//! the DB, git or a clock: `now` is a parameter.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::approval_request::PendingApproval;
use crate::attention::{self, AttentionInputs, AttentionScore};

/// Default and maximum per-section row bound.
pub const DEFAULT_LIMIT: usize = 50;
/// Hard ceiling on `limit`.
pub const MAX_LIMIT: usize = 500;
/// Longest free-text field in a reply, in characters.
pub const MAX_STRING_CHARS: usize = 200;

/// The `overview.get` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OverviewRequest {
    /// Restrict to one workspace (its repo root path).
    pub workspace: Option<String>,
    /// Rows per section; default 50, max 500.
    pub limit: Option<usize>,
}

impl OverviewRequest {
    /// The effective per-section bound.
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }
}

/// One worktree with its resolved attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeView {
    pub worktree: String,
    pub workspace: String,
    pub branch: Option<String>,
    /// [`attention::AttentionTier`] lowercase.
    pub tier: &'static str,
    pub reason: Option<String>,
    pub since: Option<i64>,
    pub unread: usize,
}

/// One active roster (dispatch) row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RosterView {
    pub issue_id: String,
    pub worktree: String,
    pub agent: String,
    pub status: String,
    /// Dispatch time, epoch ms.
    pub dispatched_at_ms: i64,
}

/// One merge-queue or PR-queue entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueView {
    /// The queue row's identity: worktree path (merge queue) or PR key.
    pub id: String,
    pub worktree: Option<String>,
    pub branch: String,
    pub status: String,
    pub detail: Option<String>,
    pub updated_at: i64,
}

/// One failing CI run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CiView {
    pub worktree: String,
    pub branch: String,
    pub run: String,
    pub url: Option<String>,
}

/// Unread notification totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NotificationCounts {
    pub unread: usize,
    pub by_kind: BTreeMap<String, usize>,
}

/// One event of today's calendar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CalendarView {
    pub title: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
}

/// The `overview.get` reply.
#[derive(Debug, Clone, Serialize)]
pub struct OverviewReply {
    pub generated_at: i64,
    pub worktrees: Vec<WorktreeView>,
    /// Active dispatches grouped by pipeline stage.
    pub roster: BTreeMap<String, Vec<RosterView>>,
    pub merge_queue: Vec<QueueView>,
    pub pr_queue: Vec<QueueView>,
    pub ci_failing: Vec<CiView>,
    pub approvals_pending: Vec<PendingApproval>,
    pub notifications: NotificationCounts,
    pub calendar_today: Vec<CalendarView>,
    /// `"section: reason"` for every source that failed or was approximate.
    pub partial: Vec<String>,
}

/// One registered worktree plus the facts the host could derive for scoring.
#[derive(Debug, Clone, Default)]
pub struct WorktreeInput {
    pub worktree: String,
    pub workspace: String,
    pub branch: Option<String>,
    /// Count of unread notifications scoped to this worktree.
    pub unread: usize,
    pub attention: AttentionInputs,
}

/// A roster row with its stage (`None` = outside any pipeline).
#[derive(Debug, Clone)]
pub struct RosterInput {
    pub stage: Option<String>,
    pub view: RosterView,
}

/// Everything the host gathered. A source that failed leaves its vector empty
/// and adds a `"section: reason"` string to `errors`.
#[derive(Debug, Clone, Default)]
pub struct OverviewInputs {
    pub worktrees: Vec<WorktreeInput>,
    pub roster: Vec<RosterInput>,
    pub merge_queue: Vec<QueueView>,
    pub pr_queue: Vec<QueueView>,
    pub ci_failing: Vec<CiView>,
    pub approvals_pending: Vec<PendingApproval>,
    pub notifications: NotificationCounts,
    pub calendar_today: Vec<CalendarView>,
    /// Source failures / approximations, copied into `partial`.
    pub errors: Vec<String>,
}

/// Clip `s` to `max` characters on a char boundary.
pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_owned(),
        None => s.to_owned(),
    }
}

fn clip_opt(s: Option<String>) -> Option<String> {
    s.map(|v| clip(&v, MAX_STRING_CHARS))
}

fn tier_name(tier: attention::AttentionTier) -> &'static str {
    use attention::AttentionTier as T;
    match tier {
        T::Blocked => "blocked",
        T::Failure => "failure",
        T::Waiting => "waiting",
        T::Ready => "ready",
        T::Working => "working",
        T::Idle => "idle",
    }
}

fn clip_queue(mut q: QueueView) -> QueueView {
    q.detail = clip_opt(q.detail);
    q.branch = clip(&q.branch, MAX_STRING_CHARS);
    q
}

/// Score, filter, order and bound `inputs` into the reply.
pub fn assemble(inputs: OverviewInputs, req: &OverviewRequest, now: i64) -> OverviewReply {
    let limit = req.effective_limit();
    let ws = req.workspace.as_deref();

    let mut scored: Vec<(WorktreeInput, AttentionScore)> = inputs
        .worktrees
        .into_iter()
        .filter(|w| ws.is_none_or(|ws| w.workspace == ws))
        .map(|w| {
            let s = attention::score(&w.attention);
            (w, s)
        })
        .collect();
    scored.sort_by(|a, b| {
        a.1.sort_key()
            .cmp(&b.1.sort_key())
            .then_with(|| a.0.worktree.cmp(&b.0.worktree))
    });
    let fresh: Vec<(String, AttentionScore)> = scored
        .iter()
        .map(|(w, s)| (w.worktree.clone(), *s))
        .collect();
    let order = attention::stable_order(&[], &fresh);
    let kept: BTreeSet<String> = order.iter().cloned().collect();

    let worktrees: Vec<WorktreeView> = scored
        .into_iter()
        .take(limit)
        .map(|(w, s)| WorktreeView {
            worktree: w.worktree,
            workspace: w.workspace,
            branch: clip_opt(w.branch),
            tier: tier_name(s.tier),
            reason: (s.tier != attention::AttentionTier::Idle)
                .then(|| clip(s.reason.label(), MAX_STRING_CHARS)),
            since: s.since,
            unread: w.unread,
        })
        .collect();

    // With a workspace filter, dependent sections keep only that workspace's
    // worktrees; without one everything passes.
    let in_scope = |wt: &str| ws.is_none() || kept.contains(wt);

    let mut roster: BTreeMap<String, Vec<RosterView>> = BTreeMap::new();
    for r in inputs
        .roster
        .into_iter()
        .filter(|r| in_scope(&r.view.worktree))
    {
        let rows = roster
            .entry(r.stage.unwrap_or_else(|| "unstaged".into()))
            .or_default();
        if rows.len() < limit {
            rows.push(r.view);
        }
    }

    let queue = |rows: Vec<QueueView>| -> Vec<QueueView> {
        rows.into_iter()
            .filter(|q| q.worktree.as_deref().is_none_or(in_scope))
            .take(limit)
            .map(clip_queue)
            .collect()
    };
    let merge_queue = queue(inputs.merge_queue);
    let pr_queue = queue(inputs.pr_queue);

    let ci_failing: Vec<CiView> = inputs
        .ci_failing
        .into_iter()
        .filter(|c| in_scope(&c.worktree))
        .take(limit)
        .map(|mut c| {
            c.run = clip(&c.run, MAX_STRING_CHARS);
            c.branch = clip(&c.branch, MAX_STRING_CHARS);
            c
        })
        .collect();

    let approvals_pending: Vec<PendingApproval> = inputs
        .approvals_pending
        .into_iter()
        .filter(|a| ws.is_none() || kept.contains(&a.worktree))
        .take(limit)
        .map(|mut a| {
            a.message = clip(&a.message, MAX_STRING_CHARS);
            a
        })
        .collect();

    let calendar_today: Vec<CalendarView> = inputs
        .calendar_today
        .into_iter()
        .take(limit)
        .map(|mut e| {
            e.title = clip(&e.title, MAX_STRING_CHARS);
            e
        })
        .collect();

    let partial: Vec<String> = inputs
        .errors
        .into_iter()
        .map(|e| clip(&e, MAX_STRING_CHARS))
        .collect();

    OverviewReply {
        generated_at: now,
        worktrees,
        roster,
        merge_queue,
        pr_queue,
        ci_failing,
        approvals_pending,
        notifications: inputs.notifications,
        calendar_today,
        partial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::UnreadNote;
    use crate::notification::NotificationKind;

    fn wt(path: &str, workspace: &str) -> WorktreeInput {
        WorktreeInput {
            worktree: path.into(),
            workspace: workspace.into(),
            ..Default::default()
        }
    }

    fn blocked(path: &str, workspace: &str) -> WorktreeInput {
        let mut w = wt(path, workspace);
        w.attention.unread = vec![UnreadNote {
            kind: NotificationKind::AgentAttention,
            at: 10,
        }];
        w.unread = 1;
        w
    }

    #[test]
    fn worktrees_order_by_tier_most_urgent_first() {
        let inputs = OverviewInputs {
            worktrees: vec![wt("/a", "/r"), blocked("/b", "/r")],
            ..Default::default()
        };
        let reply = assemble(inputs, &OverviewRequest::default(), 5);
        assert_eq!(reply.generated_at, 5);
        assert_eq!(reply.worktrees[0].worktree, "/b");
        assert_eq!(reply.worktrees[0].tier, "blocked");
        assert_eq!(reply.worktrees[0].unread, 1);
        assert!(reply.worktrees[0].reason.is_some());
        assert_eq!(reply.worktrees[1].tier, "idle");
        assert!(reply.worktrees[1].reason.is_none());
    }

    #[test]
    fn workspace_filter_scopes_worktrees_and_dependents() {
        let row = |wt: &str| RosterInput {
            stage: Some("code".into()),
            view: RosterView {
                issue_id: "x".into(),
                worktree: wt.into(),
                agent: "a".into(),
                status: "running".into(),
                dispatched_at_ms: 1,
            },
        };
        let inputs = OverviewInputs {
            worktrees: vec![wt("/a", "/r1"), wt("/b", "/r2")],
            roster: vec![row("/a"), row("/b")],
            ..Default::default()
        };
        let req = OverviewRequest {
            workspace: Some("/r2".into()),
            limit: None,
        };
        let reply = assemble(inputs, &req, 0);
        assert_eq!(reply.worktrees.len(), 1);
        assert_eq!(reply.worktrees[0].worktree, "/b");
        assert_eq!(reply.roster["code"].len(), 1);
        assert_eq!(reply.roster["code"][0].worktree, "/b");
    }

    #[test]
    fn limit_bounds_every_section_and_is_clamped() {
        let inputs = OverviewInputs {
            worktrees: (0..5).map(|i| wt(&format!("/w{i}"), "/r")).collect(),
            merge_queue: (0..5)
                .map(|i| QueueView {
                    id: format!("/w{i}"),
                    worktree: Some(format!("/w{i}")),
                    branch: "b".into(),
                    status: "queued".into(),
                    detail: None,
                    updated_at: 0,
                })
                .collect(),
            ..Default::default()
        };
        let req = OverviewRequest {
            workspace: None,
            limit: Some(2),
        };
        let reply = assemble(inputs, &req, 0);
        assert_eq!(reply.worktrees.len(), 2);
        assert_eq!(reply.merge_queue.len(), 2);
        let big = OverviewRequest {
            workspace: None,
            limit: Some(10_000),
        };
        assert_eq!(big.effective_limit(), MAX_LIMIT);
        let zero = OverviewRequest {
            workspace: None,
            limit: Some(0),
        };
        assert_eq!(zero.effective_limit(), 1);
        assert_eq!(OverviewRequest::default().effective_limit(), DEFAULT_LIMIT);
    }

    #[test]
    fn errors_land_in_partial_without_failing() {
        let inputs = OverviewInputs {
            errors: vec!["merge_queue: boom".into()],
            ..Default::default()
        };
        let reply = assemble(inputs, &OverviewRequest::default(), 0);
        assert_eq!(reply.partial, vec!["merge_queue: boom".to_string()]);
        assert!(reply.worktrees.is_empty());
    }

    #[test]
    fn long_strings_truncate_on_a_char_boundary() {
        let long = "é".repeat(MAX_STRING_CHARS + 50);
        let clipped = clip(&long, MAX_STRING_CHARS);
        assert_eq!(clipped.chars().count(), MAX_STRING_CHARS);
        assert_eq!(clip("short", MAX_STRING_CHARS), "short");
        let inputs = OverviewInputs {
            pr_queue: vec![QueueView {
                id: "k".into(),
                worktree: None,
                branch: "b".into(),
                status: "s".into(),
                detail: Some(long.clone()),
                updated_at: 0,
            }],
            approvals_pending: vec![PendingApproval {
                issue_id: "i".into(),
                stage: "s".into(),
                commit_sha: "c".into(),
                worktree: String::new(),
                requested_at: 0,
                notification_id: 1,
                message: long,
            }],
            ..Default::default()
        };
        let reply = assemble(inputs, &OverviewRequest::default(), 0);
        assert_eq!(
            reply.pr_queue[0].detail.as_ref().unwrap().chars().count(),
            MAX_STRING_CHARS
        );
        assert_eq!(
            reply.approvals_pending[0].message.chars().count(),
            MAX_STRING_CHARS
        );
    }
}
