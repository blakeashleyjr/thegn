//! Statusbar badge chips (the always-on right-cluster indicators), extracted
//! from `chrome::statusbar_items` (chrome.rs is pinned by the file-size
//! ratchet). Each `push_*` appends its chip(s) to the ordered item list; all
//! stay silent when clean (the "clean is quiet" posture).

use crate::chrome::{BarBadge, BarItemId, FrameModel};
use crate::seg::{Seg, Tok};
use thegn_core::theme::Hue;

/// Needs-you chip: how many worktrees currently need the user (attention
/// tiers T0–T2 — blocked on input, failures, finished-awaiting-review). Red
/// while anything is blocked/failing, amber when only finished work waits.
/// Activating it drills into the list; `Alt a` jumps to the next one.
///
/// Counts the *scoped* set — this repo's worktrees — and appends a dim ` +N `
/// when other repos have needs-you worktrees too, so scoping is visible rather
/// than silent. `g` in the System tab widens both.
///
/// The set comes from `needs_user_ordered`, deliberately not a filter of its own:
/// this badge used to carry a duplicate predicate that could drift from the
/// popup's. The extra `Vec` is free — the badge is only rebuilt on a chrome
/// recompose, which is already the expensive `render_plan::Full` path.
pub(crate) fn push_attention_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use crate::chrome::S;
    use thegn_core::attention::AttentionTier;
    let needs = crate::handlers::attention::needs_user_ordered(model);
    let elsewhere = crate::handlers::attention::needs_user_out_of_scope(model).len();
    if needs.is_empty() && elsewhere == 0 {
        return;
    }
    let urgent = needs.iter().any(|(_, s)| s.tier <= AttentionTier::Failure);
    let hue = if urgent { Hue::Red } else { Hue::Amber };
    let hand = crate::caps::active_glyphs().attention;
    let mut segs = Vec::new();
    if !needs.is_empty() {
        segs.push(Seg::chip(
            Tok::Hue(hue),
            format!(" {hand} {} ", needs.len()),
        ));
    }
    if elsewhere > 0 {
        // Dim, and outside the hued chip: another repo needing you is context,
        // never this repo's alarm.
        segs.push(Seg::chip(
            Tok::Slot(S::Dim),
            if needs.is_empty() {
                format!(" {hand} +{elsewhere} ")
            } else {
                format!("+{elsewhere} ")
            },
        ));
    }
    items.push((BarItemId::Badge(BarBadge::Attention), segs));
}

/// The always-on daemon/status chip — one glyph, no word, pinned to the far
/// right of the statusbar (pushed last by `statusbar_items`). Unlike the other
/// badges it is *never* silent: it is a persistent affordance whose glyph
/// reports the program's daemon relationship, and activating it opens the
/// expanded status modal (`detail.rs`). States (ASCII fallbacks in parens):
///
/// - **NonPersist** — dim `○` (`o`): the focused pane runs inline; quit ends it.
/// - **Persist** — teal `◆` (`*`): the focused pane is daemon-backed (quit
///   detaches, relaunch reattaches).
/// - **Server** — blue `▲` (`^`): this instance's daemon serves remote clients.
/// - **Client** — purple `▽` (`v`): attached to a remote pane daemon.
pub(crate) fn push_daemon_chip(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use crate::chrome::{DaemonChipState, S};
    let g = crate::caps::active_glyphs();
    let (glyph, tone) = match model.daemon_state {
        DaemonChipState::NonPersist => (g.dot_hollow, Tok::Slot(S::Dim)),
        DaemonChipState::Persist => (g.diamond_filled, Tok::Hue(Hue::Teal)),
        DaemonChipState::Server => (g.role_server, Tok::Hue(Hue::Blue)),
        DaemonChipState::Client => (g.role_client, Tok::Hue(Hue::Purple)),
    };
    items.push((
        BarItemId::Badge(BarBadge::Persist),
        vec![Seg::chip(tone, format!(" {glyph} "))],
    ));
}

/// Offline chip: an amber `⚑ offline` while the app-wide connectivity holder
/// reports offline (auto-detected or `[network] mode = offline`). Silent when
/// online/unknown (clean is quiet). Signals that remote refreshes (PR/CI/issues)
/// and network MCPs are paused; local git/DB caches are served stale.
pub(crate) fn push_network_chip(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use thegn_core::connectivity::Connectivity;
    if model.connectivity != Connectivity::Offline {
        return;
    }
    let flag = crate::caps::active_glyphs().warn;
    items.push((
        BarItemId::Badge(BarBadge::Network),
        vec![Seg::chip(Tok::Hue(Hue::Amber), format!(" {flag} offline "))],
    ));
}

/// CI rollup badge (AV group, item 158): a red ✗ chip when workflows are
/// *currently* failing, an amber ● chip while runs are in flight; silent when
/// all green (mirrors the "clean is quiet" notification posture). Only when CI
/// is configured and the cache is warm (`ci_runs` non-empty). Counts come from
/// `current_summary` — each workflow judged by its most recent run — so
/// historical failures don't pin the badge red.
pub(crate) fn push_ci_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    if model.panel.ci_runs.is_empty() {
        return;
    }
    let cur = thegn_core::ci::current_summary(&model.panel.ci_runs);
    let fail = cur.failed;
    let running = cur.running;
    if fail > 0 {
        items.push((
            BarItemId::Badge(BarBadge::Ci),
            vec![Seg::chip(
                Tok::Hue(Hue::Red),
                format!(" {} {fail} CI ", crate::caps::active_glyphs().cross),
            )],
        ));
    } else if running > 0 {
        items.push((
            BarItemId::Badge(BarBadge::Ci),
            vec![Seg::chip(
                Tok::Hue(Hue::Amber),
                format!(" {} {running} CI ", crate::caps::active_glyphs().dot_filled),
            )],
        ));
    }
}

/// Merge-queue (fold-actor) badge: a red ⚑ chip when branches are blocked
/// (deferred / gate-failed / needs-human), an amber chip while the queue is
/// working (folding / agent running), and a quiet dim chip whenever anything
/// is merely queued or held at ready — so an idle-but-populated queue is
/// visible. Silent only when the queue is empty (clean is quiet). Activating
/// it opens the queue overlay (`detail.rs`).
pub(crate) fn push_mq_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    // Repo-scoped like every other nag surface (the ✋ badge, "Needs you",
    // the inbox): `merge_queue` rows are global in the DB, but a sibling
    // repo's queued branches must not raise a red ⚑ in the repo you're
    // working in. `repo_scope == None` fails open (count everything), per
    // the contract on `SidebarStatus::repo_scope`.
    let scope = model.sidebar_status.repo_scope.as_ref();
    let q = model
        .panel
        .merge_queue
        .iter()
        .filter(|r| scope.is_none_or(|s| s.contains(&r.worktree)));
    let blocked = q
        .clone()
        .filter(|r| {
            // Same set the Merge-queue section paints as blocked — keep in
            // sync with `panel::sections::merge_queue` (`gate_error` = the
            // gate could not run; amber there, but still "needs a human").
            matches!(
                r.status.as_str(),
                "deferred" | "gate_failed" | "gate_error" | "needs_human"
            )
        })
        .count();
    let working = q
        .clone()
        .filter(|r| matches!(r.status.as_str(), "folding" | "verifying" | "agent_running"))
        .count();
    let idle = q
        .filter(|r| matches!(r.status.as_str(), "queued" | "ready"))
        .count();
    if blocked > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(Tok::Hue(Hue::Red), format!(" ⚑ {blocked} MQ "))],
        ));
    } else if working > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(Tok::Hue(Hue::Amber), format!(" ⧉ {working} MQ "))],
        ));
    } else if idle > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(
                Tok::Slot(crate::chrome::S::Dim),
                format!(" ⧉ {idle} MQ "),
            )],
        ));
    }
}

/// PR-queue badge, with the same grammar as the merge queue's so the two read
/// alike: red when a pull request needs you, amber while thegn is working on
/// one, dim when the queue is merely populated, silent when empty.
///
/// `blocked_review` is deliberately NOT red — awaiting a colleague's review is
/// the normal resting state of a healthy pull request, and a permanently red
/// statusbar teaches people to ignore it.
pub(crate) fn push_prq_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    let q = &model.panel.pr_queue;
    let blocked = q
        .iter()
        .filter(|r| {
            matches!(
                r.status.as_str(),
                "needs_human" | "blocked_ci" | "blocked_conflict"
            )
        })
        .count();
    let working = q
        .iter()
        .filter(|r| matches!(r.status.as_str(), "agent_running" | "merging"))
        .count();
    let idle = q
        .iter()
        .filter(|r| matches!(r.status.as_str(), "watching" | "blocked_review" | "ready"))
        .count();
    if blocked > 0 {
        items.push((
            BarItemId::Badge(BarBadge::PrQueue),
            vec![Seg::chip(Tok::Hue(Hue::Red), format!(" ⚑ {blocked} PR "))],
        ));
    } else if working > 0 {
        items.push((
            BarItemId::Badge(BarBadge::PrQueue),
            vec![Seg::chip(Tok::Hue(Hue::Amber), format!(" ⧉ {working} PR "))],
        ));
    } else if idle > 0 {
        items.push((
            BarItemId::Badge(BarBadge::PrQueue),
            vec![Seg::chip(
                Tok::Slot(crate::chrome::S::Dim),
                format!(" ⧉ {idle} PR "),
            )],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::{AttentionReason, AttentionScore, AttentionTier};

    fn score(tier: AttentionTier) -> AttentionScore {
        AttentionScore {
            tier,
            sub: 0,
            reason: AttentionReason::AgentWaiting,
            since: None,
            episode: 0,
        }
    }

    fn chip_text(items: &[(BarItemId, Vec<Seg>)]) -> String {
        items
            .iter()
            .flat_map(|(_, segs)| segs.iter().map(|s| s.text.clone()))
            .collect()
    }

    /// The badge used to carry its own copy of the needs-you predicate, which
    /// could drift from the popup's. Lock them together.
    #[test]
    fn attention_badge_count_equals_needs_user_ordered() {
        let mut model = FrameModel::default();
        let st = &mut model.sidebar_status;
        for (p, tier) in [
            ("/wt/a", AttentionTier::Blocked),
            ("/wt/b", AttentionTier::Waiting),
            ("/wt/c", AttentionTier::Working), // not needs_user
            ("/wt/d", AttentionTier::Failure), // acked
        ] {
            st.attention.insert(p.into(), score(tier));
        }
        st.acked.insert("/wt/d".into());

        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        let n = crate::handlers::attention::needs_user_ordered(&model).len();
        assert_eq!(n, 2, "blocked + waiting; working and acked drop out");
        assert!(chip_text(&items).contains(&format!(" {n} ")));
        // Blocked ⇒ urgent ⇒ red. `Seg::chip` paints the hue as the background.
        assert!(
            items
                .iter()
                .any(|(_, segs)| segs.iter().any(|s| s.bg == Some(Tok::Hue(Hue::Red)))),
            "a blocked worktree must make the chip red"
        );

        // Nothing needing the user ⇒ silent (the "clean is quiet" posture).
        model.sidebar_status.attention.clear();
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(items.is_empty());
    }

    #[test]
    fn attention_badge_shows_out_of_scope_rollup() {
        let mut model = FrameModel::default();
        let st = &mut model.sidebar_status;
        st.attention
            .insert("/wt/mine".into(), score(AttentionTier::Waiting));
        st.attention
            .insert("/wt/theirs".into(), score(AttentionTier::Blocked));
        st.repo_scope = Some(["/wt/mine".to_string()].into_iter().collect());

        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        let text = chip_text(&items);
        assert!(text.contains(" 1 "), "this repo's count: {text:?}");
        assert!(text.contains("+1"), "the other repo is rolled up: {text:?}");

        // With nothing local, the rollup alone still surfaces — scoping must not
        // make another repo's blocked worktree disappear entirely.
        model.sidebar_status.attention.remove("/wt/mine");
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(chip_text(&items).contains("+1"));

        // Widening (`repo_scope: None`, what the `g` toggle hydrates) folds the
        // rollup back into the real count.
        model.sidebar_status.repo_scope = None;
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        let text = chip_text(&items);
        assert!(text.contains(" 1 ") && !text.contains('+'), "{text:?}");
    }

    #[test]
    fn ci_badge_reflects_current_state_not_history() {
        use thegn_core::ci::{CiRun, CiState};
        let run = |id: &str, name: &str, state| CiRun {
            id: id.into(),
            name: name.into(),
            state,
            ..Default::default()
        };
        let mut model = FrameModel::default();
        // Newest-first: the "ci" workflow passes now but failed twice before —
        // the badge must stay quiet (the old all-runs count showed "✗ 2 CI").
        model.panel.ci_runs = vec![
            run("4", "ci", CiState::Pass),
            run("3", "ci", CiState::Fail),
            run("2", "ci", CiState::Fail),
        ];
        let mut items = Vec::new();
        push_ci_badge(&model, &mut items);
        assert!(items.is_empty(), "green-now pipeline must be quiet");
        // A currently-failing workflow counts exactly once.
        model
            .panel
            .ci_runs
            .insert(0, run("9", "lint", CiState::Fail));
        push_ci_badge(&model, &mut items);
        assert!(chip_text(&items).contains(" 1 CI"));
    }

    #[test]
    fn daemon_chip_is_always_shown_and_hues_by_state() {
        use crate::chrome::{DaemonChipState, S};
        let tone_for = |state: DaemonChipState| {
            let model = FrameModel {
                daemon_state: state,
                ..Default::default()
            };
            let mut items = Vec::new();
            push_daemon_chip(&model, &mut items);
            // Never silent — the daemon chip is a persistent affordance.
            assert_eq!(items.len(), 1, "{state:?} must always emit a chip");
            assert!(matches!(items[0].0, BarItemId::Badge(BarBadge::Persist)));
            // A glyph-only chip: exactly one seg, no label word (just ` X `).
            let text = chip_text(&items);
            assert_eq!(
                text.chars().filter(|c| !c.is_whitespace()).count(),
                1,
                "{text:?}"
            );
            items[0].1[0].bg.unwrap()
        };
        // The default (inline pane, no daemon) is a quiet dim chip.
        assert_eq!(tone_for(DaemonChipState::NonPersist), Tok::Slot(S::Dim));
        assert_eq!(tone_for(DaemonChipState::Persist), Tok::Hue(Hue::Teal));
        assert_eq!(tone_for(DaemonChipState::Server), Tok::Hue(Hue::Blue));
        assert_eq!(tone_for(DaemonChipState::Client), Tok::Hue(Hue::Purple));
    }

    #[test]
    fn daemon_chip_renders_far_right() {
        // Force another right-cluster badge (Sync) so we can prove the daemon
        // chip sorts AFTER the rest of the cluster, at the far right.
        let model = FrameModel {
            sync_panes: true,
            ..Default::default()
        };
        let items = crate::chrome::statusbar_items(&model);
        let last = items.last().expect("at least the daemon chip");
        assert!(
            matches!(last.0, BarItemId::Badge(BarBadge::Persist)),
            "daemon chip must be the last (far-right) item"
        );
        assert!(
            matches!(items[items.len() - 2].0, BarItemId::Badge(BarBadge::Sync)),
            "the daemon chip follows the rest of the cluster"
        );
    }

    #[test]
    fn attention_badge_counts_needs_user_and_hues_by_urgency() {
        let mut model = FrameModel::default();
        let mut items = Vec::new();
        // Nothing needing the user: silent.
        push_attention_badge(&model, &mut items);
        assert!(items.is_empty());

        // Two waiting + one working (not counted): amber chip " _ 2 ".
        let st = &mut model.sidebar_status;
        st.attention
            .insert("/a".into(), score(AttentionTier::Waiting));
        st.attention
            .insert("/b".into(), score(AttentionTier::Waiting));
        st.attention
            .insert("/c".into(), score(AttentionTier::Working));
        push_attention_badge(&model, &mut items);
        assert_eq!(items.len(), 1);
        assert!(chip_text(&items).contains(" 2 "));
        assert!(matches!(items[0].0, BarItemId::Badge(BarBadge::Attention)));

        // A blocked worktree makes it urgent (red) and counts too.
        model
            .sidebar_status
            .attention
            .insert("/d".into(), score(AttentionTier::Blocked));
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(chip_text(&items).contains(" 3 "));
    }

    fn mq_row(status: &str) -> thegn_core::db::MergeQueueRow {
        thegn_core::db::MergeQueueRow {
            worktree: format!("/wt/{status}"),
            branch: format!("b-{status}"),
            target_branch: "main".into(),
            status: status.into(),
            queued_at: 1,
            updated_at: 1,
            result_oid: None,
            conflict_paths: None,
            error_detail: None,
            location: String::new(),
            agent_attempts: 0,
        }
    }

    fn mq_chip_for(statuses: &[&str]) -> Option<(String, Seg)> {
        let mut model = FrameModel::default();
        model.panel.merge_queue = statuses.iter().map(|s| mq_row(s)).collect();
        let mut items = Vec::new();
        push_mq_badge(&model, &mut items);
        items
            .pop()
            .map(|(_, mut segs)| (segs[0].text.clone(), segs.remove(0)))
    }

    #[test]
    fn mq_badge_hues_by_severity_and_shows_idle_queues() {
        // Empty queue: silent (clean is quiet).
        assert!(mq_chip_for(&[]).is_none());
        // Merely queued / held at ready: a quiet dim chip — the queue must be
        // discoverable even when nothing is running or failing.
        let (text, seg) = mq_chip_for(&["queued", "ready"]).unwrap();
        assert!(text.contains("2 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Slot(crate::chrome::S::Dim))); // chips carry the tone as bg
        // Working (agent included) wins over idle: amber.
        let (text, seg) = mq_chip_for(&["queued", "agent_running"]).unwrap();
        assert!(text.contains("1 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Amber))); // chips carry the tone as bg
        // Anything blocked (needs_human included) wins over all: red ⚑.
        let (text, seg) = mq_chip_for(&["queued", "folding", "needs_human"]).unwrap();
        assert!(text.contains("⚑ 1 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Red))); // chips carry the tone as bg
        // A gate that could not run is blocked too (the section shows it in
        // amber as "gate could not run"); the chip used to go silent on it.
        let (text, seg) = mq_chip_for(&["gate_error"]).unwrap();
        assert!(text.contains("⚑ 1 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Red)));
        // Only landed rows: nothing left to signal.
        assert!(mq_chip_for(&["landed"]).is_none());
    }
}
