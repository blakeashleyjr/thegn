//! Threshold alerts for AI-account usage windows — "you are at 91% of your
//! weekly limit" (roadmap V 300).
//!
//! Pure, and a deliberate sibling of [`crate::resource_alert`] rather than a
//! second opinion: the same `sustain` / `repeat` / `clear_margin` /
//! `notify_clear` rules, the same [`AlertLevel`] and [`AlertRule`] types, worded
//! the same way in config. Two threshold systems that behave differently is how
//! a user learns to distrust both.
//!
//! It is a separate module because the *keying* differs and that is not a small
//! detail. `resource_alert` latches a fixed array of eight metrics known at
//! compile time; the accounts and windows here are discovered at runtime and
//! come and go (a login is added, a home is unplugged, a provider starts
//! reporting a new window). So latches live in a map and are pruned against each
//! observation, or the map would grow for the life of the process.
//!
//! The rules, restated for this domain:
//!
//! 1. **Rising edge, sustained.** Crossing a threshold fires once `sustain_secs`
//!    has passed at that level. With the default `sustain_secs = 0` a crossing
//!    fires on the poll that sees it — usage moves in poll-sized steps, so
//!    there is no spike to debounce, only a bad response to guard against.
//! 2. **Standing alert repeats** every `repeat_secs`, so a limit you are sitting
//!    against doesn't go quiet. `0` disables repeats.
//! 3. **Clearing needs a real retreat** — `clear_margin` below the threshold —
//!    so a window hovering on the line cannot flap. In practice a window clears
//!    because it *reset* (back to ~0%), which clears this easily.
//! 4. **An unreadable account is not an observation.** A row that is `Loading`
//!    or `Unavailable` leaves its latches exactly as they were: a network blip
//!    must not read as "recovered", and must not re-fire on return either.

use crate::resource_alert::{AlertLevel, AlertRule};
use crate::usage::{AccountUsage, UsageAlertsConfig, UsageState, UsageWindow};
use std::collections::HashMap;

/// Something worth telling the user about one window.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageAlertEvent {
    /// The account's stable key — also the notification's dedup key.
    pub account_key: String,
    /// Display label, for the message text.
    pub account_label: String,
    /// Window label (`"5h"`, `"weekly"`).
    pub window: String,
    /// The new level. [`AlertLevel::Ok`] means the alert cleared.
    pub level: AlertLevel,
    pub used_percent: f32,
    pub threshold: f32,
    /// Absolute reset deadline, epoch seconds, when known — the single most
    /// useful thing to put in the message.
    pub resets_at: Option<i64>,
    /// True when this is a periodic re-fire of a standing alert rather than a
    /// fresh crossing, so the host can word it differently or drop it.
    pub repeat: bool,
}

impl UsageAlertEvent {
    /// The notification's dedup/thread key: one standing alert per window, so a
    /// repeat updates the existing entry rather than filling the inbox.
    pub fn key(&self) -> String {
        format!("{}#{}", self.account_key, self.window)
    }

    /// Toast/inbox text, e.g.
    /// `claude work: 5h window at 91% (limit 90%) — resets in 2h 14m`.
    pub fn message(&self, now: i64) -> String {
        if self.level == AlertLevel::Ok {
            return format!(
                "{}: {} window back to {:.0}%",
                self.account_label, self.window, self.used_percent
            );
        }
        let resets = crate::usage::fmt_resets_in(self.resets_at, now)
            .map(|r| format!(" \u{2014} resets in {r}"))
            .unwrap_or_default();
        format!(
            "{}: {} window at {:.0}% (limit {:.0}%){resets}",
            self.account_label, self.window, self.used_percent, self.threshold
        )
    }
}

/// One window's latch. Mirrors `resource_alert`'s private `Latch`.
#[derive(Debug, Clone, Default)]
struct Latch {
    /// The level currently being held, pending `sustain_secs`.
    candidate: AlertLevel,
    /// The level actually announced.
    fired: AlertLevel,
    since_ms: u64,
    last_fired_ms: u64,
    seen: bool,
}

/// The rolling alert state. One per process; feed it every poll.
#[derive(Debug, Clone, Default)]
pub struct UsageAlertState {
    latches: HashMap<String, Latch>,
}

/// The map key for one window of one account.
fn latch_key(account: &AccountUsage, w: &UsageWindow) -> String {
    format!("{}#{}", account.key, w.label)
}

impl UsageAlertState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The level currently announced for one window — for tinting a readout
    /// without re-deriving the state machine.
    pub fn level(&self, account: &AccountUsage, w: &UsageWindow) -> AlertLevel {
        self.latches
            .get(&latch_key(account, w))
            .map(|l| l.fired)
            .unwrap_or_default()
    }

    /// Fold one gather into the state, returning whatever should be announced.
    ///
    /// `now_ms` is wall-clock milliseconds. A backwards jump (suspend, NTP step)
    /// is clamped to a zero elapsed time rather than underflowing into a
    /// spuriously-sustained alert — same as `resource_alert`.
    pub fn observe(
        &mut self,
        accounts: &[AccountUsage],
        cfg: &UsageAlertsConfig,
        now_ms: u64,
    ) -> Vec<UsageAlertEvent> {
        if !cfg.enabled {
            // Drop every latch so re-enabling starts clean rather than
            // immediately firing a stale standing alert.
            self.latches.clear();
            return Vec::new();
        }
        let sustain_ms = u64::from(cfg.sustain_secs) * 1000;
        let repeat_ms = u64::from(cfg.repeat_secs) * 1000;
        let margin = cfg.clear_margin.clamp(0.0, 0.9);
        let rule = cfg.used;

        let mut out = Vec::new();
        // Rule 4: only readable accounts are observations. An unreadable one
        // contributes no keys here — and crucially is NOT pruned below either,
        // so a network blip preserves its latches instead of clearing them.
        let mut observed: Vec<(String, &AccountUsage, &UsageWindow)> = Vec::new();
        let mut unreadable: Vec<&AccountUsage> = Vec::new();
        for a in accounts {
            if a.state == UsageState::Ok {
                observed.extend(a.windows.iter().map(|w| (latch_key(a, w), a, w)));
            } else {
                unreadable.push(a);
            }
        }

        for (key, account, w) in &observed {
            let v = w.used_percent;
            let now_level = if breaches(&rule, v, AlertLevel::Critical) {
                AlertLevel::Critical
            } else if breaches(&rule, v, AlertLevel::Warn) {
                AlertLevel::Warn
            } else {
                AlertLevel::Ok
            };

            let l = self.latches.entry(key.clone()).or_default();
            // Rule 3: while an alert stands, "back to Ok" needs the retreat to
            // clear the threshold by `margin`, not merely to touch it.
            let effective = if l.fired > AlertLevel::Ok && now_level < l.fired {
                let thr = threshold(&rule, l.fired).unwrap_or(0.0);
                if v < thr * (1.0 - margin) {
                    now_level
                } else {
                    l.fired
                }
            } else {
                now_level
            };

            if !l.seen || effective != l.candidate {
                l.candidate = effective;
                l.since_ms = now_ms;
                l.seen = true;
            }
            let held = now_ms.saturating_sub(l.since_ms);
            let event = |level: AlertLevel, repeat: bool| UsageAlertEvent {
                account_key: account.key.clone(),
                account_label: account.account_label.clone(),
                window: w.label.clone(),
                level,
                used_percent: v,
                threshold: threshold(&rule, level).unwrap_or(0.0),
                resets_at: w.resets_at,
                repeat,
            };

            if effective > l.fired {
                // Rule 1: rising edge, but only once it has been sustained.
                if held >= sustain_ms {
                    l.fired = effective;
                    l.last_fired_ms = now_ms;
                    out.push(event(effective, false));
                }
            } else if effective < l.fired {
                // Clearing is sustained too, so a brief dip doesn't cancel a
                // real, ongoing alert.
                if held >= sustain_ms {
                    l.fired = effective;
                    l.last_fired_ms = now_ms;
                    if cfg.notify_clear || effective > AlertLevel::Ok {
                        out.push(event(effective, false));
                    }
                }
            } else if l.fired > AlertLevel::Ok
                && repeat_ms > 0
                && now_ms.saturating_sub(l.last_fired_ms) >= repeat_ms
            {
                // Rule 2: standing alert, periodic reminder.
                l.last_fired_ms = now_ms;
                out.push(event(l.fired, true));
            }
        }

        // Prune latches for windows that are gone for good — an account removed
        // from config, or a provider that stopped reporting a window. Latches
        // belonging to an account that merely failed to read this round are
        // kept: that is a blip, not a removal.
        let live: std::collections::HashSet<&str> =
            observed.iter().map(|(k, _, _)| k.as_str()).collect();
        self.latches.retain(|k, _| {
            live.contains(k.as_str())
                || unreadable
                    .iter()
                    .any(|a| k.starts_with(&format!("{}#", a.key)))
        });
        out
    }
}

/// The threshold for `level`, or `None` when that level is disabled (`0`).
fn threshold(rule: &AlertRule, level: AlertLevel) -> Option<f32> {
    let v = match level {
        AlertLevel::Warn => rule.warn,
        AlertLevel::Critical => rule.critical,
        AlertLevel::Ok => return None,
    };
    (v > 0.0 && v.is_finite()).then_some(v)
}

/// Whether `v` is at or past `level`'s threshold. Usage is always an
/// "over is bad" metric, so there is no direction to resolve.
fn breaches(rule: &AlertRule, v: f32, level: AlertLevel) -> bool {
    threshold(rule, level).is_some_and(|t| v >= t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageWindow;

    fn cfg() -> UsageAlertsConfig {
        UsageAlertsConfig {
            sustain_secs: 0,
            repeat_secs: 0,
            clear_margin: 0.05,
            used: AlertRule {
                warn: 75.0,
                critical: 90.0,
            },
            ..Default::default()
        }
    }

    fn account(key: &str, pct: f32) -> AccountUsage {
        let mut a = AccountUsage::ok(
            "claude",
            "work",
            None,
            vec![UsageWindow::new("5h", pct, Some(1_700_000_000))],
        );
        a.key = key.to_string();
        a
    }

    #[test]
    fn crossing_fires_once_then_stays_quiet() {
        let mut s = UsageAlertState::new();
        assert!(s.observe(&[account("a", 50.0)], &cfg(), 0).is_empty());
        let ev = s.observe(&[account("a", 80.0)], &cfg(), 1000);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Warn);
        assert_eq!(ev[0].threshold, 75.0);
        assert!(!ev[0].repeat);
        // Still warn, still climbing — no second announcement.
        assert!(s.observe(&[account("a", 85.0)], &cfg(), 2000).is_empty());
        // Escalation to critical is its own event.
        let ev = s.observe(&[account("a", 95.0)], &cfg(), 3000);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Critical);
        assert_eq!(ev[0].threshold, 90.0);
    }

    #[test]
    fn clearing_needs_a_real_retreat() {
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0)], &cfg(), 0);
        // 88% is below the 90% critical line but inside the 5% clear margin
        // (needs < 85.5), so the critical alert stands rather than flapping.
        assert!(s.observe(&[account("a", 88.0)], &cfg(), 1000).is_empty());
        assert_eq!(
            s.level(&account("a", 88.0), &UsageWindow::new("5h", 0.0, None)),
            AlertLevel::Critical
        );
        // A genuine retreat past the margin steps down to warn, and that is
        // worth announcing (it is still an alert).
        let ev = s.observe(&[account("a", 80.0)], &cfg(), 2000);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Warn);
        // Dropping to Ok is silent unless notify_clear is set.
        assert!(s.observe(&[account("a", 5.0)], &cfg(), 3000).is_empty());
        assert_eq!(
            s.level(&account("a", 5.0), &UsageWindow::new("5h", 0.0, None)),
            AlertLevel::Ok
        );
    }

    #[test]
    fn notify_clear_announces_the_window_reset() {
        let c = UsageAlertsConfig {
            notify_clear: true,
            ..cfg()
        };
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0)], &c, 0);
        let ev = s.observe(&[account("a", 2.0)], &c, 1000);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Ok);
        assert!(ev[0].message(0).contains("back to 2%"));
    }

    #[test]
    fn standing_alerts_repeat_on_cadence_only() {
        let c = UsageAlertsConfig {
            repeat_secs: 60,
            ..cfg()
        };
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0)], &c, 0);
        // Too soon.
        assert!(s.observe(&[account("a", 96.0)], &c, 30_000).is_empty());
        let ev = s.observe(&[account("a", 96.0)], &c, 60_000);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].repeat);
        // `repeat_secs = 0` disables repeats entirely.
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0)], &cfg(), 0);
        assert!(
            s.observe(&[account("a", 96.0)], &cfg(), 10_000_000)
                .is_empty()
        );
    }

    #[test]
    fn sustain_holds_a_crossing_until_it_persists() {
        let c = UsageAlertsConfig {
            sustain_secs: 30,
            ..cfg()
        };
        let mut s = UsageAlertState::new();
        assert!(s.observe(&[account("a", 95.0)], &c, 0).is_empty());
        assert!(s.observe(&[account("a", 95.0)], &c, 10_000).is_empty());
        assert_eq!(s.observe(&[account("a", 95.0)], &c, 30_000).len(), 1);
    }

    #[test]
    fn an_unreadable_account_is_not_an_observation() {
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0)], &cfg(), 0);
        let mut down = AccountUsage::unavailable("claude", "work", "fetch failed");
        down.key = "a".into();
        // A network blip announces nothing...
        assert!(s.observe(&[down.clone()], &cfg(), 1000).is_empty());
        // ...and does not clear the latch, so recovery at the same level is
        // silent rather than re-firing an alert the user already saw.
        assert!(s.observe(&[account("a", 95.0)], &cfg(), 2000).is_empty());
    }

    #[test]
    fn latches_are_pruned_when_an_account_goes_away() {
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0), account("b", 95.0)], &cfg(), 0);
        assert_eq!(s.latches.len(), 2);
        // Account "b" removed from config entirely — its latch goes with it,
        // or the map grows for the life of the process.
        s.observe(&[account("a", 95.0)], &cfg(), 1000);
        assert_eq!(s.latches.len(), 1);
        assert!(s.latches.contains_key("a#5h"));
    }

    #[test]
    fn disabling_drops_every_latch() {
        let mut s = UsageAlertState::new();
        s.observe(&[account("a", 95.0)], &cfg(), 0);
        let off = UsageAlertsConfig {
            enabled: false,
            ..cfg()
        };
        assert!(s.observe(&[account("a", 95.0)], &off, 1000).is_empty());
        assert!(s.latches.is_empty());
        // Re-enabling starts clean and re-announces, rather than sitting on a
        // stale standing alert the user was never shown.
        assert_eq!(s.observe(&[account("a", 95.0)], &cfg(), 2000).len(), 1);
    }

    #[test]
    fn a_zero_rule_disables_that_level() {
        let c = UsageAlertsConfig {
            used: AlertRule {
                warn: 0.0,
                critical: 90.0,
            },
            ..cfg()
        };
        let mut s = UsageAlertState::new();
        // Warn is off, so 80% announces nothing.
        assert!(s.observe(&[account("a", 80.0)], &c, 0).is_empty());
        assert_eq!(s.observe(&[account("a", 95.0)], &c, 1000).len(), 1);
    }

    #[test]
    fn message_reads_as_a_sentence() {
        let now = 1_700_000_000;
        let ev = UsageAlertEvent {
            account_key: "claude:acc/org".into(),
            account_label: "blake@example.com (Acme)".into(),
            window: "7d".into(),
            level: AlertLevel::Critical,
            used_percent: 91.0,
            threshold: 90.0,
            resets_at: Some(now + 2 * 3600 + 14 * 60),
            repeat: false,
        };
        assert_eq!(
            ev.message(now),
            "blake@example.com (Acme): 7d window at 91% (limit 90%) \u{2014} resets in 2h 14m"
        );
        assert_eq!(ev.key(), "claude:acc/org#7d");
        // No known reset → no dangling clause.
        let no_reset = UsageAlertEvent {
            resets_at: None,
            ..ev
        };
        assert_eq!(
            no_reset.message(now),
            "blake@example.com (Acme): 7d window at 91% (limit 90%)"
        );
    }
}
