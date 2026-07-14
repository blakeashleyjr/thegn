//! Thread-local sandbox bring-up progress sink.
//!
//! The sandbox bring-up chain (`launch_spec → prepare_sandbox_env →
//! sandbox::ensure → prefetch_image`) runs synchronously on one
//! `spawn_blocking` thread, threaded through two ratchet-capped files whose
//! signatures cannot grow. Instead of plumbing a callback parameter through
//! every layer, the host installs a scoped sink on that thread and the core
//! layers [`emit`] phase events into it; with no sink installed every emit is
//! a no-op, so the CLI subcommands and tests pay nothing.
//!
//! CAVEAT: the sink is thread-local by design — an emit from a *different*
//! thread silently no-ops (graceful degradation to today's silent bring-up,
//! never a wrong-tab update). If a bring-up stage ever moves off the calling
//! thread, its events just disappear; document the loss at the move site.

use std::cell::RefCell;

use crate::pull_progress::PullSnapshot;

/// A phase event in a sandbox/worktree bring-up, emitted by the core layers
/// as they work. Phases open with their descriptive variant and close with
/// [`SandboxPhase::PhaseDone`] / [`SandboxPhase::PhaseFailed`]; the host maps
/// them onto its loading-screen step plan.
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxPhase {
    /// Backend chain resolution / bring-up entry.
    Resolve,
    /// Connecting a remote placement (ssh/mosh control channel).
    Connect { host: String },
    /// A transient connect failure is being retried.
    ConnectRetry { attempt: u32, max: u32 },
    /// Probing the runtime for the image.
    ImageProbe { image: String },
    /// The probe missed; a network pull is starting.
    ImagePull { image: String },
    /// Streaming pull progress (throttled by the parser).
    PullProgress(PullSnapshot),
    /// A Dockerfile/devcontainer image build is running.
    ImageBuild { tag: String },
    /// Container create (`run -d`) is running.
    ContainerCreate { backend: &'static str },
    /// VPN sidecar bring-up.
    Vpn,
    /// The most recently opened phase completed successfully.
    PhaseDone,
    /// The most recently opened phase failed; bring-up is aborting.
    PhaseFailed { err: String },
}

type Sink = Box<dyn FnMut(SandboxPhase) + Send>;

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Emit a phase event into this thread's sink, if one is installed. No-op
/// (and allocation-free for unit variants) otherwise. Re-entrant emits from
/// inside a sink are dropped rather than panicking.
pub fn emit(ev: SandboxPhase) {
    SINK.with(|s| {
        if let Ok(mut slot) = s.try_borrow_mut()
            && let Some(sink) = slot.as_mut()
        {
            sink(ev);
        }
    });
}

/// Install `sink` as this thread's progress sink for the guard's lifetime;
/// the previous sink (if any) is restored on drop, so scopes nest.
#[must_use = "the sink is uninstalled when the guard drops"]
pub fn scoped(sink: Sink) -> ScopeGuard {
    let prev = SINK.with(|s| s.borrow_mut().replace(sink));
    ScopeGuard { prev }
}

/// Uninstalls the scoped sink on drop, restoring the previously installed one.
pub struct ScopeGuard {
    prev: Option<Sink>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SINK.with(|s| *s.borrow_mut() = prev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn collector() -> (Arc<Mutex<Vec<SandboxPhase>>>, Sink) {
        let seen: Arc<Mutex<Vec<SandboxPhase>>> = Arc::default();
        let sink_seen = seen.clone();
        (seen, Box::new(move |ev| sink_seen.lock().unwrap().push(ev)))
    }

    #[test]
    fn emit_without_sink_is_a_noop() {
        emit(SandboxPhase::Resolve); // must not panic or leak anywhere
    }

    #[test]
    fn scoped_sink_receives_events_and_uninstalls_on_drop() {
        let (seen, sink) = collector();
        {
            let _guard = scoped(sink);
            emit(SandboxPhase::Resolve);
            emit(SandboxPhase::PhaseDone);
        }
        emit(SandboxPhase::Vpn); // after drop: dropped on the floor
        assert_eq!(
            *seen.lock().unwrap(),
            vec![SandboxPhase::Resolve, SandboxPhase::PhaseDone]
        );
    }

    #[test]
    fn scopes_nest_and_restore_the_outer_sink() {
        let (outer_seen, outer) = collector();
        let (inner_seen, inner) = collector();
        let _g1 = scoped(outer);
        emit(SandboxPhase::Resolve);
        {
            let _g2 = scoped(inner);
            emit(SandboxPhase::Vpn);
        }
        emit(SandboxPhase::PhaseDone); // outer restored
        assert_eq!(
            *outer_seen.lock().unwrap(),
            vec![SandboxPhase::Resolve, SandboxPhase::PhaseDone]
        );
        assert_eq!(*inner_seen.lock().unwrap(), vec![SandboxPhase::Vpn]);
    }

    #[test]
    fn sink_is_thread_local() {
        let (seen, sink) = collector();
        let _guard = scoped(sink);
        std::thread::spawn(|| emit(SandboxPhase::Resolve))
            .join()
            .unwrap();
        assert!(
            seen.lock().unwrap().is_empty(),
            "cross-thread emit must silently no-op"
        );
    }
}
