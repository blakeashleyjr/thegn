//! Threshold alerts on system metrics — the pure state machine behind
//! `[stats.alerts]`.
//!
//! Lives in core (not the host) so it falls under the 95%-lines coverage gate:
//! this is the one piece of the monitor with real branching logic, and a
//! mis-fired alert is worse than no alert. It takes a plain [`AlertReading`]
//! rather than a `thegn_metrics::StatsSnapshot` because core deliberately does
//! not depend on the metrics crate; the host fills the struct.
//!
//! # The four rules that keep it from being a spam vector
//!
//! 1. **Sustain.** A reading must stay past a threshold for `sustain_secs`
//!    before anything fires, so a single-sample spike is silent. A resource
//!    monitor that shouts at every scheduler blip trains you to ignore it.
//! 2. **Repeat cap.** A standing alert re-fires no more often than
//!    `repeat_secs`. A CPU-pegged compile must not emit a notification every
//!    sample for twenty minutes.
//! 3. **Hysteresis.** An alert clears only once the value retreats past the
//!    threshold by `clear_margin` *and* holds there for `sustain_secs`. Without
//!    the margin, a value hovering exactly on the line flaps fire/clear forever.
//! 4. **Absent ≠ recovered.** A `None` reading is *no observation*, not a
//!    return to normal. This is what stops a machine with no thermal sensor (or
//!    no load average) from emitting phantom "recovered" events.

use serde::{Deserialize, Serialize};

/// A metric that can carry a threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AlertMetric {
    Cpu,
    Mem,
    Swap,
    Temp,
    Gpu,
    Load,
    DiskFree,
    Battery,
}

impl AlertMetric {
    /// Every metric, in report order.
    pub const ALL: [AlertMetric; 8] = [
        AlertMetric::Cpu,
        AlertMetric::Mem,
        AlertMetric::Swap,
        AlertMetric::Temp,
        AlertMetric::Gpu,
        AlertMetric::Load,
        AlertMetric::DiskFree,
        AlertMetric::Battery,
    ];

    /// Position in [`Self::ALL`] — the index of this metric's latch and rule.
    pub fn index(self) -> usize {
        match self {
            AlertMetric::Cpu => 0,
            AlertMetric::Mem => 1,
            AlertMetric::Swap => 2,
            AlertMetric::Temp => 3,
            AlertMetric::Gpu => 4,
            AlertMetric::Load => 5,
            AlertMetric::DiskFree => 6,
            AlertMetric::Battery => 7,
        }
    }

    /// Stable slug — used as the notification key, so it must never be a
    /// display string that someone might reword.
    pub fn key(self) -> &'static str {
        match self {
            AlertMetric::Cpu => "cpu",
            AlertMetric::Mem => "mem",
            AlertMetric::Swap => "swap",
            AlertMetric::Temp => "temp",
            AlertMetric::Gpu => "gpu",
            AlertMetric::Load => "load",
            AlertMetric::DiskFree => "disk_free",
            AlertMetric::Battery => "battery",
        }
    }

    /// Human label for the toast text.
    pub fn label(self) -> &'static str {
        match self {
            AlertMetric::Cpu => "CPU",
            AlertMetric::Mem => "Memory",
            AlertMetric::Swap => "Swap",
            AlertMetric::Temp => "Temperature",
            AlertMetric::Gpu => "GPU",
            AlertMetric::Load => "Load",
            AlertMetric::DiskFree => "Disk space",
            AlertMetric::Battery => "Battery",
        }
    }

    /// True when the alert fires on the value falling BELOW the threshold.
    /// Free disk and battery charge are the two that run out rather than pile
    /// up; every other metric alerts on going high.
    pub fn is_under(self) -> bool {
        matches!(self, AlertMetric::DiskFree | AlertMetric::Battery)
    }

    /// Unit suffix for the toast text.
    pub fn unit(self) -> &'static str {
        match self {
            AlertMetric::Temp => "°C",
            AlertMetric::Load => "",
            _ => "%",
        }
    }
}

/// Severity. `Ok` doubles as "cleared" on an emitted event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertLevel {
    #[default]
    Ok,
    Warn,
    Critical,
}

/// One metric's thresholds. `0` disables that level, so a user can enable just
/// the critical tier without inventing a warn value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AlertRule {
    pub warn: f32,
    pub critical: f32,
}

impl AlertRule {
    /// The threshold for `level`, or `None` when that level is disabled.
    fn threshold(&self, level: AlertLevel) -> Option<f32> {
        let v = match level {
            AlertLevel::Warn => self.warn,
            AlertLevel::Critical => self.critical,
            AlertLevel::Ok => return None,
        };
        (v > 0.0 && v.is_finite()).then_some(v)
    }

    /// Whether `v` is past `level`'s threshold, in `metric`'s direction.
    fn breaches(&self, v: f32, level: AlertLevel, metric: AlertMetric) -> bool {
        match self.threshold(level) {
            Some(t) if metric.is_under() => v <= t,
            Some(t) => v >= t,
            None => false,
        }
    }
}

/// Thresholds and timing, already folded with the legacy `[stats]` keys by the
/// host (see `StatsConfig::effective_alerts`) so there is exactly one source of
/// truth per metric.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAlerts {
    pub enabled: bool,
    /// Pop a transient in-app toast. Defaults **off**: a threshold is a standing
    /// condition, and the widget beside it already shows one.
    pub toast: bool,
    /// Also record to the notification inbox. Defaults **off**.
    pub notify: bool,
    pub sustain_secs: u32,
    pub repeat_secs: u32,
    /// Fractional retreat past the threshold required before clearing.
    pub clear_margin: f32,
    pub notify_clear: bool,
    /// One rule per metric, indexed by [`AlertMetric::index`].
    pub rules: [AlertRule; AlertMetric::ALL.len()],
}

impl Default for ResolvedAlerts {
    fn default() -> Self {
        ResolvedAlerts {
            enabled: true,
            toast: false,
            notify: false,
            sustain_secs: 15,
            repeat_secs: 900,
            clear_margin: 0.05,
            notify_clear: false,
            rules: [AlertRule::default(); AlertMetric::ALL.len()],
        }
    }
}

impl ResolvedAlerts {
    /// The rule for one metric.
    pub fn rule(&self, m: AlertMetric) -> AlertRule {
        self.rules[m.index()]
    }

    /// Set one metric's rule — the builder the host's config fold uses.
    pub fn set(&mut self, m: AlertMetric, rule: AlertRule) {
        self.rules[m.index()] = rule;
    }
}

/// One sampled reading per metric. `None` = the platform does not expose it, or
/// it was not sampled this tick — **not** "it is fine".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AlertReading {
    pub cpu_pct: Option<f32>,
    pub mem_pct: Option<f32>,
    pub swap_pct: Option<f32>,
    pub temp_c: Option<f32>,
    pub gpu_pct: Option<f32>,
    /// Load average normalized per core, so the threshold means the same thing
    /// on a laptop and a build box.
    pub load_per_core: Option<f32>,
    pub disk_free_pct: Option<f32>,
    pub battery_pct: Option<f32>,
}

impl AlertReading {
    fn get(&self, m: AlertMetric) -> Option<f32> {
        let v = match m {
            AlertMetric::Cpu => self.cpu_pct,
            AlertMetric::Mem => self.mem_pct,
            AlertMetric::Swap => self.swap_pct,
            AlertMetric::Temp => self.temp_c,
            AlertMetric::Gpu => self.gpu_pct,
            AlertMetric::Load => self.load_per_core,
            AlertMetric::DiskFree => self.disk_free_pct,
            AlertMetric::Battery => self.battery_pct,
        };
        v.filter(|x| x.is_finite())
    }
}

/// A threshold crossing worth telling the user about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertEvent {
    pub metric: AlertMetric,
    /// The new level. [`AlertLevel::Ok`] means the alert cleared.
    pub level: AlertLevel,
    pub value: f32,
    pub threshold: f32,
    /// True when this is a periodic re-fire of a standing alert rather than a
    /// fresh crossing — the host can word it differently, or drop it.
    pub repeat: bool,
}

impl AlertEvent {
    /// Toast text, e.g. `CPU 94% (warn 90%)`.
    pub fn message(&self) -> String {
        let u = self.metric.unit();
        let (label, prec) = (self.metric.label(), usize::from(u.is_empty()) * 2);
        match self.level {
            AlertLevel::Ok => format!("{label} recovered ({:.*}{u})", prec, self.value),
            _ => {
                let tier = if self.level == AlertLevel::Critical {
                    "critical"
                } else {
                    "warn"
                };
                format!(
                    "{label} {:.*}{u} ({tier} {:.*}{u})",
                    prec, self.value, prec, self.threshold
                )
            }
        }
    }
}

/// Per-metric latch: what we have told the user, and what we are watching.
#[derive(Debug, Clone, Copy, Default)]
struct Latch {
    /// The level currently announced. `Ok` = nothing outstanding.
    fired: AlertLevel,
    /// The level the readings have been showing, and since when.
    candidate: AlertLevel,
    since_ms: u64,
    last_fired_ms: u64,
    /// False until the first observation, so `since_ms == 0` isn't mistaken for
    /// "sustained since the epoch".
    seen: bool,
}

/// The rolling alert state. One per process; feed it every sample.
///
/// Latches are indexed positionally by [`AlertMetric::ALL`] order rather than
/// carrying their key, so there is no lookup and no way for the array to fall
/// out of sync with the enum.
#[derive(Debug, Clone, Default)]
pub struct AlertState {
    latches: [Latch; AlertMetric::ALL.len()],
}

impl AlertState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The level currently announced for `m` — for tinting a readout without
    /// re-deriving the state machine.
    pub fn level(&self, m: AlertMetric) -> AlertLevel {
        self.latches[m.index()].fired
    }

    /// Fold one reading into the state, returning whatever should be announced.
    ///
    /// `now_ms` is wall-clock milliseconds. A backwards jump (suspend, NTP step)
    /// is clamped to a zero elapsed time rather than underflowing into a
    /// spuriously-sustained alert.
    pub fn observe(
        &mut self,
        r: &AlertReading,
        cfg: &ResolvedAlerts,
        now_ms: u64,
    ) -> Vec<AlertEvent> {
        if !cfg.enabled {
            // Drop every latch so re-enabling starts clean rather than
            // immediately firing a stale standing alert.
            self.latches = Default::default();
            return Vec::new();
        }
        let mut out = Vec::new();
        let sustain_ms = u64::from(cfg.sustain_secs) * 1000;
        let repeat_ms = u64::from(cfg.repeat_secs) * 1000;
        let margin = cfg.clear_margin.clamp(0.0, 0.9);

        for m in AlertMetric::ALL {
            // Rule 4: an absent reading is not an observation. Leave the latch
            // exactly as it was.
            let Some(v) = r.get(m) else { continue };
            let rule = cfg.rule(m);

            let now_level = if rule.breaches(v, AlertLevel::Critical, m) {
                AlertLevel::Critical
            } else if rule.breaches(v, AlertLevel::Warn, m) {
                AlertLevel::Warn
            } else {
                AlertLevel::Ok
            };

            let l = &mut self.latches[m.index()];
            // Rule 3: while an alert stands, "back to Ok" needs the retreat to
            // clear the threshold by `margin`, not merely to touch it.
            let effective = if l.fired > AlertLevel::Ok && now_level < l.fired {
                let thr = rule.threshold(l.fired).unwrap_or(0.0);
                let cleared = if m.is_under() {
                    v > thr * (1.0 + margin)
                } else {
                    v < thr * (1.0 - margin)
                };
                if cleared { now_level } else { l.fired }
            } else {
                now_level
            };

            if !l.seen || effective != l.candidate {
                l.candidate = effective;
                l.since_ms = now_ms;
                l.seen = true;
            }
            let held = now_ms.saturating_sub(l.since_ms);

            if effective > l.fired {
                // Rule 1: rising edge, but only once it has been sustained.
                if held >= sustain_ms {
                    let thr = rule.threshold(effective).unwrap_or(0.0);
                    l.fired = effective;
                    l.last_fired_ms = now_ms;
                    out.push(AlertEvent {
                        metric: m,
                        level: effective,
                        value: v,
                        threshold: thr,
                        repeat: false,
                    });
                }
            } else if effective < l.fired {
                // Clearing also has to be sustained, so a brief dip below the
                // clear threshold doesn't cancel a real, ongoing alert.
                if held >= sustain_ms {
                    l.fired = effective;
                    l.last_fired_ms = now_ms;
                    if cfg.notify_clear || effective > AlertLevel::Ok {
                        out.push(AlertEvent {
                            metric: m,
                            level: effective,
                            value: v,
                            threshold: rule.threshold(effective).unwrap_or(0.0),
                            repeat: false,
                        });
                    }
                }
            } else if l.fired > AlertLevel::Ok
                && now_ms.saturating_sub(l.last_fired_ms) >= repeat_ms
                && repeat_ms > 0
            {
                // Rule 2: standing alert, periodic reminder.
                l.last_fired_ms = now_ms;
                out.push(AlertEvent {
                    metric: m,
                    level: l.fired,
                    value: v,
                    threshold: rule.threshold(l.fired).unwrap_or(0.0),
                    repeat: true,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1000;

    fn cfg() -> ResolvedAlerts {
        let mut c = ResolvedAlerts {
            sustain_secs: 10,
            repeat_secs: 60,
            ..Default::default()
        };
        c.set(
            AlertMetric::Cpu,
            AlertRule {
                warn: 80.0,
                critical: 95.0,
            },
        );
        c.set(
            AlertMetric::DiskFree,
            AlertRule {
                warn: 15.0,
                critical: 5.0,
            },
        );
        c
    }

    fn cpu(v: f32) -> AlertReading {
        AlertReading {
            cpu_pct: Some(v),
            ..Default::default()
        }
    }

    #[test]
    fn a_spike_shorter_than_sustain_is_silent() {
        let (mut s, c) = (AlertState::new(), cfg());
        assert!(s.observe(&cpu(99.0), &c, 0).is_empty());
        // Still inside the sustain window, and then it's over.
        assert!(s.observe(&cpu(99.0), &c, 5 * SEC).is_empty());
        assert!(s.observe(&cpu(10.0), &c, 6 * SEC).is_empty());
        assert!(s.observe(&cpu(10.0), &c, 60 * SEC).is_empty());
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Ok);
    }

    #[test]
    fn a_sustained_breach_fires_exactly_once() {
        let (mut s, c) = (AlertState::new(), cfg());
        assert!(s.observe(&cpu(85.0), &c, 0).is_empty());
        let ev = s.observe(&cpu(85.0), &c, 10 * SEC);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Warn);
        assert_eq!(ev[0].threshold, 80.0);
        assert!(!ev[0].repeat);
        // Still breaching, but inside repeat_secs — silence.
        assert!(s.observe(&cpu(86.0), &c, 20 * SEC).is_empty());
        assert!(s.observe(&cpu(87.0), &c, 40 * SEC).is_empty());
    }

    #[test]
    fn a_value_hovering_on_the_threshold_does_not_flap() {
        // The load-bearing test. 80 is exactly the warn threshold; oscillating
        // across it by a hair must not produce a fire/clear storm.
        let (mut s, c) = (AlertState::new(), cfg());
        s.observe(&cpu(85.0), &c, 0);
        assert_eq!(s.observe(&cpu(85.0), &c, 10 * SEC).len(), 1);
        let mut t = 11 * SEC;
        for i in 0..200 {
            let v = if i % 2 == 0 { 79.9 } else { 80.1 };
            // Scheduled reminders are fine and expected (repeat_secs elapses
            // several times over 200s); what must NOT happen is the level
            // churning up and down as the value crosses the line.
            for ev in s.observe(&cpu(v), &c, t) {
                assert!(ev.repeat, "flapped at t={t} v={v}: {ev:?}");
                assert_eq!(ev.level, AlertLevel::Warn);
            }
            assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Warn, "t={t} v={v}");
            t += SEC;
        }
        // 79.9 never clears the 80 * (1 - 0.05) = 76 clear threshold.
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Warn);
    }

    #[test]
    fn clearing_needs_both_the_margin_and_the_sustain() {
        let (mut s, c) = (AlertState::new(), cfg());
        s.observe(&cpu(85.0), &c, 0);
        s.observe(&cpu(85.0), &c, 10 * SEC);
        // Below the threshold but inside the hysteresis band: not cleared.
        assert!(s.observe(&cpu(78.0), &c, 30 * SEC).is_empty());
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Warn);
        // Past the margin, but not yet sustained.
        assert!(s.observe(&cpu(50.0), &c, 31 * SEC).is_empty());
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Warn);
        // Past the margin AND sustained: cleared. Silent, since notify_clear
        // is off by default.
        assert!(s.observe(&cpu(50.0), &c, 41 * SEC).is_empty());
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Ok);
    }

    #[test]
    fn notify_clear_emits_a_recovery_event() {
        let (mut s, mut c) = (AlertState::new(), cfg());
        c.notify_clear = true;
        s.observe(&cpu(85.0), &c, 0);
        s.observe(&cpu(85.0), &c, 10 * SEC);
        s.observe(&cpu(50.0), &c, 20 * SEC);
        let ev = s.observe(&cpu(50.0), &c, 30 * SEC);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Ok);
        assert!(ev[0].message().contains("recovered"));
    }

    #[test]
    fn a_standing_alert_repeats_on_cadence_not_every_sample() {
        let (mut s, c) = (AlertState::new(), cfg());
        s.observe(&cpu(85.0), &c, 0);
        s.observe(&cpu(85.0), &c, 10 * SEC);
        let mut fires = 0;
        // Five minutes of continuous breach at 1Hz. repeat_secs is 60, so this
        // must produce ~5 reminders, not 300.
        for t in 11..=310u64 {
            fires += s.observe(&cpu(85.0), &c, t * SEC).len();
        }
        assert_eq!(fires, 5, "expected one reminder per repeat_secs");
    }

    #[test]
    fn critical_supersedes_warn_without_a_duplicate() {
        let (mut s, c) = (AlertState::new(), cfg());
        s.observe(&cpu(85.0), &c, 0);
        assert_eq!(
            s.observe(&cpu(85.0), &c, 10 * SEC)[0].level,
            AlertLevel::Warn
        );
        // Escalate: a fresh Critical, and no second Warn.
        s.observe(&cpu(99.0), &c, 11 * SEC);
        let ev = s.observe(&cpu(99.0), &c, 21 * SEC);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Critical);
        assert_eq!(ev[0].threshold, 95.0);
    }

    #[test]
    fn an_absent_reading_never_clears_a_standing_alert() {
        // Rule 4. A machine that stops reporting a sensor must not look like a
        // machine that recovered.
        let (mut s, c) = (AlertState::new(), cfg());
        s.observe(&cpu(85.0), &c, 0);
        s.observe(&cpu(85.0), &c, 10 * SEC);
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Warn);
        for t in 11..100u64 {
            assert!(s.observe(&AlertReading::default(), &c, t * SEC).is_empty());
        }
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Warn);
    }

    #[test]
    fn a_metric_the_platform_lacks_never_fires() {
        // No temps (Windows), no load (Windows) — all-None must be inert even
        // with thresholds configured.
        let (mut s, mut c) = (AlertState::new(), cfg());
        c.rules = [AlertRule {
            warn: 1.0,
            critical: 2.0,
        }; AlertMetric::ALL.len()];
        for t in 0..100u64 {
            assert!(s.observe(&AlertReading::default(), &c, t * SEC).is_empty());
        }
    }

    #[test]
    fn under_metrics_fire_when_falling_below() {
        // Disk space and battery run OUT; the comparison must invert.
        let (mut s, c) = (AlertState::new(), cfg());
        let low = |v| AlertReading {
            disk_free_pct: Some(v),
            ..Default::default()
        };
        // Plenty free: nothing.
        assert!(s.observe(&low(90.0), &c, 0).is_empty());
        assert!(s.observe(&low(90.0), &c, 20 * SEC).is_empty());
        // Down to 10% free — past the warn threshold of 15.
        s.observe(&low(10.0), &c, 21 * SEC);
        let ev = s.observe(&low(10.0), &c, 31 * SEC);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Warn);
        // Clearing needs to rise ABOVE 15 * 1.05 = 15.75.
        s.observe(&low(15.5), &c, 41 * SEC);
        assert!(s.observe(&low(15.5), &c, 60 * SEC).is_empty());
        assert_eq!(s.level(AlertMetric::DiskFree), AlertLevel::Warn);
        s.observe(&low(40.0), &c, 61 * SEC);
        s.observe(&low(40.0), &c, 80 * SEC);
        assert_eq!(s.level(AlertMetric::DiskFree), AlertLevel::Ok);
    }

    #[test]
    fn a_zero_threshold_disables_that_level() {
        let (mut s, mut c) = (AlertState::new(), cfg());
        // Critical only; warn disabled.
        c.set(
            AlertMetric::Cpu,
            AlertRule {
                warn: 0.0,
                critical: 95.0,
            },
        );
        s.observe(&cpu(90.0), &c, 0);
        assert!(s.observe(&cpu(90.0), &c, 30 * SEC).is_empty());
        s.observe(&cpu(99.0), &c, 31 * SEC);
        let ev = s.observe(&cpu(99.0), &c, 41 * SEC);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].level, AlertLevel::Critical);
    }

    #[test]
    fn disabled_is_inert_and_resets_the_latches() {
        let (mut s, mut c) = (AlertState::new(), cfg());
        s.observe(&cpu(99.0), &c, 0);
        s.observe(&cpu(99.0), &c, 10 * SEC);
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Critical);
        c.enabled = false;
        assert!(s.observe(&cpu(99.0), &c, 20 * SEC).is_empty());
        // Re-enabling must not immediately re-announce the stale standing
        // alert; it has to earn it again through sustain.
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Ok);
        c.enabled = true;
        assert!(s.observe(&cpu(99.0), &c, 21 * SEC).is_empty());
        assert_eq!(s.observe(&cpu(99.0), &c, 31 * SEC).len(), 1);
    }

    #[test]
    fn a_backwards_clock_does_not_fabricate_a_sustained_alert() {
        let (mut s, c) = (AlertState::new(), cfg());
        s.observe(&cpu(99.0), &c, 1_000_000);
        // Clock steps backwards (suspend/NTP). `held` must clamp to 0 rather
        // than underflowing into a huge elapsed time.
        assert!(s.observe(&cpu(99.0), &c, 1_000).is_empty());
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Ok);
    }

    #[test]
    fn non_finite_readings_are_treated_as_absent() {
        let (mut s, c) = (AlertState::new(), cfg());
        for t in 0..50u64 {
            assert!(s.observe(&cpu(f32::NAN), &c, t * SEC).is_empty());
            assert!(s.observe(&cpu(f32::INFINITY), &c, t * SEC).is_empty());
        }
        assert_eq!(s.level(AlertMetric::Cpu), AlertLevel::Ok);
    }

    #[test]
    fn zero_sustain_fires_on_the_first_breaching_sample() {
        let (mut s, mut c) = (AlertState::new(), cfg());
        c.sustain_secs = 0;
        assert_eq!(s.observe(&cpu(99.0), &c, 0).len(), 1);
    }

    #[test]
    fn zero_repeat_secs_means_no_reminders() {
        let (mut s, mut c) = (AlertState::new(), cfg());
        c.repeat_secs = 0;
        s.observe(&cpu(99.0), &c, 0);
        assert_eq!(s.observe(&cpu(99.0), &c, 10 * SEC).len(), 1);
        for t in 11..200u64 {
            assert!(s.observe(&cpu(99.0), &c, t * SEC).is_empty());
        }
    }

    #[test]
    fn messages_read_correctly_per_unit() {
        let ev = AlertEvent {
            metric: AlertMetric::Cpu,
            level: AlertLevel::Warn,
            value: 93.6,
            threshold: 80.0,
            repeat: false,
        };
        assert_eq!(ev.message(), "CPU 94% (warn 80%)");
        let ev = AlertEvent {
            metric: AlertMetric::Temp,
            level: AlertLevel::Critical,
            value: 91.2,
            threshold: 85.0,
            repeat: false,
        };
        assert_eq!(ev.message(), "Temperature 91°C (critical 85°C)");
        // Load is unitless and wants decimals, unlike the percentages.
        let ev = AlertEvent {
            metric: AlertMetric::Load,
            level: AlertLevel::Warn,
            value: 4.25,
            threshold: 4.0,
            repeat: false,
        };
        assert_eq!(ev.message(), "Load 4.25 (warn 4.00)");
    }

    #[test]
    fn metric_metadata_is_consistent() {
        // Slugs are stable identifiers; keeping them unique matters because
        // they key the notification inbox.
        let mut keys: Vec<&str> = AlertMetric::ALL.iter().map(|m| m.key()).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n);
        assert!(AlertMetric::DiskFree.is_under() && AlertMetric::Battery.is_under());
        assert!(AlertMetric::ALL.iter().filter(|m| m.is_under()).count() == 2);
        assert!(!AlertMetric::Cpu.is_under());
        assert_eq!(
            AlertState::default().level(AlertMetric::Cpu),
            AlertLevel::Ok
        );
    }
}
