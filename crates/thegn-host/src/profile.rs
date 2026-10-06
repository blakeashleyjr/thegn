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

/// Profiler control state machine + atomic report publication. Compiled on any
/// unix (so its unit tests run without `pprof`), but only *used* under the
/// `profiling` feature.
#[cfg(unix)]
#[cfg_attr(not(feature = "profiling"), allow(dead_code))]
mod machine {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Observable profiler state. `Idle -> Starting -> Running -> Publishing
    /// -> Idle`; a transition lands only after its operation succeeded (a
    /// failed start returns to `Idle`, so the next signal retries at once).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum State {
        Idle,
        Starting,
        Running,
        Publishing,
    }

    static STATE: AtomicU8 = AtomicU8::new(0);

    fn publish_state(s: State) {
        STATE.store(s as u8, Ordering::SeqCst);
    }

    /// Current profiler state (written by the worker only).
    pub fn current_state() -> State {
        match STATE.load(Ordering::SeqCst) {
            1 => State::Starting,
            2 => State::Running,
            3 => State::Publishing,
            _ => State::Idle,
        }
    }

    /// The profiler/report/render seam (pprof in production, a fake in tests).
    pub trait Backend {
        type Session;
        fn start(&mut self) -> Result<Self::Session, String>;
        /// Build the report and render it into `out`. Consumes the session.
        fn render(&mut self, session: Self::Session, out: &mut dyn Write) -> Result<(), String>;
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Outcome {
        Started { generation: u64 },
        StartFailed { generation: u64, error: String },
        Published { generation: u64, path: PathBuf },
        PublishFailed { generation: u64, error: String },
    }

    pub struct Machine<B: Backend> {
        backend: B,
        session: Option<B::Session>,
        generation: u64,
        dir: PathBuf,
    }

    impl<B: Backend> Machine<B> {
        pub fn new(backend: B, dir: PathBuf) -> Self {
            Self {
                backend,
                session: None,
                generation: 0,
                dir,
            }
        }

        /// Handle one toggle request. Runs on the worker thread only.
        pub fn toggle(&mut self) -> Outcome {
            match self.session.take() {
                None => {
                    self.generation += 1;
                    publish_state(State::Starting);
                    match self.backend.start() {
                        Ok(s) => {
                            self.session = Some(s);
                            publish_state(State::Running);
                            Outcome::Started {
                                generation: self.generation,
                            }
                        }
                        Err(error) => {
                            publish_state(State::Idle);
                            Outcome::StartFailed {
                                generation: self.generation,
                                error,
                            }
                        }
                    }
                }
                Some(session) => {
                    publish_state(State::Publishing);
                    let generation = self.generation;
                    let backend = &mut self.backend;
                    let res = publish(&self.dir, generation, |out| backend.render(session, out));
                    publish_state(State::Idle);
                    match res {
                        Ok(path) => Outcome::Published { generation, path },
                        Err(error) => Outcome::PublishFailed { generation, error },
                    }
                }
            }
        }
    }

    /// Create (or validate) the owner-only output directory.
    fn prepare_dir(dir: &Path) -> Result<(), String> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
        let md = std::fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
        if !md.is_dir() {
            return Err(format!("{} is not a real directory", dir.display()));
        }
        if md.uid() != nix::unistd::geteuid().as_raw() {
            return Err(format!("{} is not owned by this user", dir.display()));
        }
        // best-effort: tighten a pre-existing directory; ownership was verified above
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        Ok(())
    }

    /// Write a report atomically: exclusive no-follow temp file, then a
    /// hard-link to the final name (fails rather than overwriting).
    pub fn publish(
        dir: &Path,
        generation: u64,
        render: impl FnOnce(&mut dyn Write) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        prepare_dir(dir)?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let name = format!(
            "flamegraph-{stamp}-p{}-g{generation}.svg",
            std::process::id()
        );
        let final_path = dir.join(&name);
        let tmp = dir.join(format!(".{name}.tmp"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let res = render(&mut f)
            .and_then(|()| f.flush().map_err(|e| e.to_string()))
            .and_then(|()| {
                std::fs::hard_link(&tmp, &final_path)
                    .map_err(|e| format!("publish {}: {e}", final_path.display()))
            });
        drop(f);
        // best-effort: temp cleanup; the final name (if any) is a separate link
        let _ = std::fs::remove_file(&tmp);
        res.map(|()| final_path)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct Fake {
            fail_start: bool,
            fail_render: bool,
        }
        impl Backend for Fake {
            type Session = ();
            fn start(&mut self) -> Result<(), String> {
                if self.fail_start {
                    Err("nope".into())
                } else {
                    Ok(())
                }
            }
            fn render(&mut self, _: (), out: &mut dyn Write) -> Result<(), String> {
                if self.fail_render {
                    return Err("render boom".into());
                }
                out.write_all(b"<svg/>").map_err(|e| e.to_string())
            }
        }
        fn machine(fs: bool, fr: bool, dir: &Path) -> Machine<Fake> {
            Machine::new(
                Fake {
                    fail_start: fs,
                    fail_render: fr,
                },
                dir.to_path_buf(),
            )
        }

        #[test]
        fn start_failure_stays_idle_and_retries_on_next_toggle() {
            let d = tempfile::tempdir().unwrap();
            let mut m = machine(true, false, d.path());
            assert!(matches!(m.toggle(), Outcome::StartFailed { .. }));
            assert_eq!(current_state(), State::Idle);
            // not out of phase: the very next toggle tries to start again
            assert!(matches!(
                m.toggle(),
                Outcome::StartFailed { generation: 2, .. }
            ));
        }

        #[test]
        fn full_cycle_publishes_owner_only_file_and_returns_idle() {
            let d = tempfile::tempdir().unwrap();
            let out = d.path().join("profiles");
            let mut m = machine(false, false, &out);
            let Outcome::Started { generation } = m.toggle() else {
                panic!()
            };
            assert_eq!(current_state(), State::Running);
            let Outcome::Published {
                path,
                generation: g,
            } = m.toggle()
            else {
                panic!()
            };
            assert_eq!(generation, g);
            assert_eq!(current_state(), State::Idle);
            assert_eq!(std::fs::read(&path).unwrap(), b"<svg/>");
            let mode = |p: &Path| std::fs::metadata(p).unwrap().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&out), 0o700);
            // no temp litter
            assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
        }

        #[test]
        fn render_failure_returns_idle_and_leaves_no_file() {
            let d = tempfile::tempdir().unwrap();
            let mut m = machine(false, true, d.path());
            m.toggle();
            assert!(matches!(m.toggle(), Outcome::PublishFailed { .. }));
            assert_eq!(current_state(), State::Idle);
            assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
            // next toggle starts a fresh capture
            assert!(matches!(m.toggle(), Outcome::Started { generation: 2 }));
        }

        #[test]
        fn repeated_dumps_never_overwrite() {
            let d = tempfile::tempdir().unwrap();
            let mut m = machine(false, false, d.path());
            let mut paths = std::collections::HashSet::new();
            for _ in 0..3 {
                m.toggle();
                let Outcome::Published { path, .. } = m.toggle() else {
                    panic!()
                };
                assert!(paths.insert(path));
            }
        }

        #[test]
        fn existing_final_name_is_not_truncated() {
            let d = tempfile::tempdir().unwrap();
            let name = format!(
                "flamegraph-{}-p{}-g7.svg",
                chrono::Local::now().format("%Y%m%d-%H%M%S"),
                std::process::id()
            );
            let victim = d.path().join(&name);
            std::fs::write(&victim, b"precious").unwrap();
            let r = publish(d.path(), 7, |o| {
                o.write_all(b"x").map_err(|e| e.to_string())
            });
            // either the clock ticked (distinct name) or the link was refused
            if let Ok(p) = r {
                assert_ne!(p, victim);
            }
            assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        }

        #[test]
        fn symlinked_output_dir_is_refused() {
            let d = tempfile::tempdir().unwrap();
            let real = d.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let link = d.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let r = publish(&link, 1, |_| Ok(()));
            assert!(r.is_err());
            assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
        }

        #[test]
        fn symlinked_temp_target_is_not_followed() {
            let d = tempfile::tempdir().unwrap();
            let victim = d.path().join("victim");
            std::fs::write(&victim, b"keep").unwrap();
            let out = d.path().join("out");
            // pre-plant the exact temp name as a symlink to the victim
            std::fs::DirBuilder::new().mode(0o700).create(&out).unwrap();
            let name = format!(
                ".flamegraph-{}-p{}-g1.svg.tmp",
                chrono::Local::now().format("%Y%m%d-%H%M%S"),
                std::process::id()
            );
            std::os::unix::fs::symlink(&victim, out.join(name)).unwrap();
            let _ = publish(&out, 1, |o| o.write_all(b"x").map_err(|e| e.to_string()));
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        }
    }
}

#[cfg(all(feature = "profiling", unix))]
mod imp {
    use super::machine::{Backend, Machine, Outcome};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
    use std::sync::{Mutex, OnceLock, PoisonError};

    static REQUEST: AtomicBool = AtomicBool::new(false);

    /// Toggle requests are coalesced: the queue holds at most 2, anything
    /// beyond that is dropped, and requests that arrive while the worker is
    /// busy (starting/publishing) are discarded when it finishes.
    fn worker() -> &'static Mutex<Option<SyncSender<()>>> {
        static TX: OnceLock<Mutex<Option<SyncSender<()>>>> = OnceLock::new();
        TX.get_or_init(|| Mutex::new(None))
    }

    struct Pprof;
    impl Backend for Pprof {
        type Session = pprof::ProfilerGuard<'static>;
        fn start(&mut self) -> Result<Self::Session, String> {
            pprof::ProfilerGuardBuilder::default()
                .frequency(199)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
                .map_err(|e| e.to_string())
        }
        fn render(
            &mut self,
            session: Self::Session,
            out: &mut dyn std::io::Write,
        ) -> Result<(), String> {
            let report = session.report().build().map_err(|e| e.to_string())?;
            report.flamegraph(out).map_err(|e| e.to_string())
        }
    }

    extern "C" fn on_sigusr2(_sig: i32) {
        // Async-signal-safe: just set a flag; the loop forwards it on its next poll.
        REQUEST.store(true, Ordering::SeqCst);
    }

    /// Install the SIGUSR2 handler. Called once at startup under the feature.
    pub fn install() {
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
        // SAFETY: handler only does an atomic store (async-signal-safe).
        let res = unsafe {
            sigaction(
                Signal::SIGUSR2,
                &SigAction::new(
                    SigHandler::Handler(on_sigusr2),
                    SaFlags::empty(),
                    SigSet::empty(),
                ),
            )
        };
        match res {
            Ok(_) => tracing::info!(
                target: "thegn::startup",
                "profiler armed: SIGUSR2 toggles a flamegraph capture"
            ),
            Err(e) => tracing::warn!(
                target: "thegn::startup",
                error = %e,
                "profiler NOT armed: SIGUSR2 handler install failed"
            ),
        }
    }

    /// Called from the event loop (one relaxed load; a delivered SIGUSR2 costs
    /// one non-blocking channel send). All real work is on the worker thread.
    pub fn poll() {
        if !REQUEST.swap(false, Ordering::SeqCst) {
            return;
        }
        let mut slot = worker().lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = spawn_worker();
        }
        let Some(tx) = slot.as_ref() else {
            return;
        };
        match tx.try_send(()) {
            Ok(()) => {}
            Err(TrySendError::Full(())) => {
                tracing::info!(target: "thegn::perf", "profiler busy; SIGUSR2 coalesced");
            }
            Err(TrySendError::Disconnected(())) => *slot = None,
        }
    }

    fn spawn_worker() -> Option<SyncSender<()>> {
        let (tx, rx) = sync_channel::<()>(2);
        let spawned = std::thread::Builder::new()
            .name("thegn-profiler".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                let dir = thegn_core::util::thegn_dir().join("profiles");
                let mut m = Machine::new(Pprof, dir);
                // Exits when the sender is dropped; otherwise it is a daemon
                // thread that dies with the process (a half-written dump is a
                // temp file, never a partial final report).
                while rx.recv().is_ok() {
                    log_outcome(&m.toggle());
                    // Coalesce: toggles that arrived mid-operation are dropped.
                    while rx.try_recv().is_ok() {}
                }
            });
        match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::warn!(target: "thegn::perf", error = %e, "profiler worker spawn failed");
                None
            }
        }
    }

    fn log_outcome(o: &Outcome) {
        tracing::debug!(target: "thegn::perf", state = ?super::machine::current_state(), "profiler state");
        match o {
            Outcome::Started { generation } => tracing::info!(
                target: "thegn::perf", generation, "profiler started (SIGUSR2 again to dump)"
            ),
            Outcome::StartFailed { generation, error } => tracing::warn!(
                target: "thegn::perf", generation, error = %error, "profiler start failed"
            ),
            Outcome::Published { generation, path } => tracing::info!(
                target: "thegn::perf", generation, path = %path.display(), "flamegraph written"
            ),
            Outcome::PublishFailed { generation, error } => tracing::warn!(
                target: "thegn::perf", generation, error = %error, "flamegraph write failed"
            ),
        }
    }
}

#[cfg(all(feature = "profiling", unix))]
pub(crate) use imp::{install, poll};

/// No-op stubs when the `profiling` feature is off (the default) or the
/// platform has no SIGUSR2 (Windows).
#[cfg(not(all(feature = "profiling", unix)))]
pub(crate) fn install() {}
#[cfg(not(all(feature = "profiling", unix)))]
#[inline]
pub(crate) fn poll() {}
