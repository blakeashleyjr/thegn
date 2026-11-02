//! Background-work admission for the shared Tokio blocking pool.
//!
//! The eight permits bound admitted background closures, including closures
//! waiting in Tokio's blocking queue. Measurement runs on its own named
//! `Background` lanes outside these permits. Other direct `spawn_blocking`
//! calls and `std::thread` workers are also outside this budget.
//! Admission is non-blocking, so interactive callers never wait for a permit.
//!
//! Measurement uses eleven `AtomicU64`s (88 bytes), with fixed cardinality and
//! no labels. The in-flight payload bound is at most `BG_PERMITS` boxed
//! closures (queued plus running), each carrying its permit and, only when
//! timed, up to two `Instant`s across queued and running work. There is no lock
//! or timer. `in_flight` is `admitted - finished`; `queued` is
//! `admitted - started`. Zero wait samples with nonzero finished work means it
//! ran untimed (diagnostics off), not that it waited zero. Work refused before
//! submission never appears in admitted.
//!
//! Caller policy matrix (all ordinary refresh callers use a periodic or
//! event-driven trigger to retry after refusal):
//!
//! | Caller | Work class / marker | Completion | Refusal |
//! | --- | --- | --- | --- |
//! | `hydrate_tracker::request_issue_refresh` (`spawn_issue_cache_refresh` fire-and-forget wrapper) | replaceable cache refresh; per-worktree coalescer (one admitted pass, newest queued request kept) | cache write + waker per drained pass | abandon key incl. coalesced request; return `Busy` (`r` reports it); periodic trigger retries |
//! | `hydrate::spawn_panel_prefetch` | replaceable cache prefetch; run-loop path latch | result channel | return admission; caller clears path latch |
//! | `hydrate::spawn_pr_cache_refresh` | replaceable cache refresh; generation fence only | cache + waker | skip; switch/ticker retries |
//! | sibling PR branch cache refresh | replaceable cache refresh; generation fence only | cache | skip; next refresh retries |
//! | `hydrate::spawn_my_work_refresh` | replaceable cache refresh; no latch | cache + waker | skip; periodic trigger retries |
//! | `remote_poll::poll` | replaceable remote refresh; no latch | model refresh + waker | skip; poll tick retries |
//! | `ci_refresh::spawn_ci_cache_refresh` | single-flight cache refresh; newest request queued, stale generation cancelled | cache + waker | clear latch; ticker/switch retries |
//! | `hydrate_calendar::spawn_month_fetch` | user-visible cache read; popup awaits payload | month result channel | send typed unavailable payload |
//! | `hydrate_calendar::spawn_periodic_sync` | replaceable cache refresh; generation fence only | refresh channel | skip; periodic trigger retries |
//! | `hydrate_calendar::spawn_reminder_check` | required reminder evaluation; cursor window | typed acknowledgment | abandon window; ticker retries |
//! | `autopilot_driver` | durable side effect; DB claim happens in admitted closure | durable run row | leave issue unclaimed |
//!
//! Must-complete work uses `bg_permit` before claiming durable state and then
//! transfers that same permit to `spawn_bg_reserved`.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrency and queued-admission cap for background closures.
const BG_PERMITS: usize = 8;

struct AdmissionLedger {
    admitted: AtomicU64,
    refused: AtomicU64,
    unavailable: AtomicU64,
    started: AtomicU64,
    finished: AtomicU64,
    wait_samples: AtomicU64,
    wait_us_total: AtomicU64,
    wait_us_max: AtomicU64,
    service_samples: AtomicU64,
    service_us_total: AtomicU64,
    service_us_max: AtomicU64,
}

impl AdmissionLedger {
    const fn new() -> Self {
        Self {
            admitted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            unavailable: AtomicU64::new(0),
            started: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            wait_samples: AtomicU64::new(0),
            wait_us_total: AtomicU64::new(0),
            wait_us_max: AtomicU64::new(0),
            service_samples: AtomicU64::new(0),
            service_us_total: AtomicU64::new(0),
            service_us_max: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> AdmissionSnapshot {
        AdmissionSnapshot {
            admitted: self.admitted.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            unavailable: self.unavailable.load(Ordering::Relaxed),
            started: self.started.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
            wait_samples: self.wait_samples.load(Ordering::Relaxed),
            wait_us_total: self.wait_us_total.load(Ordering::Relaxed),
            wait_us_max: self.wait_us_max.load(Ordering::Relaxed),
            service_samples: self.service_samples.load(Ordering::Relaxed),
            service_us_total: self.service_us_total.load(Ordering::Relaxed),
            service_us_max: self.service_us_max.load(Ordering::Relaxed),
        }
    }

    fn record_wait(&self, duration: std::time::Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.wait_samples.fetch_add(1, Ordering::Relaxed);
        self.wait_us_total.fetch_add(micros, Ordering::Relaxed);
        self.wait_us_max.fetch_max(micros, Ordering::Relaxed);
    }

    fn record_service(&self, duration: std::time::Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.service_samples.fetch_add(1, Ordering::Relaxed);
        self.service_us_total.fetch_add(micros, Ordering::Relaxed);
        self.service_us_max.fetch_max(micros, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AdmissionSnapshot {
    pub(crate) admitted: u64,
    pub(crate) refused: u64,
    pub(crate) unavailable: u64,
    pub(crate) started: u64,
    pub(crate) finished: u64,
    pub(crate) wait_samples: u64,
    pub(crate) wait_us_total: u64,
    pub(crate) wait_us_max: u64,
    pub(crate) service_samples: u64,
    pub(crate) service_us_total: u64,
    pub(crate) service_us_max: u64,
}

impl AdmissionSnapshot {
    pub(crate) fn in_flight(&self) -> u64 {
        self.admitted.saturating_sub(self.finished)
    }

    pub(crate) fn queued(&self) -> u64 {
        self.admitted.saturating_sub(self.started)
    }

    pub(crate) fn wait_mean_us(&self) -> u64 {
        if self.wait_samples == 0 {
            0
        } else {
            self.wait_us_total / self.wait_samples
        }
    }

    pub(crate) fn service_mean_us(&self) -> u64 {
        if self.service_samples == 0 {
            0
        } else {
            self.service_us_total / self.service_samples
        }
    }
}

static LEDGER: AdmissionLedger = AdmissionLedger::new();

pub(crate) fn admission_snapshot() -> AdmissionSnapshot {
    LEDGER.snapshot()
}

struct Finish {
    ledger: &'static AdmissionLedger,
    started_at: Option<std::time::Instant>,
}

impl Drop for Finish {
    fn drop(&mut self) {
        self.ledger.finished.fetch_add(1, Ordering::Relaxed);
        if let Some(started_at) = self.started_at {
            self.ledger.record_service(started_at.elapsed());
        }
    }
}

/// Result of a non-blocking background submission attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "background admission outcomes must be handled"]
pub enum Admission {
    /// A permit was reserved and the closure was submitted.
    Admitted,
    /// All background permits are held; the closure was not submitted.
    Saturated,
    /// No Tokio runtime was available; the closure was not submitted.
    Unavailable,
}

fn bg_sem() -> &'static Arc<Semaphore> {
    static SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| Arc::new(Semaphore::new(BG_PERMITS)))
}

/// Try to reserve a background slot without waiting.
pub fn bg_permit() -> Option<OwnedSemaphorePermit> {
    bg_sem().clone().try_acquire_owned().ok()
}

/// Report an intentionally replaceable request. Refusal is counted with
/// bounded-cardinality telemetry; its existing trigger is responsible for a
/// later retry. Callers with latches or required results must handle
/// [`Admission`] themselves instead.
pub fn report_replaceable(admission: Admission) {
    match admission {
        Admission::Admitted => {}
        Admission::Saturated => {
            let count = REFUSED.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            tracing::debug!(target: "thegn::sched", count, "replaceable background work refused at capacity");
        }
        Admission::Unavailable => {
            tracing::debug!(target: "thegn::sched", "replaceable background work unavailable without a runtime");
        }
    }
}

/// Report a refused user-visible request whose caller has already emitted its
/// terminal result.
pub fn report_required_result(admission: Admission) {
    if admission != Admission::Admitted {
        tracing::debug!(target: "thegn::sched", ?admission, "background request completed with an unavailable result");
    }
}

/// Submit replaceable periodic work and account for admission refusal.
pub fn spawn_bg_replaceable(f: impl FnOnce() + Send + 'static) {
    report_replaceable(spawn_bg(f));
}

/// Report an impossible-to-run durable submission. The caller must arrange
/// recovery for the already claimed work; the permit is released by the
/// unavailable branch in `spawn_bg_reserved`.
pub fn report_reserved(admission: Admission) {
    if admission != Admission::Admitted {
        tracing::error!(target: "thegn::sched", ?admission, "reserved background work could not be submitted");
    }
}

static REFUSED: AtomicU64 = AtomicU64::new(0);

/// Submit `f` only after reserving a permit. The permit remains alive for the
/// complete closure and is released on return or unwind.
pub fn spawn_bg<F: FnOnce() + Send + 'static>(f: F) -> Admission {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        LEDGER.unavailable.fetch_add(1, Ordering::Relaxed);
        return Admission::Unavailable;
    };
    admit_and_submit(
        f,
        bg_permit,
        move |job| {
            drop(handle.spawn_blocking(job));
        },
        &LEDGER,
        crate::perf::enabled(),
    )
}

fn admit_and_submit<F, R, S>(
    f: F,
    reserve: R,
    submit: S,
    ledger: &'static AdmissionLedger,
    timed: bool,
) -> Admission
where
    F: FnOnce() + Send + 'static,
    R: FnOnce() -> Option<OwnedSemaphorePermit>,
    S: FnOnce(Box<dyn FnOnce() + Send + 'static>),
{
    let Some(permit) = reserve() else {
        ledger.refused.fetch_add(1, Ordering::Relaxed);
        return Admission::Saturated;
    };
    ledger.admitted.fetch_add(1, Ordering::Relaxed);
    let queued_at = timed.then(std::time::Instant::now);
    submit(Box::new(move || {
        let _permit = permit;
        ledger.started.fetch_add(1, Ordering::Relaxed);
        let started_at = queued_at.map(|queued_at| {
            ledger.record_wait(queued_at.elapsed());
            std::time::Instant::now()
        });
        let _finish = Finish { ledger, started_at };
        f();
    }));
    Admission::Admitted
}

/// Transfer a permit already reserved before durable state is claimed.
pub fn spawn_bg_reserved(
    permit: OwnedSemaphorePermit,
    f: impl FnOnce() + Send + 'static,
) -> Admission {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        LEDGER.unavailable.fetch_add(1, Ordering::Relaxed);
        drop(permit);
        return Admission::Unavailable;
    };
    submit_reserved(
        permit,
        f,
        move |job| drop(handle.spawn_blocking(job)),
        &LEDGER,
        crate::perf::enabled(),
    )
}

fn submit_reserved<F, S>(
    permit: OwnedSemaphorePermit,
    f: F,
    submit: S,
    ledger: &'static AdmissionLedger,
    timed: bool,
) -> Admission
where
    F: FnOnce() + Send + 'static,
    S: FnOnce(Box<dyn FnOnce() + Send + 'static>),
{
    ledger.admitted.fetch_add(1, Ordering::Relaxed);
    let queued_at = timed.then(std::time::Instant::now);
    submit(Box::new(move || {
        let _permit = permit;
        ledger.started.fetch_add(1, Ordering::Relaxed);
        let started_at = queued_at.map(|queued_at| {
            ledger.record_wait(queued_at.elapsed());
            std::time::Instant::now()
        });
        let _finish = Finish { ledger, started_at };
        f();
    }));
    Admission::Admitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    type QueuedJob = Box<dyn FnOnce() + Send>;
    type QueuedJobs = Arc<std::sync::Mutex<Vec<QueuedJob>>>;
    type QueuedJobSlot = Arc<std::sync::Mutex<Option<QueuedJob>>>;

    #[test]
    fn admission_bound_includes_queued_work_and_refuses_submission_at_capacity() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(BG_PERMITS));
        let submitted = Arc::new(AtomicUsize::new(0));
        let queued: QueuedJobs = Arc::new(std::sync::Mutex::new(Vec::new()));
        for _ in 0..BG_PERMITS {
            let queued_submit = queued.clone();
            let submitted_count = submitted.clone();
            let result = admit_and_submit(
                || {},
                || sem.clone().try_acquire_owned().ok(),
                move |job| {
                    queued_submit.lock().unwrap().push(job);
                    submitted_count.fetch_add(1, Ordering::Relaxed);
                },
                &L,
                false,
            );
            assert_eq!(result, Admission::Admitted);
        }
        let result = admit_and_submit(
            || {},
            || sem.clone().try_acquire_owned().ok(),
            |_| panic!("saturated closure must never be submitted"),
            &L,
            false,
        );
        assert_eq!(result, Admission::Saturated);
        assert_eq!(submitted.load(Ordering::Relaxed), BG_PERMITS);
        assert_eq!(queued.lock().unwrap().len(), BG_PERMITS);
    }

    #[test]
    fn missing_runtime_returns_unavailable_without_running_work() {
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_worker = ran.clone();
        let result = spawn_bg(move || {
            ran_worker.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(result, Admission::Unavailable);
        assert_eq!(ran.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn permit_covers_queued_and_running_work_and_releases_after_panic() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(1));
        let queued: QueuedJobSlot = Arc::new(std::sync::Mutex::new(None));
        let queued_submit = queued.clone();
        let result = admit_and_submit(
            || panic!("worker panic"),
            || sem.clone().try_acquire_owned().ok(),
            move |job| *queued_submit.lock().unwrap() = Some(job),
            &L,
            false,
        );
        assert_eq!(result, Admission::Admitted);
        assert!(
            sem.try_acquire().is_err(),
            "queued work holds its reservation"
        );
        let job = queued.lock().unwrap().take().unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err());
        assert!(sem.try_acquire().is_ok(), "unwind releases the reservation");
    }

    #[test]
    fn reserved_permit_is_transferred_without_second_acquisition() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let queued: QueuedJobSlot = Arc::new(std::sync::Mutex::new(None));
        let queued_submit = queued.clone();
        let result = submit_reserved(
            permit,
            || {},
            move |job| *queued_submit.lock().unwrap() = Some(job),
            &L,
            false,
        );
        assert_eq!(result, Admission::Admitted);
        assert!(
            sem.try_acquire().is_err(),
            "transferred permit stays held in queue"
        );
        queued.lock().unwrap().take().unwrap()();
        assert!(sem.try_acquire().is_ok());
    }

    #[test]
    fn ledger_counts_admission_refusal_start_and_finish_exactly() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(2));
        let queued: QueuedJobs = Arc::new(std::sync::Mutex::new(Vec::new()));
        for _ in 0..2 {
            let queued_submit = queued.clone();
            let result = admit_and_submit(
                || {},
                || sem.clone().try_acquire_owned().ok(),
                move |job| queued_submit.lock().unwrap().push(job),
                &L,
                false,
            );
            assert_eq!(result, Admission::Admitted);
        }
        let result = admit_and_submit(
            || {},
            || sem.clone().try_acquire_owned().ok(),
            |_| panic!("saturated closure must not be submitted"),
            &L,
            false,
        );
        assert_eq!(result, Admission::Saturated);

        let snapshot = L.snapshot();
        assert_eq!(snapshot.admitted, 2);
        assert_eq!(snapshot.refused, 1);
        assert_eq!(snapshot.started, 0);
        assert_eq!(snapshot.queued(), 2);
        assert_eq!(snapshot.in_flight(), 2);

        let mut jobs = queued.lock().unwrap();
        jobs.remove(0)();
        let snapshot = L.snapshot();
        assert_eq!(snapshot.started, 1);
        assert_eq!(snapshot.finished, 1);
        assert_eq!(snapshot.queued(), 1);
        assert_eq!(snapshot.in_flight(), 1);
        jobs.remove(0)();
        let snapshot = L.snapshot();
        assert_eq!(snapshot.started, 2);
        assert_eq!(snapshot.finished, 2);
        assert_eq!(snapshot.in_flight(), 0);
    }

    #[test]
    fn ledger_counts_finished_work_on_unwind() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(1));
        let queued: QueuedJobSlot = Arc::new(std::sync::Mutex::new(None));
        let queued_submit = queued.clone();
        let result = admit_and_submit(
            || panic!("worker panic"),
            || sem.clone().try_acquire_owned().ok(),
            move |job| *queued_submit.lock().unwrap() = Some(job),
            &L,
            false,
        );
        assert_eq!(result, Admission::Admitted);
        let job = queued.lock().unwrap().take().unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err());
        let snapshot = L.snapshot();
        assert_eq!(snapshot.started, 1);
        assert_eq!(snapshot.finished, 1);
        assert_eq!(snapshot.in_flight(), 0);
    }

    #[test]
    fn ledger_times_only_when_diagnostics_are_on() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(2));
        let queued: QueuedJobs = Arc::new(std::sync::Mutex::new(Vec::new()));
        for timed in [false, true] {
            let queued_submit = queued.clone();
            let result = admit_and_submit(
                || {},
                || sem.clone().try_acquire_owned().ok(),
                move |job| queued_submit.lock().unwrap().push(job),
                &L,
                timed,
            );
            assert_eq!(result, Admission::Admitted);
        }
        for job in queued.lock().unwrap().drain(..) {
            job();
        }
        let snapshot = L.snapshot();
        assert_eq!(snapshot.wait_samples, 1);
        assert_eq!(snapshot.service_samples, 1);
        assert_eq!(snapshot.finished, 2);
    }

    #[test]
    fn reserved_submission_is_counted_as_admitted() {
        static L: AdmissionLedger = AdmissionLedger::new();
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let queued: QueuedJobSlot = Arc::new(std::sync::Mutex::new(None));
        let queued_submit = queued.clone();
        let result = submit_reserved(
            permit,
            || {},
            move |job| *queued_submit.lock().unwrap() = Some(job),
            &L,
            false,
        );
        assert_eq!(result, Admission::Admitted);
        let snapshot = L.snapshot();
        assert_eq!(snapshot.admitted, 1);
        assert_eq!(snapshot.refused, 0);
    }

    #[test]
    fn snapshot_means_are_zero_without_samples() {
        let snapshot = AdmissionSnapshot::default();
        assert_eq!(snapshot.wait_mean_us(), 0);
        assert_eq!(snapshot.service_mean_us(), 0);
        assert_eq!(snapshot.in_flight(), 0);
    }
}
