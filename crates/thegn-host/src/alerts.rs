//! Host glue for threshold alerts: turn a metrics snapshot into the plain
//! reading the pure evaluator wants, and route what it emits.
//!
//! The state machine itself lives in [`thegn_core::resource_alert`] — pure, and
//! therefore under core's coverage gate. Everything here is adaptation.

use thegn_core::resource_alert::{AlertEvent, AlertLevel, AlertReading};
use thegn_metrics::StatsSnapshot;

/// Adapt a metrics snapshot to the evaluator's input.
///
/// `None` stays `None` throughout: an unexposed sensor must reach the evaluator
/// as "not observed", not as a comfortable zero, or a machine with no thermal
/// sensor would look like a permanently cool one.
pub(crate) fn reading(s: &StatsSnapshot) -> AlertReading {
    AlertReading {
        cpu_pct: s.cpu_pct.map(f32::from),
        mem_pct: s
            .mem_gib
            .filter(|(_, t)| *t > 0.0)
            .map(|(u, t)| u / t * 100.0),
        swap_pct: s
            .swap_gib
            .filter(|(_, t)| *t > 0.0)
            .map(|(u, t)| u / t * 100.0),
        temp_c: s.cpu_temp_c,
        gpu_pct: s.gpu_pct.map(f32::from),
        // Per core, so one threshold means the same thing on a laptop and a
        // build box.
        load_per_core: s.load_avg.map(|(one, _, _)| {
            let cores = s.cpu_cores.len().max(1) as f32;
            one / cores
        }),
        disk_free_pct: s.disk_free_pct.map(f32::from),
        battery_pct: s.battery.map(|(p, _)| f32::from(p)),
    }
}

/// Toast tone for an event.
pub(crate) fn priority(ev: &AlertEvent) -> thegn_core::notification::Priority {
    use thegn_core::notification::Priority;
    match ev.level {
        AlertLevel::Critical => Priority::Alert,
        AlertLevel::Warn => Priority::Notice,
        // A recovery is good news; it should never raise a flag.
        AlertLevel::Ok => Priority::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_metrics_stay_absent() {
        // The load-bearing adaptation: a missing sensor must not become 0.
        let r = reading(&StatsSnapshot::default());
        assert!(r.cpu_pct.is_none());
        assert!(r.temp_c.is_none());
        assert!(r.load_per_core.is_none());
        assert!(r.battery_pct.is_none());
        assert!(r.mem_pct.is_none());
    }

    #[test]
    fn percentages_are_derived_from_the_raw_pairs() {
        let s = StatsSnapshot {
            mem_gib: Some((8.0, 32.0)),
            swap_gib: Some((2.0, 8.0)),
            cpu_pct: Some(42),
            ..Default::default()
        };
        let r = reading(&s);
        assert_eq!(r.mem_pct, Some(25.0));
        assert_eq!(r.swap_pct, Some(25.0));
        assert_eq!(r.cpu_pct, Some(42.0));
        // A zero total would divide by nothing; treat it as unobserved.
        let z = reading(&StatsSnapshot {
            mem_gib: Some((0.0, 0.0)),
            ..Default::default()
        });
        assert!(z.mem_pct.is_none());
    }

    #[test]
    fn load_is_normalized_per_core() {
        // 4.0 is saturated on 4 cores and idle on 64; the threshold has to mean
        // the same thing on both.
        let s = StatsSnapshot {
            load_avg: Some((4.0, 0.0, 0.0)),
            cpu_cores: vec![0; 4],
            ..Default::default()
        };
        assert_eq!(reading(&s).load_per_core, Some(1.0));
        let big = StatsSnapshot {
            load_avg: Some((4.0, 0.0, 0.0)),
            cpu_cores: vec![0; 16],
            ..Default::default()
        };
        assert_eq!(reading(&big).load_per_core, Some(0.25));
        // No core count reported: divide by one rather than by zero.
        let bare = StatsSnapshot {
            load_avg: Some((4.0, 0.0, 0.0)),
            ..Default::default()
        };
        assert_eq!(reading(&bare).load_per_core, Some(4.0));
    }

    #[test]
    fn priorities_map_by_level() {
        use thegn_core::notification::Priority;
        use thegn_core::resource_alert::AlertMetric;
        let ev = |level| AlertEvent {
            metric: AlertMetric::Cpu,
            level,
            value: 1.0,
            threshold: 1.0,
            repeat: false,
        };
        assert_eq!(priority(&ev(AlertLevel::Critical)), Priority::Alert);
        assert_eq!(priority(&ev(AlertLevel::Warn)), Priority::Notice);
        // Recovering is good news — never a flag.
        assert_eq!(priority(&ev(AlertLevel::Ok)), Priority::Info);
    }
}
