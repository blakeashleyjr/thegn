//! Statusbar badge chips (the always-on right-cluster indicators), extracted
//! from `chrome::statusbar_items` (chrome.rs is pinned by the file-size
//! ratchet). Each `push_*` appends its chip(s) to the ordered item list; all
//! stay silent when clean (the "clean is quiet" posture).

use crate::chrome::{BarBadge, BarItemId, FrameModel};
use crate::seg::{Seg, Tok};
use thegn_core::theme::Hue;

/// **The** attention chip — the one statusbar signal that something needs the
/// user. Counts the rows the unified popup's "Needs you" + "Alerts" groups
/// show (attention tiers T0–T2 for this repo's worktrees, plus unread
/// alert-priority inbox rows not already covered by one of those worktrees):
/// red while anything is blocked/failing, amber when only finished work waits.
/// When nothing needs the user but notice-priority unread rows exist, a quiet
/// blue `✉ N` inbox count takes its place; info-priority rows never show.
/// Activating it opens the unified surface; `Alt a` jumps to the next item.
///
/// Appends a dim ` +N ` when other repos have needs-you worktrees too, so
/// scoping is visible rather than silent. `g` in the System tab widens both.
///
/// Everything comes from [`crate::handlers::attention::rollup`], deliberately
/// not a filter of its own: there used to be two chips (this `✋` and a `⚑`
/// inbox flag) fed by two predicates, and one failed pane lit both.
pub(crate) fn push_attention_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use crate::chrome::S;
    let r = crate::handlers::attention::rollup(model);
    let count = r.count();
    let g = crate::caps::active_glyphs();
    let hand = g.attention;
    let mut segs = Vec::new();
    if count > 0 {
        let hue = if r.urgent() { Hue::Red } else { Hue::Amber };
        segs.push(Seg::chip(Tok::Hue(hue), format!(" {hand} {count} ")));
    } else if r.elsewhere == 0 && r.notices > 0 {
        // Nothing urgent anywhere: the neutral inbox count, blue and quiet.
        segs.push(Seg::chip(
            Tok::Hue(Hue::Blue),
            format!(" {} {} ", g.mail, r.notices),
        ));
    }
    if r.elsewhere > 0 {
        // Dim, and outside the hued chip: another repo needing you is context,
        // never this repo's alarm.
        segs.push(Seg::chip(
            Tok::Slot(S::Dim),
            if count == 0 {
                format!(" {hand} +{} ", r.elsewhere)
            } else {
                format!("+{} ", r.elsewhere)
            },
        ));
    }
    if segs.is_empty() {
        return;
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
        // A crashed/wedged daemon: a red warning glyph, so degradation is
        // visible without `THEGN_LOG`. Activating the chip runs the probe.
        DaemonChipState::Error => (g.warn, Tok::Hue(Hue::Red)),
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
        vec![Seg::chip(
            Tok::Hue(Hue::Amber),
            format!(
                " {flag} {} ",
                crate::i18n_surface::status(crate::i18n_surface::StatusText::Offline)
            ),
        )],
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

/// Return the active workspace's merge-queue rollup. Queue rows are global in
/// the cache, so retain the existing repo scope before applying core policy.
fn mq_rollup(model: &FrameModel) -> Option<thegn_core::merge_queue_view::MqRollup> {
    let scope = model.sidebar_status.repo_scope.as_ref();
    thegn_core::merge_queue_view::rollup(model.panel.merge_queue.iter().filter_map(|r| {
        if scope.is_none_or(|s| s.contains(&r.worktree)) {
            thegn_core::attention::MqStatus::parse(&r.status)
        } else {
            None
        }
    }))
}

/// Render the compact queue indicator used by the ordinary `mq` widget.
/// Counts precede the tier marker, and marker glyphs use the existing queue
/// status/capability vocabulary.
pub(crate) fn push_mq_widget(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    let Some(rollup) = mq_rollup(model) else {
        return;
    };
    let glyphs = crate::caps::active_glyphs();
    let (marker, tone) = match rollup.tier {
        thegn_core::merge_queue_view::MqTier::Blocked => (
            thegn_core::attention::MqStatus::Deferred.glyph(glyphs).0,
            Tok::Hue(Hue::Red),
        ),
        thegn_core::merge_queue_view::MqTier::Working => (
            thegn_core::attention::MqStatus::Folding.glyph(glyphs).0,
            Tok::Hue(Hue::Amber),
        ),
        thegn_core::merge_queue_view::MqTier::Populated => (
            thegn_core::attention::MqStatus::Queued.glyph(glyphs).0,
            Tok::Slot(crate::chrome::S::Dim),
        ),
    };
    items.push((
        BarItemId::Widget("mq".into()),
        vec![Seg::chip(tone, format!(" {} {marker} MQ ", rollup.count))],
    ));
}

/// Compatibility helper for the old unit-test call site. The default bar
/// never emits the legacy badge; configured `mq` uses [`push_mq_widget`].
#[cfg(test)]
fn push_mq_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    push_mq_widget(model, items);
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
            vec![Seg::chip(
                Tok::Hue(Hue::Red),
                format!(" {} {blocked} PR ", crate::caps::active_glyphs().flag),
            )],
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

/// AI-account usage chip (`[usage]`): the most-consumed rate-limit window across
/// every tracked account, as `◔ 87% 2h14m`, toned green/amber/red at the
/// configured thresholds. Activating it opens the full per-account overlay.
///
/// Unlike the other badges this one is **not** silent when healthy. The rest of
/// the cluster follows "clean is quiet" because they report exceptions — a
/// failing queue, a full disk. This one is a live gauge: its whole job is to
/// answer "how much have I got left" at a glance, and a gauge that only appears
/// once you are nearly out has already stopped being useful. `[usage] statusbar
/// = false` turns it off.
///
/// Still silent when there is nothing to report: before the first poll lands, or
/// when every account is unreadable — a chip reading `0%` would be a lie about
/// an account we simply cannot see.
pub(crate) fn push_usage_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    let cfg = &model.usage_cfg;
    if !cfg.enabled || !cfg.statusbar {
        return;
    }
    let Some((idx, w)) = thegn_core::usage::peak_across(&model.usage) else {
        return;
    };
    let hue = match thegn_core::usage::tone_at(w.used_percent, cfg.warn_percent, cfg.crit_percent) {
        thegn_core::usage::UsageTone::Ok => Hue::Green,
        thegn_core::usage::UsageTone::Warn => Hue::Amber,
        thegn_core::usage::UsageTone::Crit => Hue::Red,
    };
    let g = crate::caps::active_glyphs();
    // The countdown is the actionable half — "91%" tells you to stop, "91%,
    // resets in 12m" tells you to get a coffee. Omitted when unknown rather
    // than padded, so the chip shrinks instead of showing an empty slot.
    let resets = thegn_core::usage::fmt_resets_in(w.resets_at, thegn_core::util::now())
        .map(|r| format!(" {r}"))
        .unwrap_or_default();
    let mut segs = vec![Seg::chip(
        Tok::Hue(hue),
        format!(" {} {:.0}%{resets} ", g.gauge, w.used_percent),
    )];
    // Which account is peaking only matters when there is more than one; with a
    // single account the label is noise on every frame.
    if model.usage.len() > 1
        && let Some(a) = model.usage.get(idx)
    {
        let short = a.short_label();
        let label = crate::seg::take_cols(&short, 14);
        segs.push(Seg::chip(
            Tok::Slot(crate::chrome::S::Dim),
            format!("{label} "),
        ));
    }
    items.push((BarItemId::Badge(BarBadge::Usage), segs));
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

    fn notif(
        id: i64,
        kind: thegn_core::notification::NotificationKind,
        path: &str,
        read: bool,
    ) -> thegn_core::notification::Notification {
        thegn_core::notification::Notification {
            id,
            kind,
            source_ref: "src".into(),
            message: "msg".into(),
            created_at_ms: 0,
            read,
            worktree_path: path.into(),
        }
    }

    /// The chip and the popup share one rollup: an unread alert row counts
    /// once even when its worktree already needs you; notice rows show only
    /// as the quiet inbox count; info and read rows never show.
    #[test]
    fn attention_badge_folds_inbox_alerts_into_one_chip() {
        use thegn_core::notification::NotificationKind as K;
        let mut model = FrameModel::default();
        model
            .sidebar_status
            .attention
            .insert("/wt/a".into(), score(AttentionTier::Waiting));
        model.panel.notifications = vec![
            notif(1, K::AgentFailed, "/wt/a", false), // covered by /wt/a ⇒ not double-counted
            notif(2, K::ProcessFailed, "", false),    // host-global alert ⇒ counts
            notif(3, K::TestFailed, "/wt/z", true),   // read ⇒ ignored
            notif(4, K::Mentioned, "", false),        // notice ⇒ not in the count
            notif(5, K::WorktreeCreated, "", false),  // info ⇒ never
        ];
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        let text = chip_text(&items);
        assert!(
            text.contains(" 2 "),
            "waiting worktree + host alert: {text:?}"
        );
        assert!(
            items[0].1[0].bg == Some(Tok::Hue(Hue::Red)),
            "an unread alert row makes the chip red"
        );

        // Only notices left ⇒ the quiet blue inbox count, not the hand.
        model.sidebar_status.attention.clear();
        model.panel.notifications = vec![notif(4, K::Mentioned, "", false)];
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(matches!(items[0].0, BarItemId::Badge(BarBadge::Attention)));
        assert_eq!(items[0].1[0].bg, Some(Tok::Hue(Hue::Blue)));
        assert!(chip_text(&items).contains(" 1 "));

        // Info-only ⇒ silent.
        model.panel.notifications = vec![notif(5, K::WorktreeCreated, "", false)];
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(items.is_empty());
    }

    /// A config override promoting a kind to `alert` must reach the chip the
    /// same way it reaches the counts (it used to be default-priority only).
    #[test]
    fn attention_badge_honours_effective_priority_override() {
        use thegn_core::notification::{NotificationKind as K, Priority};
        let mut model = FrameModel::default();
        model.panel.notifications = vec![notif(1, K::AgentDone, "", false)];
        model
            .panel
            .notification_priority
            .insert(K::AgentDone.as_str(), Priority::Alert);
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert_eq!(items[0].1[0].bg, Some(Tok::Hue(Hue::Red)));
    }

    /// The optimistic clear: marking rows read in the model drops the chip on
    /// the next frame (the rehydrate later lands on the same state).
    #[test]
    fn attention_badge_drops_on_optimistic_mark_read() {
        use thegn_core::notification::NotificationKind as K;
        let mut model = FrameModel::default();
        model.panel.notifications = vec![
            notif(1, K::AgentFailed, "", false),
            notif(2, K::Mentioned, "", false),
        ];
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert_eq!(items[0].1[0].bg, Some(Tok::Hue(Hue::Red)));
        model.panel.mark_read_where(|n| n.id == 1);
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert_eq!(
            items[0].1[0].bg,
            Some(Tok::Hue(Hue::Blue)),
            "only the notice left"
        );
        assert_eq!(model.panel.alert_notifications, 0);
        assert_eq!(model.panel.unread_notifications, 1);
        model.panel.mark_read_where(|_| true);
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(items.is_empty());
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
        // A crashed/wedged daemon renders red — visible without THEGN_LOG.
        assert_eq!(tone_for(DaemonChipState::Error), Tok::Hue(Hue::Red));
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

    fn usage_model(pcts: &[(&str, f32)]) -> FrameModel {
        use thegn_core::usage::{AccountUsage, UsageWindow};
        FrameModel {
            usage_cfg: thegn_core::config::UsageConfig::default(),
            usage: pcts
                .iter()
                .map(|(label, pct)| {
                    AccountUsage::ok(
                        "claude",
                        label,
                        None,
                        vec![UsageWindow::new("5h", *pct, None)],
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn usage_chip(model: &FrameModel) -> Option<(String, Seg)> {
        let mut items = Vec::new();
        push_usage_badge(model, &mut items);
        items.pop().map(|(_, segs)| {
            (
                segs.iter().map(|s| s.text.clone()).collect(),
                segs[0].clone(),
            )
        })
    }

    #[test]
    fn usage_badge_is_a_gauge_not_an_alarm() {
        // Unlike every other badge this one shows at healthy levels too — its
        // job is to answer "how much have I got left", and a gauge that only
        // appears once you are nearly out has stopped being one.
        let (text, seg) = usage_chip(&usage_model(&[("work", 12.0)])).expect("a chip");
        assert!(text.contains("12%"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Green))); // chips carry the tone as bg
        // Tone follows the configured thresholds.
        let (_, seg) = usage_chip(&usage_model(&[("work", 80.0)])).unwrap();
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Amber)));
        let (_, seg) = usage_chip(&usage_model(&[("work", 95.0)])).unwrap();
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Red)));
    }

    #[test]
    fn usage_badge_reports_the_worst_window_and_names_it() {
        // With one account the label would be noise on every frame.
        let (text, _) = usage_chip(&usage_model(&[("solo", 40.0)])).unwrap();
        assert!(!text.contains("solo"), "{text}");
        // With several, the chip has to say WHICH one is peaking.
        let (text, _) = usage_chip(&usage_model(&[("calm", 10.0), ("hot", 91.0)])).unwrap();
        assert!(text.contains("91%"), "{text}");
        assert!(text.contains("hot"), "{text}");
    }

    #[test]
    fn usage_badge_is_silent_when_it_has_nothing_honest_to_say() {
        // Before the first poll: no chip. A `0%` here would be a claim about
        // accounts we have not read yet.
        assert!(usage_chip(&usage_model(&[])).is_none());
        // Every account unreadable — same reasoning.
        let model = FrameModel {
            usage_cfg: thegn_core::config::UsageConfig::default(),
            usage: vec![thegn_core::usage::AccountUsage::unavailable(
                "claude",
                "x",
                "network off",
            )],
            ..Default::default()
        };
        assert!(usage_chip(&model).is_none());
        // Turned off two different ways.
        let mut off = usage_model(&[("work", 50.0)]);
        off.usage_cfg.statusbar = false;
        assert!(usage_chip(&off).is_none());
        let mut disabled = usage_model(&[("work", 50.0)]);
        disabled.usage_cfg.enabled = false;
        assert!(usage_chip(&disabled).is_none());
    }

    #[test]
    fn mq_badge_hues_by_severity_and_shows_idle_queues() {
        // Empty queue: silent (clean is quiet).
        assert!(mq_chip_for(&[]).is_none());
        // Merely queued / held at ready: a quiet dim chip — the queue must be
        // discoverable even when nothing is running or failing.
        let (text, seg) = mq_chip_for(&["queued", "ready"]).unwrap();
        assert!(text.contains("2 ") && text.contains("MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Slot(crate::chrome::S::Dim))); // chips carry the tone as bg
        // Working (agent included) wins over idle: amber.
        let (text, seg) = mq_chip_for(&["queued", "agent_running"]).unwrap();
        assert!(text.contains("1 ") && text.contains("MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Amber))); // chips carry the tone as bg
        // Anything blocked (needs_human included) wins over all: red marker.
        let (text, seg) = mq_chip_for(&["queued", "folding", "needs_human"]).unwrap();
        assert!(text.contains("1 ") && text.contains("MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Red))); // chips carry the tone as bg
        // A gate that could not run is blocked too (the section shows it in
        // amber as "gate could not run"); the chip used to go silent on it.
        let (text, seg) = mq_chip_for(&["gate_error"]).unwrap();
        assert!(text.contains("1 ") && text.contains("MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Red)));
        // Only landed rows: nothing left to signal.
        assert!(mq_chip_for(&["landed"]).is_none());
    }

    #[test]
    fn merge_queue_is_only_emitted_when_configured_as_a_widget() {
        let mut model = FrameModel::default();
        model.panel.merge_queue = vec![mq_row("ready")];

        let default_ids: Vec<_> = crate::chrome::statusbar_items(&model)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(!default_ids.contains(&BarItemId::Badge(BarBadge::MergeQueue)));
        assert!(!default_ids.contains(&BarItemId::Widget("mq".into())));

        model.bars.bottom_right = vec!["mq".into()];
        let configured = crate::chrome::statusbar_items(&model);
        let item = configured
            .iter()
            .find(|(id, _)| *id == BarItemId::Widget("mq".into()))
            .expect("configured mq widget");
        assert!(item.1[0].text.contains("1"));
        assert_eq!(item.1[0].bg, Some(Tok::Slot(crate::chrome::S::Dim)));
        assert!(
            !configured
                .iter()
                .any(|(id, _)| *id == BarItemId::Badge(BarBadge::MergeQueue))
        );
    }
}
