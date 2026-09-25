//! The live owner of the shared hydration schedule.
//!
//! The refresh worker deliberately remains one shared 500ms worker.  This
//! module owns the small effective configuration projection that may be
//! replaced while the host is running, and gives every scheduled message a
//! generation so work from a superseded projection can be ignored.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use thegn_core::config::Config;

pub(crate) type ScheduleFence = (Arc<AtomicU64>, u64);

/// The drain coalesces scheduled and event-driven requests for each class.
/// Once an untagged request joins that work, it must win over every scheduled
/// request in the same drain, regardless of arrival order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RefreshGeneration(Option<Option<u64>>);

impl RefreshGeneration {
    pub(crate) fn scheduled(&mut self, generation: u64) {
        if self.0 != Some(None) {
            self.0 = Some(Some(generation));
        }
    }

    pub(crate) fn untagged(&mut self) {
        self.0 = Some(None);
    }

    pub(crate) fn generation(self) -> Option<u64> {
        self.0.flatten()
    }
}

pub(crate) fn generation_is_current(generation: Option<&ScheduleFence>) -> bool {
    generation.is_none_or(|(current, expected)| current.load(Ordering::Acquire) == *expected)
}

/// The configuration that actually changes the shared hydration schedule.
/// Keeping this projection narrow means a theme or unrelated config edit does
/// not restart a ticker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScheduleConfig {
    pub(crate) clock_period_secs: u64,
    pub(crate) ci_poll_secs: u64,
    pub(crate) prq_poll_secs: Option<u64>,
    pub(crate) auto_fetch_secs: Option<u64>,
    pub(crate) calendar_poll_secs: Option<u64>,
    pub(crate) calendar_reminders: bool,
    pub(crate) disk_ttl_secs: u64,
    pub(crate) loc_ttl_secs: Option<u64>,
    pub(crate) usage_poll_secs: Option<u64>,
    pub(crate) weather_poll_secs: Option<u64>,
}

impl ScheduleConfig {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Self {
            clock_period_secs: if thegn_core::config::strftime_needs_seconds(&cfg.bars.clock_format)
                || thegn_core::config::strftime_needs_seconds(&cfg.bars.date_format)
            {
                1
            } else {
                60
            },
            ci_poll_secs: cfg.ci.poll_interval_secs,
            prq_poll_secs: cfg.pr_queue.enabled.then(|| cfg.pr_queue.poll_secs()),
            auto_fetch_secs: cfg
                .git
                .auto_fetch
                .then_some(cfg.git.auto_fetch_interval_secs),
            calendar_poll_secs: cfg.calendar.poll_secs(),
            calendar_reminders: cfg.calendar.reminders_enabled,
            disk_ttl_secs: cfg.disk.scan_interval_secs,
            loc_ttl_secs: cfg.loc.enabled.then_some(cfg.loc.scan_interval_secs),
            usage_poll_secs: cfg.usage.enabled.then(|| cfg.usage.effective_poll_secs()),
            weather_poll_secs: cfg.weather.poll_secs(),
        }
    }

    #[cfg(test)]
    fn changed_slots(&self, next: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        macro_rules! slot {
            ($name:literal, $field:ident) => {
                if self.$field != next.$field {
                    changed.push($name);
                }
            };
        }
        slot!("clock", clock_period_secs);
        slot!("ci", ci_poll_secs);
        slot!("pr_queue", prq_poll_secs);
        slot!("auto_fetch", auto_fetch_secs);
        slot!("calendar", calendar_poll_secs);
        slot!("calendar_reminders", calendar_reminders);
        slot!("disk", disk_ttl_secs);
        slot!("loc", loc_ttl_secs);
        slot!("usage", usage_poll_secs);
        slot!("weather", weather_poll_secs);
        changed
    }
}

/// Owns the one refresh worker and its cancellation/join boundary.
pub(crate) struct ScheduleOwner {
    ticker: crate::hydrate::RefreshTicker,
    effective: ScheduleConfig,
    generation: Arc<AtomicU64>,
}

impl ScheduleOwner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        cfg: &Config,
        tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
        stats_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::StatsTick>,
        container_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::ContainerRefresh>,
        daemon_tx: tokio::sync::mpsc::UnboundedSender<crate::chrome::DaemonStatus>,
        stats_interval_ms: Arc<AtomicU64>,
        stats_live: Arc<std::sync::atomic::AtomicBool>,
        containers_live: Arc<std::sync::atomic::AtomicBool>,
        disk_path: std::path::PathBuf,
        waker: termwiz::terminal::TerminalWaker,
    ) -> Self {
        let effective = ScheduleConfig::from_config(cfg);
        let generation = Arc::new(AtomicU64::new(1));
        let ticker = crate::hydrate::spawn_refresh_ticker(
            effective.clone(),
            generation.clone(),
            tx,
            stats_tx,
            container_tx,
            daemon_tx,
            stats_interval_ms,
            stats_live,
            containers_live,
            disk_path,
            waker,
        );
        Self {
            ticker,
            effective,
            generation,
        }
    }

    pub(crate) fn reconfigure(&mut self, cfg: &Config) {
        let next = ScheduleConfig::from_config(cfg);
        if next == self.effective {
            return;
        }
        self.effective = next.clone();
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.ticker.reconfigure(next, generation);
    }

    #[cfg(test)]
    pub(crate) fn from_test(
        ticker: crate::hydrate::RefreshTicker,
        effective: ScheduleConfig,
        generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            ticker,
            effective,
            generation,
        }
    }

    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    pub(crate) fn fence(&self) -> Arc<AtomicU64> {
        self.generation.clone()
    }

    pub(crate) fn shutdown(&mut self) {
        self.ticker.shutdown();
    }
}

impl Drop for ScheduleOwner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct FakeSlot {
        every: Option<u64>,
        next: Option<u64>,
        identity: u64,
    }

    impl FakeSlot {
        fn new(every: Option<u64>, identity: u64) -> Self {
            Self {
                next: every,
                every,
                identity,
            }
        }

        fn replace(&mut self, every: Option<u64>, now: u64) {
            if self.every != every {
                self.every = every;
                self.next = every.map(|period| now + period);
            }
        }

        fn fire(&mut self, now: u64) -> bool {
            if self.next == Some(now) {
                self.next = self.every.map(|period| now + period);
                true
            } else {
                false
            }
        }
    }

    #[test]
    fn projection_changes_only_the_named_schedule_slots() {
        let mut a = ScheduleConfig::from_config(&Config::default());
        let mut b = a.clone();
        b.ci_poll_secs = b.ci_poll_secs.saturating_add(5);
        b.loc_ttl_secs = Some(120);
        b.weather_poll_secs = Some(600);
        assert_eq!(a.changed_slots(&b), ["ci", "loc", "weather"]);
        a = b;
        assert!(a.changed_slots(&a).is_empty());
    }

    #[test]
    fn projection_diff_covers_every_live_ticker_class() {
        let base = ScheduleConfig::from_config(&Config::default());
        let mut next = base.clone();
        next.clock_period_secs += 1;
        next.ci_poll_secs += 1;
        next.prq_poll_secs = Some(1);
        next.auto_fetch_secs = Some(1);
        next.calendar_poll_secs = Some(1);
        next.calendar_reminders = !next.calendar_reminders;
        next.disk_ttl_secs += 1;
        next.loc_ttl_secs = Some(1);
        next.usage_poll_secs = Some(1);
        next.weather_poll_secs = Some(1);

        assert_eq!(
            base.changed_slots(&next),
            [
                "clock",
                "ci",
                "pr_queue",
                "auto_fetch",
                "calendar",
                "calendar_reminders",
                "disk",
                "loc",
                "usage",
                "weather",
            ]
        );
    }

    #[test]
    fn all_named_classes_are_in_the_effective_projection() {
        let cfg = Config::default();
        let projected = ScheduleConfig::from_config(&cfg);
        assert!(projected.clock_period_secs > 0);
        assert!(projected.ci_poll_secs > 0);
        let _ = (
            projected.prq_poll_secs,
            projected.auto_fetch_secs,
            projected.calendar_poll_secs,
            projected.calendar_reminders,
            projected.disk_ttl_secs,
            projected.loc_ttl_secs,
            projected.usage_poll_secs,
            projected.weather_poll_secs,
        );
    }

    #[test]
    fn fake_time_rearms_every_named_class_without_an_immediate_refresh() {
        let classes = [
            ("clock", Some(2), Some(5)),
            ("ci", Some(4), Some(7)),
            ("pr", Some(3), Some(6)),
            ("usage", Some(8), Some(11)),
            ("weather", Some(10), Some(13)),
            ("calendar", Some(12), Some(15)),
            ("loc", Some(14), Some(17)),
        ];
        for (name, old, new) in classes {
            let mut slot = FakeSlot::new(old, 1);
            slot.replace(new, 20);
            assert!(!slot.fire(20), "{name} refreshed immediately after reload");
            assert!(
                slot.fire(20 + new.unwrap()),
                "{name} missed its next boundary"
            );
            assert_eq!(slot.identity, 1, "{name} restarted its shared worker");
        }
    }

    #[test]
    fn rapid_replacements_keep_only_the_latest_generation() {
        let current = AtomicU64::new(1);
        for generation in [2, 3, 4] {
            current.store(generation, Ordering::Release);
        }
        assert_ne!(current.load(Ordering::Acquire), 2);
        assert_eq!(current.load(Ordering::Acquire), 4);
    }

    #[test]
    fn untagged_refresh_wins_even_when_scheduled_request_arrives_after_it() {
        let mut request = RefreshGeneration::default();
        request.untagged();
        request.scheduled(9);
        assert_eq!(request.generation(), None);

        request = RefreshGeneration::default();
        request.scheduled(8);
        request.untagged();
        assert_eq!(request.generation(), None);
    }

    #[test]
    fn publication_fence_rejects_a_result_after_reload() {
        let current = Arc::new(AtomicU64::new(7));
        let fence = (current.clone(), 7);
        assert!(generation_is_current(Some(&fence)));
        current.store(8, Ordering::Release);
        assert!(!generation_is_current(Some(&fence)));
        // The untagged path has no fence and remains publishable.
        assert!(generation_is_current(None));
    }

    #[test]
    fn all_live_classes_can_be_coalesced_as_untagged_work() {
        for class in [
            "pr",
            "ci",
            "calendar",
            "reminders",
            "loc",
            "usage",
            "weather",
        ] {
            let mut request = RefreshGeneration::default();
            request.scheduled(11);
            request.untagged();
            assert_eq!(
                request.generation(),
                None,
                "{class} inherited a stale fence"
            );
        }
    }
}
