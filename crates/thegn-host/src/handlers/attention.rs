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

/// Is `path` inside the current nag scope — the active worktree's repo?
///
/// Scoping the nag — but not the per-row sidebar glyphs — is what keeps a
/// sibling repo's failing CI from raising the `✋` badge in the repo you're
/// actually working in.
///
/// Pure over the model: `repo_scope` is resolved once on the hydration thread
/// (`attention_status::collect_attention`), where the "show everything" toggle
/// is folded in, so `None` already means "scope nothing".
fn in_scope(status: &crate::sidebar::SidebarStatus, path: &str) -> bool {
    status.repo_scope.as_ref().is_none_or(|s| s.contains(path))
}

/// **The** needs-you predicate. Every nag surface — the `✋` badge, the "Needs
/// you" popup, the `Alt a` ring — goes through this; there is deliberately no
/// second copy (the statusbar badge used to carry its own and could drift).
///
/// Acknowledged (quieted) worktrees drop out, as does the focused worktree
/// itself: the tab you're already on never self-nags.
fn is_nagging(
    status: &crate::sidebar::SidebarStatus,
    active: Option<&str>,
    path: &str,
    score: &AttentionScore,
) -> bool {
    score.needs_user()
        && !status.acked.contains(path)
        && Some(path) != active
        && in_scope(status, path)
}

/// Order a needs-you set by the hysteresis-stable hydration rank, so every
/// surface agrees with what the Attention sort displays.
fn ordered_by_rank(
    status: &crate::sidebar::SidebarStatus,
    mut v: Vec<(String, AttentionScore)>,
) -> Vec<(String, AttentionScore)> {
    v.sort_by_key(|(p, s)| {
        (
            status.attention_ranks.get(p).copied().unwrap_or(u32::MAX),
            s.sort_key(),
        )
    });
    v
}

/// The worktrees currently needing the user (tiers T0–T2) **in the active
/// repo**, most urgent first. Out-of-scope worktrees are surfaced separately by
/// [`needs_user_out_of_scope`] rather than silently dropped.
pub(crate) fn needs_user_ordered(model: &FrameModel) -> Vec<(String, AttentionScore)> {
    let status = &model.sidebar_status;
    let active = active_worktree_path(model);
    ordered_by_rank(
        status,
        status
            .attention
            .iter()
            .filter(|(p, s)| is_nagging(status, active, p.as_str(), s))
            .map(|(p, s)| (p.clone(), *s))
            .collect(),
    )
}

/// The needs-you worktrees that fall *outside* the active repo — everything
/// [`needs_user_ordered`] scoped away. Drives the `+N` rollup on the `✋` badge
/// and the "Other repos" group in the popup, so scoping stays visible instead of
/// silently swallowing another repo's failure. Always empty when the "all"
/// toggle is on (then everything is in scope).
pub(crate) fn needs_user_out_of_scope(model: &FrameModel) -> Vec<(String, AttentionScore)> {
    let status = &model.sidebar_status;
    let active = active_worktree_path(model);
    ordered_by_rank(
        status,
        status
            .attention
            .iter()
            .filter(|(p, s)| {
                s.needs_user()
                    && !status.acked.contains(p.as_str())
                    && Some(p.as_str()) != active
                    && !in_scope(status, p.as_str())
            })
            .map(|(p, s)| (p.clone(), *s))
            .collect(),
    )
}

/// The set to **acknowledge** on a clear-all: every un-acked needs-you
/// worktree — the *active* one and the out-of-scope ("Other repos") ones
/// included.
///
/// The active worktree is hidden from the nag list because you are already
/// there — but leaving it un-acked meant its signal re-nagged the moment focus
/// moved, and most visibly after a restart landed you somewhere else. The
/// other-repo rows are painted in the same popup that binds `a`, so a clear-all
/// that skipped them reported success and left them on screen. Exclusion is a
/// property of the display, not of the ack, and these are separate functions so
/// the two can't be confused again. (`Alt a` cycles over this set too.)
pub(crate) fn needs_user_for_ack(model: &FrameModel) -> Vec<(String, AttentionScore)> {
    let status = &model.sidebar_status;
    ordered_by_rank(
        status,
        status
            .attention
            .iter()
            .filter(|(p, s)| s.needs_user() && !status.acked.contains(p.as_str()))
            .map(|(p, s)| (p.clone(), *s))
            .collect(),
    )
}

/// Resolve the jump: the next needs-you worktree after the active one, as the
/// sidebar row target that focuses it (a live tab, or a workspace switch for a
/// dormant workspace's worktree) plus a status line. `None` when nothing needs
/// the user or no row resolves the path.
///
/// Scoped to the active repo like every other nag surface (it reads
/// [`needs_user_ordered`]) — the dormant-workspace switch still applies within
/// that repo, and `g` in the System tab widens the ring to every worktree.
pub(crate) fn next_target(
    model: &FrameModel,
    session: &crate::session::Session,
) -> Option<(crate::sidebar::RowTarget, String)> {
    // The ring must *include* the active worktree (when it needs you) so the
    // cursor has a position to advance from — `needs_user_ordered` hides the
    // active tab for display, and cycling over that list always restarted at
    // its head (two worktrees ping-ponged; a third was unreachable).
    let ring = needs_user_for_ack(model);
    let active_path = session.active_group().map(|g| g.path.clone());
    let start = attention::next_attention(&ring, active_path.as_deref())?.to_string();
    let start_ix = ring.iter().position(|(p, _)| p == &start)?;
    // Walk from there, skipping the active tab itself and any candidate with
    // no sidebar row to land on, so one unreachable entry never dead-ends the
    // whole ring.
    (0..ring.len())
        .map(|k| &ring[(start_ix + k) % ring.len()])
        .filter(|(p, _)| Some(p.as_str()) != active_path.as_deref())
        .find_map(|(next, score)| {
            let row = model.sidebar_rows.iter().find(|r| {
                r.kind == crate::sidebar::RowKind::Worktree
                    && r.worktree_path.as_deref() == Some(next.as_str())
                    && r.tab_target.is_some()
            })?;
            let target = row.tab_target.clone()?;
            Some((target, format!("{} — {}", row.label, score.reason.label())))
        })
}

/// Mark everything read: every stored notification read **and** every live
/// needs-you signal acknowledged (quieted). The full "clear the nag" gesture —
/// behind `Alt Shift R`, the inbox's `a`, and the unified overlay's `a`/`R`,
/// which all route here so "clear all" means the same thing everywhere.
///
/// Acking the *live* signals is the load-bearing half: a CI failure is derived
/// from the PR/CI cache, not from a notification row, so marking notifications
/// read alone leaves it to reappear on the very next hydration pass.
///
/// Snapshots the ack set on the caller's thread (cheap) and writes off the loop,
/// then pulses a model refresh.
pub(crate) fn mark_all_read(
    model: &mut FrameModel,
    tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) {
    let acks: Vec<(String, String, Option<i64>, thegn_core::attention::Episode)> =
        needs_user_for_ack(model)
            .into_iter()
            .filter_map(|(p, s)| {
                serde_json::to_string(&s.reason)
                    .ok()
                    .map(|r| (p, r, s.since, s.episode))
            })
            .collect();
    // Clear WHAT THE INBOX SHOWS: repo-scoped by default (this repo's rows +
    // untagged host-global ones), everything only under the `g` all-worktrees
    // view. The unscoped clear silently marked other repos' never-seen
    // notifications read. The active repo root is resolved off-loop.
    let scope_all = crate::panel::scope::system_all();
    let active = active_worktree_path(model).map(std::path::PathBuf::from);
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(db) = thegn_core::db::Db::open() {
            // best-effort: DB is a cache
            match (scope_all, &active) {
                (false, Some(wt)) => {
                    let repo_root =
                        thegn_core::repo::main_worktree(wt).unwrap_or_else(|| wt.clone());
                    let paths: Vec<String> = crate::hydrate::repo_worktree_paths(&db, &repo_root)
                        .into_iter()
                        .collect();
                    let _ = db.mark_notifications_read_scoped(&paths);
                }
                _ => {
                    let _ = db.mark_all_notifications_read();
                }
            }
            for (p, r, since, episode) in acks {
                let _ = db.put_attention_ack(&p, &r, since, episode);
            }
        }
        if tx.send(RefreshKind::Model).is_ok() {
            let _ = waker.wake();
        }
    });
    model.status = if scope_all {
        "Marked all as read (all worktrees)".into()
    } else {
        "Marked all as read (this repo — g widens)".into()
    };
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
            episode: 0,
        }
    }

    /// Regression: `Alt a` must walk the *whole* needs-you ring. It used to
    /// cycle over the display list (which hides the active tab), so the cursor
    /// never had a position to advance from and every jump landed on the head:
    /// two worktrees ping-ponged and a third was unreachable.
    #[test]
    fn next_target_cycles_through_every_needs_you_worktree() {
        use crate::session::{GroupKind, Session, WorktreeGroup};
        use crate::sidebar::{RowKind, RowTarget, SidebarRow};

        let mut model = FrameModel::default();
        let st = &mut model.sidebar_status;
        for (i, p) in ["/wt/b", "/wt/c", "/wt/d"].iter().enumerate() {
            st.attention
                .insert((*p).into(), score(AttentionTier::Blocked));
            st.attention_ranks.insert((*p).into(), i as u32);
        }
        for (i, p) in ["/wt/b", "/wt/c", "/wt/d"].iter().enumerate() {
            model.sidebar_rows.push(SidebarRow {
                worktree_path: Some((*p).into()),
                tab_target: Some(RowTarget::Tab(i, 0)),
                ..SidebarRow::base(RowKind::Worktree, 1, &p[4..], "app")
            });
        }
        let mut session = Session::default();
        for p in ["/wt/b", "/wt/c", "/wt/d"] {
            session
                .worktrees
                .push(WorktreeGroup::new(&p[4..], GroupKind::Branch, p));
        }

        let mut visited = Vec::new();
        for _ in 0..3 {
            let (target, _) = next_target(&model, &session).expect("a target");
            let RowTarget::Tab(g, _) = target else {
                panic!("tab target")
            };
            session.active = g;
            visited.push(session.worktrees[g].path.clone());
        }
        assert_eq!(
            visited,
            vec!["/wt/c", "/wt/d", "/wt/b"],
            "three jumps from b must visit c, d, then wrap to b"
        );
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

    /// Build a model with two repos' worktrees needing the user, scoped to the
    /// first. `/wt/mine` is in scope, `/wt/theirs` is not.
    fn two_repo_model() -> FrameModel {
        let mut model = FrameModel::default();
        let st = &mut model.sidebar_status;
        st.attention
            .insert("/wt/mine".into(), score(AttentionTier::Blocked));
        st.attention
            .insert("/wt/theirs".into(), score(AttentionTier::Failure));
        st.attention_ranks.insert("/wt/mine".into(), 0);
        st.attention_ranks.insert("/wt/theirs".into(), 1);
        st.repo_scope = Some(["/wt/mine".to_string()].into_iter().collect());
        model
    }

    #[test]
    fn needs_user_excludes_other_repos_unless_widened() {
        let model = two_repo_model();

        let scoped: Vec<String> = needs_user_ordered(&model)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(scoped, vec!["/wt/mine"], "a sibling repo must not nag here");
        let out: Vec<String> = needs_user_out_of_scope(&model)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(out, vec!["/wt/theirs"], "but it stays visible as a rollup");

        // `repo_scope: None` is what both the "show everything" toggle and an
        // unresolvable active repo produce: scope nothing, and then there is no
        // "out of scope" left over. Fail-open is the point — a scoping bug must
        // never hide a signal that needs the user.
        let mut open = two_repo_model();
        open.sidebar_status.repo_scope = None;
        assert_eq!(needs_user_ordered(&open).len(), 2);
        assert!(needs_user_out_of_scope(&open).is_empty());
    }

    #[test]
    fn ack_set_includes_the_active_worktree_display_set_does_not() {
        let mut model = two_repo_model();
        // Mark `/wt/mine` as the focused row.
        model.sidebar_rows.push(crate::sidebar::SidebarRow {
            active: true,
            worktree_path: Some("/wt/mine".into()),
            ..crate::sidebar::SidebarRow::base(crate::sidebar::RowKind::Worktree, 1, "mine", "app")
        });

        // Display: the tab you're on never self-nags.
        assert!(
            needs_user_ordered(&model).is_empty(),
            "the active worktree is hidden from the nag list"
        );
        // Ack: it must still be acknowledged, or its signal re-nags the moment
        // focus moves — which is what made it reappear after a restart.
        let acked: Vec<String> = needs_user_for_ack(&model)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        // The out-of-scope worktree is acked too: the popup shows it under
        // "Other repos" and binds `a` on that row, so clear-all must cover it.
        assert_eq!(acked, vec!["/wt/mine", "/wt/theirs"]);

        // Already-acked worktrees drop out of the ack set (no pointless rewrite).
        model.sidebar_status.acked.insert("/wt/mine".into());
        model.sidebar_status.acked.insert("/wt/theirs".into());
        assert!(needs_user_for_ack(&model).is_empty());
    }
}
