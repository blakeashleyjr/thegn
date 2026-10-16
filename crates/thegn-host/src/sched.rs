//! Background-work admission for the shared Tokio blocking pool.
//!
//! The eight permits bound admitted background closures, including closures
//! waiting in Tokio's blocking queue. Measurement runs on its own named
//! `Background` lanes outside these permits. Other direct `spawn_blocking`
//! calls and `std::thread` workers are also outside this budget.
//! Admission is non-blocking, so interactive callers never wait for a permit.
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
        return Admission::Unavailable;
    };
    admit_and_submit(f, bg_permit, move |job| {
        drop(handle.spawn_blocking(job));
    })
}

fn admit_and_submit<F, R, S>(f: F, reserve: R, submit: S) -> Admission
where
    F: FnOnce() + Send + 'static,
    R: FnOnce() -> Option<OwnedSemaphorePermit>,
    S: FnOnce(Box<dyn FnOnce() + Send + 'static>),
{
    let Some(permit) = reserve() else {
        return Admission::Saturated;
    };
    submit(Box::new(move || {
        let _permit = permit;
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
        drop(permit);
        return Admission::Unavailable;
    };
    submit_reserved(permit, f, move |job| drop(handle.spawn_blocking(job)))
}

fn submit_reserved<F, S>(permit: OwnedSemaphorePermit, f: F, submit: S) -> Admission
where
    F: FnOnce() + Send + 'static,
    S: FnOnce(Box<dyn FnOnce() + Send + 'static>),
{
    submit(Box::new(move || {
        let _permit = permit;
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
            );
            assert_eq!(result, Admission::Admitted);
        }
        let result = admit_and_submit(
            || {},
            || sem.clone().try_acquire_owned().ok(),
            |_| panic!("saturated closure must never be submitted"),
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
        let sem = Arc::new(Semaphore::new(1));
        let queued: QueuedJobSlot = Arc::new(std::sync::Mutex::new(None));
        let queued_submit = queued.clone();
        let result = admit_and_submit(
            || panic!("worker panic"),
            || sem.clone().try_acquire_owned().ok(),
            move |job| *queued_submit.lock().unwrap() = Some(job),
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
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let queued: QueuedJobSlot = Arc::new(std::sync::Mutex::new(None));
        let queued_submit = queued.clone();
        let result = submit_reserved(
            permit,
            || {},
            move |job| *queued_submit.lock().unwrap() = Some(job),
        );
        assert_eq!(result, Admission::Admitted);
        assert!(
            sem.try_acquire().is_err(),
            "transferred permit stays held in queue"
        );
        queued.lock().unwrap().take().unwrap()();
        assert!(sem.try_acquire().is_ok());
    }
}
