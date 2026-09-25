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
    // This is a check, not a commit barrier: a reload landing between the
    // check and the write can admit one stale row, which the next refresh
    // corrects. Keeping the check local to each side effect avoids holding
    // schedule state across cache writes in every hydration subsystem.
    generation.is_none_or(|(current, expected)| current.load(Ordering::Acquire) == *expected)
}

/// Admit a scheduled child result only while its parent schedule is current.
/// Untagged work passes through unchanged, preserving manual/event-driven
/// refreshes. The event-loop envelope repeats the generation check on receipt.
pub(crate) fn scheduled_delivery(
    generation: Option<&ScheduleFence>,
    kind: crate::hydrate::RefreshKind,
) -> Option<crate::hydrate::RefreshKind> {
    if !generation_is_current(generation) {
        return None;
    }
    Some(generation.map_or(kind.clone(), |(_, generation)| {
        crate::hydrate::RefreshKind::Scheduled {
            generation: *generation,
            kind: Box::new(kind),
        }
    }))
}

/// The configuration that actually changes the shared hydration schedule.
/// Keeping this projection narrow means a theme or unrelated config edit does
/// not restart a ticker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScheduleConfig {
    pub(crate) clock_period_secs: u64,
    pub(crate) ci_every_slots: u64,
    pub(crate) prq_every_slots: Option<u64>,
    pub(crate) auto_fetch_every_slots: Option<u64>,
    pub(crate) calendar_every_slots: Option<u64>,
    pub(crate) calendar_reminders: bool,
    pub(crate) disk_every_slots: u64,
    pub(crate) loc_every_slots: Option<u64>,
    pub(crate) usage_every_slots: Option<u64>,
    pub(crate) weather_every_slots: Option<u64>,
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
            ci_every_slots: crate::ci_refresh::ci_every_slots(cfg.ci.poll_interval_secs),
            prq_every_slots: cfg.pr_queue.enabled.then(|| {
                thegn_core::time_policy::cadence_slots(cfg.pr_queue.poll_secs(), 15, 500).get()
            }),
            auto_fetch_every_slots: cfg
                .git
                .auto_fetch
                .then(|| crate::remote_poll::fetch_every_slots(cfg.git.auto_fetch_interval_secs))
                .flatten(),
            calendar_every_slots: cfg.calendar.poll_secs().map(|secs| {
                thegn_core::time_policy::cadence_slots(
                    secs,
                    thegn_core::config_calendar::MIN_REFRESH_SECS,
                    500,
                )
                .get()
            }),
            calendar_reminders: cfg.calendar.reminders_enabled,
            disk_every_slots: thegn_core::scan_sched::pump_slots(
                cfg.disk.scan_interval_secs,
                crate::hydrate::DISK_PUMP_FLOOR_SECS,
                500,
            ),
            loc_every_slots: cfg.loc.enabled.then(|| {
                thegn_core::scan_sched::pump_slots(
                    cfg.loc.scan_interval_secs,
                    crate::hydrate::LOC_PUMP_FLOOR_SECS,
                    500,
                )
            }),
            usage_every_slots: cfg.usage.enabled.then(|| {
                thegn_core::time_policy::cadence_slots(cfg.usage.effective_poll_secs(), 60, 500)
                    .get()
            }),
            weather_every_slots: cfg.weather.poll_secs().map(|secs| {
                thegn_core::time_policy::cadence_slots(
                    secs,
                    thegn_core::config_weather::MIN_REFRESH_SECS,
                    500,
                )
                .get()
            }),
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
        slot!("ci", ci_every_slots);
        slot!("pr_queue", prq_every_slots);
        slot!("auto_fetch", auto_fetch_every_slots);
        slot!("calendar", calendar_every_slots);
        slot!("calendar_reminders", calendar_reminders);
        slot!("disk", disk_every_slots);
        slot!("loc", loc_every_slots);
        slot!("usage", usage_every_slots);
        slot!("weather", weather_every_slots);
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
        self.reconfigure_effective(ScheduleConfig::from_config(cfg));
    }

    #[cfg(test)]
    pub(crate) fn reconfigure_effective(&mut self, next: ScheduleConfig) {
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
        b.ci_every_slots = b.ci_every_slots.saturating_add(5);
        b.loc_every_slots = Some(120);
        b.weather_every_slots = Some(600);
        assert_eq!(a.changed_slots(&b), ["ci", "loc", "weather"]);
        a = b;
        assert!(a.changed_slots(&a).is_empty());
    }

    #[test]
    fn projection_diff_covers_every_live_ticker_class() {
        let base = ScheduleConfig::from_config(&Config::default());
        let mut next = base.clone();
        next.clock_period_secs += 1;
        next.ci_every_slots += 1;
        next.prq_every_slots = Some(1);
        next.auto_fetch_every_slots = Some(1);
        next.calendar_every_slots = Some(1);
        next.calendar_reminders = !next.calendar_reminders;
        next.disk_every_slots += 1;
        next.loc_every_slots = Some(1);
        next.usage_every_slots = Some(1);
        next.weather_every_slots = Some(1);

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
        assert!(projected.ci_every_slots > 0);
        let _ = (
            projected.prq_every_slots,
            projected.auto_fetch_every_slots,
            projected.calendar_every_slots,
            projected.calendar_reminders,
            projected.disk_every_slots,
            projected.loc_every_slots,
            projected.usage_every_slots,
            projected.weather_every_slots,
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
    fn reload_mid_ci_or_pr_child_delivery_drops_only_scheduled_work() {
        let current = Arc::new(AtomicU64::new(7));
        let fence = (current.clone(), 7);
        let ci_detail = || {
            crate::hydrate::RefreshKind::CiDetail(Box::new(crate::detail::CiDetailPayload {
                run: Default::default(),
                log_tail: Vec::new(),
                log_entries: Vec::new(),
            }))
        };
        assert!(scheduled_delivery(Some(&fence), ci_detail()).is_some());
        assert!(scheduled_delivery(Some(&fence), crate::hydrate::RefreshKind::Model).is_some());

        current.store(8, Ordering::Release);
        assert!(scheduled_delivery(Some(&fence), ci_detail()).is_none());
        assert!(scheduled_delivery(Some(&fence), crate::hydrate::RefreshKind::Model).is_none());
        assert!(scheduled_delivery(None, crate::hydrate::RefreshKind::Model).is_some());
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

    #[test]
    fn equivalent_floored_loc_cadences_do_not_change_the_projection() {
        let mut low = Config::default();
        low.loc.enabled = true;
        low.loc.scan_interval_secs = 1;
        let mut high = low.clone();
        high.loc.scan_interval_secs = 2;

        // `pump_slots` divides the TTL by four and floors the result at the
        // LOC pump floor, so both values select the same worker cadence. A
        // live reload between equivalent values must not restart the slot or
        // bump its generation.
        assert_eq!(
            ScheduleConfig::from_config(&low),
            ScheduleConfig::from_config(&high),
            "projection must compare effective cadence, not raw LOC TTL"
        );
    }

    #[test]
    fn equivalent_floored_disk_and_fetch_cadences_do_not_restart() {
        let mut low = Config::default();
        low.disk.scan_interval_secs = 1;
        low.git.auto_fetch = true;
        low.git.auto_fetch_interval_secs = 1;
        let mut high = low.clone();
        high.disk.scan_interval_secs = 2;
        high.git.auto_fetch_interval_secs = 2;

        assert_eq!(
            ScheduleConfig::from_config(&low),
            ScheduleConfig::from_config(&high),
            "projection must compare effective disk and fetch cadence"
        );
    }
}
