//! The daemon's PR poll and mentions producers (THE-2208, THE-2210).
//! Blocking; the owner runs them in `spawn_blocking`.
//!
//! **PR poll.** Each tick refreshes the local worktrees whose cached PR
//! (`pr_cache`, written by `hydrate::pr_refresh_blocking`) is OPEN, at most
//! [`PR_POLL_CAP`] per tick, rotating through the rest on later ticks. Each
//! refresh is the same body the UI runs (`PrSite::Daemon`), so the events
//! (`pr_state_changed`, `pr_checks`, `pr_review_requested`, `merge_landed`)
//! are unchanged. While offline only one worktree is refreshed: it doubles as
//! the recovery probe that flips the daemon's connectivity holder back.
//!
//! A worktree with no cached PR (or a closed or merged one) is NOT polled by
//! the daemon, and the UI never yields it (`hydrate::daemon_polls_pr` is the
//! shared predicate). So a PR opened outside thegn appears after the UI's
//! next refresh of that worktree, and the daemon picks it up from then on.
//! The repo's open-PR list (`pr_branch_cache`, the `pr_linked` producer) stays
//! UI-hosted.
//!
//! **Mentions.** Each tick polls the registered repo roots (most recent
//! first, at most [`MENTIONS_CAP`]) through `hydrate::mentions_blocking`, which
//! keeps the shared `gh_mentions` ui_state throttle (one forge call per repo
//! per five minutes) and the emit-once `mentioned` rows.
#![allow(
    dead_code,
    reason = "the ticks are driven by the producer owner (b161 lane 5)"
)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use thegn_core::connectivity::Connectivity;
use thegn_core::store::{CacheStore, WorkspaceStore};

use super::{TickCtx, TickOutcome};
use crate::hydrate::{PrRefreshOutcome, PrSite};

/// Most worktrees one PR tick refreshes.
///
/// 8 x ~3 forge calls per >=30 s tick stays under GitHub's 5,000/h with room
/// for the UI; rotation covers the rest.
pub(crate) const PR_POLL_CAP: usize = 8;

/// Most repo roots one mentions tick polls.
pub(crate) const MENTIONS_CAP: usize = crate::hydrate::DAEMON_MENTION_ROOTS;

/// Rotation cursor into the eligible worktree list (sorted by path).
static PR_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// One `pr_cache` row as the PR poll's selector sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrCandidate {
    /// The cache key: the worktree's host path.
    pub worktree: String,
    /// The worktree resolves to a local checkout (`GitLoc::Local`).
    pub local: bool,
    /// The cached PR's state (`OPEN`, `MERGED`, `CLOSED`), or `None` when the
    /// cached panel holds no PR.
    pub cached_state: Option<String>,
}

/// Pick this tick's worktrees: eligible ones only (a local checkout with an
/// OPEN cached PR), sorted by path, at most `cap`, starting at `cursor` and
/// wrapping. Returns the picks and the next cursor. Pure.
pub(crate) fn select_pr_worktrees(
    candidates: &[PrCandidate],
    cursor: usize,
    cap: usize,
) -> (Vec<String>, usize) {
    let mut eligible: Vec<&str> = candidates
        .iter()
        .filter(|c| crate::hydrate::daemon_polls_pr(c.cached_state.as_deref(), c.local))
        .map(|c| c.worktree.as_str())
        .collect();
    eligible.sort_unstable();
    eligible.dedup();
    let len = eligible.len();
    if len == 0 || cap == 0 {
        return (Vec::new(), 0);
    }
    let start = cursor % len;
    let take = cap.min(len);
    let picks = (0..take)
        .map(|i| eligible[(start + i) % len].to_owned())
        .collect();
    (picks, (start + take) % len)
}

/// The cached PR state in one `pr_cache` JSON row (a serialized `PrPanel`).
fn cached_pr_state(json: &str) -> Option<String> {
    let panel: thegn_core::forge::model::PrPanel = serde_json::from_str(json).ok()?;
    match panel.state {
        thegn_core::forge::model::PanelState::Pr(pr) => Some(pr.state),
        _ => None,
    }
}

/// How many worktrees a tick may refresh at this connectivity.
fn pr_cap(connectivity: Connectivity) -> usize {
    if connectivity == Connectivity::Offline {
        1
    } else {
        PR_POLL_CAP
    }
}

/// One PR poll tick (checks, review requests, landings).
pub(crate) fn tick_pr(ctx: &TickCtx) -> TickOutcome {
    let Ok(db) = thegn_core::db::Db::open() else {
        return TickOutcome::Skipped("state DB unavailable");
    };
    let Ok(rows) = db.list_pr_cache() else {
        return TickOutcome::Skipped("pr_cache unreadable");
    };
    if rows.is_empty() {
        return TickOutcome::Skipped("no cached PRs");
    }
    let locations: HashMap<String, String> = db
        .worktrees()
        .unwrap_or_default()
        .into_iter()
        .map(|w| (w.worktree, w.location))
        .collect();
    let candidates: Vec<PrCandidate> = rows
        .into_iter()
        .map(|(worktree, json, _fetched_at)| {
            let location = locations.get(&worktree).map(String::as_str);
            let local = matches!(
                thegn_core::remote::GitLoc::from_db(&worktree, location),
                thegn_core::remote::GitLoc::Local(_)
            ) && Path::new(&worktree).is_dir();
            PrCandidate {
                cached_state: cached_pr_state(&json),
                local,
                worktree,
            }
        })
        .collect();
    drop(db);

    let cap = pr_cap(thegn_core::connectivity::current());
    let cursor = PR_CURSOR.load(Ordering::Relaxed);
    let (picks, next) = select_pr_worktrees(&candidates, cursor, cap);
    PR_CURSOR.store(next, Ordering::Relaxed);
    if picks.is_empty() {
        return TickOutcome::Skipped("no open PRs on local worktrees");
    }
    let work = picks
        .iter()
        .filter(|wt| {
            crate::hydrate::pr_refresh_blocking(Path::new(wt.as_str()), &ctx.cfg, PrSite::Daemon)
                == PrRefreshOutcome::Fetched
        })
        .count();
    TickOutcome::Done { work }
}

/// One mentions tick.
pub(crate) fn tick_mentions(ctx: &TickCtx) -> TickOutcome {
    if !ctx.cfg.notifications.github_mentions {
        return TickOutcome::Skipped("github_mentions off");
    }
    if thegn_core::connectivity::current() == Connectivity::Offline {
        return TickOutcome::Skipped("offline");
    }
    let Ok(db) = thegn_core::db::Db::open() else {
        return TickOutcome::Skipped("state DB unavailable");
    };
    let roots = crate::hydrate::daemon_mention_roots(&db);
    drop(db);
    let work = roots
        .iter()
        .map(|root| crate::hydrate::mentions_blocking(Path::new(root), &ctx.cfg))
        .sum();
    TickOutcome::Done { work }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(worktree: &str, local: bool, state: Option<&str>) -> PrCandidate {
        PrCandidate {
            worktree: worktree.into(),
            local,
            cached_state: state.map(Into::into),
        }
    }

    #[test]
    fn selection_keeps_open_local_worktrees_only() {
        let cands = [
            cand("/wt/open", true, Some("OPEN")),
            cand("/wt/merged", true, Some("MERGED")),
            cand("/wt/closed", true, Some("CLOSED")),
            cand("/wt/nopr", true, None),
            cand("/wt/remote-open", false, Some("OPEN")),
            cand("/wt/lower", true, Some("open")),
        ];
        let (picks, _) = select_pr_worktrees(&cands, 0, PR_POLL_CAP);
        assert_eq!(picks, ["/wt/lower", "/wt/open"]);
    }

    #[test]
    fn selection_is_bounded_and_rotates_through_every_worktree() {
        let cands: Vec<PrCandidate> = (0..20)
            .map(|i| cand(&format!("/wt/{i:03}"), true, Some("OPEN")))
            .collect();
        let (first, c1) = select_pr_worktrees(&cands, 0, PR_POLL_CAP);
        assert_eq!(first.len(), PR_POLL_CAP);
        assert_eq!(first[0], "/wt/000");
        assert_eq!(c1, 8);
        let (second, c2) = select_pr_worktrees(&cands, c1, PR_POLL_CAP);
        assert_eq!(second.len(), PR_POLL_CAP);
        assert_eq!(second[0], "/wt/008");
        assert_eq!(c2, 16);
        let (third, c3) = select_pr_worktrees(&cands, c2, PR_POLL_CAP);
        assert_eq!(third.len(), PR_POLL_CAP);
        assert_eq!(third[0], "/wt/016");
        assert_eq!(third[3], "/wt/019");
        assert_eq!(third[4], "/wt/000", "wraps to the start");
        assert_eq!(c3, (16 + 8) % 20);
        let mut seen: std::collections::HashSet<String> = first.into_iter().collect();
        seen.extend(second);
        seen.extend(third);
        assert_eq!(seen.len(), 20, "three ticks cover every worktree");
    }

    #[test]
    fn selection_survives_a_shrunken_list_and_empty_input() {
        let cands = [
            cand("/wt/a", true, Some("OPEN")),
            cand("/wt/b", true, Some("OPEN")),
        ];
        // A cursor left over from a longer list wraps instead of panicking.
        let (picks, next) = select_pr_worktrees(&cands, 97, PR_POLL_CAP);
        assert_eq!(picks.len(), 2);
        assert_eq!(next, 1);
        assert_eq!(select_pr_worktrees(&[], 5, PR_POLL_CAP), (Vec::new(), 0));
        assert_eq!(select_pr_worktrees(&cands, 0, 0), (Vec::new(), 0));
    }

    #[test]
    fn offline_polls_one_worktree_as_the_recovery_probe() {
        assert_eq!(pr_cap(Connectivity::Offline), 1);
        assert_eq!(pr_cap(Connectivity::Online), PR_POLL_CAP);
        assert_eq!(pr_cap(Connectivity::Unknown), PR_POLL_CAP);
    }

    #[test]
    fn cached_state_reads_the_pr_panel_row() {
        assert_eq!(cached_pr_state("not json"), None);
        assert_eq!(
            cached_pr_state(r#"{"kind":"no_pr","worktree":"/wt","branch":"b","fetched_at":1}"#),
            None
        );
    }

    mod yield_tests {
        use crate::hydrate::{
            MENTION_POLL_SECS, PrSite, daemon_polls_pr, mention_poll_due, should_fetch,
        };

        #[test]
        fn the_ui_yields_its_forge_calls_only_to_a_covering_live_daemon() {
            // (daemon_owns, daemon_covers, ui_offline) -> UI fetches?
            let table = [
                ((false, false, false), true),
                ((false, true, false), true),
                ((true, false, false), true),
                ((true, true, false), false),
                ((true, true, true), true),
                ((false, false, true), true),
            ];
            for ((owns, covers, offline), want) in table {
                assert_eq!(
                    should_fetch(PrSite::Ui, owns, covers, offline),
                    want,
                    "owns={owns} covers={covers} offline={offline}"
                );
            }
        }

        #[test]
        fn the_daemon_always_fetches() {
            for owns in [false, true] {
                for covers in [false, true] {
                    for offline in [false, true] {
                        assert!(should_fetch(PrSite::Daemon, owns, covers, offline));
                    }
                }
            }
        }

        #[test]
        fn the_daemon_covers_local_open_prs_only() {
            assert!(daemon_polls_pr(Some("OPEN"), true));
            assert!(daemon_polls_pr(Some("open"), true));
            assert!(!daemon_polls_pr(Some("OPEN"), false), "remote");
            assert!(!daemon_polls_pr(Some("MERGED"), true));
            assert!(!daemon_polls_pr(Some("CLOSED"), true));
            assert!(
                !daemon_polls_pr(None, true),
                "no cached PR: the UI keeps polling"
            );
        }

        #[test]
        fn the_mentions_throttle_is_five_minutes_in_seconds() {
            let now = 1_800_000_000;
            assert!(mention_poll_due(None, now), "never polled");
            assert!(!mention_poll_due(Some(now), now));
            assert!(!mention_poll_due(Some(now - MENTION_POLL_SECS + 1), now));
            assert!(mention_poll_due(Some(now - MENTION_POLL_SECS), now));
            assert!(mention_poll_due(Some(now - 10 * 60), now));
            assert_eq!(MENTION_POLL_SECS, 300);
            // A stamp from the future (clock step) does not poll hot.
            assert!(!mention_poll_due(Some(now + 60), now));
        }
    }
}
