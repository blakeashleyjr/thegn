use super::{
    Clock, Demand, Factory, MonitorSampler, Phase, Shared, SpawnInputs, base_ms_for, mode_for,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::Instant,
};
use thegn_metrics::collect::{
    self, Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    schedule::{Cadence, Mode},
};

struct FakeClock {
    now: Mutex<Instant>,
    timed: AtomicUsize,
    untimed: AtomicUsize,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            now: Mutex::new(Instant::now()),
            timed: AtomicUsize::new(0),
            untimed: AtomicUsize::new(0),
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }

    fn wait<'a>(
        &self,
        shared: &Shared,
        state: std::sync::MutexGuard<'a, super::State>,
        deadline: Option<Instant>,
    ) -> std::sync::MutexGuard<'a, super::State> {
        match deadline {
            Some(deadline) => {
                self.timed.fetch_add(1, Ordering::SeqCst);
                *self.now.lock().unwrap() = deadline;
                state
            }
            None => {
                self.untimed.fetch_add(1, Ordering::SeqCst);
                shared.changed.wait(state).unwrap()
            }
        }
    }
}

struct FakeCollector {
    id: CollectorId,
    absent: bool,
    samples: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    errors: bool,
}

impl Collector for FakeCollector {
    fn id(&self) -> CollectorId {
        self.id
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        if self.absent {
            Health::Absent(Absent::NotImplemented)
        } else {
            Health::Warming
        }
    }
    fn sample(
        &mut self,
        _ctx: &TickCtx,
        _frame: &mut MonitorFrame,
    ) -> Result<(), collect::CollectError> {
        self.samples.fetch_add(1, Ordering::SeqCst);
        if self.errors {
            Err(collect::CollectError::Gone)
        } else {
            Ok(())
        }
    }
    fn close(&mut self) {
        self.closes.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    sampler: MonitorSampler,
    clock: Arc<FakeClock>,
    samples: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    spawns: Arc<AtomicUsize>,
    name: Arc<Mutex<Option<String>>>,
    wakes: Arc<AtomicUsize>,
}

fn harness(absent: bool, errors: bool) -> Harness {
    let clock = Arc::new(FakeClock::new());
    let samples = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let spawns = Arc::new(AtomicUsize::new(0));
    let name = Arc::new(Mutex::new(None));
    let wakes = Arc::new(AtomicUsize::new(0));
    let factory: Factory = {
        let samples = Arc::clone(&samples);
        let closes = Arc::clone(&closes);
        Box::new(move || {
            let ids = if absent {
                CollectorId::ALL.to_vec()
            } else {
                vec![CollectorId::Loadavg]
            };
            ids.into_iter()
                .map(|id| {
                    Box::new(FakeCollector {
                        id,
                        absent,
                        samples: Arc::clone(&samples),
                        closes: Arc::clone(&closes),
                        errors,
                    }) as Box<dyn Collector>
                })
                .collect()
        })
    };
    let spawn = {
        let spawns = Arc::clone(&spawns);
        let name = Arc::clone(&name);
        Box::new(
            move |thread_name: &str,
                  task: Box<dyn FnOnce() + Send>|
                  -> std::io::Result<JoinHandle<()>> {
                spawns.fetch_add(1, Ordering::SeqCst);
                *name.lock().unwrap() = Some(thread_name.to_owned());
                Ok(std::thread::spawn(task))
            },
        )
    };
    let wake = {
        let wakes = Arc::clone(&wakes);
        Arc::new(move || {
            wakes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    };
    let sampler = MonitorSampler::with_inputs(SpawnInputs {
        factory,
        clock: Arc::clone(&clock),
        wake,
        spawn,
    });
    Harness {
        sampler,
        clock,
        samples,
        closes,
        spawns,
        name,
        wakes,
    }
}

fn demand() -> Demand {
    let mut collectors = CollectorSet::empty();
    collectors.insert(CollectorId::Loadavg);
    Demand {
        collectors,
        base_ms: 500,
    }
}

fn wait_phase(harness: &Harness, expected: Phase) {
    for _ in 0..100_000 {
        if harness.sampler.shared.lock().phase == expected {
            return;
        }
        std::thread::yield_now();
    }
    assert_eq!(harness.sampler.shared.lock().phase, expected);
}

#[test]
fn closed_does_not_spawn_and_open_reuses_one_named_worker() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Closed, demand());
    assert_eq!(h.spawns.load(Ordering::SeqCst), 0);
    assert!(h.sampler.poster().is_none());
    for _ in 0..3 {
        h.sampler.sync(Mode::Open, demand());
    }
    wait_phase(&h, Phase::Waiting);
    assert_eq!(h.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(h.name.lock().unwrap().as_deref(), Some("thegn-monitor"));
}

#[test]
fn absent_collector_publishes_discovery_then_parks_without_sampling() {
    let mut h = harness(true, false);
    h.sampler.sync(Mode::Open, demand());
    wait_phase(&h, Phase::Parked);
    let frame = h.sampler.take_latest().unwrap();
    assert!(frame.sampled.is_empty());
    for id in CollectorId::ALL {
        assert_eq!(
            frame.health[id.index()],
            Health::Absent(Absent::NotImplemented)
        );
    }
    assert_eq!(h.samples.load(Ordering::SeqCst), 0);
    assert_eq!(h.clock.timed.load(Ordering::SeqCst), 0);
    assert!(h.clock.untimed.load(Ordering::SeqCst) > 0);
    h.sampler.recycle(frame);
}

#[test]
fn hidden_and_paused_use_untimed_parks_then_open_samples() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Hidden, demand());
    wait_phase(&h, Phase::Parked);
    assert_eq!(h.clock.timed.load(Ordering::SeqCst), 0);
    assert!(h.clock.untimed.load(Ordering::SeqCst) > 0);
    assert_eq!(h.samples.load(Ordering::SeqCst), 0);
    h.sampler.sync(Mode::Paused, demand());
    wait_phase(&h, Phase::Parked);
    assert_eq!(h.clock.timed.load(Ordering::SeqCst), 0);
    h.sampler.sync(Mode::Open, demand());
    wait_phase(&h, Phase::Waiting);
    assert!(h.samples.load(Ordering::SeqCst) > 0);
    assert!(h.clock.timed.load(Ordering::SeqCst) > 0);
}

#[test]
fn close_closes_once_and_wakes_for_loop_owned_reap() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Open, demand());
    wait_phase(&h, Phase::Waiting);
    h.sampler.sync(Mode::Closed, demand());
    wait_phase(&h, Phase::Stopped);
    assert_eq!(h.closes.load(Ordering::SeqCst), 1);
    assert!(h.wakes.load(Ordering::SeqCst) >= 2);
    h.sampler.sync(Mode::Closed, demand());
    assert!(h.sampler.handle.is_none());
}

#[test]
fn sample_errors_set_failing_health_and_mode_helpers_are_pure() {
    let mut h = harness(false, true);
    h.sampler.sync(Mode::Open, demand());
    wait_phase(&h, Phase::Waiting);
    let frame = h.sampler.take_latest().unwrap();
    assert!(matches!(
        frame.health[CollectorId::Loadavg.index()],
        Health::Failing { .. }
    ));
    h.sampler.recycle(frame);
    assert_eq!(base_ms_for(Some(59)), 500);
    assert_eq!(base_ms_for(Some(60)), 500);
    assert_eq!(base_ms_for(Some(61)), 1000);
    assert_eq!(base_ms_for(Some(1799)), 1000);
    assert_eq!(base_ms_for(Some(1800)), 5000);
    assert_eq!(base_ms_for(Some(21599)), 5000);
    assert_eq!(base_ms_for(Some(21600)), 15000);
    assert_eq!(base_ms_for(None), 15000);
    assert_eq!(mode_for(false, false, false), Mode::Closed);
    assert_eq!(mode_for(true, true, false), Mode::Paused);
    assert_eq!(mode_for(true, false, true), Mode::Hidden);
    assert_eq!(mode_for(true, false, false), Mode::Open);
}

#[test]
fn frame_publication_is_latest_wins_and_wakes_only_on_empty_to_full() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Open, demand());
    wait_phase(&h, Phase::Waiting);
    let held = h.sampler.take_latest().unwrap();
    let mut addresses = std::collections::HashSet::new();
    addresses.insert((&*held as *const MonitorFrame) as usize);
    let mut last_seq = held.seq;
    assert_eq!(h.wakes.load(Ordering::SeqCst), 1);
    for _ in 0..20 {
        let mut newest = None;
        for _ in 0..100_000 {
            if let Some(frame) = h.sampler.take_latest() {
                if frame.seq > last_seq {
                    newest = Some(frame);
                    break;
                }
                h.sampler.recycle(frame);
            }
            std::thread::yield_now();
        }
        let frame = newest.expect("worker should publish another frame");
        last_seq = frame.seq;
        addresses.insert((&*frame as *const MonitorFrame) as usize);
        h.sampler.recycle(frame);
    }
    assert!(addresses.len() <= 3);
    h.sampler.recycle(held);
}

#[test]
fn stale_generation_frames_are_discarded_after_demand_changes() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Open, demand());
    wait_phase(&h, Phase::Waiting);
    h.sampler.sync(Mode::Paused, demand());
    wait_phase(&h, Phase::Parked);
    assert!(h.sampler.take_latest().is_none());
}

#[test]
fn external_writes_are_latest_wins_while_parked() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Paused, demand());
    wait_phase(&h, Phase::Parked);
    let poster = h.sampler.poster().unwrap();
    poster.post_external(CollectorId::Loadavg, Health::Warming, |_parts| {});
    poster.post_external(CollectorId::Loadavg, Health::Ok, |_parts| {});
    h.sampler.sync(Mode::Open, demand());
    let mut applied = false;
    for _ in 0..100_000 {
        if let Some(frame) = h.sampler.take_latest() {
            applied |= frame.sampled.contains(CollectorId::Loadavg)
                && frame.health[CollectorId::Loadavg.index()] == Health::Ok;
            h.sampler.recycle(frame);
            if applied {
                break;
            }
        }
        std::thread::yield_now();
    }
    assert!(applied);
}

#[test]
fn pending_external_write_is_dropped_when_sampler_closes() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Paused, demand());
    wait_phase(&h, Phase::Parked);
    let poster = h.sampler.poster().unwrap();
    poster.post_external(CollectorId::Loadavg, Health::Ok, |_parts| {});
    h.sampler.sync(Mode::Closed, demand());
    wait_phase(&h, Phase::Stopped);
    assert!(h.sampler.shared.lock().external.iter().all(Option::is_none));
}

#[test]
fn external_write_waits_while_paused_and_is_applied_after_resume() {
    let mut h = harness(false, false);
    h.sampler.sync(Mode::Paused, demand());
    wait_phase(&h, Phase::Parked);
    let poster = h.sampler.poster().unwrap();
    poster.post_external(CollectorId::Loadavg, Health::Ok, |_parts| {});
    h.sampler.sync(Mode::Open, demand());
    let mut applied = false;
    for _ in 0..100_000 {
        if let Some(frame) = h.sampler.take_latest() {
            applied |= frame.sampled.contains(CollectorId::Loadavg)
                && frame.health[CollectorId::Loadavg.index()] == Health::Ok;
            h.sampler.recycle(frame);
            if applied {
                break;
            }
        }
        std::thread::yield_now();
    }
    assert!(applied);
}
