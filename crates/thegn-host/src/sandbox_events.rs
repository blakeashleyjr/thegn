//! Background subscription to the sandbox backend's container events for
//! audit logging (THE-79: the vendor transport moved behind the sandbox seam —
//! this module is a thin orchestrator with zero vendor knowledge).
//!
//! Selection: the configured (or auto-detected) backend hands out an events
//! transport iff its profile cap is `Yes` (`thegn_core::sandbox_events`). Two
//! event streams are subscribed on dedicated threads:
//!
//! - **Exec events** (`event=exec`, `event=die`): logged to `container_events`
//!   so the panel audit log shows what commands ran inside each container.
//! - **Network events** (`event=network`): logged when `network_audit = true`
//!   is configured.
//!
//! The transports' subscription loops block their calling thread and pulse
//! the sink per persisted batch; the sink forwards the count through a
//! channel so the event loop can refresh the panel dirty flag without
//! polling.

use thegn_core::config::{SandboxBackend, SandboxConfig};
use thegn_core::sandbox::Backend;
use thegn_core::sandbox_events::{ContainerEventSink, ContainerEvents, EventKind, EventsCap};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

/// Pulse that wakes the event loop after a batch send. Shared with the
/// short-lived trailing-flush thread, hence `Arc` + `Sync`.
type Wake = Arc<dyn Fn() + Send + Sync>;

/// Coalescing window: at most one send + loop wake per window, plus one
/// trailing flush for anything deferred inside it.
const PULSE_WINDOW: Duration = Duration::from_millis(250);

fn waker_pulse(waker: TerminalWaker) -> Wake {
    Arc::new(move || {
        if let Err(error) = waker.wake() {
            tracing::debug!(%error, "sandbox events: waker pulse failed");
        }
    })
}

/// Update type sent to the event loop: tells it to refresh the audit panel.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SandboxEventBatch {
    /// Number of new events written to the DB.
    pub count: usize,
}

/// Start the background container-events subscriber for the configured
/// sandbox backend.
///
/// Silently does nothing when sandboxing is disabled or the selection yields no
/// transport (auto with no events-capable chain entry, a reserved or `No` cap,
/// or the transport's binary not on PATH) — audit is best-effort.
pub fn spawn(
    cfg: &SandboxConfig,
    tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>,
    waker: TerminalWaker,
) {
    let Some(backend) = select_backend(cfg) else {
        return;
    };
    // Honest one-line note when the selected backend can never stream events
    // (reserved cap) — debug level, off the hot path.
    if let EventsCap::Reserved(reason) = backend.profile().events {
        tracing::debug!(backend = %backend.label(), reason, "container events: reserved");
    }
    let Some(events) = backend.events() else {
        return;
    };
    // The old PATH-presence gate on the vendor binary, relocated into the
    // transport: no subscriber threads when the runtime binary is missing.
    if !events.available() {
        return;
    }
    // Exec events.
    subscribe_thread(
        "thegn-sbx-exec",
        events,
        EventKind::Exec,
        tx.clone(),
        waker_pulse(waker.clone()),
    );
    // Network events (optional).
    if cfg.network_audit {
        let Some(net_events) = backend.events() else {
            return;
        };
        subscribe_thread(
            "thegn-sbx-net",
            net_events,
            EventKind::Network,
            tx,
            waker_pulse(waker),
        );
    }
}

/// Spawn one named subscriber thread running the transport's blocking
/// subscription loop to scope end.
fn subscribe_thread(
    name: &'static str,
    events: Box<dyn ContainerEvents>,
    kind: EventKind,
    tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>,
    wake: Wake,
) {
    thegn_core::thread_name::try_spawn_named(name, move || {
        // Housekeeping: blocks on the container event stream for the
        // process lifetime, writing audit rows nobody waits on. Declared
        // FIRST — the thread-qos ratchet.
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        let mut sink = BatchForwarder::new(tx, wake);
        events.subscribe(kind, &mut sink);
    })
    // best-effort: listener thread: a failed spawn just disables sandbox
    // event recording for this session
    .ok();
}

/// Outcome of offering a batch to the [`PulseCoalescer`].
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Outside the window: send this accumulated count now and pulse.
    Send(usize),
    /// Inside the window: the count was accumulated; send nothing yet.
    Defer,
}

/// Pure rate limiter for loop wakes. No clock reads: `now` is injected.
#[derive(Debug, Default)]
struct PulseCoalescer {
    last_sent: Option<Instant>,
    pending: usize,
}

impl PulseCoalescer {
    fn on_batch(&mut self, now: Instant, count: usize) -> Decision {
        self.pending = self.pending.saturating_add(count);
        let in_window = self
            .last_sent
            .is_some_and(|t| now.saturating_duration_since(t) < PULSE_WINDOW);
        if in_window {
            Decision::Defer
        } else {
            Decision::Send(self.take(now))
        }
    }

    /// Time until the trailing flush is due; `None` when nothing is pending.
    fn due_flush(&self, now: Instant) -> Option<Duration> {
        if self.pending == 0 {
            return None;
        }
        let elapsed = self
            .last_sent
            .map_or(PULSE_WINDOW, |t| now.saturating_duration_since(t));
        Some(PULSE_WINDOW.saturating_sub(elapsed))
    }

    /// Drain the accumulated count and mark a send at `now`.
    fn take(&mut self, now: Instant) -> usize {
        self.last_sent = Some(now);
        std::mem::take(&mut self.pending)
    }
}

/// State shared between the subscriber thread and the trailing-flush thread.
struct Shared {
    coalescer: Mutex<PulseCoalescer>,
    flush_scheduled: AtomicBool,
}

/// Send one batch and pulse only when the send landed: the loop blocks on
/// poll_input, so an unpulsed batch would wait for an unrelated wake.
fn send_and_pulse(tx: &tokio_mpsc::UnboundedSender<SandboxEventBatch>, wake: &Wake, count: usize) {
    if count > 0 && tx.send(SandboxEventBatch { count }).is_ok() {
        wake();
    }
}

/// Forwards the transports' persisted-rows pulses to the event loop channel,
/// coalesced per [`PULSE_WINDOW`] with a trailing flush.
struct BatchForwarder {
    tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>,
    wake: Wake,
    shared: Arc<Shared>,
}

impl BatchForwarder {
    fn new(tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>, wake: Wake) -> Self {
        Self {
            tx,
            wake,
            shared: Arc::new(Shared {
                coalescer: Mutex::new(PulseCoalescer::default()),
                flush_scheduled: AtomicBool::new(false),
            }),
        }
    }

    /// Drain pending and clear the flush flag under the coalescer lock, so a
    /// deferral racing the flush either lands before the drain or schedules
    /// a fresh flush after it.
    fn drain_for_flush(shared: &Shared) -> usize {
        let mut c = shared
            .coalescer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let n = c.take(Instant::now());
        shared.flush_scheduled.store(false, Ordering::SeqCst);
        n
    }

    /// Spawn the one short-lived trailing-flush thread. It exists only while
    /// a deferred batch is pending (0% idle: no permanent timer).
    fn schedule_flush(&self, delay: Duration) {
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        let shared = self.shared.clone();
        let spawned = thegn_core::thread_name::try_spawn_named("thegn-sbx-flush", move || {
            // Declared FIRST — the thread-qos ratchet.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            std::thread::sleep(delay);
            let n = Self::drain_for_flush(&shared);
            send_and_pulse(&tx, &wake, n);
        });
        if spawned.is_err() {
            // Never lose the pulse: send what is pending right now.
            let n = Self::drain_for_flush(&self.shared);
            send_and_pulse(&self.tx, &self.wake, n);
        }
    }
}

impl ContainerEventSink for BatchForwarder {
    fn on_batch(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let (decision, due) = {
            let mut c = self
                .shared
                .coalescer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let d = c.on_batch(now, count);
            (d, c.due_flush(now))
        };
        match decision {
            Decision::Send(total) => send_and_pulse(&self.tx, &self.wake, total),
            Decision::Defer => {
                if !self.shared.flush_scheduled.swap(true, Ordering::SeqCst) {
                    self.schedule_flush(due.unwrap_or(PULSE_WINDOW));
                }
            }
        }
    }
}

/// Pure selection (no I/O): the explicit config kind resolves through
/// `Backend::from_config`; `auto` walks `backend_chain` and picks the FIRST
/// entry whose events cap is `Yes` — mirroring how chain resolution picks a
/// runtime. `None` only when an explicit `auto` has no events-capable chain
/// entry; it also returns `None` when sandboxing is disabled. An explicit kind
/// always resolves (a reserved/`No` cap is answered downstream, after the
/// honest `Reserved` note).
fn select_backend(cfg: &SandboxConfig) -> Option<Backend> {
    if !cfg.enabled {
        return None;
    }
    match cfg.backend {
        SandboxBackend::Auto => cfg
            .backend_chain
            .iter()
            .find_map(|name| Backend::parse(name).filter(|b| b.profile().events == EventsCap::Yes)),
        explicit => Backend::from_config(explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn cfg(backend: SandboxBackend, chain: &[&str]) -> SandboxConfig {
        SandboxConfig {
            backend,
            backend_chain: chain.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn batch_sends_and_pulses_once_empty_batch_does_neither() {
        use std::sync::atomic::AtomicUsize;
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let pulses = Arc::new(AtomicUsize::new(0));
        let p = pulses.clone();
        let mut sink = BatchForwarder::new(
            tx,
            Arc::new(move || {
                p.fetch_add(1, Ordering::SeqCst);
            }),
        );
        sink.on_batch(0);
        assert_eq!(pulses.load(Ordering::SeqCst), 0);
        assert!(rx.try_recv().is_err());
        sink.on_batch(3);
        assert_eq!(pulses.load(Ordering::SeqCst), 1);
        assert_eq!(rx.try_recv().unwrap().count, 3);
        let (tx2, rx2) = tokio_mpsc::unbounded_channel();
        drop(rx2);
        let p2 = pulses.clone();
        let mut closed = BatchForwarder::new(
            tx2,
            Arc::new(move || {
                p2.fetch_add(1, Ordering::SeqCst);
            }),
        );
        closed.on_batch(1);
        assert_eq!(
            pulses.load(Ordering::SeqCst),
            1,
            "no pulse when send failed"
        );
    }

    #[test]
    fn coalescer_first_sends_second_defers_after_window_sends_sum() {
        let t0 = Instant::now();
        let mut c = PulseCoalescer::default();
        assert_eq!(c.due_flush(t0), None);
        assert_eq!(c.on_batch(t0, 2), Decision::Send(2));
        assert_eq!(
            c.on_batch(t0 + Duration::from_millis(50), 3),
            Decision::Defer
        );
        assert_eq!(
            c.on_batch(t0 + Duration::from_millis(100), 4),
            Decision::Defer
        );
        assert_eq!(
            c.due_flush(t0 + Duration::from_millis(100)),
            Some(Duration::from_millis(150))
        );
        assert_eq!(
            c.on_batch(t0 + PULSE_WINDOW + Duration::from_millis(1), 1),
            Decision::Send(8)
        );
        assert_eq!(c.due_flush(t0 + PULSE_WINDOW * 2), None);
    }

    #[test]
    fn coalescer_due_flush_saturates_past_the_window() {
        let t0 = Instant::now();
        let mut c = PulseCoalescer::default();
        c.on_batch(t0, 1);
        c.on_batch(t0 + Duration::from_millis(10), 1);
        assert_eq!(c.due_flush(t0 + PULSE_WINDOW * 4), Some(Duration::ZERO));
    }

    #[test]
    fn coalescer_count_saturates() {
        let t0 = Instant::now();
        let mut c = PulseCoalescer::default();
        c.on_batch(t0, 1);
        c.on_batch(t0, usize::MAX);
        c.on_batch(t0, usize::MAX);
        let total: usize = c.take(t0);
        assert_eq!(total, usize::MAX);
    }

    #[test]
    fn deferred_batch_is_flushed_by_trailing_thread() {
        use std::sync::atomic::AtomicUsize;
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let pulses = Arc::new(AtomicUsize::new(0));
        let p = pulses.clone();
        let mut sink = BatchForwarder::new(
            tx,
            Arc::new(move || {
                p.fetch_add(1, Ordering::SeqCst);
            }),
        );
        sink.on_batch(1);
        let mut total: usize = 0;
        let mut msgs: usize = 0;
        let first = rx.try_recv().unwrap();
        total += first.count;
        msgs += 1;
        assert_eq!(first.count, 1);
        sink.on_batch(2);
        sink.on_batch(3);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            while let Ok(b) = rx.try_recv() {
                total += b.count;
                msgs += 1;
            }
            if total == 6 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(total, 6);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && pulses.load(Ordering::SeqCst) != msgs {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            pulses.load(Ordering::SeqCst),
            msgs,
            "one pulse per landed send"
        );
        assert!(msgs >= 2);
    }

    #[test]
    fn auto_walks_the_chain_for_the_first_events_capable_entry() {
        // Docker is reserved, so the chain falls through to podman.
        assert_eq!(
            select_backend(&cfg(SandboxBackend::Auto, &["docker", "podman-rootless"])),
            Some(Backend::Podman)
        );
        assert_eq!(
            select_backend(&cfg(
                SandboxBackend::Auto,
                &["bwrap", "podman-rootful", "host"]
            )),
            Some(Backend::PodmanRootful)
        );
        // Nothing events-capable in the chain.
        assert_eq!(
            select_backend(&cfg(SandboxBackend::Auto, &["bwrap", "host"])),
            None
        );
    }

    #[test]
    fn explicit_podman_selects_the_transport() {
        assert_eq!(
            select_backend(&cfg(SandboxBackend::Podman, &[])),
            Some(Backend::Podman)
        );
    }

    #[test]
    fn explicit_docker_stops_at_the_reserved_branch() {
        let backend = select_backend(&cfg(SandboxBackend::Docker, &[]))
            .expect("docker resolves as a backend");
        assert_eq!(backend, Backend::Docker);
        assert!(matches!(backend.profile().events, EventsCap::Reserved(_)));
        // …and the op answers None after the Reserved note.
        assert!(backend.events().is_none());
    }

    #[test]
    fn explicit_none_has_no_transport() {
        let backend =
            select_backend(&cfg(SandboxBackend::None, &[])).expect("none resolves as a backend");
        assert_eq!(backend, Backend::None);
        assert!(backend.events().is_none());
    }

    #[test]
    fn disabled_sandbox_selects_no_event_transport() {
        for backend in [SandboxBackend::Auto, SandboxBackend::Podman] {
            let cfg = SandboxConfig {
                enabled: false,
                ..cfg(backend, &["podman-rootless"])
            };
            assert_eq!(select_backend(&cfg), None);
        }
    }
}
