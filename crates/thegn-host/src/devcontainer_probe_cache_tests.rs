use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

struct Fixture {
    dir: tempfile::TempDir,
    cache: Cache,
    calls: AtomicUsize,
}
impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            cache: Cache::default(),
            calls: AtomicUsize::new(0),
        }
    }
    fn executable(&self) -> PathBuf {
        self.dir.path().join(CLI_NAME)
    }
    fn install(&self) {
        std::fs::write(self.executable(), b"fixture executable identity").unwrap();
    }
    fn inputs(&self, revision: usize) -> Inputs {
        Inputs::from_snapshot(
            vec![
                ("PATH".into(), self.dir.path().as_os_str().into()),
                ("FIXTURE_REVISION".into(), revision.to_string().into()),
            ],
            self.dir.path().into(),
        )
        .unwrap()
    }
    fn report(&self, inputs: Inputs) -> ProbeReport {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if inputs.key.executable.is_some() {
            ready()
        } else {
            ProbeReport::unavailable("fixture CLI absent")
        }
    }
    fn get(&self, revision: usize, now: Instant) -> ProbeReport {
        self.cache.get(
            || Ok(self.inputs(revision)),
            |inputs, _| self.report(inputs),
            || now,
            PROBE_TIMEOUT,
        )
    }
}
fn ready() -> ProbeReport {
    ProbeReport {
        state: ProbeState::Ready,
        executable: Some("private fixture".into()),
        version: Some("fixture version".into()),
        reason: None,
    }
}

#[test]
fn demand_only_expiry_has_distinct_ready_and_failure_lifetimes() {
    let fixture = Fixture::new();
    let now = Instant::now();
    assert_eq!(fixture.get(0, now).state, ProbeState::Unavailable);
    assert_eq!(
        fixture
            .get(0, now + FAILED_TTL - Duration::from_nanos(1))
            .state,
        ProbeState::Unavailable
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.get(0, now + FAILED_TTL).state,
        ProbeState::Unavailable
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    fixture.install();
    assert!(
        fixture.get(0, now + FAILED_TTL).ready(),
        "installation bypasses negative TTL"
    );
    assert!(
        fixture
            .get(0, now + FAILED_TTL + READY_TTL - Duration::from_nanos(1))
            .ready()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    assert!(fixture.get(0, now + FAILED_TTL + READY_TTL).ready());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
}

#[test]
fn environment_cwd_path_and_equal_metadata_replacement_invalidate() {
    let fixture = Fixture::new();
    fixture.install();
    let now = Instant::now();
    assert!(fixture.get(0, now).ready());
    assert!(fixture.get(1, now).ready());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let before = fixture.inputs(1);
    let metadata = std::fs::metadata(fixture.executable()).unwrap();
    let replacement = fixture.dir.path().join("replacement");
    std::fs::write(&replacement, b"fixture executable identity").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
    // Remove then rename is portable; the old cache handle remains pinned.
    std::fs::remove_file(fixture.executable()).unwrap();
    std::fs::rename(replacement, fixture.executable()).unwrap();
    let after = fixture.inputs(1);
    assert_eq!(
        before.key.executable.as_ref().unwrap().bytes,
        after.key.executable.as_ref().unwrap().bytes
    );
    assert_eq!(
        before.key.executable.as_ref().unwrap().modified,
        after.key.executable.as_ref().unwrap().modified
    );
    assert!(
        before.key != after.key,
        "pinned identity must distinguish equal metadata"
    );
    assert!(fixture.get(1, now).ready());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    let other = tempfile::tempdir().unwrap();
    let moved_cwd = Inputs::from_snapshot(before.env.clone(), other.path().into()).unwrap();
    assert!(moved_cwd.key != before.key);
    let other_path = Inputs::from_snapshot(
        vec![("PATH".into(), other.path().as_os_str().into())],
        fixture.dir.path().into(),
    )
    .unwrap();
    assert!(other_path.key != before.key);
}

#[test]
fn concurrent_same_key_coalesces_and_waiter_reobserves_after_wake() {
    let fixture = Fixture::new();
    fixture.install();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (observed_tx, observed_rx) = mpsc::sync_channel(4);
    let observations = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let fixture = &fixture;
        let producer = scope.spawn(move || {
            fixture.cache.get(
                || Ok(fixture.inputs(0)),
                |inputs, _| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
                    fixture.report(inputs)
                },
                Instant::now,
                PROBE_TIMEOUT,
            )
        });
        entered_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
        let observations = &observations;
        let observed_tx = &observed_tx;
        let waiter = scope.spawn(move || {
            fixture.cache.get(
                || {
                    if observations.fetch_add(1, Ordering::SeqCst) == 0 {
                        observed_tx.try_send(()).unwrap();
                    }
                    Ok(fixture.inputs(0))
                },
                |inputs, _| fixture.report(inputs),
                Instant::now,
                PROBE_TIMEOUT,
            )
        });
        observed_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
        // The counter increments immediately before the real Condvar wait;
        // reacquiring the lock proves that wait released it. No timing sleep.
        let deadline = Instant::now() + PROBE_TIMEOUT;
        while fixture.cache.lock().waits == 0 {
            assert!(Instant::now() < deadline, "waiter never entered Condvar");
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        assert!(producer.join().unwrap().ready());
        assert!(waiter.join().unwrap().ready());
    });
    assert!(observations.load(Ordering::SeqCst) >= 2);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn changed_inputs_and_aba_cannot_publish_an_old_producer() {
    let fixture = Fixture::new();
    fixture.install();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let fixture = &fixture;
        let producer = scope.spawn(move || {
            fixture.cache.get(
                || Ok(fixture.inputs(0)),
                |inputs, _| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
                    fixture.report(inputs)
                },
                Instant::now,
                PROBE_TIMEOUT,
            )
        });
        entered_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
        for revision in [1, 0] {
            let refused = fixture.cache.get(
                || Ok(fixture.inputs(revision)),
                |_, _| panic!("a second producer must never start"),
                Instant::now,
                Duration::ZERO,
            );
            assert_eq!(refused.state, ProbeState::Degraded);
            assert_eq!(refused.version, None);
        }
        release_tx.send(()).unwrap();
        let stale = producer.join().unwrap();
        assert_eq!(stale.state, ProbeState::Degraded);
        assert_eq!(stale.version, None);
        assert!(stale.reason.unwrap().contains("inputs changed"));
    });
    assert!(fixture.cache.lock().cached.is_none());
    assert!(fixture.get(0, Instant::now()).ready());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn changed_post_probe_inputs_refuse_result_and_unwind_releases_custody() {
    let fixture = Fixture::new();
    fixture.install();
    let revision = AtomicUsize::new(0);
    let stale = fixture.cache.get(
        || Ok(fixture.inputs(revision.load(Ordering::SeqCst))),
        |inputs, _| {
            revision.store(1, Ordering::SeqCst);
            fixture.report(inputs)
        },
        Instant::now,
        PROBE_TIMEOUT,
    );
    assert_eq!(stale.state, ProbeState::Degraded);
    assert!(fixture.cache.lock().running.is_none());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.cache.get(
            || Ok(fixture.inputs(1)),
            |_, _| panic!("fixture producer unwind"),
            Instant::now,
            PROBE_TIMEOUT,
        )
    }));
    assert!(panic.is_err());
    assert!(fixture.cache.lock().running.is_none());
    assert!(fixture.get(1, Instant::now()).ready());
    let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _state = fixture.cache.state.lock().unwrap();
        panic!("fixture state poison");
    }));
    assert!(poison.is_err());
    assert!(fixture.get(1, Instant::now()).ready());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn input_budgets_refuse_without_probe_or_truncated_environment() {
    let fixture = Fixture::new();
    let huge = "x".repeat(MAX_ENV_ENTRY_BYTES + 1);
    assert!(
        Inputs::from_snapshot(
            vec![("PRIVATE_SECRET".into(), huge.into())],
            fixture.dir.path().into()
        )
        .is_err()
    );
    let many = (0..=MAX_ENV_ENTRIES)
        .map(|i| (format!("key{i}").into(), "v".into()))
        .collect();
    assert!(Inputs::from_snapshot(many, fixture.dir.path().into()).is_err());
    let total = (0..5)
        .map(|i| {
            (
                format!("key{i}").into(),
                "x".repeat(MAX_ENV_ENTRY_BYTES - 8).into(),
            )
        })
        .collect();
    assert!(Inputs::from_snapshot(total, fixture.dir.path().into()).is_err());
    let paths =
        std::env::join_paths(std::iter::repeat_n(fixture.dir.path(), MAX_SEARCH_DIRS + 1)).unwrap();
    assert!(
        Inputs::from_snapshot(vec![("PATH".into(), paths)], fixture.dir.path().into()).is_err()
    );
    let report = fixture.cache.get(
        || Err("version probe environment exceeds input bound"),
        |_, _| panic!("refused inputs must not execute"),
        Instant::now,
        PROBE_TIMEOUT,
    );
    assert_eq!(report.state, ProbeState::Degraded);
    assert!(!report.reason.unwrap().contains("PRIVATE_SECRET"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn waiter_changed_environment_is_reobserved_before_reusing_completion() {
    let fixture = Fixture::new();
    fixture.install();
    let revision = AtomicUsize::new(0);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let fixture = &fixture;
        let producer = scope.spawn(move || {
            fixture.cache.get(
                || Ok(fixture.inputs(0)),
                |inputs, _| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
                    fixture.report(inputs)
                },
                Instant::now,
                PROBE_TIMEOUT,
            )
        });
        entered_rx.recv_timeout(PROBE_TIMEOUT).unwrap();
        let revision = &revision;
        let waiter = scope.spawn(move || {
            fixture.cache.get(
                || Ok(fixture.inputs(revision.load(Ordering::SeqCst))),
                |inputs, _| {
                    assert!(
                        inputs.key == fixture.inputs(1).key,
                        "waiter executed its obsolete input snapshot"
                    );
                    fixture.report(inputs)
                },
                Instant::now,
                PROBE_TIMEOUT,
            )
        });
        let deadline = Instant::now() + PROBE_TIMEOUT;
        while fixture.cache.lock().waits == 0 {
            assert!(
                Instant::now() < deadline,
                "waiter did not release state into Condvar"
            );
            std::thread::yield_now();
        }
        revision.store(1, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        let producer_report = producer.join().unwrap();
        assert!(matches!(
            producer_report.state,
            ProbeState::Ready | ProbeState::Degraded
        ));
        // A legitimate spurious wake may observe the changed key before the
        // producer publishes, correctly invalidating that earlier generation.
        assert!(waiter.join().unwrap().ready());
    });
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        2,
        "changed waiter must not reuse old completion"
    );
}
