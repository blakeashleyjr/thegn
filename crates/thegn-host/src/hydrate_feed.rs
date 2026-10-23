//! Notification-feed population for model hydration (extracted from the
//! size-capped `hydrate.rs`). Runs on the hydration worker, never the loop.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use thegn_core::db::Db;
use thegn_core::notification::Notification;
use thegn_core::store::{NotificationStore, WorkspaceStore};

static NOTIFY_WRITE_ISSUED: AtomicU64 = AtomicU64::new(0);
static NOTIFY_WRITE_COMMITTED: AtomicU64 = AtomicU64::new(0);
static NOTIFY_WRITE_FINISHED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

pub(crate) fn issue_notification_write() -> u64 {
    NOTIFY_WRITE_ISSUED.fetch_add(1, Ordering::AcqRel) + 1
}

pub(crate) fn commit_notification_write(epoch: u64) {
    let mut finished = NOTIFY_WRITE_FINISHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    finished.push(epoch);
    let mut committed = NOTIFY_WRITE_COMMITTED.load(Ordering::Relaxed);
    while let Some(next) = committed.checked_add(1)
        && let Some(index) = finished
            .iter()
            .position(|finished_epoch| *finished_epoch == next)
    {
        finished.remove(index);
        committed = next;
    }
    NOTIFY_WRITE_COMMITTED.store(committed, Ordering::Release);
}

pub(crate) fn committed_notification_epoch() -> u64 {
    NOTIFY_WRITE_COMMITTED.load(Ordering::Acquire)
}

/// The ERROR count carried by the most recent existing `log:thegn`
/// notification, parsed from its `"{n} error(s) in thegn.log"` message.
/// `None` when there is no such notification (or its message doesn't lead with
/// an integer). `notifs` is newest-first (see [`get_all_notifications`]), so the
/// first match is the newest.
///
/// [`get_all_notifications`]: thegn_core::store::NotificationStore::get_all_notifications
pub(crate) fn last_logged_error_count(notifs: &[Notification]) -> Option<usize> {
    notifs
        .iter()
        .find(|n| n.source_ref == "log:thegn")
        .and_then(|n| n.message.split_whitespace().next())
        .and_then(|tok| tok.parse::<usize>().ok())
}

/// Emit a `log_error` notification only when the ERROR count has *grown* beyond
/// what was last notified — i.e. genuinely new errors appeared. A persistent,
/// unchanged set of errors emits nothing, so a previously-read notification
/// stays read (the append-only log otherwise re-lit the badge forever).
///
/// Option ordering makes `None < Some(n)`, so the first-ever error still fires.
///
/// When the live log has *no* errors (rotated/truncated away), mark any lingering
/// unread `log:thegn` row read so the badge clears — otherwise a stale row stays
/// clickable and drills into an error-free log ("no matching log lines").
///
/// `surface` is the `[notifications] surface_self_log_errors` dev flag: when
/// false (the default) thegn's own log errors are never surfaced as user
/// notifications — but the stale-clear path still runs, so flipping the flag off
/// clears any dev-mode rows left behind.
pub(crate) fn maybe_emit_log_error(
    db: &Db,
    notifs: &[Notification],
    error_count: usize,
    surface: bool,
) {
    if error_count == 0 {
        for id in stale_log_notifs_to_clear(notifs, error_count) {
            // best-effort: DB is a cache; a failed mark just re-attempts next refresh.
            let _ = db.mark_notification_read(id);
        }
        return;
    }
    if surface && Some(error_count) > last_logged_error_count(notifs) {
        let msg = format!(
            "{} error{} in thegn.log",
            error_count,
            if error_count == 1 { "" } else { "s" }
        );
        let _ = crate::automation_events::emit(db, "log_error", "log:thegn", &msg, ""); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
}

/// The `log:thegn` notification ids to mark read when the live log carries no
/// errors — a rotated/truncated log otherwise leaves a stale, unread badge that
/// drills into an error-free log. Returns nothing while errors remain.
fn stale_log_notifs_to_clear(notifs: &[Notification], error_count: usize) -> Vec<i64> {
    if error_count > 0 {
        return Vec::new();
    }
    notifs
        .iter()
        .filter(|n| n.source_ref == "log:thegn" && !n.read)
        .map(|n| n.id)
        .collect()
}

/// Full notification list for the inbox panel; badge counts are derived from
/// it by effective priority (Info kinds never count; the red flag is
/// Alert-only). Scoped to this repo's own worktrees by default (host-global
/// notifications, with an empty `worktree_path`, always show); the System-tab
/// "all" toggle reveals every worktree's — so a sibling repo's error doesn't
/// leak here or light the badge.
pub(crate) fn populate_notifications(
    db: &Db,
    repo_root: &std::path::Path,
    app_cfg: &thegn_core::config::Config,
    panel: &mut crate::panel::PanelData,
) {
    panel.notifications_epoch = committed_notification_epoch();
    // Scope BEFORE capping: with the cap applied first, a noisy sibling repo
    // (50+ rows in the window) pushed this repo's rows out entirely — the
    // ⚑/✉ chips read 0 while the ✋ chip and the sidebar badge still counted.
    if let Ok(mut notifications) = db.get_inbox_notifications(FETCH_CAP) {
        use thegn_core::notification::Priority;
        if !crate::panel::scope::system_all() {
            let repo_paths = crate::hydrate::repo_worktree_paths(db, repo_root);
            // The scope rule (and why it is fail-open) lives in
            // `thegn_core::notification_scope`; "clear all" projects the same
            // function, so the two cannot drift apart again (THE-68).
            let all_known: std::collections::HashSet<String> = db
                .worktrees()
                .map(|wts| {
                    thegn_core::notification_scope::registry_known_paths(
                        wts.iter()
                            .map(|w| (w.worktree.as_str(), w.repo_root.as_str())),
                    )
                })
                .unwrap_or_default();
            notifications.retain(|n| {
                thegn_core::notification_scope::shows_in_repo_inbox(
                    &n.worktree_path,
                    &repo_paths,
                    &all_known,
                )
            });
        }
        // Counts are over the whole scoped set; only the list is capped.
        let unread = notifications.iter().filter(|n| !n.read);
        let (mut alert, mut counted) = (0usize, 0usize);
        for n in unread {
            match app_cfg.notifications.priority_of(n.kind) {
                Priority::Alert => {
                    alert += 1;
                    counted += 1;
                }
                Priority::Notice => counted += 1,
                Priority::Info => {}
            }
        }
        panel.alert_notifications = alert;
        panel.unread_notifications = counted;
        panel.notification_priority = thegn_core::notification::NotificationKind::ALL
            .iter()
            .map(|k| (k.as_str(), app_cfg.notifications.priority_of(*k)))
            .collect();
        let (notifications, hidden_read) = cap_inbox(notifications, INBOX_READ_ROWS);
        panel.notifications_hidden_read = hidden_read;
        panel.notifications = notifications;
    }
}

/// Read rows retained in the inbox, in addition to every fetched unread row.
const INBOX_READ_ROWS: usize = 50;
/// Rows read from the DB before scoping — generous so scoping never starves
/// the active repo; the table is pruned of old read rows on a schedule.
const FETCH_CAP: usize = 2000;

/// Keep every unread row plus the newest `read_cap` read rows, newest first.
/// Returns the number of read rows omitted from the visible inbox.
fn cap_inbox(mut rows: Vec<Notification>, read_cap: usize) -> (Vec<Notification>, usize) {
    rows.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    let read_total = rows.iter().filter(|row| row.read).count();
    let mut kept_reads = 0;
    rows.retain(|row| {
        if !row.read {
            true
        } else if kept_reads < read_cap {
            kept_reads += 1;
            true
        } else {
            false
        }
    });
    let hidden_read = read_total.saturating_sub(kept_reads);
    (rows, hidden_read)
}

/// Sidebar sort weight for an issue priority (urgent → high → … → none).
pub(crate) fn issue_urgency(p: thegn_core::issue::IssuePriority) -> u8 {
    use thegn_core::issue::IssuePriority as P;
    match p {
        P::Urgent => 4,
        P::High => 3,
        P::Medium => 2,
        P::Low => 1,
        P::None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::notification::NotificationKind;
    use thegn_core::store::NotificationStore;

    fn log_notif(created_at_ms: i64, message: &str) -> Notification {
        Notification {
            id: 0,
            kind: NotificationKind::LogError,
            source_ref: "log:thegn".into(),
            message: message.into(),
            created_at_ms,
            read: false,
            worktree_path: String::new(),
        }
    }

    #[test]
    fn cap_inbox_keeps_every_unread_row() {
        let mut rows = (1..=3)
            .map(|time| Notification {
                id: time,
                kind: NotificationKind::Assigned,
                source_ref: format!("U-{time}"),
                message: "unread".into(),
                created_at_ms: time,
                read: false,
                worktree_path: String::new(),
            })
            .collect::<Vec<_>>();
        rows.extend((0..55).map(|index| Notification {
            id: 100 + index,
            kind: NotificationKind::Assigned,
            source_ref: format!("R-{index}"),
            message: "read".into(),
            created_at_ms: 100 + index,
            read: true,
            worktree_path: String::new(),
        }));

        let (kept, hidden) = cap_inbox(rows, 50);
        assert_eq!(kept.len(), 53);
        assert_eq!(hidden, 5);
        assert_eq!(kept.iter().filter(|row| !row.read).count(), 3);
        assert!(kept.windows(2).all(|pair| {
            pair[0].created_at_ms > pair[1].created_at_ms
                || (pair[0].created_at_ms == pair[1].created_at_ms && pair[0].id > pair[1].id)
        }));
    }

    #[test]
    fn populate_notifications_reaches_old_unread_rows() {
        let db = Db::open_memory().unwrap();
        for index in 0..3 {
            db.put_notification("assigned", &format!("U-{index}"), "unread", "")
                .unwrap();
        }
        for index in 0..55 {
            let id = db
                .put_notification("assigned", &format!("R-{index}"), "read", "")
                .unwrap();
            db.mark_notification_read(id).unwrap();
        }

        let mut panel = crate::panel::PanelData::default();
        let config = thegn_core::config::Config::default();
        populate_notifications(&db, std::path::Path::new("/repo"), &config, &mut panel);

        assert_eq!(
            panel.notifications.iter().filter(|row| !row.read).count(),
            3
        );
        assert_eq!(panel.notifications_hidden_read, 5);
    }

    #[test]
    fn last_logged_error_count_parses_newest_first() {
        // `get_all_notifications` returns newest-first; the first `log:thegn`
        // match is authoritative.
        let notifs = vec![
            log_notif(200, "5 errors in thegn.log"),
            log_notif(100, "2 errors in thegn.log"),
        ];
        assert_eq!(last_logged_error_count(&notifs), Some(5));
    }

    #[test]
    fn last_logged_error_count_handles_singular_and_mixed() {
        // Singular "1 error" parses, and unrelated kinds are skipped.
        let notifs = vec![
            Notification {
                kind: NotificationKind::Assigned,
                source_ref: "linear:ABC-1".into(),
                message: "42 whatever".into(),
                ..log_notif(300, "")
            },
            log_notif(200, "1 error in thegn.log"),
        ];
        assert_eq!(last_logged_error_count(&notifs), Some(1));
    }

    #[test]
    fn last_logged_error_count_none_without_log_notif() {
        let notifs = vec![Notification {
            kind: NotificationKind::Assigned,
            source_ref: "linear:ABC-1".into(),
            message: "3 blah".into(),
            ..log_notif(100, "")
        }];
        assert_eq!(last_logged_error_count(&notifs), None);
    }

    // The emit decision (the actual bug): mirrors `maybe_emit_log_error`'s gate
    // without a DB, so the reset regression is locked without a real Db. The
    // `surface` flag is the `surface_self_log_errors` dev gate.
    fn would_emit(notifs: &[Notification], error_count: usize, surface: bool) -> bool {
        surface && error_count > 0 && Some(error_count) > last_logged_error_count(notifs)
    }

    #[test]
    fn emits_first_error_when_no_prior_notification() {
        assert!(would_emit(&[], 3, true));
    }

    #[test]
    fn does_not_re_emit_when_count_unchanged() {
        // The regression: same errors, no change → stay read, emit nothing.
        let notifs = vec![log_notif(100, "3 errors in thegn.log")];
        assert!(!would_emit(&notifs, 3, true));
    }

    #[test]
    fn emits_when_count_grows() {
        let notifs = vec![log_notif(100, "3 errors in thegn.log")];
        assert!(would_emit(&notifs, 4, true));
    }

    #[test]
    fn never_emits_with_zero_errors() {
        assert!(!would_emit(&[], 0, true));
        let notifs = vec![log_notif(100, "3 errors in thegn.log")];
        assert!(!would_emit(&notifs, 0, true));
    }

    #[test]
    fn never_emits_when_self_log_errors_off() {
        // The default: thegn's own log errors are diagnostics, never surfaced.
        assert!(!would_emit(&[], 3, false));
        let notifs = vec![log_notif(100, "3 errors in thegn.log")];
        assert!(!would_emit(&notifs, 9, false));
    }

    #[test]
    fn clears_stale_unread_log_badge_when_log_has_no_errors() {
        // Rotated/truncated log → 0 errors: the lingering unread row is cleared.
        let notifs = vec![Notification {
            id: 7,
            ..log_notif(100, "3 errors in thegn.log")
        }];
        assert_eq!(stale_log_notifs_to_clear(&notifs, 0), vec![7]);
    }

    #[test]
    fn does_not_clear_while_errors_remain_or_already_read() {
        let unread = vec![Notification {
            id: 7,
            ..log_notif(100, "3 errors in thegn.log")
        }];
        // Errors still present → leave the badge alone.
        assert!(stale_log_notifs_to_clear(&unread, 3).is_empty());
        // Already-read rows aren't re-touched.
        let read = vec![Notification {
            id: 7,
            read: true,
            ..log_notif(100, "3 errors in thegn.log")
        }];
        assert!(stale_log_notifs_to_clear(&read, 0).is_empty());
    }
}
