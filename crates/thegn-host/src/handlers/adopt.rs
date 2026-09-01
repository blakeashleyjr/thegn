//! The `adopt_session` intent drain — grafting a daemon session nobody is
//! showing into a real pane.
//!
//! # Why this exists
//!
//! `sessions.open --adopt` has written `adopt_session` mailbox rows since the
//! roster change landed, and **nothing consumed them**: the only occurrences in
//! the tree were the producer (`daemon/service.rs`), the payload type and the
//! docs. The intent was inert, so a stage agent launched from outside the UI
//! stayed headless forever and the rows accumulated in the `intents` table with
//! no reader. This module is the missing consumer — it is what makes the
//! pipeline's "each stage agent appears as a live pane" true.
//!
//! # The door it reuses
//!
//! There is exactly ONE way a daemon-owned session becomes a pane in this
//! codebase: [`crate::panes::Panes::spawn_daemon_backed`] with `attach =
//! Some(session)` — the warm-reattach branch `materialize_with_specs` takes for
//! a persisted `provider = "daemon"` leaf. Adoption goes through that same call,
//! so an adopted pane is byte-for-byte an ordinary daemon pane: same relay, same
//! reconnect ladder, same `pane_sessions` capture at persist (so it warm-
//! reattaches after a restart like any other), same detach-on-drop semantics. No
//! second attachment path was invented.
//!
//! # Staleness
//!
//! `IntentStore::take_intents` is claim-and-delete over the
//! whole kind, so the drain is **drain-all**: nothing is left behind to
//! accumulate, which is the leak this fixes. Drain-all alone is not enough
//! though — rows written while no compositor was running would all be applied at
//! once on the next launch, spraying panes for sessions that died hours ago (the
//! reattach's fresh-session fallback would even give them a live shell). So a
//! row older than [`MAX_ADOPT_AGE_SECS`] is **dropped, not applied**: claimed,
//! logged, and forgotten.

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::panes::Panes;
use crate::session::Session;
use thegn_core::models::AdoptIntent;
use thegn_core::store::IntentRow;

/// How stale an `adopt_session` row may be and still be honoured (seconds).
///
/// Five minutes: long enough to cover a session opened while the compositor was
/// mid-launch or mid-switch, short enough that a mailbox filled overnight with
/// no UI running does not erupt into panes at the next start.
pub(crate) const MAX_ADOPT_AGE_SECS: i64 = 300;

/// What one intent row resolved to. Split out from the side-effecting drain so
/// the policy — staleness, deduplication, which group — is unit-testable
/// without a session, a daemon or a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdoptPlan {
    /// Graft `session` into `group`'s active tab; `focus` switches to it.
    Graft {
        session: String,
        group: usize,
        focus: bool,
        tab: bool,
    },
    /// Claimed and dropped, with the reason for the log/status line.
    Drop(DropReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// The payload did not parse as an [`AdoptIntent`].
    Malformed,
    /// Older than [`MAX_ADOPT_AGE_SECS`].
    Stale,
    /// A pane in this session is already showing that daemon session.
    AlreadyShown,
    /// The named worktree is not a resident group — the user is in another
    /// workspace, and grafting across one would need a cold resurrect.
    NoGroup(String),
}

/// Decide what to do with one claimed row. Pure.
///
/// `live_sessions` is every daemon session id a pane is currently showing;
/// `groups` the resident worktree group paths in session order; `active` the
/// active group index (where a worktree-less intent lands).
pub(crate) fn plan(
    row: &IntentRow,
    now_secs: i64,
    live_sessions: &[String],
    groups: &[String],
    active: usize,
) -> AdoptPlan {
    let Ok(intent) = serde_json::from_str::<AdoptIntent>(&row.payload) else {
        return AdoptPlan::Drop(DropReason::Malformed);
    };
    if intent.session.trim().is_empty() {
        return AdoptPlan::Drop(DropReason::Malformed);
    }
    // `created_at` is unix seconds (`util::now`). A row from the future (clock
    // skew) is treated as fresh, never as infinitely stale.
    if now_secs.saturating_sub(row.created_at) > MAX_ADOPT_AGE_SECS {
        return AdoptPlan::Drop(DropReason::Stale);
    }
    if live_sessions.contains(&intent.session) {
        return AdoptPlan::Drop(DropReason::AlreadyShown);
    }
    let group = match intent
        .worktree
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        Some(wt) => match groups.iter().position(|g| g == wt) {
            Some(gi) => gi,
            None => return AdoptPlan::Drop(DropReason::NoGroup(wt.to_string())),
        },
        // No worktree recorded: the session belongs wherever the user is. The
        // active group is the only honest answer the compositor has — it cannot
        // see the daemon session's own cwd from here.
        None => active,
    };
    if group >= groups.len() {
        return AdoptPlan::Drop(DropReason::Malformed);
    }
    AdoptPlan::Graft {
        session: intent.session,
        group,
        focus: intent.focus,
        tab: intent.tab,
    }
}

/// Apply every claimed `adopt_session` row. Returns `true` when the frame
/// changed.
///
/// Runs on the loop, but does no I/O of its own: `spawn_daemon_backed` builds a
/// `Stream` pane whose relay task connects to the daemon **inside the task**, so
/// the attach never blocks a frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    rows: Vec<IntentRow>,
    now_secs: i64,
    session: &mut Session,
    panes: &mut Panes,
    model: &mut FrameModel,
    sb: &mut crate::run::SidebarState,
    cfg: &thegn_core::config::Config,
    center: Rect,
    need_relayout: &mut bool,
) -> bool {
    if rows.is_empty() {
        return false;
    }
    // A daemon route that isn't installed can't adopt anything: say so once
    // rather than dropping every row silently. The sessions stay alive and
    // headless, which is the documented no-compositor outcome anyway.
    if !panes.daemon_route_enabled() {
        tracing::warn!(
            target: "thegn::daemon",
            claimed = rows.len(),
            "adopt_session intents claimed but [daemon] is disabled; sessions stay headless"
        );
        model.status = "adopt: [daemon] is disabled — session left headless".into();
        return true;
    }
    let live_sessions: Vec<String> = panes
        .table
        .values()
        .filter_map(|p| p.provider_session())
        .filter(|ps| ps.provider == "daemon")
        .map(|ps| ps.session)
        .collect();
    let groups: Vec<String> = session.worktrees.iter().map(|g| g.path.clone()).collect();

    let mut changed = false;
    let mut adopted = 0usize;
    let mut focus_to: Option<usize> = None;
    for row in rows {
        match plan(&row, now_secs, &live_sessions, &groups, session.active) {
            AdoptPlan::Drop(reason) => {
                tracing::debug!(
                    target: "thegn::daemon",
                    intent = row.id, ?reason,
                    "adopt_session intent dropped"
                );
                if let DropReason::NoGroup(wt) = reason {
                    model.status = format!("adopt: {wt} is not open here — session left headless");
                    changed = true;
                }
            }
            AdoptPlan::Graft {
                session: sid,
                group,
                focus,
                tab,
            } => {
                // The intent names no tab (the CLI has no tab notion), so it
                // lands in the group's ACTIVE tab; `graft` itself now takes
                // `(gi, ti)` so the attach-on-open drain can target any tab.
                let tab_index = if tab {
                    session.worktrees[group].add_tab()
                } else {
                    session.worktrees[group].active_tab
                };
                if graft(&sid, group, tab_index, session, panes, cfg, center, None) {
                    adopted += 1;
                    changed = true;
                    *need_relayout = true;
                    if focus {
                        focus_to = Some(group);
                    }
                } else {
                    model.status = format!("adopt: could not attach session {sid}");
                    changed = true;
                }
            }
        }
    }
    // `focus: false` (the default) must never yank the user out of what they
    // are doing — a fan-out of eight stage agents moves nothing.
    if let Some(gi) = focus_to
        && gi < session.worktrees.len()
    {
        session.switch_to(gi);
    }
    if adopted > 0 {
        crate::run::refresh_tab_model(model, session, sb);
        if model.status.is_empty() {
            model.status = format!(
                "adopted {adopted} agent session{}",
                if adopted == 1 { "" } else { "s" }
            );
        }
    }
    changed
}

/// Attach one session as a fresh leaf in tab `(gi, ti)` of group `gi`.
/// `true` on success. Shared by the `--adopt` drain and the attach-on-open
/// surplus path (`handlers::worktree_attach`) — the one split-a-session-in
/// primitive. `label` overrides the fallback argv's program name for the pane
/// label (the surplus path passes the daemon-recorded agent program; the
/// adopt drain has none and keeps the argv-derived label).
#[allow(clippy::too_many_arguments)] // the split primitive's full context; grouped structs would obscure it
pub(crate) fn graft(
    sid: &str,
    gi: usize,
    ti: usize,
    session: &mut Session,
    panes: &mut Panes,
    cfg: &thegn_core::config::Config,
    center: Rect,
    label: Option<&str>,
) -> bool {
    let Some(g) = session.worktrees.get_mut(gi) else {
        return false;
    };
    let cwd = (!g.path.is_empty() && std::path::Path::new(&g.path).is_dir())
        .then(|| std::path::PathBuf::from(&g.path));
    // The fallback spec the relay's reconnect ladder uses if this session turns
    // out to be dead — the same "degrade to a fresh daemon shell" contract the
    // warm-reattach branch of `materialize_with_specs` relies on.
    let (cmd, args) = crate::panes::pane_shell_argv(cfg, "");
    let mut argv = vec![cmd];
    argv.extend(args);
    let id = match panes.spawn_daemon_backed(
        &argv,
        cwd.as_deref(),
        &[],
        center,
        Some(sid.to_string()),
        label,
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(target: "thegn::daemon", "adopt of session {sid} failed: {e}");
            return false;
        }
    };
    // Clamp like `active_tab_mut` so an out-of-range index degrades to the
    // last tab instead of dropping the session on the floor.
    let ti = ti.min(g.tabs.len().saturating_sub(1));
    let Some(tab) = g.tabs.get_mut(ti) else {
        panes.table.remove(&id);
        return false;
    };
    if !tab
        .center
        .split(tab.focused_pane, crate::center::Dir::Row, id)
    {
        // Nothing to split against (an empty tree). Drop the pane rather than
        // leaving it orphaned outside the tab it was meant to join.
        panes.table.remove(&id);
        return false;
    }
    tab.focused_pane = id;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(payload: &str, created_at: i64) -> IntentRow {
        IntentRow {
            id: 1,
            kind: "adopt_session".into(),
            payload: payload.into(),
            created_at,
        }
    }

    const GROUPS: [&str; 2] = ["/wt/a", "/wt/b"];

    fn groups() -> Vec<String> {
        GROUPS.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_worktree_intent_lands_in_that_group() {
        let r = row(r#"{"session":"s1","worktree":"/wt/b","focus":true}"#, 100);
        assert_eq!(
            plan(&r, 100, &[], &groups(), 0),
            AdoptPlan::Graft {
                session: "s1".into(),
                group: 1,
                focus: true,
                tab: false
            }
        );
    }

    #[test]
    fn a_worktreeless_intent_lands_in_the_active_group_and_defaults_to_no_focus() {
        let r = row(r#"{"session":"s1"}"#, 100);
        assert_eq!(
            plan(&r, 100, &[], &groups(), 1),
            AdoptPlan::Graft {
                session: "s1".into(),
                group: 1,
                focus: false,
                tab: false
            },
            "focus defaults false so a fan-out never yanks the user away"
        );
    }

    #[test]
    fn a_stale_row_is_claimed_and_dropped() {
        let r = row(r#"{"session":"s1"}"#, 0);
        assert_eq!(
            plan(&r, MAX_ADOPT_AGE_SECS + 1, &[], &groups(), 0),
            AdoptPlan::Drop(DropReason::Stale)
        );
        // Exactly at the cutoff is still fresh.
        assert!(matches!(
            plan(&r, MAX_ADOPT_AGE_SECS, &[], &groups(), 0),
            AdoptPlan::Graft { .. }
        ));
        // A row stamped in the future (clock skew) is fresh, never stale.
        assert!(matches!(
            plan(&row(r#"{"session":"s1"}"#, 10_000), 0, &[], &groups(), 0),
            AdoptPlan::Graft { .. }
        ));
    }

    #[test]
    fn a_session_already_on_screen_is_not_adopted_twice() {
        let r = row(r#"{"session":"s1"}"#, 100);
        assert_eq!(
            plan(&r, 100, &["s1".to_string()], &groups(), 0),
            AdoptPlan::Drop(DropReason::AlreadyShown)
        );
    }

    #[test]
    fn an_unknown_worktree_is_dropped_with_its_path() {
        let r = row(r#"{"session":"s1","worktree":"/wt/zz"}"#, 100);
        assert_eq!(
            plan(&r, 100, &[], &groups(), 0),
            AdoptPlan::Drop(DropReason::NoGroup("/wt/zz".into()))
        );
    }

    #[test]
    fn malformed_and_empty_payloads_are_dropped_not_applied() {
        for bad in [
            "not json",
            "{}",
            r#"{"session":""}"#,
            r#"{"session":"   "}"#,
        ] {
            assert_eq!(
                plan(&row(bad, 100), 100, &[], &groups(), 0),
                AdoptPlan::Drop(DropReason::Malformed),
                "payload {bad:?}"
            );
        }
    }

    #[test]
    fn an_out_of_range_active_index_drops_rather_than_panicking() {
        let r = row(r#"{"session":"s1"}"#, 100);
        assert_eq!(
            plan(&r, 100, &[], &groups(), 9),
            AdoptPlan::Drop(DropReason::Malformed)
        );
        // …and with no groups at all there is nowhere to land.
        assert_eq!(
            plan(&r, 100, &[], &[], 0),
            AdoptPlan::Drop(DropReason::Malformed)
        );
    }
}
