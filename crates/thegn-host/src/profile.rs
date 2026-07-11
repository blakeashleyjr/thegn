//! In-process sampling profiler (the `profiling` cargo feature).
//!
//! `ptrace_scope=1` blocks attaching `perf`/`gdb` to an already-running thegn,
//! so instead the process profiles *itself*: send `SIGUSR2` once to start, again
//! to stop and write a flamegraph to `$XDG_STATE_HOME/thegn/profiles/`. This
//! is the only path that can profile the live daily multiplexer.
//!
//! Entirely behind `#[cfg(feature = "profiling")]` — when the feature is off the
//! `pprof` dependency isn't compiled and [`install`] is an empty stub, so a
//! normal build pays nothing. See `just profile` for the wrapper.

#[cfg(feature = "profiling")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    static ARMED: AtomicBool = AtomicBool::new(false);
    static REQUEST: AtomicBool = AtomicBool::new(false);

    fn guard_slot() -> &'static Mutex<Option<pprof::ProfilerGuard<'static>>> {
        static SLOT: OnceLock<Mutex<Option<pprof::ProfilerGuard<'static>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    extern "C" fn on_sigusr2(_sig: i32) {
        // Async-signal-safe: just set a flag; the work happens on the next poll.
        REQUEST.store(true, Ordering::SeqCst);
    }

    /// Install the SIGUSR2 handler. Called once at startup under the feature.
    pub fn install() {
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
        // SAFETY: handler only does a relaxed atomic store (async-signal-safe).
        unsafe {
            sigaction(
                Signal::SIGUSR2,
                &SigAction::new(
                    SigHandler::Handler(on_sigusr2),
                    SaFlags::empty(),
                    SigSet::empty(),
                ),
            )
            .ok();
        }
        tracing::info!(
            target: "thegn::startup",
            "profiler armed: SIGUSR2 toggles a flamegraph capture"
        );
    }

    /// Called from the event loop when convenient (it's cheap — one relaxed
    /// load). Toggling captures on/off in response to a delivered SIGUSR2.
    pub fn poll() {
        if !REQUEST.swap(false, Ordering::SeqCst) {
            return;
        }
        if !ARMED.swap(true, Ordering::SeqCst) {
            start();
        } else {
            ARMED.store(false, Ordering::SeqCst);
            stop_and_dump();
        }
    }

    fn start() {
        match pprof::ProfilerGuardBuilder::default()
            .frequency(199)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
        {
            Ok(g) => {
                *guard_slot().lock().unwrap() = Some(g);
                tracing::info!(target: "thegn::perf", "profiler started (SIGUSR2 again to dump)");
            }
            Err(e) => tracing::warn!(target: "thegn::perf", error = %e, "profiler start failed"),
        }
    }

    fn stop_and_dump() {
        let Some(guard) = guard_slot().lock().unwrap().take() else {
            return;
        };
        let report = match guard.report().build() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(target: "thegn::perf", error = %e, "profiler report failed");
                return;
            }
        };
        let dir = thegn_core::util::thegn_dir().join("profiles");
        let _ = std::fs::create_dir_all(&dir);
        // No `Date::now` in this codebase's hot paths, but here at the edge a
        // wall-clock stamp is fine for a unique filename.
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("flamegraph-{stamp}.svg"));
        match std::fs::File::create(&path).and_then(|f| {
            report
                .flamegraph(f)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }) {
            Ok(()) => tracing::info!(
                target: "thegn::perf",
                path = %path.display(),
                "flamegraph written"
            ),
            Err(e) => tracing::warn!(target: "thegn::perf", error = %e, "flamegraph write failed"),
        }
    }
}

#[cfg(feature = "profiling")]
pub(crate) use imp::{install, poll};

/// No-op stubs when the `profiling` feature is off (the default).
#[cfg(not(feature = "profiling"))]
pub(crate) fn install() {}
#[cfg(not(feature = "profiling"))]
#[inline]
pub(crate) fn poll() {}
