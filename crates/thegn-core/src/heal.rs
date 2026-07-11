//! Background host-heal scheduling: when a remote host row is parked on
//! `Failed(retryable)`, thegn re-drives its bring-up on a growing backoff
//! until it recovers — the sticky-failed-state fix. Pure: the host crate owns
//! the ticker, the DB reads, and the off-thread re-drives; this module only
//! answers "is attempt N due yet?".

/// Re-probe cadence for a failed host. `steps_secs[n]` is the wait before
/// attempt `n+1`; past the end, the LAST step repeats forever — an hour-long
/// outage still heals eventually, at the capped cadence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealSchedule {
    pub steps_secs: Vec<u64>,
}

impl Default for HealSchedule {
    fn default() -> Self {
        HealSchedule {
            steps_secs: vec![15, 30, 60, 300],
        }
    }
}

/// The installed `[remote] heal_backoff_secs` schedule (first set wins — see
/// `config_remote::RemoteConfig::install`).
static SCHEDULE: std::sync::OnceLock<HealSchedule> = std::sync::OnceLock::new();

/// Install the resolved heal schedule from `[remote]` config.
pub fn set_schedule(s: HealSchedule) {
    let _ = SCHEDULE.set(s); // best-effort: first-set-wins by design
}

/// The active schedule (defaults before config load and in tests).
pub fn active_schedule() -> HealSchedule {
    SCHEDULE.get().cloned().unwrap_or_default()
}

impl HealSchedule {
    /// Build from a config list; empty/invalid input falls back to the default.
    pub fn from_config(steps: &[u64]) -> Self {
        let steps: Vec<u64> = steps.iter().copied().filter(|s| *s > 0).collect();
        if steps.is_empty() {
            HealSchedule::default()
        } else {
            HealSchedule { steps_secs: steps }
        }
    }

    /// The wait (seconds) before heal attempt `attempt` (0-based: the wait
    /// after the `attempt`-th failure). Clamps to the last step.
    pub fn wait_secs(&self, attempt: u32) -> u64 {
        let i = (attempt as usize).min(self.steps_secs.len() - 1);
        self.steps_secs[i]
    }

    /// Is heal attempt number `attempt` (0-based count of attempts already
    /// made) due, given the unix time of the last attempt (or of the failure,
    /// for attempt 0) and now?
    pub fn due(&self, attempt: u32, last_attempt: i64, now: i64) -> bool {
        now.saturating_sub(last_attempt) >= self.wait_secs(attempt) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_grows_then_caps() {
        let s = HealSchedule::default();
        assert_eq!(s.wait_secs(0), 15);
        assert_eq!(s.wait_secs(1), 30);
        assert_eq!(s.wait_secs(2), 60);
        assert_eq!(s.wait_secs(3), 300);
        assert_eq!(s.wait_secs(4), 300, "last step repeats");
        assert_eq!(s.wait_secs(100), 300, "forever");
    }

    #[test]
    fn due_table() {
        let s = HealSchedule::default();
        // Attempt 0 is due 15s after the failure.
        assert!(!s.due(0, 1000, 1010));
        assert!(s.due(0, 1000, 1015));
        // Attempt 3+ waits the 300s cap.
        assert!(!s.due(3, 1000, 1250));
        assert!(s.due(3, 1000, 1300));
        assert!(s.due(50, 1000, 1300), "capped cadence repeats forever");
        // Clock skew (now < last) never fires early.
        assert!(!s.due(0, 1000, 900));
    }

    #[test]
    fn from_config_filters_and_falls_back() {
        assert_eq!(
            HealSchedule::from_config(&[10, 0, 20]).steps_secs,
            vec![10, 20],
            "zeros dropped"
        );
        assert_eq!(
            HealSchedule::from_config(&[]),
            HealSchedule::default(),
            "empty falls back"
        );
        assert_eq!(
            HealSchedule::from_config(&[0, 0]),
            HealSchedule::default(),
            "all-zero falls back"
        );
    }
}
