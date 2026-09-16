//! Private instances of the shipping worker/custody mechanism. Virtual time
//! advances only when a fixture explicitly notifies the predicate condvar.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
#[path = "proc_worker_fixture_custody.rs"]
mod cleanup;

#[derive(Debug, PartialEq)]
pub(crate) enum Event {
    Wait(Option<u64>),
    Sample,
    Reset,
    Wake,
}
struct ManualClock {
    base: Instant,
    millis: AtomicU64,
    events: Sender<Event>,
}
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.millis.load(Ordering::SeqCst))
    }
    fn wait<'a>(
        &self,
        shared: &Shared,
        state: MutexGuard<'a, State>,
        deadline: Option<Instant>,
    ) -> MutexGuard<'a, State> {
        self.events
            .send(Event::Wait(
                deadline.map(|at| at.duration_since(self.base).as_millis() as u64),
            ))
            .unwrap();
        shared.changed.wait(state).unwrap()
    }
}
impl ManualClock {
    fn advance(&self, control: &Control, millis: u64) {
        let state = control.shared.lock();
        self.millis.store(millis, Ordering::SeqCst);
        control.shared.changed.notify_all();
        drop(state);
    }
}
struct FixtureCollector {
    events: Sender<Event>,
    sample_gate: Option<Receiver<()>>,
    reset_gate: Option<Receiver<()>>,
    fail_sample: bool,
    fail_reset: bool,
    samples: usize,
    primed: bool,
    fixture_snapshot: Option<ProcSnapshot>,
}
impl Collector for FixtureCollector {
    fn sample(&mut self) -> ProcSnapshot {
        self.events.send(Event::Sample).unwrap();
        if let Some(gate) = self.sample_gate.take() {
            let _released_or_fixture_dropped = gate.recv();
        }
        assert!(!self.fail_sample, "injected collector failure");
        self.samples += 1;
        let snapshot = self
            .fixture_snapshot
            .clone()
            .unwrap_or_else(|| ProcSnapshot {
                total: self.samples,
                enabled: true,
                primed: self.primed,
                ..Default::default()
            });
        self.primed = true;
        snapshot
    }
    fn reset(&mut self) {
        self.events.send(Event::Reset).unwrap();
        if let Some(gate) = self.reset_gate.take() {
            let _released_or_fixture_dropped = gate.recv();
        }
        assert!(!self.fail_reset, "injected reset failure");
        self.primed = false;
    }
}
#[derive(Default)]
struct Options {
    sample_gate: Option<Receiver<()>>,
    reset_gate: Option<Receiver<()>>,
    wake_gate: Option<Receiver<()>>,
    fail_sample: bool,
    fail_reset: bool,
    fail_wake: bool,
    panic_wake: bool,
    panic_factory: bool,
    fixture_snapshot: Option<ProcSnapshot>,
    release_on_drop: Vec<Sender<()>>,
}
pub(crate) struct Fixture {
    cleanup: cleanup::Cleanup,
    worker: ProcessWorker,
    control: Control,
    clock: Arc<ManualClock>,
    events: Receiver<Event>,
}
impl Fixture {
    pub(crate) fn with_blocked_snapshot(
        gate: Receiver<()>,
        release: Sender<()>,
        snapshot: ProcSnapshot,
    ) -> Self {
        Self::new(Options {
            sample_gate: Some(gate),
            fixture_snapshot: Some(snapshot),
            release_on_drop: vec![release],
            ..Default::default()
        })
    }
    pub(crate) fn control(&self) -> &Control {
        &self.control
    }
    fn new(options: Options) -> Self {
        Self::in_slot(Arc::new(Slot::default()), options)
    }
    fn in_slot(slot: Arc<Slot>, options: Options) -> Self {
        let lease = cleanup::Lease::reserve(slot.clone());
        let release_on_drop = options.release_on_drop;
        let (events, receiver) = mpsc::channel();
        let clock = Arc::new(ManualClock {
            base: Instant::now(),
            millis: AtomicU64::new(0),
            events: events.clone(),
        });
        let sample_events = events.clone();
        let wake_gate = Mutex::new(options.wake_gate);
        let worker = ProcessWorker::spawn_with(
            slot,
            clock.clone(),
            Box::new(move || {
                assert!(!options.panic_factory, "injected factory failure");
                Box::new(FixtureCollector {
                    events: sample_events,
                    sample_gate: options.sample_gate,
                    reset_gate: options.reset_gate,
                    fail_sample: options.fail_sample,
                    fail_reset: options.fail_reset,
                    samples: 0,
                    primed: false,
                    fixture_snapshot: options.fixture_snapshot,
                })
            }),
            Box::new(move || {
                events.send(Event::Wake).unwrap();
                if let Some(gate) = wake_gate.lock().unwrap().take() {
                    let _released_or_fixture_dropped = gate.recv();
                }
                assert!(!options.panic_wake, "injected wake failure");
                if options.fail_wake { Err(()) } else { Ok(()) }
            }),
            ProcessWorker::spawn_fixture_task,
        )
        .unwrap();
        let control = worker.control();
        Self {
            cleanup: cleanup::Cleanup::new(
                lease,
                worker.shared.clone(),
                worker.token,
                release_on_drop,
            ),
            worker,
            control,
            clock,
            events: receiver,
        }
    }
    pub(crate) fn event(&self, expected: Event) {
        // Failure watchdog only; scheduling assertions use the manual clock.
        assert_eq!(
            self.events.recv_timeout(Duration::from_secs(10)).unwrap(),
            expected
        );
    }
    fn advance(&self, millis: u64) {
        self.clock.advance(&self.control, millis);
    }
    pub(crate) fn settle(&self) -> Settlement {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(
                self.worker
                    .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(10)),
            )
    }
    fn terminal(&self) -> Status {
        let mut receiver = self.control.shared.status.subscribe();
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let status = *receiver.borrow_and_update();
                        if status.terminal() {
                            return status;
                        }
                        receiver.changed().await.unwrap();
                    }
                })
                .await
                .unwrap()
            })
    }
}

#[test]
fn hidden_park_has_no_deadline_and_close_cancels_without_a_sample() {
    let fixture = Fixture::new(Options::default());
    fixture.event(Event::Wait(None));
    assert_eq!(
        fixture.control.status(),
        Status {
            generation: 0,
            phase: Phase::Parked
        }
    );
    // Even an explicit spurious notification preserves an indefinite park.
    fixture.advance(120_000);
    fixture.event(Event::Wait(None));
    assert!(fixture.control.take_latest().is_none());
    fixture.control.shared.stop();
    assert_eq!(fixture.settle(), Settlement::Settled);
    assert!(fixture.events.try_recv().is_err());
}

#[test]
fn repeated_samples_coalesce_and_reopen_resets_without_burst() {
    let fixture = Fixture::new(Options::default());
    fixture.event(Event::Wait(None));
    fixture.control.set_enabled(true);
    fixture.event(Event::Sample);
    fixture.event(Event::Wake);
    fixture.event(Event::Wait(Some(2_000)));
    fixture.advance(1_999);
    fixture.event(Event::Wait(Some(2_000)));
    for at in [2_000, 4_000, 6_000] {
        fixture.advance(at);
        fixture.event(Event::Sample);
        fixture.event(Event::Wait(Some(at + 2_000)));
    }
    let latest = fixture.control.take_latest().unwrap();
    assert_eq!((latest.revision, latest.snapshot.total), (4, 4));
    assert!(latest.snapshot.primed);
    assert!(fixture.control.take_latest().is_none());
    let paused = fixture.control.set_enabled(false);
    fixture.event(Event::Reset);
    fixture.event(Event::Wait(None));
    assert_eq!(
        fixture.control.status(),
        Status {
            generation: paused,
            phase: Phase::Parked
        }
    );
    fixture.control.set_enabled(true);
    fixture.event(Event::Wait(Some(8_000)));
    fixture.advance(8_000);
    fixture.event(Event::Sample);
    fixture.event(Event::Wake);
    fixture.event(Event::Wait(Some(10_000)));
    let reopened = fixture.control.take_latest().unwrap();
    assert_eq!(reopened.revision, 5);
    assert!(!reopened.snapshot.primed);
    fixture.advance(10_000);
    fixture.event(Event::Sample);
    fixture.event(Event::Wake);
    fixture.event(Event::Wait(Some(12_000)));
    assert!(fixture.control.take_latest().unwrap().snapshot.primed);
    assert_eq!(fixture.settle(), Settlement::Settled);
}

#[test]
fn in_flight_sample_is_revoked_across_hide_and_reopen() {
    let (release, gate) = mpsc::channel();
    let fixture = Fixture::new(Options {
        sample_gate: Some(gate),
        release_on_drop: vec![release.clone()],
        ..Default::default()
    });
    fixture.event(Event::Wait(None));
    fixture.control.set_enabled(true);
    fixture.event(Event::Sample);
    let old = fixture.control.set_enabled(false);
    let current = fixture.control.set_enabled(true);
    assert!(current > old);
    assert_ne!(fixture.control.status().generation, old);
    release.send(()).unwrap();
    fixture.event(Event::Reset);
    fixture.event(Event::Wait(Some(2_000)));
    assert!(fixture.control.take_latest().is_none());
    fixture.advance(2_000);
    fixture.event(Event::Sample);
    fixture.event(Event::Wake);
    fixture.event(Event::Wait(Some(4_000)));
    let publication = fixture.control.take_latest().unwrap();
    assert_eq!(publication.generation, current);
    assert!(!publication.snapshot.primed);
    assert_eq!(fixture.settle(), Settlement::Settled);
}

#[test]
fn parked_ack_waits_for_wake_and_reset_and_old_ticket_is_superseded() {
    let (release_wake, wake_gate) = mpsc::channel();
    let (release_reset, reset_gate) = mpsc::channel();
    let fixture = Fixture::new(Options {
        wake_gate: Some(wake_gate),
        reset_gate: Some(reset_gate),
        release_on_drop: vec![release_wake.clone(), release_reset.clone()],
        ..Default::default()
    });
    fixture.event(Event::Wait(None));
    fixture.control.set_enabled(true);
    fixture.event(Event::Sample);
    fixture.event(Event::Wake);
    let paused = fixture.control.set_enabled(false);
    assert_ne!(
        fixture.control.status(),
        Status {
            generation: paused,
            phase: Phase::Parked
        }
    );
    assert!(fixture.control.take_latest().is_none());
    let reopened = fixture.control.set_enabled(true);
    release_wake.send(()).unwrap();
    fixture.event(Event::Reset);
    assert_ne!(fixture.control.status().generation, paused);
    release_reset.send(()).unwrap();
    fixture.event(Event::Wait(Some(2_000)));
    assert_eq!(
        fixture.control.status(),
        Status {
            generation: reopened,
            phase: Phase::Waiting
        }
    );
    assert!(fixture.control.take_latest().is_none());
    assert_eq!(fixture.settle(), Settlement::Settled);
}

#[test]
fn wake_errors_and_panics_revoke_pending_publication_and_future_admission() {
    for panic_wake in [false, true] {
        let fixture = Fixture::new(Options {
            fail_wake: !panic_wake,
            panic_wake,
            ..Default::default()
        });
        fixture.event(Event::Wait(None));
        fixture.control.set_enabled(true);
        fixture.event(Event::Sample);
        fixture.event(Event::Wake);
        assert!(matches!(fixture.terminal().phase, Phase::Failed(_)));
        assert!(fixture.control.take_latest().is_none());
        fixture.control.set_enabled(false);
        fixture.control.set_enabled(true);
        assert_eq!(fixture.settle(), Settlement::Failed);
        fixture.event(Event::Wake); // exactly one bounded terminal notification
        assert!(fixture.events.try_recv().is_err());
    }
}

#[test]
fn factory_collection_and_reset_panics_produce_owned_failed_outcome() {
    for mode in 0..3 {
        let fixture = Fixture::new(Options {
            panic_factory: mode == 0,
            fail_sample: mode == 1,
            fail_reset: mode == 2,
            ..Default::default()
        });
        if mode != 0 {
            fixture.event(Event::Wait(None));
            fixture.control.set_enabled(true);
            fixture.event(Event::Sample);
            if mode == 2 {
                fixture.event(Event::Wake);
                fixture.event(Event::Wait(Some(2_000)));
                fixture.control.set_enabled(false);
                fixture.event(Event::Reset);
            }
        }
        assert!(matches!(fixture.terminal().phase, Phase::Failed(_)));
        assert_eq!(fixture.settle(), Settlement::Failed);
        fixture.event(Event::Wake); // pre-sample failures notify the compositor
        assert!(fixture.events.try_recv().is_err());
    }
}

#[test]
fn stalled_collector_retains_custody_through_deadline_cancel_and_owner_drop() {
    let slot = Arc::new(Slot::default());
    let (release, gate) = mpsc::channel();
    let fixture = Fixture::in_slot(
        slot.clone(),
        Options {
            sample_gate: Some(gate),
            release_on_drop: vec![release.clone()],
            ..Default::default()
        },
    );
    fixture.event(Event::Wait(None));
    fixture.control.set_enabled(true);
    fixture.event(Event::Sample);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let held = runtime.block_on(fixture.worker.shutdown_until(tokio::time::Instant::now()));
    assert_eq!(held, Settlement::Held);
    assert!(slot.reserve(Arc::new(Shared::new())).is_err());
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(
                Duration::from_millis(1),
                fixture
                    .worker
                    .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(60))
            )
            .await
            .is_err()
        );
    });
    let Fixture {
        cleanup: _cleanup,
        worker,
        control,
        clock: _,
        events: _,
    } = fixture;
    let token = worker.token;
    let mut outcome = worker.shared.status.subscribe();
    drop(control);
    drop(worker);
    assert!(slot.reserve(Arc::new(Shared::new())).is_err());
    release.send(()).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if outcome.borrow_and_update().terminal() {
                    break;
                }
                outcome.changed().await.unwrap();
            }
            while !slot.reap_finished(token) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    let replacement = Fixture::in_slot(slot.clone(), Options::default());
    replacement.event(Event::Wait(None));
    assert!(
        slot.reap_finished(token),
        "stale retirement cannot remove replacement"
    );
    replacement.control.set_enabled(true);
    replacement.event(Event::Sample);
    replacement.event(Event::Wake);
    replacement.event(Event::Wait(Some(2_000)));
    assert_eq!(replacement.settle(), Settlement::Settled);
}

#[test]
fn spawn_refusal_releases_only_the_unused_reservation() {
    let slot = Arc::new(Slot::default());
    let result = ProcessWorker::spawn_with(
        slot.clone(),
        Arc::new(LiveClock),
        Box::new(|| panic!("must not construct")),
        Box::new(|| panic!("must not wake")),
        |_| Err(std::io::Error::other("injected spawn refusal")),
    );
    assert!(result.is_err());
    let fixture = Fixture::in_slot(slot, Options::default());
    fixture.event(Event::Wait(None));
    let Fixture {
        cleanup: _cleanup,
        worker,
        control,
        clock: _,
        events: _,
    } = fixture;
    drop(control);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert_eq!(
        runtime
            .block_on(worker.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(10))),
        Settlement::Settled
    );
}

#[test]
fn poisoned_control_wakes_hidden_worker_and_reports_failure() {
    let fixture = Fixture::new(Options::default());
    fixture.event(Event::Wait(None));
    let _injected_panic = std::panic::catch_unwind(|| {
        let _guard = fixture.control.shared.state.lock().unwrap();
        panic!("injected control poison");
    });
    fixture.control.set_enabled(true);
    assert!(matches!(fixture.terminal().phase, Phase::Failed(_)));
    assert_eq!(fixture.settle(), Settlement::Failed);
    fixture.event(Event::Wake);
    assert!(
        fixture.events.try_recv().is_err(),
        "poison must not admit collection"
    );
}

#[test]
fn panic_receipt_and_terminal_notification_precede_opaque_payload_drop() {
    #[derive(Debug)]
    enum Order {
        Notified(Status),
        PayloadDropped,
    }
    struct Payload(Sender<Order>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.send(Order::PayloadDropped).unwrap();
        }
    }
    let (events, receiver) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let status = Arc::new(Mutex::new(None::<tokio::sync::watch::Receiver<Status>>));
    let read_status = status.clone();
    let payload = Payload(events.clone());
    let worker = ProcessWorker::spawn_with(
        Arc::new(Slot::default()),
        Arc::new(LiveClock),
        Box::new(move || {
            gate.recv().unwrap();
            std::panic::panic_any(payload);
        }),
        Box::new(move || {
            events
                .send(Order::Notified(
                    *read_status.lock().unwrap().as_ref().unwrap().borrow(),
                ))
                .unwrap();
            Ok(())
        }),
        ProcessWorker::spawn_fixture_task,
    )
    .unwrap();
    *status.lock().unwrap() = Some(worker.shared.status.subscribe());
    release.send(()).unwrap();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        Order::Notified(Status {
            phase: Phase::Failed("process sampler panicked"),
            ..
        })
    ));
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        Order::PayloadDropped
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert_eq!(
        runtime
            .block_on(worker.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(10))),
        Settlement::Failed
    );
}

#[test]
fn terminal_receipt_does_not_release_an_unfinished_exact_handle() {
    struct TailClock {
        entered: Sender<()>,
        release: Mutex<Receiver<()>>,
    }
    impl Clock for TailClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn wait<'a>(
            &self,
            shared: &Shared,
            state: MutexGuard<'a, State>,
            deadline: Option<Instant>,
        ) -> MutexGuard<'a, State> {
            LiveClock.wait(shared, state, deadline)
        }
    }
    impl Drop for TailClock {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            let _released_or_fixture_dropped = self.release.lock().unwrap().recv();
        }
    }
    let (entered, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let slot = Arc::new(Slot::default());
    let (events, _receiver) = mpsc::channel();
    let worker = ProcessWorker::spawn_with(
        slot.clone(),
        Arc::new(TailClock {
            entered,
            release: Mutex::new(gate),
        }),
        Box::new(move || {
            Box::new(FixtureCollector {
                events,
                sample_gate: None,
                reset_gate: None,
                fail_sample: false,
                fail_reset: false,
                samples: 0,
                primed: false,
                fixture_snapshot: None,
            })
        }),
        Box::new(|| Ok(())),
        ProcessWorker::spawn_fixture_task,
    )
    .unwrap();
    worker.request_stop();
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(worker.shared.status.borrow().phase, Phase::Stopped);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert_eq!(
        runtime.block_on(worker.shutdown_until(tokio::time::Instant::now())),
        Settlement::Held
    );
    assert!(slot.reserve(Arc::new(Shared::new())).is_err());
    release.send(()).unwrap();
    assert_eq!(
        runtime
            .block_on(worker.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(10))),
        Settlement::Settled
    );
}

#[test]
fn assertion_unwind_releases_owned_collector_gate_and_retires_exact_fixture_thread() {
    let (release, gate) = mpsc::channel();
    let fixture = Fixture::new(Options {
        sample_gate: Some(gate),
        release_on_drop: vec![release.clone()],
        ..Default::default()
    });
    fixture.event(Event::Wait(None));
    fixture.control.set_enabled(true);
    fixture.event(Event::Sample);
    let slot = fixture.worker.slot.clone();
    let shared = fixture.worker.shared.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _fixture = fixture;
        panic!("injected assertion unwind with collector blocked");
    }));
    assert!(result.is_err());
    // The external sender remains alive: only the cleanup guard's explicit
    // release can unblock collection before the exact finished-handle join.
    assert!(slot.is_vacant());
    assert_eq!(shared.status.borrow().phase, Phase::Stopped);
    drop(release);
}
