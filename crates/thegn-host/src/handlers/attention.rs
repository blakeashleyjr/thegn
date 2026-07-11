//! Jump-to-next-attention (`Alt a` / `attention-next`): focus the most urgent
//! worktree that needs the user, wrapping through the needs-you set on repeat.
//! Works in any sidebar sort mode — the set and order come from the hydrated
//! attention scores, not the display tree. Extracted from `run.rs` (pinned by
//! the file-size ratchet).

use termwiz::terminal::TerminalWaker;
use thegn_core::attention::{self, AttentionScore};
use thegn_core::store::NotificationStore;
use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::FrameModel;
use crate::hydrate::RefreshKind;

/// The focused worktree's path, if any — the sidebar row currently marked
/// active. The needs-you set excludes it: you can't need to *go attend to* the
/// worktree you are already in (and Enter="focus" on it would be a no-op).
pub(crate) fn active_worktree_path(model: &FrameModel) -> Option<&str> {
    model
        .sidebar_rows
        .iter()
        .find(|r| r.active && r.kind == crate::sidebar::RowKind::Worktree)
        .and_then(|r| r.worktree_path.as_deref())
}

/// The worktrees currently needing the user (tiers T0–T2), most urgent first
/// — the hysteresis-stable hydration order, so the jump ring matches what the
/// Attention sort displays.
pub(crate) fn needs_user_ordered(model: &FrameModel) -> Vec<(String, AttentionScore)> {
    let status = &model.sidebar_status;
    let active = active_worktree_path(model);
    let mut v: Vec<(String, AttentionScore)> = status
        .attention
        .iter()
        // Acknowledged (quieted) worktrees drop out of the needs-you set, as
        // does the focused worktree itself: the "Needs you" popup, the `✋`
        // badge, and the `Alt a` jump ring all read this, so acking silences
        // every nag surface at once and the tab you're on never self-nags.
        .filter(|(p, s)| {
            s.needs_user() && !status.acked.contains(p.as_str()) && Some(p.as_str()) != active
        })
        .map(|(p, s)| (p.clone(), *s))
        .collect();
    v.sort_by_key(|(p, s)| {
        (
            status.attention_ranks.get(p).copied().unwrap_or(u32::MAX),
            s.sort_key(),
        )
    });
    v
}

/// Resolve the jump: the next needs-you worktree after the active one, as the
/// sidebar row target that focuses it (a live tab, or a workspace switch for a
/// dormant workspace's worktree) plus a status line. `None` when nothing needs
/// the user or no row resolves the path.
pub(crate) fn next_target(
    model: &FrameModel,
    session: &crate::session::Session,
) -> Option<(crate::sidebar::RowTarget, String)> {
    let ordered = needs_user_ordered(model);
    let active_path = session.active_group().map(|g| g.path.clone());
    // Cycle from the active worktree even when it isn't in the needs-you set;
    // `next_attention` then starts at the most urgent.
    let next = attention::next_attention(&ordered, active_path.as_deref())?.to_string();
    let score = ordered.iter().find(|(p, _)| p == &next).map(|(_, s)| *s)?;
    let row = model.sidebar_rows.iter().find(|r| {
        r.kind == crate::sidebar::RowKind::Worktree
            && r.worktree_path.as_deref() == Some(next.as_str())
            && r.tab_target.is_some()
    })?;
    let target = row.tab_target.clone()?;
    let status = format!("{} — {}", row.label, score.reason.label());
    Some((target, status))
}

/// Mark everything read: every stored notification read **and** every live
/// needs-you signal acknowledged (quieted). The full "clear the nag" gesture
/// behind `Alt Shift R`. Snapshots the needs-you set on the caller's thread
/// (cheap) and writes off the loop, then pulses a model refresh.
pub(crate) fn mark_all_read(
    model: &mut FrameModel,
    tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) {
    let acks: Vec<(String, String, Option<i64>)> = needs_user_ordered(model)
        .into_iter()
        .filter_map(|(p, s)| {
            serde_json::to_string(&s.reason)
                .ok()
                .map(|r| (p, r, s.since))
        })
        .collect();
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(db) = thegn_core::db::Db::open() {
            let _ = db.mark_all_notifications_read(); // best-effort: DB is a cache
            for (p, r, since) in acks {
                let _ = db.put_attention_ack(&p, &r, since);
            }
        }
        if tx.send(RefreshKind::Model).is_ok() {
            let _ = waker.wake();
        }
    });
    model.status = "Marked all as read".into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::{AttentionReason, AttentionTier};

    fn score(tier: AttentionTier) -> AttentionScore {
        AttentionScore {
            tier,
            sub: 0,
            reason: AttentionReason::AgentNeedsInput,
            since: None,
        }
    }

    #[test]
    fn needs_user_filters_and_orders_by_rank() {
        let mut model = FrameModel::default();
        let st = &mut model.sidebar_status;
        st.attention
            .insert("/wt/a".into(), score(AttentionTier::Waiting));
        st.attention
            .insert("/wt/b".into(), score(AttentionTier::Blocked));
        st.attention
            .insert("/wt/c".into(), score(AttentionTier::Working)); // not needs_user
        st.attention_ranks.insert("/wt/b".into(), 0);
        st.attention_ranks.insert("/wt/a".into(), 1);
        st.attention_ranks.insert("/wt/c".into(), 2);
        let v = needs_user_ordered(&model);
        let paths: Vec<&str> = v.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["/wt/b", "/wt/a"]);
    }
}
