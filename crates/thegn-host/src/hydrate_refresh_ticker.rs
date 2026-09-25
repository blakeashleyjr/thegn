//! Shared refresh worker and its owned I/O boundary. Scheduling is identical
//! for the live host and the channel-clock integration fixture.
use super::{
    CONTAINER_DF_EVERY_TICKS, CONTAINER_REFRESH_INTERVAL, ContainerRefresh,
    DAEMON_REFRESH_INTERVAL, DISK_PUMP_FLOOR_SECS, ISSUE_REFRESH_INTERVAL, LOC_PUMP_FLOOR_SECS,
    PR_REFRESH_INTERVAL, RefreshKind, STARTUP_FETCH_SLOT, STARTUP_MEASURE_SLOT, StatsTick,
    USAGE_FIRST_SLOT, WEATHER_FIRST_SLOT, weather_every_slots,
};
use crate::hydrate_schedule::ScheduleConfig;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

/// Runs on a dedicated OS thread (not `tokio::spawn`) so it can never be starved
/// by the main loop blocking a runtime worker in `poll_input(None)` — true even
/// on a single-core runtime. The thread sleeps in 500ms half-ticks: fine enough
/// for the Telemetry section's live graphs (`stats_live` set while it's open)
/// while the model/PR cadences (default 1s/20s, model tunable via
/// `THEGN_MODEL_REFRESH_MS`) stay whole multiples of the half-tick.
pub(crate) struct RefreshTicker {
    command: std::sync::mpsc::Sender<TickerCommand>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

enum TickerCommand {
    Replace {
        schedule: ScheduleConfig,
        generation: u64,
    },
}

impl RefreshTicker {
    pub(crate) fn reconfigure(&self, schedule: ScheduleConfig, generation: u64) {
        let _ = self.command.send(TickerCommand::Replace {
            schedule,
            generation,
        });
    }

    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for RefreshTicker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)] // one-call-site startup wiring, not an API
pub(crate) fn spawn_refresh_ticker(
    schedule: ScheduleConfig,
    generation: Arc<AtomicU64>,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    stats_tx: tokio_mpsc::UnboundedSender<StatsTick>,
    container_tx: tokio_mpsc::UnboundedSender<ContainerRefresh>,
    daemon_tx: tokio_mpsc::UnboundedSender<crate::chrome::DaemonStatus>,
    stats_interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stats_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Set while a per-container-stats surface is visible; gates the expensive
    // `stats --no-stream` + `system df` container enrichment.
    containers_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    disk_path: std::path::PathBuf,
    waker: TerminalWaker,
) -> RefreshTicker {
    let (command, commands) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker = spawn_worker_with_commands(
        Cadences::from_schedule(&schedule),
        generation,
        commands,
        stop.clone(),
        tx,
        LiveIo {
            stats_tx,
            container_tx,
            daemon_tx,
            stats_interval_ms,
            stats_live,
            containers_live,
            disk_path: Some(disk_path),
            sampler: None,
            last_stats: None,
            daemon_scope: None,
        },
        move || {
            drop(waker.wake());
        },
    );
    RefreshTicker {
        command,
        stop,
        worker: Some(worker),
    }
}

/// Owned ambient boundaries only. All cadence conversion and event scheduling
/// stays in `spawn_worker`; tests replace I/O without replacing that loop.
trait TickerIo: Send + 'static {
    fn background_qos(&mut self);
    fn model_refresh_interval(&self) -> Duration;
    fn prime_stats(&mut self);
    fn prime_daemon(&mut self);
    fn now_secs(&self) -> i64;
    fn wait_tick(&mut self, tick: Duration) -> bool;
    fn send_daemon(&mut self) -> Result<bool, ()>;
    fn connection_recovery_due(&self) -> bool;
    fn send_stats_if_due(&mut self) -> Result<bool, ()>;
    fn send_containers(&mut self, ticks: u64, every: u64) -> Result<(), ()>;
    fn tick_complete(&mut self, _tick: u64) {}
}

struct Cadences {
    ci_poll_secs: u64,
    prq_poll_secs: Option<u64>,
    auto_fetch_secs: Option<u64>,
    clock_period_secs: Arc<AtomicU64>,
    calendar_poll_secs: Option<u64>,
    calendar_reminders: bool,
    disk_ttl_secs: u64,
    loc_ttl_secs: Option<u64>,
    usage_poll_secs: Option<u64>,
    weather_poll_secs: Option<u64>,
}

impl Cadences {
    fn from_schedule(schedule: &ScheduleConfig) -> Self {
        Self {
            ci_poll_secs: schedule.ci_poll_secs,
            prq_poll_secs: schedule.prq_poll_secs,
            auto_fetch_secs: schedule.auto_fetch_secs,
            clock_period_secs: Arc::new(AtomicU64::new(schedule.clock_period_secs)),
            calendar_poll_secs: schedule.calendar_poll_secs,
            calendar_reminders: schedule.calendar_reminders,
            disk_ttl_secs: schedule.disk_ttl_secs,
            loc_ttl_secs: schedule.loc_ttl_secs,
            usage_poll_secs: schedule.usage_poll_secs,
            weather_poll_secs: schedule.weather_poll_secs,
        }
    }
}

fn clock_unit(period: &AtomicU64, now: i64) -> i64 {
    let period = thegn_core::time_policy::saturating_i64(u128::from(
        period
            .load(Ordering::Relaxed)
            .clamp(1, thegn_core::time_policy::MAX_CADENCE_SECS),
    ));
    now.div_euclid(period)
}

fn send_tick(
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    generation: u64,
    kind: RefreshKind,
) -> Result<(), ()> {
    let kind = if generation == 0 {
        kind
    } else {
        RefreshKind::Scheduled {
            generation,
            kind: Box::new(kind),
        }
    };
    tx.send(kind).map_err(|_| ())
}

fn due(tick: u64, every: u64, after: u64) -> bool {
    tick >= after && tick.is_multiple_of(every)
}

/// This is the shared production spawner, also used by the hermetic fixture.
/// Retaining its handle lets tests close their clock and join the actual worker.
fn spawn_worker(
    cadences: Cadences,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    mut io: impl TickerIo,
    notify: impl Fn() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    spawn_worker_inner(cadences, 0, None, None, tx, io, notify)
}

fn spawn_worker_with_commands(
    cadences: Cadences,
    _generation: Arc<AtomicU64>,
    commands: std::sync::mpsc::Receiver<TickerCommand>,
    stop: Arc<AtomicBool>,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    io: impl TickerIo,
    notify: impl Fn() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    spawn_worker_inner(cadences, 1, Some(commands), Some(stop), tx, io, notify)
}

fn spawn_worker_inner(
    cadences: Cadences,
    generation: u64,
    commands: Option<std::sync::mpsc::Receiver<TickerCommand>>,
    stop: Option<Arc<AtomicBool>>,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    mut io: impl TickerIo,
    notify: impl Fn() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _failure = crate::worker_failure::PanicNotify::new(|| {
            tracing::error!(target: "thegn::hydrate", "shared refresh ticker terminated by panic; periodic refresh is unavailable");
            notify();
        });
        let Cadences {
            ci_poll_secs,
            prq_poll_secs,
            auto_fetch_secs,
            clock_period_secs,
            calendar_poll_secs,
            calendar_reminders,
            disk_ttl_secs,
            loc_ttl_secs,
            usage_poll_secs,
            weather_poll_secs,
        } = cadences;
        let mut ci_poll_secs = ci_poll_secs;
        let mut prq_poll_secs = prq_poll_secs;
        let mut auto_fetch_secs = auto_fetch_secs;
        let mut clock_period_secs = clock_period_secs;
        let mut calendar_poll_secs = calendar_poll_secs;
        let mut calendar_reminders = calendar_reminders;
        let mut disk_ttl_secs = disk_ttl_secs;
        let mut loc_ttl_secs = loc_ttl_secs;
        let mut usage_poll_secs = usage_poll_secs;
        let mut weather_poll_secs = weather_poll_secs;
        // The 500ms refresh ticker: it only decides when to *ask* for work, and
        // every consumer is off the render path.
        io.background_qos();
        let tick = Duration::from_millis(500);
        let model_every = thegn_core::time_policy::cadence_millis_slots(
            io.model_refresh_interval().as_millis(),
            500,
        );
        let pr_every =
            thegn_core::time_policy::cadence_millis_slots(PR_REFRESH_INTERVAL.as_millis(), 500);
        let mut ci_every = crate::ci_refresh::ci_every_slots(ci_poll_secs);
        let mut fetch_every = auto_fetch_secs.and_then(crate::remote_poll::fetch_every_slots);
        let issue_every =
            thegn_core::time_policy::cadence_millis_slots(ISSUE_REFRESH_INTERVAL.as_millis(), 500)
                .get();
        // Floored the same way `[pr_queue] poll_secs` is, so a misconfigured 0
        // can't spin the ticker against the forge's rate limit.
        let mut prq_every =
            prq_poll_secs.map(|s| thegn_core::time_policy::cadence_slots(s, 15, 500).get());
        // Floored the same way, so a misconfigured 0 can't spin against a
        // provider's rate limit. (`CalendarAccount::refresh_secs` already
        // clamps; this is belt-and-braces at the one place that loops.)
        let mut calendar_every = calendar_poll_secs.map(|s| {
            thegn_core::time_policy::cadence_slots(
                s,
                thegn_core::config_calendar::MIN_REFRESH_SECS,
                500,
            )
            .get()
        });
        // Reminders are checked on a coarse fixed cadence: worst-case 30s
        // lateness is irrelevant for a "10 minutes before" alert, and the check
        // is pure, so this is far cheaper than a per-reminder timer.
        let reminder_every = 60u64;
        // `UsageConfig::effective_poll_secs` already floors this at 60; the
        // `.max(60)` here is the same belt-and-braces as the calendar slot, so
        // the one place that loops can't be made to spin from config.
        let mut usage_every =
            usage_poll_secs.map(|s| thegn_core::time_policy::cadence_slots(s, 60, 500).get());
        let mut weather_every = weather_every_slots(weather_poll_secs);
        let container_every = thegn_core::time_policy::cadence_millis_slots(
            CONTAINER_REFRESH_INTERVAL.as_millis(),
            500,
        )
        .get();
        let mut disk_every =
            thegn_core::scan_sched::pump_slots(disk_ttl_secs, DISK_PUMP_FLOOR_SECS, 500);
        let mut loc_every =
            loc_ttl_secs.map(|s| thegn_core::scan_sched::pump_slots(s, LOC_PUMP_FLOOR_SECS, 500));
        let daemon_every =
            thegn_core::time_policy::cadence_millis_slots(DAEMON_REFRESH_INTERVAL.as_millis(), 500)
                .get();
        let heal_every = 30; // 15s host-heal consideration (backoff: core::heal)
        let mut ticks: u64 = 0;
        let mut ci_after = 0;
        let mut prq_after = 0;
        let mut fetch_after = 0;
        let mut calendar_after = 0;
        let mut reminder_after = 0;
        let mut disk_after = 0;
        let mut loc_after = 0;
        let mut usage_after = 0;
        let mut weather_after = 0;
        let mut active_generation = generation;
        io.prime_stats();
        // Keep the initial clock observation between stats and daemon priming.
        let mut last_clock_unit = clock_unit(&clock_period_secs, io.now_secs());
        io.prime_daemon();
        loop {
            if !io.wait_tick(tick) {
                break;
            }
            if stop
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                break;
            }
            if let Some(commands) = &commands {
                let mut replacement = None;
                while let Ok(command) = commands.try_recv() {
                    replacement = Some(command);
                }
                if let Some(TickerCommand::Replace {
                    schedule,
                    generation,
                }) = replacement
                {
                    let next = Cadences::from_schedule(&schedule);
                    let ci_changed = ci_poll_secs != next.ci_poll_secs;
                    let prq_changed = prq_poll_secs != next.prq_poll_secs;
                    let fetch_changed = auto_fetch_secs != next.auto_fetch_secs;
                    let calendar_changed = calendar_poll_secs != next.calendar_poll_secs;
                    let disk_changed = disk_ttl_secs != next.disk_ttl_secs;
                    let loc_changed = loc_ttl_secs != next.loc_ttl_secs;
                    let usage_changed = usage_poll_secs != next.usage_poll_secs;
                    let weather_changed = weather_poll_secs != next.weather_poll_secs;
                    let rearm = |old: u64, new: u64, tick: u64| {
                        (old != new).then_some(tick.saturating_add(new))
                    };
                    let rearm_opt = |old: Option<u64>, new: Option<u64>, tick: u64| {
                        (old != new)
                            .then_some(new.map(|n| tick.saturating_add(n)))
                            .flatten()
                    };
                    ci_after = rearm(ci_poll_secs, next.ci_poll_secs, ticks).unwrap_or(ci_after);
                    prq_after =
                        rearm_opt(prq_poll_secs, next.prq_poll_secs, ticks).unwrap_or(prq_after);
                    fetch_after = rearm_opt(auto_fetch_secs, next.auto_fetch_secs, ticks)
                        .unwrap_or(fetch_after);
                    calendar_after = rearm_opt(calendar_poll_secs, next.calendar_poll_secs, ticks)
                        .unwrap_or(calendar_after);
                    if calendar_poll_secs != next.calendar_poll_secs
                        || calendar_reminders != next.calendar_reminders
                    {
                        reminder_after = next
                            .calendar_poll_secs
                            .map(|_| ticks.saturating_add(60))
                            .unwrap_or(0);
                    }
                    disk_after =
                        rearm(disk_ttl_secs, next.disk_ttl_secs, ticks).unwrap_or(disk_after);
                    loc_after =
                        rearm_opt(loc_ttl_secs, next.loc_ttl_secs, ticks).unwrap_or(loc_after);
                    usage_after = rearm_opt(usage_poll_secs, next.usage_poll_secs, ticks)
                        .unwrap_or(usage_after);
                    weather_after = rearm_opt(weather_poll_secs, next.weather_poll_secs, ticks)
                        .unwrap_or(weather_after);
                    ci_poll_secs = next.ci_poll_secs;
                    prq_poll_secs = next.prq_poll_secs;
                    auto_fetch_secs = next.auto_fetch_secs;
                    clock_period_secs = Arc::new(AtomicU64::new(next.clock_period_secs));
                    calendar_poll_secs = next.calendar_poll_secs;
                    calendar_reminders = next.calendar_reminders;
                    disk_ttl_secs = next.disk_ttl_secs;
                    loc_ttl_secs = next.loc_ttl_secs;
                    usage_poll_secs = next.usage_poll_secs;
                    weather_poll_secs = next.weather_poll_secs;
                    ci_every = crate::ci_refresh::ci_every_slots(ci_poll_secs);
                    fetch_every = auto_fetch_secs.and_then(crate::remote_poll::fetch_every_slots);
                    prq_every = prq_poll_secs
                        .map(|s| thegn_core::time_policy::cadence_slots(s, 15, 500).get());
                    calendar_every = calendar_poll_secs.map(|s| {
                        thegn_core::time_policy::cadence_slots(
                            s,
                            thegn_core::config_calendar::MIN_REFRESH_SECS,
                            500,
                        )
                        .get()
                    });
                    disk_every = thegn_core::scan_sched::pump_slots(
                        disk_ttl_secs,
                        DISK_PUMP_FLOOR_SECS,
                        500,
                    );
                    loc_every = loc_ttl_secs
                        .map(|s| thegn_core::scan_sched::pump_slots(s, LOC_PUMP_FLOOR_SECS, 500));
                    usage_every = usage_poll_secs
                        .map(|s| thegn_core::time_policy::cadence_slots(s, 60, 500).get());
                    weather_every = weather_every_slots(weather_poll_secs);
                    if ci_changed {
                        ci_after = ticks.saturating_add(ci_every);
                    }
                    if prq_changed {
                        prq_after = prq_every.map(|n| ticks.saturating_add(n)).unwrap_or(0);
                    }
                    if fetch_changed {
                        fetch_after = fetch_every.map(|n| ticks.saturating_add(n)).unwrap_or(0);
                    }
                    if calendar_changed {
                        calendar_after =
                            calendar_every.map(|n| ticks.saturating_add(n)).unwrap_or(0);
                    }
                    if disk_changed {
                        disk_after = ticks.saturating_add(disk_every);
                    }
                    if loc_changed {
                        loc_after = loc_every.map(|n| ticks.saturating_add(n)).unwrap_or(0);
                    }
                    if usage_changed {
                        usage_after = usage_every.map(|n| ticks.saturating_add(n)).unwrap_or(0);
                    }
                    if weather_changed {
                        weather_after = weather_every.map(|n| ticks.saturating_add(n)).unwrap_or(0);
                    }
                    last_clock_unit = clock_unit(&clock_period_secs, io.now_secs());
                    active_generation = generation;
                }
            }
            ticks = ticks.wrapping_add(1);
            let mut wake = false;
            if let Some(work) = crate::refresh_schedule::model_or_pr(ticks, model_every, pr_every) {
                let kind = match work {
                    crate::refresh_schedule::ModelRefresh::Pr => RefreshKind::Pr,
                    crate::refresh_schedule::ModelRefresh::Model => RefreshKind::Model,
                };
                if send_tick(&tx, active_generation, kind).is_err() {
                    break; // loop gone
                }
                wake = true;
            }
            // CI run-history on its own `[ci] poll_interval_secs` cadence (AV
            // group); the refresh itself further coalesces via `[ci] ttl_secs`.
            if due(ticks, ci_every, ci_after) {
                if send_tick(&tx, active_generation, RefreshKind::Ci { force: false }).is_err() {
                    break;
                }
                wake = true;
            }
            if ticks.is_multiple_of(issue_every) {
                if send_tick(&tx, active_generation, RefreshKind::Issues).is_err() {
                    break;
                }
                wake = true;
            }
            if let Some(n) = prq_every
                && due(ticks, n, prq_after)
            {
                if send_tick(&tx, active_generation, RefreshKind::PrQueue).is_err() {
                    break;
                }
                wake = true;
            }
            // Remote poll (`[git] auto_fetch`). The one-shot STARTUP_FETCH_SLOT
            // kick is what makes a freshly-opened session show the night's
            // commits; it deliberately trails the first frame by a few seconds so
            // a network round trip can never sit on the launch path. After that
            // the configured cadence takes over (and sweeps the background
            // worktrees). Both are coalesced per-repo by `remote_poll`.
            if auto_fetch_secs.is_some()
                && ((fetch_after == 0 && ticks == STARTUP_FETCH_SLOT)
                    || fetch_every.is_some_and(|n| due(ticks, n, fetch_after)))
            {
                let sweep = ticks != STARTUP_FETCH_SLOT;
                if send_tick(&tx, active_generation, RefreshKind::AutoFetch { sweep }).is_err() {
                    break;
                }
                wake = true;
            }
            // Measurement pumps, plus a one-shot startup kick so the first
            // sizes/counts land seconds after launch rather than after a full
            // pump interval. Both scans coalesce internally (a target inside its
            // TTL is planned away), so the startup slot coinciding with a pump
            // costs nothing.
            if (disk_after == 0 && ticks == STARTUP_MEASURE_SLOT)
                || due(ticks, disk_every, disk_after)
            {
                if send_tick(&tx, active_generation, RefreshKind::Disk).is_err() {
                    break;
                }
                wake = true;
            }
            if let Some(n) = loc_every
                && ((loc_after == 0 && ticks == STARTUP_MEASURE_SLOT) || due(ticks, n, loc_after))
            {
                if send_tick(&tx, active_generation, RefreshKind::Loc { watch: false }).is_err() {
                    break;
                }
                wake = true;
            }
            // AI-account usage. The first poll rides `USAGE_FIRST_SLOT` rather
            // than the cadence so the badge fills within seconds of launch
            // instead of after the first full interval — but deliberately not
            // at tick 0, so a network round trip is never on the launch path.
            if usage_every.is_some_and(|n| {
                (usage_after == 0 && ticks == USAGE_FIRST_SLOT) || due(ticks, n, usage_after)
            }) {
                if send_tick(&tx, active_generation, RefreshKind::UsagePoll).is_err() {
                    break;
                }
                wake = true;
            }
            // Weather, on the same shape: a one-shot startup slot so the widget
            // fills within seconds of launch (from the cache, usually with no
            // request at all), then the floored cadence. `weather_every` is
            // `None` while `[weather]` is off, so a disabled feature emits no
            // slot and costs no idle wake.
            if weather_every.is_some_and(|n| {
                (weather_after == 0 && ticks == WEATHER_FIRST_SLOT) || due(ticks, n, weather_after)
            }) {
                if send_tick(&tx, active_generation, RefreshKind::WeatherPoll).is_err() {
                    break;
                }
                wake = true;
            }
            // Daemon/status refresh: re-resolves the daemon PID for the
            // per-process sampler and updates the chip + modal. A cheap
            // registry read, so it runs every 10 s — with the 30 s disk slot
            // a dead daemon read "healthy" for up to 90 s.
            if ticks.is_multiple_of(daemon_every) {
                match io.send_daemon() {
                    Ok(sent) => wake |= sent,
                    Err(()) => break,
                }
            }
            // Host-heal consideration: O(1) send; the handler no-ops unless a
            // Failed(retryable) host exists (0%-idle invariant preserved).
            if ticks.is_multiple_of(heal_every) {
                if send_tick(&tx, active_generation, RefreshKind::HostHeal).is_err() {
                    break;
                }
                wake = true;
            }
            // Offline recovery: only while offline, throttled. `is_offline()` is
            // a lock-free atomic — an online machine never sends.
            if io.connection_recovery_due() {
                if send_tick(&tx, active_generation, RefreshKind::ConnRecover).is_err() {
                    break;
                }
                wake = true;
            }
            // Coarse backstop for the main-checkout self-heal: the diff watcher
            // catches a `refs/heads/*` move sub-second, but a missed event (a
            // `packed-refs` rewrite, the watcher-retarget window, a network mount)
            // is caught here within the PR cadence. The heal itself is a cheap
            // guarded no-op when the checkout is already coherent (the common case).
            if due(ticks, pr_every.get(), 0)
                && send_tick(&tx, active_generation, RefreshKind::MainRefMoved).is_err()
            {
                break;
            }
            if let Some(n) = calendar_every
                && due(ticks, n, calendar_after)
            {
                if send_tick(&tx, active_generation, RefreshKind::Calendar).is_err() {
                    break;
                }
                wake = true;
            }
            if calendar_every.is_some()
                && calendar_reminders
                && due(ticks, reminder_every, reminder_after)
            {
                if send_tick(&tx, active_generation, RefreshKind::CalendarReminders).is_err() {
                    break;
                }
                wake = true;
            }
            // Clock: fire only when the rendered text would actually change,
            // i.e. when the current wall time crosses a display boundary. Cheap
            // (one `now()` per half-tick, no allocation) and self-correcting
            // across suspend/resume or a wall-clock jump, because it compares
            // absolute units rather than counting elapsed ticks.
            {
                let unit = clock_unit(&clock_period_secs, io.now_secs());
                if unit != last_clock_unit {
                    last_clock_unit = unit;
                    if send_tick(&tx, active_generation, RefreshKind::ClockTick).is_err() {
                        break;
                    }
                    wake = true;
                }
            }
            match io.send_stats_if_due() {
                Ok(sent) => wake |= sent,
                Err(()) => break,
            }
            // Container list refresh: runs OCI `ps` subprocesses, so keep it on
            // its own cadence (5s) rather than tying it to the fast stats tick.
            // The cheap `ps` always runs; the expensive `stats --no-stream`
            // enrichment (and the `system df` footprint) runs ONLY while a
            // per-container-stats surface is visible (`containers_live`) — the
            // gate that removes the standing stats cost. All under the
            // `Subsys::Container` CPU attribution so the perf rollup shows a
            // closed monitor pays nothing.
            if ticks.is_multiple_of(container_every) {
                if io.send_containers(ticks, container_every).is_err() {
                    break;
                }
                wake = true;
            }
            if wake {
                notify();
            }
            io.tick_complete(ticks);
        }
    })
}

/// The live adapter owns exactly the sampler/channels formerly local to the
/// worker closure. Constructing it performs no I/O; priming stays on the worker.
struct LiveIo {
    stats_tx: tokio_mpsc::UnboundedSender<StatsTick>,
    container_tx: tokio_mpsc::UnboundedSender<ContainerRefresh>,
    daemon_tx: tokio_mpsc::UnboundedSender<crate::chrome::DaemonStatus>,
    stats_interval_ms: Arc<AtomicU64>,
    stats_live: Arc<AtomicBool>,
    containers_live: Arc<AtomicBool>,
    disk_path: Option<std::path::PathBuf>,
    sampler: Option<thegn_metrics::StatsSampler>,
    last_stats: Option<Instant>,
    daemon_scope: Option<String>,
}

impl LiveIo {
    fn read_daemon(&mut self) -> Option<crate::chrome::DaemonStatus> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
            .unwrap_or(0);
        let scope = self.daemon_scope.as_deref().expect("daemon primed");
        let status = thegn_core::db::Db::open()
            .and_then(|db| crate::handlers::status::snapshot(&db, scope, now_ms));
        match status {
            Ok(status) => {
                self.sampler
                    .as_mut()
                    .expect("stats primed")
                    .set_daemon_pid(status.pid);
                Some(status)
            }
            Err(error) => {
                tracing::warn!(target: "thegn::hydrate", %error, "daemon registry read failed; keeping last status");
                None
            }
        }
    }
}

impl TickerIo for LiveIo {
    fn background_qos(&mut self) {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
    }

    fn model_refresh_interval(&self) -> Duration {
        super::model_refresh_interval()
    }

    fn prime_stats(&mut self) {
        let mut sampler =
            thegn_metrics::StatsSampler::new(self.disk_path.take().expect("one prime"));
        // Best-effort initial sends preserve the existing startup behavior.
        drop(self.stats_tx.send(StatsTick::now(sampler.sample())));
        self.last_stats = Some(Instant::now());
        self.sampler = Some(sampler);
    }

    fn prime_daemon(&mut self) {
        self.daemon_scope = Some(crate::daemon::scope_key());
        if let Some(status) = self.read_daemon() {
            drop(self.daemon_tx.send(status));
        }
    }

    fn now_secs(&self) -> i64 {
        chrono::Local::now().timestamp()
    }

    fn wait_tick(&mut self, tick: Duration) -> bool {
        std::thread::sleep(tick);
        true
    }

    fn send_daemon(&mut self) -> Result<bool, ()> {
        if let Some(status) = self.read_daemon() {
            self.daemon_tx.send(status).map_err(|_| ())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn connection_recovery_due(&self) -> bool {
        thegn_core::connectivity::is_offline() && thegn_core::connectivity::should_probe()
    }

    fn send_stats_if_due(&mut self) -> Result<bool, ()> {
        let interval =
            Duration::from_millis(self.stats_interval_ms.load(Ordering::Relaxed).max(500));
        if !self.stats_live.load(Ordering::Relaxed)
            && self.last_stats.expect("stats primed").elapsed() < interval
        {
            return Ok(false);
        }
        self.last_stats = Some(Instant::now());
        let sampler = self.sampler.as_mut().expect("stats primed");
        sampler.set_tracked(
            thegn_core::proc_registry::tracked()
                .into_iter()
                .map(|tracked| thegn_metrics::TrackedSpec {
                    pid: tracked.pid,
                    group: tracked.group.to_string(),
                })
                .collect(),
        );
        let snap = {
            let _guard = crate::perf::measure(crate::perf::Subsys::Stats);
            sampler.sample()
        };
        self.stats_tx.send(StatsTick::now(snap)).map_err(|_| ())?;
        Ok(true)
    }

    fn send_containers(&mut self, ticks: u64, every: u64) -> Result<(), ()> {
        let live = self.containers_live.load(Ordering::Relaxed);
        let refresh = {
            let _guard = crate::perf::measure(crate::perf::Subsys::Container);
            let containers = if live {
                thegn_core::sandbox::running_containers_with_stats()
            } else {
                thegn_core::sandbox::running_containers()
            };
            let footprint = (live && ticks.is_multiple_of(every * CONTAINER_DF_EVERY_TICKS))
                .then(thegn_core::sandbox::container_footprint);
            ContainerRefresh {
                containers,
                footprint,
            }
        };
        self.container_tx.send(refresh).map_err(|_| ())
    }
}

#[cfg(test)]
#[path = "hydrate_refresh_ticker_tests.rs"]
mod tests;
