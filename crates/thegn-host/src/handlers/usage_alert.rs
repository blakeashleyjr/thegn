//! Host glue for AI-account usage alerts: fold a gather into the pure evaluator
//! and route whatever it emits to a toast and (opt-in) the notification inbox.
//!
//! The state machine itself is [`thegn_core::usage_alert`] — pure, and therefore
//! under core's coverage gate. Everything here is adaptation, and it mirrors the
//! resource-alert path in `run.rs` deliberately: same toast duration, same
//! priority→color mapping, same "the inbox is a separate opt-in" rule.

use thegn_core::resource_alert::AlertLevel;
use thegn_core::usage::{AccountUsage, UsageAlertsConfig};
use thegn_core::usage_alert::{UsageAlertEvent, UsageAlertState};

/// How long a usage toast stays up. Longer than the 6s resource-alert toast:
/// this one carries a reset countdown the user will want to actually read.
const TOAST_SECS: u64 = 8;

/// Toast tone for an event, matching the bar colors: red at critical, amber at
/// warn, neutral for a recovery (good news never raises a flag).
pub(crate) fn priority(ev: &UsageAlertEvent) -> thegn_core::notification::Priority {
    use thegn_core::notification::Priority;
    match ev.level {
        AlertLevel::Critical => Priority::Alert,
        AlertLevel::Warn => Priority::Notice,
        AlertLevel::Ok => Priority::Info,
    }
}

/// Wall-clock milliseconds for the evaluator's sustain/repeat arithmetic.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Observe one gather and route whatever the evaluator announces to a toast and
/// (opt-in) the inbox. Returns whether anything was announced, so the caller can
/// repaint for a toast raised by a poll that changed no numbers.
///
/// Must be called on EVERY poll, not only on ones whose numbers moved: a
/// standing alert's `repeat_secs` reminder is driven by elapsed time, so gating
/// this on a change would silence exactly the case it exists for — a window
/// parked against its limit.
pub(crate) fn observe_and_route(
    state: &mut UsageAlertState,
    accounts: &[AccountUsage],
    cfg: &UsageAlertsConfig,
    toasts: &mut crate::toast::Toasts,
    notify_state: &std::sync::Arc<crate::notify::NotifyState>,
) -> bool {
    observe_and_route_at(state, accounts, cfg, toasts, notify_state, now_ms())
}

#[derive(Debug, PartialEq, Eq)]
enum UsageRouteWrite {
    Record {
        claim: String,
        key: String,
        message: String,
    },
    Rearm(String),
}

#[derive(Debug, PartialEq, Eq)]
struct UsageRoute {
    toast: bool,
    write: Option<UsageRouteWrite>,
}

fn route(ev: &UsageAlertEvent, cfg: &UsageAlertsConfig, now: i64) -> UsageRoute {
    let message = ev.message(now);
    if ev.level == AlertLevel::Ok {
        UsageRoute {
            toast: cfg.notify_clear,
            write: Some(UsageRouteWrite::Rearm(ev.key())),
        }
    } else {
        UsageRoute {
            toast: true,
            write: cfg.notify.then(|| UsageRouteWrite::Record {
                claim: ev.claim_key(),
                key: ev.key(),
                message,
            }),
        }
    }
}

pub(crate) fn observe_and_route_at(
    state: &mut UsageAlertState,
    accounts: &[AccountUsage],
    cfg: &UsageAlertsConfig,
    toasts: &mut crate::toast::Toasts,
    notify_state: &std::sync::Arc<crate::notify::NotifyState>,
    now_ms: u64,
) -> bool {
    let events = state.observe(accounts, cfg, now_ms);
    if events.is_empty() {
        return false;
    }
    let now = (now_ms / 1000) as i64;
    for ev in &events {
        let routed = route(ev, cfg, now);
        let msg = ev.message(now);
        if routed.toast {
            toasts.push(
                crate::toast::priority_color(priority(ev)),
                msg.clone(),
                std::time::Instant::now(),
                std::time::Duration::from_secs(TOAST_SECS),
            );
        }
        match routed.write {
            Some(UsageRouteWrite::Rearm(key)) => {
                crate::db_task::persist(move |db| {
                    use thegn_core::store::NotificationStore;
                    if let Err(error) = db.rearm_usage_alerts(&key, None) {
                        tracing::debug!(target: "thegn::usage_alert", %error, "usage alert re-arm failed");
                    }
                });
            }
            Some(UsageRouteWrite::Record {
                claim,
                key,
                message,
            }) => {
                let notify_state = std::sync::Arc::clone(notify_state);
                crate::db_task::persist(move |db| {
                    if let Err(error) = crate::automation_events::insert_usage_alert(
                        db,
                        &notify_state,
                        &claim,
                        &key,
                        &message,
                    ) {
                        tracing::debug!(target: "thegn::usage_alert", %error, "usage alert persistence failed");
                    }
                });
            }
            None => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::notification::NotificationKind;
    use thegn_core::store::{NotificationStore, UsageAlertWrite};

    fn with_test_db(
        run: impl FnOnce(&thegn_core::db::Db, &std::sync::Arc<crate::notify::NotifyState>),
    ) {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let _isolated = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open_memory().unwrap();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let state = crate::notify::NotifyState::new(
            thegn_core::config::Config::default().notifications,
            String::new(),
            waker,
        );
        run(&db, &state);
    }

    fn usage_cfg() -> UsageAlertsConfig {
        UsageAlertsConfig {
            repeat_secs: 1,
            used: thegn_core::resource_alert::AlertRule {
                warn: 75.0,
                critical: 90.0,
            },
            ..UsageAlertsConfig::default()
        }
    }

    fn usage_account(pct: f32, reset: Option<i64>) -> AccountUsage {
        let mut account = AccountUsage::ok(
            "claude",
            "work",
            None,
            vec![thegn_core::usage::UsageWindow::new("weekly", pct, reset)],
        );
        account.key = "claude:acct/org".to_string();
        account
    }

    fn persist_event(
        db: &thegn_core::db::Db,
        state: &crate::notify::NotifyState,
        event: &UsageAlertEvent,
    ) -> UsageAlertWrite {
        crate::automation_events::insert_usage_alert(
            db,
            state,
            &event.claim_key(),
            &event.key(),
            &event.message(1_700_000_000),
        )
        .unwrap()
    }

    #[test]
    fn standing_repeat_updates_one_row() {
        with_test_db(|db, state| {
            let cfg = usage_cfg();
            let mut latch = UsageAlertState::new();
            let first = latch.observe(&[usage_account(80.0, Some(1_700_001_000))], &cfg, 0);
            assert!(matches!(
                persist_event(db, state, &first[0]),
                UsageAlertWrite::Inserted(_)
            ));
            let repeat = latch.observe(&[usage_account(85.0, Some(1_700_001_000))], &cfg, 2_000);
            assert_eq!(
                persist_event(db, state, &repeat[0]),
                UsageAlertWrite::Updated
            );
            let rows = db.get_all_notifications(10).unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].message.contains("85%"));
        });
    }

    #[test]
    fn restart_does_not_reinsert() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            let account = usage_account(80.0, Some(1_700_001_000));
            let mut first_process = UsageAlertState::new();
            let ev = first_process.observe(std::slice::from_ref(&account), &cfg, 0);
            persist_event(db, notify, &ev[0]);
            let mut restarted = UsageAlertState::new();
            let ev = restarted.observe(&[account], &cfg, 2_000);
            persist_event(db, notify, &ev[0]);
            assert_eq!(db.get_all_notifications(10).unwrap().len(), 1);
        });
    }

    /// Independent latches model two UI processes; the shared SQLite store
    /// serializes their same-period claims, so the second event must reuse the
    /// first row even though neither process knows about the other's state.
    #[test]
    fn two_processes_share_one_row() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            let account = usage_account(80.0, Some(1_700_001_000));
            let mut process_a = UsageAlertState::new();
            let mut process_b = UsageAlertState::new();
            let a = process_a.observe(std::slice::from_ref(&account), &cfg, 0);
            let b = process_b.observe(&[account], &cfg, 0);
            persist_event(db, notify, &a[0]);
            persist_event(db, notify, &b[0]);
            assert_eq!(db.get_all_notifications(10).unwrap().len(), 1);
        });
    }

    #[test]
    fn two_database_handles_share_one_claim() {
        let _isolated = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let db_a = thegn_core::db::Db::open_at(&path).unwrap();
        let db_b = thegn_core::db::Db::open_at(&path).unwrap();
        let key = "claude:acct/org#weekly";
        let claim = "usage:claude:acct/org#weekly#warn#1700000100";
        assert!(matches!(
            db_a.record_usage_alert(claim, key, "first").unwrap(),
            UsageAlertWrite::Inserted(_)
        ));
        assert_eq!(
            db_b.record_usage_alert(claim, key, "second").unwrap(),
            UsageAlertWrite::Updated
        );
        assert_eq!(db_a.get_all_notifications(10).unwrap().len(), 1);
    }

    #[test]
    fn observe_routes_recovery_and_default_retreat_rearms_same_period() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            assert!(!cfg.notify_clear);
            let mut latch = UsageAlertState::new();
            let first = latch.observe(&[usage_account(80.0, Some(1_700_001_000))], &cfg, 0);
            let first_route = route(&first[0], &cfg, 1_700_000_000);
            let Some(UsageRouteWrite::Record {
                claim,
                key,
                message,
            }) = first_route.write
            else {
                panic!("notified crossing routes claim before account window");
            };
            assert_eq!(claim, first[0].claim_key());
            assert_eq!(key, first[0].key());
            let UsageAlertWrite::Inserted(id) =
                crate::automation_events::insert_usage_alert(db, notify, &claim, &key, &message)
                    .unwrap()
            else {
                panic!("first crossing inserts the row");
            };
            db.delete_notification(id).unwrap();

            let retreat = latch.observe(&[usage_account(5.0, Some(1_700_001_000))], &cfg, 1_000);
            assert_eq!(retreat.len(), 1);
            assert_eq!(retreat[0].level, AlertLevel::Ok);
            assert_eq!(
                route(&retreat[0], &cfg, 1_700_000_001),
                UsageRoute {
                    toast: false,
                    write: Some(UsageRouteWrite::Rearm(retreat[0].key())),
                }
            );
            db.rearm_usage_alerts(&retreat[0].key(), None).unwrap();

            let crossed = latch.observe(&[usage_account(80.0, Some(1_700_001_000))], &cfg, 2_000);
            assert_eq!(crossed.len(), 1);
            let UsageRouteWrite::Record {
                claim,
                key,
                message,
            } = route(&crossed[0], &cfg, 1_700_000_002).write.unwrap()
            else {
                panic!("re-cross routes a fresh claim");
            };
            assert!(matches!(
                crate::automation_events::insert_usage_alert(db, notify, &claim, &key, &message)
                    .unwrap(),
                UsageAlertWrite::Inserted(_)
            ));
            assert_eq!(db.get_all_notifications(10).unwrap().len(), 1);
        });
    }

    #[test]
    fn route_table_gates_toasts_but_always_rearms_on_clear() {
        let event = UsageAlertEvent {
            account_key: "claude:acct/org".into(),
            account_label: "work".into(),
            window: "weekly".into(),
            level: AlertLevel::Warn,
            used_percent: 80.0,
            threshold: 75.0,
            resets_at: Some(1_700_001_000),
            repeat: false,
        };
        let quiet_inbox = UsageAlertsConfig {
            notify: false,
            ..usage_cfg()
        };
        assert_eq!(
            route(&event, &quiet_inbox, 1_700_000_000),
            UsageRoute {
                toast: true,
                write: None,
            }
        );
        let clear = UsageAlertEvent {
            level: AlertLevel::Ok,
            ..event
        };
        assert_eq!(
            route(&clear, &quiet_inbox, 1_700_000_000),
            UsageRoute {
                toast: false,
                write: Some(UsageRouteWrite::Rearm(clear.key())),
            }
        );
    }

    #[test]
    fn dismissed_alert_stays_read() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            let mut latch = UsageAlertState::new();
            let account = usage_account(80.0, Some(1_700_001_000));
            let first = latch.observe(std::slice::from_ref(&account), &cfg, 0);
            let UsageAlertWrite::Inserted(id) = persist_event(db, notify, &first[0]) else {
                panic!("initial alert inserts")
            };
            db.mark_notification_read(id).unwrap();
            let repeated = latch.observe(&[usage_account(85.0, Some(1_700_001_000))], &cfg, 2_000);
            assert_eq!(
                persist_event(db, notify, &repeated[0]),
                UsageAlertWrite::Updated
            );
            let rows = db.get_all_notifications(10).unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].read);
        });
    }

    #[test]
    fn deleted_alert_not_resurrected_within_period() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            let mut latch = UsageAlertState::new();
            let account = usage_account(80.0, Some(1_700_001_000));
            let first = latch.observe(std::slice::from_ref(&account), &cfg, 0);
            let UsageAlertWrite::Inserted(id) = persist_event(db, notify, &first[0]) else {
                panic!("initial alert inserts")
            };
            db.delete_notification(id).unwrap();
            let repeated = latch.observe(&[usage_account(85.0, Some(1_700_001_000))], &cfg, 2_000);
            assert_eq!(
                persist_event(db, notify, &repeated[0]),
                UsageAlertWrite::Suppressed
            );
            assert!(db.get_all_notifications(10).unwrap().is_empty());
        });
    }

    #[test]
    fn new_period_rearms() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            let mut first = UsageAlertState::new();
            let ev = first.observe(&[usage_account(80.0, Some(1_700_001_000))], &cfg, 0);
            assert!(matches!(
                persist_event(db, notify, &ev[0]),
                UsageAlertWrite::Inserted(_)
            ));
            let mut next_period = UsageAlertState::new();
            let ev = next_period.observe(&[usage_account(80.0, Some(1_700_002_000))], &cfg, 1);
            assert!(matches!(
                persist_event(db, notify, &ev[0]),
                UsageAlertWrite::Inserted(_)
            ));
            assert_eq!(db.get_all_notifications(10).unwrap().len(), 2);
        });
    }

    #[test]
    fn escalation_supersedes_warn() {
        with_test_db(|db, notify| {
            let cfg = usage_cfg();
            let mut latch = UsageAlertState::new();
            let warn = latch.observe(&[usage_account(80.0, Some(1_700_001_000))], &cfg, 0);
            persist_event(db, notify, &warn[0]);
            let critical = latch.observe(&[usage_account(95.0, Some(1_700_001_000))], &cfg, 1);
            persist_event(db, notify, &critical[0]);
            let rows = db.get_all_notifications(10).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows.iter().filter(|row| !row.read).count(), 1);
            assert!(
                rows.iter()
                    .any(|row| row.read && row.message.contains("80%"))
            );
        });
    }
    use thegn_core::notification::Priority;

    fn ev(level: AlertLevel) -> UsageAlertEvent {
        UsageAlertEvent {
            account_key: "claude:a/o".into(),
            account_label: "work".into(),
            window: "5h".into(),
            level,
            used_percent: 95.0,
            threshold: 90.0,
            resets_at: None,
            repeat: false,
        }
    }

    #[test]
    fn tone_follows_the_bar_colors() {
        assert_eq!(priority(&ev(AlertLevel::Critical)), Priority::Alert);
        assert_eq!(priority(&ev(AlertLevel::Warn)), Priority::Notice);
        // A recovery must never raise a flag.
        assert_eq!(priority(&ev(AlertLevel::Ok)), Priority::Info);
    }

    #[test]
    fn opted_in_alert_does_not_block_on_a_held_write_lock() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        drop(db);
        let lock = rusqlite::Connection::open(state.state_home().join("thegn/thegn.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let notify_state = crate::notify::NotifyState::new(
            thegn_core::config::Config::default().notifications,
            String::new(),
            waker,
        );
        let account = AccountUsage::ok(
            "claude",
            "work",
            None,
            vec![thegn_core::usage::UsageWindow::new("5h", 95.0, None)],
        );
        let cfg = UsageAlertsConfig {
            used: thegn_core::resource_alert::AlertRule {
                warn: 90.0,
                critical: 100.0,
            },
            ..UsageAlertsConfig::default()
        };
        let mut alerts = UsageAlertState::new();
        let mut toasts = crate::toast::Toasts::default();

        let started = std::time::Instant::now();
        let announced =
            observe_and_route(&mut alerts, &[account], &cfg, &mut toasts, &notify_state);
        assert!(announced);
        assert!(started.elapsed() < std::time::Duration::from_millis(500));

        lock.execute_batch("ROLLBACK").unwrap();
        assert!(crate::db_task::flush(std::time::Duration::from_secs(10)));

        let (tx, rx) = std::sync::mpsc::channel();
        crate::db_task::persist(move |db| {
            use thegn_core::store::NotificationStore;
            if tx.send(db.get_all_notifications(10)).is_err() {
                tracing::debug!(target: "thegn::usage_alert", "usage alert read-back receiver closed");
            }
        });
        let notifications = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(
            notifications
                .iter()
                .filter(|notification| matches!(notification.kind, NotificationKind::UsageLimit))
                .count(),
            1,
            "the queued usage alert is persisted after the write lock is released",
        );
    }
}
