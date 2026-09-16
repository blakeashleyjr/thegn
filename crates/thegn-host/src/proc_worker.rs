//! Owned process sampling: no timed waits while hidden, one unpublished value.
//! Collection remains an OS operation: cancellation revokes publication but
//! cannot interrupt that call. Unfinished handles remain in bounded custody.
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use thegn_metrics::ProcSnapshot;

#[path = "proc_worker_custody.rs"]
mod custody;
use custody::Slot;
#[path = "proc_worker_metrics.rs"]
mod metrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Starting,
    Parked,
    Waiting,
    Collecting,
    Stopped,
    Failed(&'static str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status {
    pub generation: u64,
    pub phase: Phase,
}
impl Status {
    fn terminal(self) -> bool {
        matches!(self.phase, Phase::Stopped | Phase::Failed(_))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settlement {
    Settled,
    Failed,
    Held,
}

pub(crate) struct Publication {
    pub revision: u64,
    generation: u64,
    pub snapshot: ProcSnapshot,
}
struct State {
    enabled: bool,
    stop: bool,
    generation: u64,
    reset_epoch: u64,
    revision: u64,
    failure: Option<&'static str>,
    latest: Option<Publication>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    status: tokio::sync::watch::Sender<Status>,
}
impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                enabled: false,
                stop: false,
                generation: 0,
                reset_epoch: 0,
                revision: 0,
                failure: None,
                latest: None,
            }),
            changed: Condvar::new(),
            status: tokio::sync::watch::channel(Status {
                generation: 0,
                phase: Phase::Starting,
            })
            .0,
        }
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| {
            let mut state = error.into_inner();
            state.stop = true;
            state.failure = Some("process sampler control poisoned");
            self.changed.notify_all();
            state
        })
    }
    fn announce(&self, generation: u64, phase: Phase) {
        self.status.send_replace(Status { generation, phase });
        #[cfg(test)]
        match phase {
            Phase::Parked => crate::proc_workload::observed_gate(false),
            Phase::Waiting => crate::proc_workload::observed_gate(true),
            _ => {}
        }
    }
    fn stop(&self) {
        let pending = {
            let mut state = self.lock();
            state.stop = true;
            state.latest.take()
        };
        self.changed.notify_all();
        drop(pending);
    }
    fn finish(&self, failure: Option<&'static str>) {
        let (generation, failure, pending) = {
            let mut state = self.lock();
            state.stop = true;
            state.failure = state.failure.or(failure);
            (state.generation, state.failure, state.latest.take())
        };
        drop(pending);
        self.announce(generation, failure.map_or(Phase::Stopped, Phase::Failed));
    }
}

trait Collector: Send {
    fn sample(&mut self) -> ProcSnapshot;
    fn reset(&mut self);
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
        let result = if let Some(deadline) = deadline {
            shared
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(self.now()))
                .map(|(state, timeout)| {
                    #[cfg(test)]
                    crate::proc_workload::event(if timeout.timed_out() {
                        crate::proc_workload::Event::ScheduledWait
                    } else {
                        crate::proc_workload::Event::ControlWait
                    });
                    #[cfg(not(test))]
                    let _timeout = timeout;
                    state
                })
                .map_err(|error| error.into_inner().0)
        } else {
            shared
                .changed
                .wait(state)
                .map_err(|error| error.into_inner())
        };
        result.unwrap_or_else(|mut state| {
            state.stop = true;
            state.failure = Some("process sampler control poisoned");
            state
        })
    }
}

type Factory = Box<dyn FnOnce() -> Box<dyn Collector> + Send>;
type Wake = Box<dyn Fn() -> Result<(), ()> + Send + Sync>;

fn run(shared: &Shared, clock: &dyn Clock, factory: Factory, wake: &Wake) {
    // Creation and every destructor are covered by the thread's unwind guard.
    let mut sampler = factory();
    let mut reset_epoch = 0;
    let mut last_start: Option<Instant> = None;
    loop {
        let mut state = shared.lock();
        if state.stop {
            return;
        }
        if state.reset_epoch != reset_epoch {
            reset_epoch = state.reset_epoch;
            drop(state);
            sampler.reset();
            continue;
        }
        if !state.enabled {
            // Announced with the predicate lock held: a new request cannot be
            // lost between acknowledgement and the atomic condvar park.
            shared.announce(state.generation, Phase::Parked);
            drop(clock.wait(shared, state, None));
            continue;
        }
        let now = clock.now();
        let deadline = match last_start {
            Some(start) => match start.checked_add(thegn_metrics::PROC_MIN_INTERVAL) {
                Some(deadline) => Some(deadline),
                None => {
                    state.stop = true;
                    state.failure = Some("process sampler deadline unavailable");
                    continue;
                }
            },
            None => None,
        };
        if let Some(deadline) = deadline
            && now < deadline
        {
            shared.announce(state.generation, Phase::Waiting);
            drop(clock.wait(shared, state, Some(deadline)));
            continue;
        }
        let generation = state.generation;
        last_start = Some(now);
        shared.announce(generation, Phase::Collecting);
        drop(state);
        let snapshot = sampler.sample();
        state = shared.lock();
        if state.stop || !state.enabled || state.generation != generation {
            drop(state);
            drop(snapshot);
            continue;
        }
        let Some(revision) = state.revision.checked_add(1) else {
            state.stop = true;
            state.failure = Some("process sampler revision exhausted");
            continue;
        };
        state.revision = revision;
        let replaced = state.latest.replace(Publication {
            revision,
            generation,
            snapshot,
        });
        let needs_wake = replaced.is_none();
        #[cfg(test)]
        crate::proc_workload::event(crate::proc_workload::Event::Publication);
        drop(state);
        drop(replaced);
        // In particular, do not acknowledge a subsequent pause until this
        // attempt completes. Err is terminal, otherwise coalescing would strand
        // an unpublished slot whose only wake was never delivered.
        if needs_wake && wake().is_err() {
            shared.finish(Some("process sampler wake failed"));
            return;
        }
    }
}

/// One consumer. Dropping it cancels even a hidden worker; no send is needed.
pub(crate) struct Control {
    shared: Arc<Shared>,
}
impl Control {
    /// Returns the request generation, not a promise that collection stopped.
    pub fn set_enabled(&self, enabled: bool) -> u64 {
        let mut state = self.shared.lock();
        if state.stop || state.enabled == enabled {
            return state.generation;
        }
        let Some(generation) = state.generation.checked_add(1) else {
            state.stop = true;
            state.failure = Some("process sampler generation exhausted");
            let generation = state.generation;
            drop(state);
            self.shared.changed.notify_all();
            return generation;
        };
        state.generation = generation;
        if !enabled {
            // At most one reset per generation; generation exhaustion above
            // also bounds this counter before addition.
            state.reset_epoch += 1;
        }
        state.enabled = enabled;
        let pending = state.latest.take();
        drop(state);
        drop(pending);
        self.shared.changed.notify_all();
        generation
    }
    pub fn take_latest(&self) -> Option<Publication> {
        let mut state = self.shared.lock();
        let value = state.latest.take();
        let admitted = !state.stop && state.enabled;
        let generation = state.generation;
        drop(state);
        value.filter(|value| admitted && value.generation == generation)
    }
    pub fn status(&self) -> Status {
        *self.shared.status.borrow()
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

pub(crate) struct ProcessWorker {
    slot: Arc<Slot>,
    token: u64,
    shared: Arc<Shared>,
}
impl ProcessWorker {
    pub fn spawn(
        pane_pids: crate::hydrate::PanePids,
        daemon_pid: Arc<std::sync::atomic::AtomicU32>,
        rows: usize,
        waker: termwiz::terminal::TerminalWaker,
    ) -> Result<Self, &'static str> {
        Self::spawn_with(
            custody::global(),
            Arc::new(LiveClock),
            metrics::factory(pane_pids, daemon_pid, rows),
            Box::new(move || waker.wake().map_err(|_| ())),
            |task| {
                std::thread::Builder::new()
                    .name("thegn-procs".into())
                    .spawn(task)
            },
        )
    }
    #[cfg(test)]
    pub(crate) fn spawn_measured(
        audit: Arc<crate::proc_workload::Audit>,
    ) -> Result<Self, &'static str> {
        let factory = metrics::factory(
            Arc::new(Mutex::new(Arc::from(Vec::<(u32, u32)>::new()))),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            32,
        );
        Self::spawn_with(
            custody::global(),
            Arc::new(LiveClock),
            Box::new(move || {
                crate::proc_workload::install(audit);
                factory()
            }),
            Box::new(|| {
                crate::proc_workload::event(crate::proc_workload::Event::Wake);
                Ok(())
            }),
            |task| {
                std::thread::Builder::new()
                    .name("thegn-procs-audit".into())
                    .spawn(task)
            },
        )
    }

    #[cfg(test)]
    fn spawn_fixture_task(
        task: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new().spawn(task)
    }

    fn spawn_with(
        slot: Arc<Slot>,
        clock: Arc<dyn Clock>,
        factory: Factory,
        wake: Wake,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Self, &'static str> {
        let shared = Arc::new(Shared::new());
        let mut reservation = slot.reserve(shared.clone())?;
        let worker_shared = shared.clone();
        let task = Box::new(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                run(&worker_shared, clock.as_ref(), factory, &wake);
            }));
            // Publish truth before dropping an opaque panic payload: its Drop
            // may itself panic. A failed exact join still classifies settlement.
            worker_shared.finish(result.is_err().then_some("process sampler panicked"));
            let failed = matches!(worker_shared.status.borrow().phase, Phase::Failed(_));
            if failed {
                // The compositor does not await the watch channel. One guarded
                // terminal notification makes pre-sample failures visible. An
                // unavailable waker cannot recurse into failure handling.
                let notification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&wake));
                drop(notification);
            }
            drop(result);
        });
        // Assignment immediately gives the successful handle to the reservation
        // guard. Installation and guard Drop recover poisoned custody locks.
        reservation.handle = Some(spawn(task).map_err(|_| "process sampler could not start")?);
        let token = reservation.install();
        Ok(Self {
            slot,
            token,
            shared,
        })
    }
    pub fn control(&self) -> Control {
        Control {
            shared: self.shared.clone(),
        }
    }
    pub fn request_stop(&self) {
        self.shared.stop();
    }
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> Settlement {
        self.request_stop();
        let mut outcome = self.shared.status.subscribe();
        loop {
            if self.slot.reap_finished(self.token) {
                return if matches!(outcome.borrow().phase, Phase::Failed(_)) {
                    Settlement::Failed
                } else {
                    Settlement::Settled
                };
            }
            if tokio::time::Instant::now() >= deadline {
                return Settlement::Held;
            }
            let terminal = outcome.borrow().terminal();
            if terminal {
                // The receipt precedes actual thread return. This short tail
                // wait exists only in shutdown, never in parked steady state.
                tokio::time::sleep_until(
                    deadline.min(tokio::time::Instant::now() + Duration::from_millis(2)),
                )
                .await;
            } else if tokio::time::timeout_at(deadline, outcome.changed())
                .await
                .is_err()
            {
                return Settlement::Held;
            }
        }
    }
}
impl Drop for ProcessWorker {
    fn drop(&mut self) {
        self.request_stop();
        // The slot, not this session handle, retains OS-thread ownership if
        // run() is cancelled or unwinds before asynchronous cleanup completes.
        if !self.slot.reap_finished(self.token) {
            tracing::debug!(target: "thegn::procs", "process sampler remains in bounded custody");
        }
    }
}

#[cfg(test)]
#[path = "proc_worker_tests.rs"]
pub(crate) mod tests;
