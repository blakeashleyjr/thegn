//! Real production-thread regression with a finite, owned clock. No fixture
//! starts a sampler, opens a database, resolves credentials, or runs a provider.
use super::*;
use std::sync::{Mutex, mpsc};
use thegn_core::config::{CalendarAccount, CalendarProviderKind, Config};

const LIMIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Observations {
    priming: Vec<&'static str>,
    daemon: Vec<u64>,
    containers: Vec<u64>,
}

struct FixtureIo {
    permits: mpsc::Receiver<()>,
    ack: mpsc::SyncSender<u64>,
    finished: mpsc::SyncSender<()>,
    observed: Arc<Mutex<Observations>>,
    tick: u64,
}
impl Drop for FixtureIo {
    fn drop(&mut self) {
        // Nonblocking terminal receipt, including a worker panic before priming.
        self.finished.try_send(()).unwrap_or(());
    }
}
impl TickerIo for FixtureIo {
    fn background_qos(&mut self) {
        self.observed.lock().unwrap().priming.push("qos");
    }
    fn model_refresh_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn prime_stats(&mut self) {
        self.observed.lock().unwrap().priming.push("stats");
    }
    fn prime_daemon(&mut self) {
        self.observed.lock().unwrap().priming.push("daemon");
    }
    fn now_secs(&self) -> i64 {
        if self.tick == 0 {
            self.observed.lock().unwrap().priming.push("clock");
        }
        1_800_000_000 + i64::try_from(self.tick / 2).unwrap()
    }
    fn wait_tick(&mut self, tick: Duration) -> bool {
        assert_eq!(tick, Duration::from_millis(500));
        match self.permits.recv_timeout(LIMIT) {
            Ok(()) => {
                self.tick += 1;
                true
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => false,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("fixture clock was not released"),
        }
    }
    fn send_daemon(&mut self) -> Result<bool, ()> {
        self.observed.lock().unwrap().daemon.push(self.tick);
        Ok(true)
    }
    fn connection_recovery_due(&self) -> bool {
        false
    }
    fn send_stats_if_due(&mut self) -> Result<bool, ()> {
        Ok(false)
    }
    fn send_containers(&mut self, ticks: u64, _every: u64) -> Result<(), ()> {
        self.observed.lock().unwrap().containers.push(ticks);
        Ok(())
    }
    fn tick_complete(&mut self, tick: u64) {
        assert_eq!(tick, self.tick);
        self.ack
            .try_send(tick)
            .expect("one outstanding fixture slot");
    }
}

struct Fixture {
    permits: Option<mpsc::SyncSender<()>>,
    ack: mpsc::Receiver<u64>,
    finished: mpsc::Receiver<()>,
    worker: Option<std::thread::JoinHandle<()>>,
    refresh: tokio_mpsc::UnboundedReceiver<RefreshKind>,
    observed: Arc<Mutex<Observations>>,
    wakes: Arc<std::sync::atomic::AtomicUsize>,
    events: Vec<(u64, &'static str)>,
    tick: u64,
}
impl Fixture {
    fn start(cfg: &Config) -> Self {
        let (permits, clock) = mpsc::sync_channel(1);
        let (ack, receipts) = mpsc::sync_channel(1);
        let (finished, completion) = mpsc::sync_channel(1);
        let (tx, refresh) = tokio_mpsc::unbounded_channel();
        let observed = Arc::new(Mutex::new(Observations::default()));
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notify = wakes.clone();
        // Exactly the config-to-effective-cadence projection used by the live
        // ScheduleOwner. The worker consumes slots, not raw TTLs.
        let cadences = Cadences::from_schedule(&ScheduleConfig::from_config(cfg));
        let worker = spawn_worker(
            cadences,
            tx,
            FixtureIo {
                permits: clock,
                ack,
                finished,
                observed: observed.clone(),
                tick: 0,
            },
            move || {
                notify.fetch_add(1, Ordering::Relaxed);
            },
        );
        Self {
            permits: Some(permits),
            ack: receipts,
            finished: completion,
            worker: Some(worker),
            refresh,
            observed,
            wakes,
            events: Vec::new(),
            tick: 0,
        }
    }

    fn advance_to(&mut self, end: u64) {
        let deadline = Instant::now() + LIMIT;
        while self.tick < end {
            self.permits
                .as_ref()
                .unwrap()
                .try_send(())
                .expect("ticker accepts one slot");
            self.tick += 1;
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert_eq!(
                self.ack
                    .recv_timeout(remaining)
                    .expect("production ticker stopped before slot completed"),
                self.tick
            );
            while let Ok(event) = self.refresh.try_recv() {
                let name = match event {
                    RefreshKind::Model => "model",
                    RefreshKind::Pr => "pr",
                    RefreshKind::MainRefMoved => "main-ref",
                    RefreshKind::HostHeal => "heal",
                    RefreshKind::Ci { force: false } => "ci",
                    RefreshKind::PrQueue => "prq",
                    RefreshKind::Issues => "issues",
                    RefreshKind::Calendar => "calendar",
                    RefreshKind::CalendarReminders => "reminder",
                    RefreshKind::CalendarReminderResult { .. } => "reminder-result",
                    RefreshKind::UsagePoll => "usage",
                    RefreshKind::WeatherPoll => "weather",
                    RefreshKind::AutoFetch { sweep: false } => "fetch-startup",
                    RefreshKind::AutoFetch { sweep: true } => "fetch",
                    RefreshKind::Disk => "disk",
                    RefreshKind::Loc { watch: false } => "loc",
                    RefreshKind::ClockTick => "clock",
                    other => panic!("unexpected production tick event: {other:?}"),
                };
                self.events.push((self.tick, name));
            }
        }
    }

    fn ticks(&self, name: &str) -> Vec<u64> {
        self.events
            .iter()
            .filter_map(|(tick, found)| (*found == name).then_some(*tick))
            .collect()
    }

    fn finish(&mut self) {
        // Disconnecting the fixture clock releases a parked worker, including
        // on assertion unwinding. No producer can leave another slot pending.
        drop(self.permits.take());
        if let Some(worker) = self.worker.take() {
            self.finished
                .recv_timeout(LIMIT)
                .expect("ticker retained after clock shutdown");
            worker.join().expect("production ticker panicked");
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Always close/join, but avoid a second panic during assertion unwinding.
        if std::thread::panicking() {
            drop(self.permits.take());
            if let Some(worker) = self.worker.take()
                && self.finished.recv_timeout(LIMIT).is_ok()
            {
                drop(worker.join());
            }
        } else {
            self.finish();
        }
    }
}

fn configured() -> Config {
    let mut cfg = Config::default();
    cfg.ci.poll_interval_secs = 5;
    cfg.pr_queue.enabled = true;
    cfg.pr_queue.poll_interval_secs = 15;
    cfg.calendar.enabled = true;
    cfg.calendar.refresh_interval_secs = 60;
    cfg.calendar.reminders_enabled = true;
    cfg.calendar.accounts = vec![CalendarAccount {
        name: "fixture".into(),
        provider: CalendarProviderKind::Ics,
        path: "unused-fixture.ics".into(),
        ..Default::default()
    }];
    cfg.usage.enabled = true;
    cfg.usage.poll_interval_secs = 60;
    cfg.weather.enabled = true;
    cfg.weather.refresh_interval_secs = 600;
    cfg.git.auto_fetch = false;
    cfg.loc.enabled = false;
    cfg
}

#[test]
fn adversarial_cadences_keep_the_running_shared_ticker_delivering_unrelated_work() {
    for value in [1u64 << 61, u64::MAX] {
        for family in [
            "ci",
            "prq",
            "calendar-global",
            "calendar-account",
            "usage",
            "weather",
            "all",
        ] {
            let mut cfg = configured();
            if matches!(family, "ci" | "all") {
                cfg.ci.poll_interval_secs = value;
            }
            if matches!(family, "prq" | "all") {
                cfg.pr_queue.poll_interval_secs = value;
            }
            if matches!(family, "calendar-global" | "all") {
                cfg.calendar.refresh_interval_secs = value;
            }
            if family == "calendar-account" {
                cfg.calendar.accounts[0].refresh_interval_secs = value;
            }
            if matches!(family, "usage" | "all") {
                cfg.usage.poll_interval_secs = value;
            }
            if matches!(family, "weather" | "all") {
                cfg.weather.refresh_interval_secs = value;
            }
            let mut ticker = Fixture::start(&cfg);
            ticker.advance_to(80);
            assert_eq!(ticker.ticks("pr"), [40, 80], "{family} {value}");
            assert_eq!(ticker.ticks("main-ref"), [40, 80], "{family} {value}");
            assert_eq!(ticker.ticks("heal"), [30, 60], "{family} {value}");
            assert!(
                ticker.ticks("model").contains(&78),
                "{family} {value}: later model work missing"
            );
            assert_eq!(ticker.ticks("usage"), [USAGE_FIRST_SLOT]);
            assert_eq!(ticker.ticks("weather"), [WEATHER_FIRST_SLOT]);
            if !matches!(family, "ci" | "all") {
                assert_eq!(ticker.ticks("ci"), [10, 20, 30, 40, 50, 60, 70, 80]);
            }
            assert_eq!(
                ticker.observed.lock().unwrap().priming,
                ["qos", "stats", "clock", "daemon"]
            );
            assert_eq!(ticker.observed.lock().unwrap().daemon, [20, 40, 60, 80]);
            assert_eq!(ticker.wakes.load(Ordering::Relaxed), 40);
            ticker.finish();
        }
    }
}

#[test]
fn disabled_optional_cadences_remain_silent_in_the_running_shared_ticker() {
    let mut cfg = configured();
    cfg.pr_queue.enabled = false;
    cfg.calendar.enabled = false;
    cfg.usage.enabled = false;
    cfg.weather.enabled = false;
    let mut ticker = Fixture::start(&cfg);
    ticker.advance_to(120);
    for name in [
        "prq",
        "calendar",
        "reminder",
        "usage",
        "weather",
        "fetch",
        "fetch-startup",
        "loc",
    ] {
        assert!(
            ticker.ticks(name).is_empty(),
            "disabled {name} emitted work"
        );
    }
    assert_eq!(ticker.ticks("pr"), [40, 80, 120]);
    assert_eq!(ticker.ticks("clock"), [120]);
    assert_eq!(ticker.ticks("issues"), [120]);
    ticker.finish();
}

#[test]
fn startup_and_periodic_requests_coalesce_in_the_running_shared_ticker() {
    let mut cfg = configured();
    cfg.git.auto_fetch = true;
    cfg.git.auto_fetch_interval_secs = 3;
    let mut ticker = Fixture::start(&cfg);
    ticker.advance_to(120);
    assert_eq!(ticker.ticks("fetch-startup"), [STARTUP_FETCH_SLOT]);
    assert!(!ticker.ticks("fetch").contains(&STARTUP_FETCH_SLOT));
    assert_eq!(ticker.ticks("prq"), [30, 60, 90, 120]);
    assert_eq!(ticker.ticks("calendar"), [120]);
    assert_eq!(ticker.ticks("issues"), [120]);
    assert_eq!(ticker.ticks("usage"), [USAGE_FIRST_SLOT, 120]);
    assert!(!ticker.ticks("model").contains(&120));
    assert_eq!(ticker.ticks("pr"), [40, 80, 120]);
    ticker.finish();
}

fn scheduled_name(kind: &RefreshKind) -> Option<&'static str> {
    match kind {
        RefreshKind::ClockTick => Some("clock"),
        RefreshKind::Ci { force: false } => Some("ci"),
        RefreshKind::PrQueue => Some("pr_queue"),
        RefreshKind::AutoFetch { .. } => Some("auto_fetch"),
        RefreshKind::Calendar => Some("calendar"),
        RefreshKind::CalendarReminders => Some("calendar_reminders"),
        RefreshKind::Disk => Some("disk"),
        RefreshKind::Loc { .. } => Some("loc"),
        RefreshKind::UsagePoll => Some("usage"),
        RefreshKind::WeatherPoll => Some("weather"),
        _ => None,
    }
}

#[test]
fn live_owner_reloads_every_ticker_class_through_the_event_envelope() {
    let old = ScheduleConfig {
        clock_period_secs: 1,
        ci_every_slots: 2,
        prq_every_slots: Some(3),
        auto_fetch_every_slots: Some(4),
        calendar_every_slots: Some(5),
        calendar_reminders: false,
        disk_every_slots: 6,
        loc_every_slots: Some(7),
        usage_every_slots: Some(8),
        weather_every_slots: Some(9),
    };
    let middle = ScheduleConfig {
        ci_every_slots: 3,
        calendar_reminders: true,
        ..old.clone()
    };
    let new = ScheduleConfig {
        clock_period_secs: 2,
        ci_every_slots: 4,
        prq_every_slots: Some(5),
        auto_fetch_every_slots: Some(6),
        calendar_every_slots: Some(7),
        calendar_reminders: true,
        disk_every_slots: 8,
        loc_every_slots: Some(9),
        usage_every_slots: Some(10),
        weather_every_slots: Some(11),
    };

    let (permits, clock) = mpsc::sync_channel(1);
    let (ack, receipts) = mpsc::sync_channel(1);
    let (finished, completion) = mpsc::sync_channel(1);
    let (tx, mut refresh) = tokio_mpsc::unbounded_channel();
    let envelope_tx = tx.clone();
    let observed = Arc::new(Mutex::new(Observations::default()));
    let commands = Arc::new(Mutex::new(None));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = spawn_worker_with_commands(
        Cadences::from_schedule(&old),
        Arc::new(AtomicU64::new(1)),
        commands.clone(),
        stop.clone(),
        tx,
        FixtureIo {
            permits: clock,
            ack,
            finished,
            observed,
            tick: 0,
        },
        || {},
    );
    let ticker = RefreshTicker {
        command: commands,
        stop,
        worker: Some(worker),
    };
    let mut owner =
        crate::hydrate_schedule::ScheduleOwner::from_test(ticker, old, Arc::new(AtomicU64::new(1)));
    // Two replacements before the worker's next boundary exercise the
    // latest-value command slot and leave only generation 3 live.
    owner.reconfigure_effective(middle);
    owner.reconfigure_effective(new.clone());
    owner.reconfigure_effective(new);
    let stale = RefreshKind::Scheduled {
        generation: 1,
        kind: Box::new(RefreshKind::Ci { force: false }),
    };
    envelope_tx.send(stale).unwrap();

    let mut admitted = std::collections::BTreeSet::new();
    let mut stale_dropped = 0;
    for tick in 1..=100 {
        permits.send(()).unwrap();
        assert_eq!(receipts.recv_timeout(LIMIT).unwrap(), tick);
        while let Ok(event) = refresh.try_recv() {
            let RefreshKind::Scheduled { generation, kind } = event else {
                continue;
            };
            if !owner.is_current(generation) {
                stale_dropped += 1;
                continue;
            }
            if let Some(name) = scheduled_name(&kind) {
                admitted.insert(name);
            }
        }
    }

    // The event-loop admission rule is the same one used by run.rs: stale
    // scheduled envelopes are discarded, while current-generation envelopes
    // reach each of the seven named ticker consumers.
    assert_eq!(stale_dropped, 1, "stale envelope was admitted after reload");
    assert_eq!(
        admitted,
        [
            "auto_fetch",
            "calendar",
            "calendar_reminders",
            "ci",
            "clock",
            "disk",
            "loc",
            "pr_queue",
            "usage",
            "weather",
        ]
        .into_iter()
        .collect()
    );
    assert!(
        owner.is_current(3),
        "unchanged effective schedule restarted"
    );

    // Disable all optional ticker classes while the same production worker is
    // alive. The next command boundary must silence the old schedule without
    // creating replacement wakeups.
    owner.reconfigure_effective(ScheduleConfig {
        clock_period_secs: 60,
        ci_every_slots: 4,
        prq_every_slots: None,
        auto_fetch_every_slots: None,
        calendar_every_slots: None,
        calendar_reminders: false,
        disk_every_slots: 8,
        loc_every_slots: None,
        usage_every_slots: None,
        weather_every_slots: None,
    });
    let mut disabled = std::collections::BTreeSet::new();
    for tick in 101..=140 {
        permits.send(()).unwrap();
        assert_eq!(receipts.recv_timeout(LIMIT).unwrap(), tick);
        while let Ok(event) = refresh.try_recv() {
            if let RefreshKind::Scheduled { generation, kind } = event
                && owner.is_current(generation)
                && let Some(name) = scheduled_name(&kind)
            {
                disabled.insert(name);
            }
        }
    }
    for name in [
        "pr_queue",
        "auto_fetch",
        "calendar",
        "calendar_reminders",
        "loc",
        "usage",
        "weather",
    ] {
        assert!(!disabled.contains(name), "disabled {name} still emitted");
    }
    drop(permits);
    owner.shutdown();
    completion.recv_timeout(LIMIT).unwrap();
}
