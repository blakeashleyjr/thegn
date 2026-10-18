//! Loop-owned lifecycle for the deep system-monitor sampler.
use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use thegn_metrics::collect::{
    self, Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::{FramePool, MonitorFrame, Parts},
    health::Health,
    schedule::{self, Mode},
};

const THREAD_NAME: &str = "thegn-monitor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Demand {
    pub collectors: CollectorSet,
    pub base_ms: u32,
}

impl Demand {
    pub(crate) fn all(base_ms: u32) -> Self {
        let mut collectors = CollectorSet::empty();
        for id in CollectorId::ALL {
            collectors.insert(id);
        }
        Self {
            collectors,
            base_ms,
        }
    }
}

pub(crate) const fn base_ms_for(window_secs: Option<u32>) -> u32 {
    match window_secs {
        Some(secs) if secs <= 60 => 500,
        Some(secs) if secs < 1800 => 1000,
        Some(secs) if secs < 21600 => 5000,
        _ => 15000,
    }
}

pub(crate) const fn mode_for(open: bool, paused: bool, obscured: bool) -> Mode {
    if !open {
        Mode::Closed
    } else if paused {
        Mode::Paused
    } else if obscured {
        Mode::Hidden
    } else {
        Mode::Open
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Parked,
    Waiting,
    Collecting,
    Stopped,
    Failed,
}

type ExternalWrite = Box<dyn FnOnce(&mut Parts) + Send + 'static>;
struct PendingExternal {
    health: Health,
    write: ExternalWrite,
}

struct State {
    mode: Mode,
    demand: Demand,
    generation: u64,
    stop: bool,
    phase: Phase,
    latest: Option<Box<MonitorFrame>>,
    pool: FramePool,
    external: [Option<PendingExternal>; collect::COLLECTORS],
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn announce(&self, phase: Phase) {
        self.lock().phase = phase;
        self.changed.notify_all();
    }
}

trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn wait<'a>(
        &self,
        shared: &Shared,
        state: MutexGuard<'a, State>,
        deadline: Option<Instant>,
    ) -> MutexGuard<'a, State>;
}

struct LiveClock;
impl Clock for LiveClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait<'a>(
        &self,
        shared: &Shared,
        state: MutexGuard<'a, State>,
        deadline: Option<Instant>,
    ) -> MutexGuard<'a, State> {
        match deadline {
            Some(deadline) => shared
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(self.now()))
                .map(|(state, _timeout)| state)
                .unwrap_or_else(|error| error.into_inner().0),
            None => shared
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner()),
        }
    }
}

type Factory = Arc<dyn Fn() -> Vec<Box<dyn Collector>> + Send + Sync>;
type Wake = Arc<dyn Fn() -> Result<(), ()> + Send + Sync>;
type Spawn =
    Arc<dyn Fn(&str, Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>> + Send + Sync>;

struct SpawnInputs {
    factory: Factory,
    clock: Arc<dyn Clock>,
    wake: Wake,
    spawn: Spawn,
}

fn live_spawn(name: &str, task: Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_owned()).spawn(task)
}

pub(crate) struct MonitorSampler {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    inputs: SpawnInputs,
    failed: bool,
}

impl MonitorSampler {
    pub(crate) fn new(waker: termwiz::terminal::TerminalWaker) -> Self {
        Self::with_inputs(SpawnInputs {
            factory: Arc::new(thegn_metrics::collect::stub::stub_collectors),
            clock: Arc::new(LiveClock),
            wake: Arc::new({
                let wake = waker.clone();
                move || wake.wake().map_err(|_| ())
            }),
            spawn: Arc::new(live_spawn),
        })
    }

    fn with_inputs(inputs: SpawnInputs) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    mode: Mode::Closed,
                    demand: Demand {
                        collectors: CollectorSet::empty(),
                        base_ms: 15000,
                    },
                    generation: 0,
                    stop: false,
                    phase: Phase::Stopped,
                    latest: None,
                    pool: FramePool::new(),
                    external: std::array::from_fn(|_| None),
                }),
                changed: Condvar::new(),
            }),
            handle: None,
            inputs,
            failed: false,
        }
    }

    pub(crate) fn sync(&mut self, mode: Mode, demand: Demand) {
        self.reap_finished();
        if mode == Mode::Closed {
            self.request_stop();
            return;
        }
        if self.handle.is_none() && !self.failed {
            self.start_worker(mode, demand);
            return;
        }
        let mut state = self.shared.lock();
        if state.stop {
            return;
        }
        if state.mode != mode || state.demand != demand {
            state.mode = mode;
            state.demand = demand;
            state.generation = state.generation.saturating_add(1);
            self.shared.changed.notify_all();
        }
    }

    #[allow(dead_code)]
    pub(crate) fn poster(&self) -> Option<ExternalPoster> {
        let state = self.shared.lock();
        (self.handle.is_some() && !state.stop && state.mode != Mode::Closed).then(|| {
            ExternalPoster {
                shared: Arc::clone(&self.shared),
            }
        })
    }

    pub(crate) fn take_latest(&mut self) -> Option<Box<MonitorFrame>> {
        let mut state = self.shared.lock();
        let frame = state.latest.take()?;
        if state.stop || frame.demand_gen != state.generation {
            state.pool.recycle(frame);
            return None;
        }
        Some(frame)
    }

    pub(crate) fn recycle(&mut self, frame: Box<MonitorFrame>) {
        self.shared.lock().pool.recycle(frame);
    }

    fn request_stop(&mut self) {
        let mut state = self.shared.lock();
        if self.handle.is_none() && state.phase == Phase::Stopped {
            return;
        }
        if !state.stop {
            state.stop = true;
            state.mode = Mode::Closed;
            state.generation = state.generation.saturating_add(1);
            if let Some(frame) = state.latest.take() {
                state.pool.recycle(frame);
            }
            for pending in &mut state.external {
                drop(pending.take());
            }
            self.shared.changed.notify_all();
        }
    }

    fn reap_finished(&mut self) {
        if matches!(self.shared.lock().phase, Phase::Stopped | Phase::Failed)
            && let Some(handle) = self.handle.take()
        {
            if let Err(error) = handle.join() {
                tracing::debug!(target: "thegn::monitor", ?error, "monitor worker panicked while joining");
                self.failed = true;
            }
            if self.shared.lock().phase == Phase::Failed {
                self.failed = true;
            }
        }
    }

    fn start_worker(&mut self, mode: Mode, demand: Demand) {
        {
            let mut state = self.shared.lock();
            state.stop = false;
            state.mode = mode;
            state.demand = demand;
            state.generation = state.generation.saturating_add(1);
            state.phase = Phase::Starting;
        }
        let shared = Arc::clone(&self.shared);
        let wake = Arc::clone(&self.inputs.wake);
        let clock = Arc::clone(&self.inputs.clock);
        let factory = Arc::clone(&self.inputs.factory);
        let spawn = Arc::clone(&self.inputs.spawn);
        match spawn(
            THREAD_NAME,
            Box::new(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker(&shared, clock.as_ref(), factory, &wake)
                }));
                if result.is_err() {
                    tracing::error!(target: "thegn::monitor", "monitor worker panicked; sampler is disabled");
                    let mut state = shared.lock();
                    state.phase = Phase::Failed;
                    state.stop = true;
                    state.external.iter_mut().for_each(|pending| {
                        drop(pending.take());
                    });
                    shared.changed.notify_all();
                    drop(state);
                    if let Err(error) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake()))
                    {
                        tracing::debug!(target: "thegn::monitor", ?error, "monitor panic wake panicked");
                    }
                }
            }),
        ) {
            Ok(handle) => self.handle = Some(handle),
            Err(error) => {
                tracing::error!(target: "thegn::monitor", %error, "could not spawn monitor sampler");
                self.failed = true;
                self.shared.announce(Phase::Failed);
            }
        }
    }
}

impl Drop for MonitorSampler {
    fn drop(&mut self) {
        self.request_stop();
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
            && let Err(error) = handle.join()
        {
            tracing::debug!(target: "thegn::monitor", ?error, "monitor worker panicked while dropping");
        } else if self.handle.is_some() {
            tracing::debug!(target: "thegn::monitor", "monitor worker still retiring at shutdown");
        }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct ExternalPoster {
    shared: Arc<Shared>,
}

#[allow(dead_code)]
impl ExternalPoster {
    pub(crate) fn post_external(
        &self,
        id: CollectorId,
        health: Health,
        write: impl FnOnce(&mut Parts) + Send + 'static,
    ) {
        let mut state = self.shared.lock();
        if state.stop || state.mode == Mode::Closed {
            return;
        }
        state.external[id.index()] = Some(PendingExternal {
            health,
            write: Box::new(write),
        });
        self.shared.changed.notify_all();
    }
}

fn worker(shared: &Shared, clock: &dyn Clock, factory: Factory, wake: &Wake) {
    let mut collectors = factory();
    let env = Env::default();
    let mut cadences = Vec::with_capacity(collectors.len());
    let mut health = [Health::default(); collect::COLLECTORS];
    for collector in &mut collectors {
        let id = collector.id();
        health[id.index()] = collector.discover(&env);
        cadences.push((id, collector.cadence()));
    }
    let phases = schedule::phases(&cadences);
    let mut seq = 0_u64;
    let mut tick = 0_u64;
    let mut last_ms = None;
    let mut next_due = None;
    let mut scheduled_base = None;
    let mut scheduled_generation = None;
    let mut parts = Parts::default();
    let mut scratch = MonitorFrame::default();
    let discover = MonitorFrame {
        health,
        ..MonitorFrame::default()
    };
    let discover_generation = shared.lock().generation;
    publish(shared, &discover, discover_generation, wake);

    loop {
        let mut state = shared.lock();
        if state.stop {
            let failed = state.phase == Phase::Failed;
            drop(state);
            for collector in &mut collectors {
                collector.close();
            }
            if failed {
                shared.announce(Phase::Failed);
            } else {
                shared.announce(Phase::Stopped);
                if wake().is_err() {
                    shared.announce(Phase::Failed);
                }
            }
            return;
        }
        let demand = state.demand;
        let generation = state.generation;
        let effective = demand
            .collectors
            .iter()
            .filter(|id| !matches!(health[id.index()], Health::Absent(_)))
            .fold(CollectorSet::empty(), |mut set, id| {
                set.insert(id);
                set
            });
        if state.mode != Mode::Open {
            state.phase = Phase::Parked;
            shared.changed.notify_all();
            drop(clock.wait(shared, state, None));
            continue;
        }
        if state.external.iter().any(Option::is_some) {
            let mut frame = take_frame(&mut state);
            let mut sampled = CollectorSet::empty();
            for id in CollectorId::ALL {
                if let Some(pending) = state.external[id.index()].take() {
                    (pending.write)(&mut parts);
                    health[id.index()] = pending.health;
                    sampled.insert(id);
                }
            }
            seq = seq.saturating_add(1);
            frame.seq = seq;
            frame.at_ms = elapsed_ms(clock.now());
            frame.demand_gen = generation;
            frame.sampled = sampled;
            frame.health = health;
            frame.parts.clone_from(&parts);
            drop(state);
            publish_box(shared, frame, wake);
            continue;
        }
        if effective.is_empty() {
            state.phase = Phase::Parked;
            shared.changed.notify_all();
            drop(clock.wait(shared, state, None));
            continue;
        }
        let now = clock.now();
        if scheduled_base != Some(demand.base_ms) || scheduled_generation != Some(generation) {
            next_due = Some(next_deadline(now, demand.base_ms, false));
            scheduled_base = Some(demand.base_ms);
            scheduled_generation = Some(generation);
        }
        let deadline = next_due.expect("worker deadline is initialized");
        if now < deadline {
            state.phase = Phase::Waiting;
            shared.changed.notify_all();
            drop(clock.wait(shared, state, Some(deadline)));
            continue;
        }
        state.phase = Phase::Collecting;
        drop(state);

        let due = schedule::due_set(Mode::Open, effective, &cadences, &phases, tick);
        let now_ms = elapsed_ms(now);
        let dt_ms = last_ms.map_or(0, |last: u64| {
            now_ms.saturating_sub(last).min(u64::from(u32::MAX)) as u32
        });
        let mut sampled = CollectorSet::empty();
        scratch.parts.clone_from(&parts);
        for collector in &mut collectors {
            let id = collector.id();
            if due.contains(id) && health[id.index()].retry_due(tick) {
                match collector.sample(&TickCtx { tick, now_ms }, &mut scratch) {
                    Ok(()) => {
                        health[id.index()] = health[id.index()].after_ok();
                    }
                    Err(_) => {
                        restore_collector_part(&mut scratch.parts, &parts, id);
                        // `after_error` schedules relative to the tick passed in. The
                        // current tick is still in progress here, so anchor the
                        // backoff at the completed tick to leave the full interval
                        // before a retry is eligible.
                        health[id.index()] = health[id.index()].after_error(tick.saturating_add(1));
                    }
                }
                sampled.insert(id);
            }
        }
        parts.clone_from(&scratch.parts);
        last_ms = Some(now_ms);
        seq = seq.saturating_add(1);
        scratch.seq = seq;
        scratch.at_ms = now_ms;
        scratch.dt_ms = dt_ms;
        scratch.demand_gen = generation;
        scratch.sampled = sampled;
        scratch.health = health;
        next_due = Some(next_deadline(now, demand.base_ms, true));
        publish(shared, &scratch, generation, wake);
        tick = tick.saturating_add(1);
    }
}

fn elapsed_ms(now: Instant) -> u64 {
    now.saturating_duration_since(origin())
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn origin() -> Instant {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

fn next_deadline(now: Instant, base_ms: u32, strictly_after: bool) -> Instant {
    let period_ns = u128::from(base_ms.max(1)) * 1_000_000;
    let elapsed_ns = now.saturating_duration_since(origin()).as_nanos();
    let next_ns = elapsed_ns.div_ceil(period_ns) * period_ns;
    let mut deadline = origin() + Duration::from_nanos(next_ns.min(u128::from(u64::MAX)) as u64);
    if strictly_after && deadline <= now {
        deadline += Duration::from_millis(u64::from(base_ms.max(1)));
    }
    deadline
}

fn restore_collector_part(target: &mut Parts, source: &Parts, id: CollectorId) {
    match id {
        CollectorId::Loadavg => target.loadavg.clone_from(&source.loadavg),
        CollectorId::Uptime => target.uptime.clone_from(&source.uptime),
        CollectorId::Psi => target.psi.clone_from(&source.psi),
        CollectorId::CpuStat => target.cpu_stat.clone_from(&source.cpu_stat),
        CollectorId::CpuFreq => target.cpu_freq.clone_from(&source.cpu_freq),
        CollectorId::Hwmon => target.hwmon.clone_from(&source.hwmon),
        CollectorId::Cgroup => target.cgroup.clone_from(&source.cgroup),
        CollectorId::CgroupSlice => target.cgroup_slice.clone_from(&source.cgroup_slice),
        CollectorId::AmdSmu => target.amd_smu.clone_from(&source.amd_smu),
        CollectorId::Meminfo => target.meminfo.clone_from(&source.meminfo),
        CollectorId::Vmstat => target.vmstat.clone_from(&source.vmstat),
        CollectorId::Netdev => target.netdev.clone_from(&source.netdev),
        CollectorId::Diskstats => target.diskstats.clone_from(&source.diskstats),
        CollectorId::Powercap => target.powercap.clone_from(&source.powercap),
        CollectorId::PowerSupply => target.power_supply.clone_from(&source.power_supply),
    }
}

fn take_frame(state: &mut State) -> Box<MonitorFrame> {
    if let Some(frame) = state.pool.take() {
        return frame;
    }
    if let Some(frame) = state.latest.take() {
        return frame;
    }
    panic!("monitor frame pool invariant violated")
}

fn publish(shared: &Shared, frame: &MonitorFrame, generation: u64, wake: &Wake) {
    let mut state = shared.lock();
    let mut pooled = take_frame(&mut state);
    pooled.seq = frame.seq;
    pooled.at_ms = frame.at_ms;
    pooled.dt_ms = frame.dt_ms;
    pooled.sampled = frame.sampled;
    pooled.health = frame.health;
    pooled.parts.clone_from(&frame.parts);
    pooled.demand_gen = generation;
    drop(state);
    publish_box(shared, pooled, wake);
}

fn publish_box(shared: &Shared, frame: Box<MonitorFrame>, wake: &Wake) {
    let (replaced, needs_wake) = {
        let mut state = shared.lock();
        if state.stop {
            state.pool.recycle(frame);
            return;
        }
        let replaced = state.latest.replace(frame);
        let needs_wake = replaced.is_none();
        (replaced, needs_wake)
    };
    if let Some(replaced) = replaced {
        shared.lock().pool.recycle(replaced);
    }
    if needs_wake && wake().is_err() {
        tracing::error!(target: "thegn::monitor", "monitor wake failed; sampler is disabled");
        let mut state = shared.lock();
        state.phase = Phase::Failed;
        state.stop = true;
        if let Some(frame) = state.latest.take() {
            state.pool.recycle(frame);
        }
        shared.changed.notify_all();
    }
}

#[cfg(test)]
#[path = "monitor_sampler_tests.rs"]
mod tests;
