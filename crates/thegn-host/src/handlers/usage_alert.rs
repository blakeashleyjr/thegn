//! Host glue for AI-account usage alerts: fold a gather into the pure evaluator
//! and route whatever it emits to a toast and (opt-in) the notification inbox.
//!
//! The state machine itself is [`thegn_core::usage_alert`] — pure, and therefore
//! under core's coverage gate. Everything here is adaptation, and it mirrors the
//! resource-alert path in `run.rs` deliberately: same toast duration, same
//! priority→color mapping, same "the inbox is a separate opt-in" rule.

use thegn_core::notification::NotificationKind;
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
    let events = state.observe(accounts, cfg, now_ms());
    if events.is_empty() {
        return false;
    }
    let now = thegn_core::util::now();
    for ev in &events {
        let msg = ev.message(now);
        toasts.push(
            crate::toast::priority_color(priority(ev)),
            msg.clone(),
            std::time::Instant::now(),
            std::time::Duration::from_secs(TOAST_SECS),
        );
        // The inbox is a separate opt-in (`[usage.alerts] notify`), and a
        // recovery never goes in it — an entry saying "you are no longer near
        // your limit" is not something anyone comes back to read.
        if cfg.notify && ev.level != AlertLevel::Ok {
            // `ev.key()` is per account+window, so a standing alert's periodic
            // repeat updates one inbox row instead of stacking a new one every
            // `repeat_secs` for as long as the window is hot.
            let notify_state = std::sync::Arc::clone(notify_state);
            let key = ev.key();
            let message = msg.clone();
            let kind = NotificationKind::UsageLimit.as_str().to_string();
            crate::db_task::persist(move |db| {
                crate::notify::record(db, &notify_state, &kind, &key, &message, "");
            });
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
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
