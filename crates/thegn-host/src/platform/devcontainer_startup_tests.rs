//! Private startup workers and inert executables. No production controller reset.
use super::*;
#[cfg(unix)]
use std::sync::Condvar;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Point {
    OwnerStarted,
    BeforeOwnerInstall,
    BeforePrepare,
    BeforeSpawnClaim,
    SpawnClaimed,
    Spawned,
    ReaderStarted,
    BeforeReady,
}

type Callback = dyn Fn(Point) + Send + Sync;
#[derive(Default)]
pub(super) struct Hooks {
    pub owner_failure: AtomicBool,
    pub reader_failure: AtomicBool,
    pub read_failure: AtomicBool,
    pub wait_failure: AtomicBool,
    pub wait_unknown_failure: AtomicBool,
    pub native_wait_calls: AtomicUsize,
    #[cfg(unix)]
    cleanup_wait_calls: AtomicUsize,
    pub panic_after_insert: AtomicBool,
    pub wait_calls: AtomicUsize,
    pub spawn_calls: AtomicUsize,
    callback: Mutex<Option<Arc<Callback>>>,
    #[cfg(unix)]
    gates: Mutex<Vec<Arc<Gate>>>,
    #[cfg(unix)]
    directory: Mutex<Option<Arc<tempfile::TempDir>>>,
    #[cfg(unix)]
    fixture_retention: Mutex<Option<Arc<Operation>>>,
    #[cfg(unix)]
    fixture_wait_uncertain: AtomicBool,
}
impl Hooks {
    pub fn at(&self, point: Point) {
        let callback = self.callback.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback(point);
        }
    }
    #[cfg(unix)]
    fn hold(&self, point: Point) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.gates.lock().unwrap().push(gate.clone());
        let retained = gate.clone();
        *self.callback.lock().unwrap() = Some(Arc::new(move |observed| {
            if point == observed {
                retained.wait();
            }
        }));
        gate
    }
    #[cfg(unix)]
    fn release(&self) {
        for gate in self.gates.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            gate.release();
        }
    }
}

#[cfg(unix)]
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
#[cfg(unix)]
impl Gate {
    fn wait(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |s| !s.1)
            .unwrap();
        assert!(state.1, "private fixture release was not delivered");
    }
    fn entered(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(3), |s| !s.0)
            .unwrap();
        assert!(state.0, "production worker did not reach fixture barrier");
    }
    fn release(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.changed.notify_all();
    }
}

/// Older direct-up tests intentionally did not run a --version probe. Retain
/// that scope while exercising the actual owned startup/capture worker.
#[cfg(unix)]
struct ReadyCli(super::super::CliProvider);
#[cfg(unix)]
impl DevcontainerProvider for ReadyCli {
    fn probe(&self) -> super::super::ProbeReport {
        super::super::ProbeReport {
            state: super::super::ProbeState::Ready,
            executable: None,
            version: None,
            reason: None,
        }
    }
    fn prepare_start(
        &self,
        workspace: &Path,
        path: &Path,
        digest: &[u8; 32],
        content: &[u8],
        env: &[(String, String)],
    ) -> anyhow::Result<PreparedStartup> {
        self.0.prepare_start(workspace, path, digest, content, env)
    }
    fn exec_argv(
        &self,
        handle: &super::super::DevcontainerHandle,
        command: &str,
    ) -> anyhow::Result<Vec<String>> {
        self.0.exec_argv(handle, command)
    }
}

#[cfg(unix)]
pub(crate) struct OwnedFixture {
    controller: Arc<StartupController>,
    sessions: Arc<SessionMap>,
    workspace: PathBuf,
    config_path: PathBuf,
    digest: [u8; 32],
    content: Vec<u8>,
    env: Vec<(String, String)>,
    releases: Vec<PathBuf>,
    cleanup_timeout: Duration,
}
#[cfg(unix)]
impl OwnedFixture {
    pub(crate) fn up(
        directory: Arc<tempfile::TempDir>,
        executable: PathBuf,
        config_path: &Path,
        digest: [u8; 32],
        content: &[u8],
        env: &[(String, String)],
    ) -> Self {
        let fixture = Self::with_factory(
            directory.path(),
            config_path,
            digest,
            content,
            env,
            Arc::new(move || {
                Arc::new(ReadyCli(super::super::CliProvider::with_executable(
                    &executable,
                )))
            }),
        );
        *fixture.controller.hooks.directory.lock().unwrap() = Some(directory);
        fixture
    }
    fn with_factory(
        workspace: &Path,
        config_path: &Path,
        digest: [u8; 32],
        content: &[u8],
        env: &[(String, String)],
        factory: Arc<Factory>,
    ) -> Self {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let mut controller = StartupController::new(factory);
        controller.test_sessions = Some(sessions.clone());
        Self {
            controller: Arc::new(controller),
            sessions,
            workspace: workspace.to_owned(),
            config_path: config_path.to_owned(),
            digest,
            content: content.to_vec(),
            env: env.to_vec(),
            releases: Vec::new(),
            cleanup_timeout: Duration::from_secs(5),
        }
    }
    fn inputs(&self) -> StartInputs<'_> {
        StartInputs {
            workspace: &self.workspace,
            config_path: &self.config_path,
            digest: &self.digest,
            content: &self.content,
            env: &self.env,
        }
    }
    fn pending(&self, timeout: Duration) -> Result<PendingSession, StartFailure> {
        self.controller.start(self.inputs(), timeout)
    }
    pub(crate) fn start(&self) -> Result<DevcontainerSession, StartFailure> {
        self.pending(Duration::from_secs(3))?
            .publish_into("fixture", &self.sessions)?;
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get("fixture")
            .unwrap()
            .clone())
    }
    fn operation(&self) -> Arc<Operation> {
        self.controller
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .unwrap()
            .clone()
    }
    fn release(&self) -> Result<(), &'static str> {
        self.controller.hooks.release();
        let mut result = Ok(());
        for path in &self.releases {
            if std::fs::write(path, b"released").is_err() {
                result = Err("owned fixture release-file write failed");
            }
        }
        result
    }
    fn finish_inner(&self) -> Result<(), &'static str> {
        let released = self.release();
        let operation = self
            .controller
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(operation) = operation else {
            return released;
        };
        operation.held(StartReason::Abandoned);
        let result = finish_operation(&operation, self.cleanup_timeout).and(released);
        if result.is_err() {
            retain_fixture(&operation);
        }
        result
    }
    fn finish(&self) {
        if let Err(reason) = self.finish_inner() {
            panic!("{reason}; exact fixture custody and directory retained");
        }
    }
}
#[cfg(unix)]
impl Drop for OwnedFixture {
    fn drop(&mut self) {
        // During another assertion's unwind, retain the whole operation
        // without a second panic. Normal cleanup failure still fails test.
        if let Err(reason) = self.finish_inner()
            && !std::thread::panicking()
        {
            panic!("{reason}; exact fixture custody and directory retained");
        }
    }
}

#[cfg(unix)]
fn retain_fixture(operation: &Arc<Operation>) {
    // Deliberate TEST-ONLY fixed self-retention: no global leak list. It also
    // holds Hooks' real Arc<TempDir>. Only proved settlement below clears it.
    let mut retained = operation
        .hooks
        .fixture_retention
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if retained.is_none() {
        *retained = Some(operation.clone());
    }
}
#[cfg(unix)]
fn finish_operation(operation: &Arc<Operation>, timeout: Duration) -> Result<(), &'static str> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("invalid fixture cleanup deadline")?;
    loop {
        if operation
            .hooks
            .fixture_wait_uncertain
            .load(Ordering::Acquire)
        {
            return Err("fixture wait identity uncertain; no retry");
        }
        let owner_done = operation
            .owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(JoinHandle::is_finished);
        if owner_done {
            let mut parked = operation.parked.lock().unwrap_or_else(|e| e.into_inner());
            let mut settled = true;
            if let Some(resources) = parked.as_mut() {
                if resources.wait_uncertain && !operation.hooks.wait_failure.load(Ordering::Acquire)
                {
                    operation
                        .hooks
                        .fixture_wait_uncertain
                        .store(true, Ordering::Release);
                    return Err(
                        "native wait identity uncertain; fixture must retain without another wait",
                    );
                }
                if let Some(child) = resources.child.as_mut() {
                    // Synthetic wait-failure fixtures still own the exact Child.
                    // This fixture cleanup is separate from production's sole wait.
                    operation
                        .hooks
                        .cleanup_wait_calls
                        .fetch_add(1, Ordering::AcqRel);
                    match child.try_wait() {
                        Ok(Some(_)) => {}
                        Ok(None) => settled = false,
                        Err(_) => {
                            operation
                                .hooks
                                .fixture_wait_uncertain
                                .store(true, Ordering::Release);
                            return Err("fixture child cleanup wait failed");
                        }
                    }
                }
                if let Some(reader) = resources.reader.as_ref() {
                    settled &= reader.is_finished();
                }
                if settled && let Some(reader) = resources.reader.take() {
                    match reader.join() {
                        Ok(completion) => resources.read = Some(completion),
                        Err(payload) => {
                            *operation
                                .unexpected_panic
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = Some(payload);
                            operation
                                .hooks
                                .fixture_wait_uncertain
                                .store(true, Ordering::Release);
                            return Err("fixture reader unexpectedly panicked");
                        }
                    }
                }
            }
            drop(parked);
            if settled {
                if let Some(owner) = operation
                    .owner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    && let Err(payload) = owner.join()
                {
                    *operation
                        .unexpected_panic
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(payload);
                    operation
                        .hooks
                        .fixture_wait_uncertain
                        .store(true, Ordering::Release);
                    return Err("fixture owner unexpectedly panicked");
                }
                // No unfinished handle/resource was taken while polling. Leave
                // settled resources in their fixed cell until ordinary owned
                // fixture disposal, after its registered Drop gates are released.
                drop(
                    operation
                        .hooks
                        .fixture_retention
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take(),
                );
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("owned fixture cleanup deadline expired");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

thread_local! {
    static ACTIVE: std::cell::RefCell<Option<(PathBuf, Arc<StartupController>, Duration)>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn override_for(workspace: &Path) -> Option<(Arc<StartupController>, Duration)> {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .filter(|(path, _, _)| path == workspace)
            .map(|(_, controller, timeout)| (controller.clone(), *timeout))
    })
}
#[cfg(unix)]
struct AgentScope(Option<(PathBuf, Arc<StartupController>, Duration)>);
#[cfg(unix)]
impl Drop for AgentScope {
    fn drop(&mut self) {
        ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
    }
}
#[cfg(unix)]
impl OwnedFixture {
    fn agent_scope(&self, timeout: Duration) -> AgentScope {
        AgentScope(ACTIVE.with(|active| {
            active.replace(Some((
                self.workspace.clone(),
                self.controller.clone(),
                timeout,
            )))
        }))
    }
    // This invokes the actual shipping helper. The counter is an inert test
    // continuation, paired with source proof that prepare_sandbox_env uses ?
    // on this result before any OCI path; it is not a full-agent/DB fixture.
    fn agent_decision(
        &self,
        timeout: Duration,
        fallback: &AtomicUsize,
        warnings: &mut Vec<String>,
    ) -> Result<bool, StartFailure> {
        let _scope = self.agent_scope(timeout);
        let started = crate::agent::prepare_devcontainer(
            self.inputs(),
            self.workspace.to_str().unwrap(),
            warnings,
        )?;
        if !started {
            fallback.fetch_add(1, Ordering::AcqRel);
        }
        Ok(started)
    }
}

#[test]
fn zero_timeout_refuses_before_provider_factory() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let controller = StartupController::new(Arc::new(move || {
        counted.fetch_add(1, Ordering::AcqRel);
        panic!("refused request constructed a provider")
    }));
    let result = controller.start(
        StartInputs {
            workspace: Path::new("unused"),
            config_path: Path::new("unused"),
            digest: &[0; 32],
            content: b"{}",
            env: &[],
        },
        Duration::ZERO,
    );
    assert!(matches!(
        result,
        Err(StartFailure::NotStarted(StartReason::Deadline))
    ));
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(controller.slot.lock().unwrap().is_none());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Harness {
        fixture: OwnedFixture,
        directory: Arc<tempfile::TempDir>,
    }
    fn tool(name: &str) -> PathBuf {
        let path = PathBuf::from(
            thegn_core::util::which_path(name)
                .unwrap_or_else(|| panic!("Unix fixture requires {name}")),
        );
        let lexical = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().unwrap().join(path)
        };
        assert!(lexical.canonicalize().unwrap().is_file());
        lexical
    }
    fn harness(body: &str) -> Harness {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let workspace = directory.path();
        let script = workspace.join("devcontainer");
        let config = workspace.join("devcontainer.json");
        let release = workspace.join("release");
        let content = b"{\"image\":\"fixture\"}";
        std::fs::write(&config, content).unwrap();
        let digest = super::super::super::config_digest(&config).unwrap();
        std::fs::write(&script, format!("#!{}\nif [ \"$1\" = --version ]; then printf 'fixture 1\\n'; exit 0; fi\n[ \"$#\" = 5 ] && [ \"$1\" = up ] && [ \"$2\" = --workspace-folder ] && [ \"$4\" = --config ] || exit 91\n[ -f \"$5\" ] || exit 92\nprintf 'up\\n' >> \"$3/starts\"\n{body}\n", tool("sh").display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = vec![
            ("FIXTURE_RELEASE".into(), release.display().to_string()),
            ("FIXTURE_SLEEP".into(), tool("sleep").display().to_string()),
        ];
        let executable = script.clone();
        let mut fixture = OwnedFixture::with_factory(
            workspace,
            &config,
            digest,
            content,
            &env,
            Arc::new(move || {
                Arc::new(super::super::super::CliProvider::with_executable(
                    &executable,
                ))
            }),
        );
        fixture.releases.push(release);
        *fixture.controller.hooks.directory.lock().unwrap() = Some(directory.clone());
        Harness { fixture, directory }
    }
    struct CountedCli {
        inner: super::super::super::CliProvider,
        dropped: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        gate: Arc<Gate>,
    }
    impl Drop for CountedCli {
        fn drop(&mut self) {
            self.dropped
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            self.gate.wait();
        }
    }
    impl DevcontainerProvider for CountedCli {
        fn probe(&self) -> super::super::super::ProbeReport {
            self.inner.probe()
        }
        fn prepare_start(
            &self,
            workspace: &Path,
            path: &Path,
            digest: &[u8; 32],
            content: &[u8],
            env: &[(String, String)],
        ) -> anyhow::Result<PreparedStartup> {
            self.inner
                .prepare_start(workspace, path, digest, content, env)
        }
        fn exec_argv(
            &self,
            handle: &super::super::super::DevcontainerHandle,
            command: &str,
        ) -> anyhow::Result<Vec<String>> {
            self.inner.exec_argv(handle, command)
        }
    }
    struct DropEvidence {
        calls: Arc<AtomicUsize>,
        dropped: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        gate: Arc<Gate>,
    }
    fn count_provider(h: &mut Harness) -> DropEvidence {
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Gate::default());
        h.fixture
            .controller
            .hooks
            .gates
            .lock()
            .unwrap()
            .push(gate.clone());
        let counted = calls.clone();
        let drops = dropped.clone();
        let release = gate.clone();
        let executable = h.directory.path().join("devcontainer");
        Arc::get_mut(&mut h.fixture.controller).unwrap().factory = Arc::new(move || {
            counted.fetch_add(1, Ordering::AcqRel);
            Arc::new(CountedCli {
                inner: super::super::super::CliProvider::with_executable(&executable),
                dropped: drops.clone(),
                gate: release.clone(),
            })
        });
        DropEvidence {
            calls,
            dropped,
            gate,
        }
    }

    const HOLD: &str = "while [ ! -f \"$FIXTURE_RELEASE\" ]; do \"$FIXTURE_SLEEP\" 0.01; done";
    fn assert_held(
        result: Result<PendingSession, StartFailure>,
        reason: StartReason,
    ) -> StartFailure {
        match result {
            Err(error @ StartFailure::HeldUnknown(actual, _)) => {
                assert_eq!(actual, reason);
                error
            }
            Err(other) => panic!("unexpected failure: {other}"),
            Ok(_) => panic!("unresolved fixture produced success"),
        }
    }

    #[test]
    fn real_startup_stdout_null_success_and_exact_stderr_bound() {
        for stderr in [0, STDERR_LIMIT] {
            let h = harness(&format!(
                "i=0; while [ $i -lt 4096 ]; do printf '%0512d' 0; i=$((i+1)); done\ni=0; while [ $i -lt {} ]; do printf '%0512d' 0 >&2; i=$((i+1)); done",
                stderr / 512
            ));
            let session = h.fixture.start().unwrap();
            let argv = session.exec_argv("printf fixture").unwrap();
            assert!(argv.contains(&"exec".to_string()));
            assert!(session.handle.config_snapshot.path().exists());
            assert_eq!(
                std::fs::read_to_string(h.directory.path().join("starts")).unwrap(),
                "up\n"
            );
            h.fixture.finish();
            assert!(h.fixture.operation().clean.load(Ordering::Acquire));
        }
    }

    #[test]
    fn stderr_overflow_is_held_and_does_not_admit_a_second_start() {
        let h = harness("i=0; while [ $i -lt 4097 ]; do printf '%0512d' 0 >&2; i=$((i+1)); done");
        assert_held(
            h.fixture.pending(Duration::from_secs(3)),
            StartReason::Overflow,
        );
        assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
        h.fixture.finish();
        let operation = h.fixture.operation();
        assert_eq!(operation.state.load(Ordering::Acquire), HELD);
        assert_eq!(
            std::fs::read_to_string(h.directory.path().join("starts")).unwrap(),
            "up\n"
        );
    }

    #[test]
    fn nonzero_exit_keeps_bounded_stderr_diagnostic_and_resource_marker() {
        let h =
            harness("printf created > \"$3/resource\"; printf 'provider broke\\n' >&2; exit 23");
        let error = assert_held(
            h.fixture.pending(Duration::from_secs(3)),
            StartReason::Nonzero,
        );
        assert!(error.to_string().contains("provider broke"));
        assert!(error.to_string().contains("failed with exit code Some(23)"));
        assert!(h.directory.path().join("resource").exists());
        assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
    }

    #[test]
    fn hung_leader_and_nonzero_inherited_stderr_respect_the_caller_deadline() {
        for inherited in [false, true] {
            let body = if inherited {
                format!("({HOLD}) &\nprintf 'created\\n' > \"$3/resource\"; exit 23")
            } else {
                format!("printf 'created\\n' > \"$3/resource\"; {HOLD}")
            };
            let h = harness(&body);
            let before = Instant::now();
            assert_held(
                h.fixture.pending(Duration::from_millis(300)),
                StartReason::Deadline,
            );
            assert!(before.elapsed() < Duration::from_secs(2));
            assert!(
                h.directory.path().join("resource").exists(),
                "deadline fixture never actually spawned"
            );
            let operation = h.fixture.operation();
            assert!(
                operation
                    .retained
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .handle
                    .config_snapshot
                    .path()
                    .exists()
            );
            assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
            h.fixture.finish();
            assert_eq!(operation.state.load(Ordering::Acquire), HELD);
            assert!(!operation.clean.load(Ordering::Acquire));
        }
    }

    #[test]
    fn dropped_success_and_immediate_post_insert_unwind_keep_custody() {
        let h = harness("exit 0");
        let pending = h.fixture.pending(Duration::from_secs(3)).unwrap();
        let operation = h.fixture.operation();
        drop(pending);
        assert_eq!(operation.state.load(Ordering::Acquire), HELD);
        assert!(h.fixture.sessions.lock().unwrap().is_empty());
        h.fixture.finish();

        let mut previous = harness("exit 0");
        let evidence = count_provider(&mut previous);
        let old = previous.fixture.start().unwrap();
        previous.fixture.finish();
        // Stop retaining the old session in its original registry. The value
        // below is now the last provider/snapshot owner to be displaced.
        drop(previous.fixture.sessions.lock().unwrap().remove("fixture"));
        let old_snapshot = old.handle.config_snapshot.path().to_owned();
        let h = harness("exit 0");
        h.fixture
            .controller
            .hooks
            .gates
            .lock()
            .unwrap()
            .push(evidence.gate.clone());
        let pending = h.fixture.pending(Duration::from_secs(3)).unwrap();
        let operation = h.fixture.operation();
        h.fixture
            .sessions
            .lock()
            .unwrap()
            .insert("fixture".into(), old);
        h.fixture
            .controller
            .hooks
            .panic_after_insert
            .store(true, Ordering::Release);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || pending.publish_into("fixture", &h.fixture.sessions)
            ))
            .is_err()
        );
        assert_eq!(operation.state.load(Ordering::Acquire), HELD);
        assert!(operation.retired.is_poisoned());
        assert!(operation.retired.lock().unwrap_err().into_inner().is_some());
        assert!(
            h.fixture
                .sessions
                .lock()
                .unwrap_err()
                .into_inner()
                .contains_key("fixture")
        );
        assert!(old_snapshot.exists());
        assert!(
            evidence.dropped.lock().unwrap().is_empty(),
            "displaced provider destructor ran on publisher"
        );
        assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
    }

    #[test]
    fn reader_and_wait_failures_after_spawn_never_fall_back_or_repeat_wait() {
        for kind in [
            StartReason::ReaderUnavailable,
            StartReason::Read,
            StartReason::Wait,
        ] {
            let h = harness(HOLD);
            match kind {
                StartReason::ReaderUnavailable => h
                    .fixture
                    .controller
                    .hooks
                    .reader_failure
                    .store(true, Ordering::Release),
                StartReason::Read => h
                    .fixture
                    .controller
                    .hooks
                    .read_failure
                    .store(true, Ordering::Release),
                StartReason::Wait => h
                    .fixture
                    .controller
                    .hooks
                    .wait_failure
                    .store(true, Ordering::Release),
                _ => unreachable!(),
            }
            assert_held(h.fixture.pending(Duration::from_secs(2)), kind);
            let operation = h.fixture.operation();
            assert!(
                operation
                    .retained
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .handle
                    .config_snapshot
                    .path()
                    .exists()
            );
            assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
            if kind == StartReason::Wait {
                assert_eq!(
                    h.fixture
                        .controller
                        .hooks
                        .wait_calls
                        .load(Ordering::Acquire),
                    1
                );
            }
            h.fixture.finish();
            if kind == StartReason::Wait {
                assert_eq!(
                    h.fixture
                        .controller
                        .hooks
                        .wait_calls
                        .load(Ordering::Acquire),
                    1
                );
            }
        }
    }

    #[test]
    fn reservation_and_owner_launch_refuse_before_provider_construction() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let h = harness("exit 0");
        let fixture = OwnedFixture::with_factory(
            &h.fixture.workspace,
            &h.fixture.config_path,
            h.fixture.digest,
            &h.fixture.content,
            &[],
            Arc::new(move || {
                observed.fetch_add(1, Ordering::AcqRel);
                panic!("unexpected provider construction")
            }),
        );
        *fixture.controller.hooks.directory.lock().unwrap() = Some(h.directory.clone());
        fixture
            .controller
            .hooks
            .owner_failure
            .store(true, Ordering::Release);
        assert!(matches!(
            fixture.pending(Duration::from_secs(1)),
            Err(StartFailure::NotStarted(StartReason::OwnerUnavailable))
        ));
        assert_held(fixture.pending(Duration::from_secs(1)), StartReason::Busy);
        assert_eq!(calls.load(Ordering::Acquire), 0);
        let controller = fixture.controller.clone();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = controller.slot.lock().unwrap();
                panic!("fixture poison");
            }))
            .is_err()
        );
        assert_held(fixture.pending(Duration::from_secs(1)), StartReason::Busy);
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn caller_timeout_during_preparation_prevents_late_spawn() {
        let h = harness("exit 0");
        let gate = h.fixture.controller.hooks.hold(Point::BeforePrepare);
        let result = h.fixture.pending(Duration::from_millis(100));
        gate.entered();
        assert_held(result, StartReason::Deadline);
        h.fixture.finish();
        assert!(!h.directory.path().join("starts").exists());
        assert!(h.fixture.operation().clean.load(Ordering::Acquire));
    }
    #[test]
    fn shipping_agent_startup_decision_holds_nonzero_and_inherited_pipe_without_fallback() {
        for body in [
            "printf created > \"$3/resource\"; printf 'failure\\n' >&2; exit 23".to_string(),
            format!("({HOLD}) & printf created > \"$3/resource\"; exit 23"),
        ] {
            let h = harness(&body);
            let fallback = AtomicUsize::new(0);
            let mut warnings = Vec::new();
            let result =
                h.fixture
                    .agent_decision(Duration::from_millis(400), &fallback, &mut warnings);
            assert!(matches!(result, Err(StartFailure::HeldUnknown(_, _))));
            assert_eq!(fallback.load(Ordering::Acquire), 0);
            assert!(warnings.is_empty());
            assert!(h.directory.path().join("resource").exists());
            assert!(h.fixture.sessions.lock().unwrap().is_empty());
            let second = h
                .fixture
                .agent_decision(Duration::from_secs(1), &fallback, &mut warnings);
            assert!(matches!(
                second,
                Err(StartFailure::HeldUnknown(StartReason::Busy, _))
            ));
            assert_eq!(fallback.load(Ordering::Acquire), 0);
            assert_eq!(
                std::fs::read_to_string(h.directory.path().join("starts")).unwrap(),
                "up\n"
            );
        }
        let h = harness("exit 0");
        let missing = h.directory.path().join("absent-executable");
        let fixture = OwnedFixture::up(
            h.directory.clone(),
            missing,
            &h.fixture.config_path,
            h.fixture.digest,
            &h.fixture.content,
            &[],
        );
        let fallback = AtomicUsize::new(0);
        let mut warnings = Vec::new();
        assert!(
            !fixture
                .agent_decision(Duration::from_secs(1), &fallback, &mut warnings)
                .unwrap()
        );
        assert_eq!(fallback.load(Ordering::Acquire), 1);
        assert_eq!(warnings.len(), 1);
        assert!(!h.directory.path().join("starts").exists());
    }
    #[test]
    fn shipping_agent_success_and_unavailable_probe_preserve_existing_fallback_behavior() {
        let h = harness("exit 0");
        let fallback = AtomicUsize::new(0);
        let mut warnings = Vec::new();
        assert!(
            h.fixture
                .agent_decision(Duration::from_secs(3), &fallback, &mut warnings)
                .unwrap()
        );
        assert_eq!(fallback.load(Ordering::Acquire), 0);
        assert!(warnings.is_empty());
        assert!(
            h.fixture
                .sessions
                .lock()
                .unwrap()
                .contains_key(h.fixture.workspace.to_str().unwrap())
        );

        let mut h = harness("exit 0");
        let missing = h.directory.path().join("missing-capability");
        Arc::get_mut(&mut h.fixture.controller).unwrap().factory =
            Arc::new(move || Arc::new(super::super::super::CliProvider::with_executable(&missing)));
        assert!(
            !h.fixture
                .agent_decision(Duration::from_secs(2), &fallback, &mut warnings)
                .unwrap()
        );
        assert_eq!(fallback.load(Ordering::Acquire), 1);
        assert!(
            warnings.is_empty(),
            "unavailable optional CLI previously fell through silently"
        );
        assert!(!h.directory.path().join("starts").exists());
    }

    #[test]
    fn busy_refusal_does_not_construct_or_drop_another_provider() {
        let mut h = harness(HOLD);
        let evidence = count_provider(&mut h);
        assert_held(
            h.fixture.pending(Duration::from_millis(300)),
            StartReason::Deadline,
        );
        assert_eq!(evidence.calls.load(Ordering::Acquire), 1);
        for _ in 0..3 {
            assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
        }
        assert_eq!(evidence.calls.load(Ordering::Acquire), 1);
        assert!(evidence.dropped.lock().unwrap().is_empty());
    }

    #[test]
    fn accepted_replacement_disposes_old_provider_on_owner_before_admission_reopens() {
        let mut previous = harness("exit 0");
        let evidence = count_provider(&mut previous);
        let old = previous.fixture.start().unwrap();
        previous.fixture.finish();
        drop(previous.fixture.sessions.lock().unwrap().remove("fixture"));
        // finish releases fixture gates; close this one again before handing
        // the sole old-session owner into the new controller's private map.
        evidence.gate.state.lock().unwrap().1 = false;
        let h = harness("exit 0");
        h.fixture
            .controller
            .hooks
            .gates
            .lock()
            .unwrap()
            .push(evidence.gate.clone());
        h.fixture
            .sessions
            .lock()
            .unwrap()
            .insert("fixture".into(), old);
        let pending = h.fixture.pending(Duration::from_secs(3)).unwrap();
        pending
            .publish_into("fixture", &h.fixture.sessions)
            .unwrap();
        evidence.gate.entered();
        assert_eq!(evidence.dropped.lock().unwrap().len(), 1);
        assert_ne!(
            evidence.dropped.lock().unwrap()[0],
            std::thread::current().id()
        );
        assert!(!h.fixture.operation().clean.load(Ordering::Acquire));
        assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
        evidence.gate.release();
        let operation = h.fixture.operation();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !operation
            .owner
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_finished()
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(operation.clean.load(Ordering::Acquire));
        // Actual successful completion, not just publication notification,
        // allows the same controller to perform another start.
        h.fixture
            .pending(Duration::from_secs(3))
            .unwrap()
            .publish_into("fixture", &h.fixture.sessions)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(h.directory.path().join("starts")).unwrap(),
            "up\nup\n"
        );
    }

    struct BlockingPayload {
        gate: Arc<Gate>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for BlockingPayload {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            self.gate.wait();
        }
    }
    #[test]
    fn producer_panic_retains_child_and_payload_without_running_its_destructor_on_caller() {
        let h = harness(HOLD);
        let gate = Arc::new(Gate::default());
        let dropped = Arc::new(AtomicUsize::new(0));
        h.fixture
            .controller
            .hooks
            .gates
            .lock()
            .unwrap()
            .push(gate.clone());
        let retained_gate = gate.clone();
        let observed = dropped.clone();
        *h.fixture.controller.hooks.callback.lock().unwrap() = Some(Arc::new(move |point| {
            if point == Point::Spawned {
                std::panic::panic_any(BlockingPayload {
                    gate: retained_gate.clone(),
                    dropped: observed.clone(),
                });
            }
        }));
        let before = Instant::now();
        assert_held(
            h.fixture.pending(Duration::from_secs(2)),
            StartReason::Panic,
        );
        assert!(before.elapsed() < Duration::from_secs(2));
        assert_eq!(dropped.load(Ordering::Acquire), 0);
        assert_held(h.fixture.pending(Duration::from_secs(1)), StartReason::Busy);
        h.fixture.finish();
        drop(h);
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn held_startup_does_not_occupy_git_or_capability_capture_lanes() {
        let h = harness(HOLD);
        assert_held(
            h.fixture.pending(Duration::from_millis(300)),
            StartReason::Deadline,
        );
        let git_path = tool("git");
        let mut git = std::process::Command::new(git_path);
        git.arg("--version")
            .env_clear()
            .current_dir(h.directory.path());
        let output =
            crate::bounded_git_probe::capture(git, None, "owned startup independence").unwrap();
        assert!(output.starts_with(b"git version "));
        let mut capability = std::process::Command::new(tool("sh"));
        capability
            .args(["-c", "printf capability"])
            .env_clear()
            .current_dir(h.directory.path());
        let output = crate::bounded_git_probe::capture_capability(
            capability,
            Duration::from_secs(2),
            16 * 1024,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"capability");
        assert_eq!(h.fixture.operation().state.load(Ordering::Acquire), HELD);
    }
    #[test]
    fn cancellation_before_spawn_claim_refuses_child_but_after_claim_remains_unknown() {
        for (point, expected_spawns) in [(Point::BeforeSpawnClaim, 0), (Point::SpawnClaimed, 1)] {
            let h = harness("exit 0");
            let gate = h.fixture.controller.hooks.hold(point);
            assert_held(
                h.fixture.pending(Duration::from_millis(150)),
                StartReason::Deadline,
            );
            gate.entered();
            h.fixture.finish();
            assert_eq!(
                h.fixture
                    .controller
                    .hooks
                    .spawn_calls
                    .load(Ordering::Acquire),
                expected_spawns
            );
            assert_eq!(
                h.directory.path().join("starts").exists(),
                expected_spawns == 1
            );
            if expected_spawns == 0 {
                assert!(h.fixture.operation().clean.load(Ordering::Acquire));
            } else {
                assert_eq!(h.fixture.operation().state.load(Ordering::Acquire), HELD);
            }
        }
    }
    #[test]
    fn fixture_timeout_retains_real_directory_and_unfinished_handles_until_explicit_settlement() {
        let mut h = harness("exit 0");
        let path = h.directory.path().to_owned();
        // This barrier represents blocked owned IO, deliberately outside the
        // normal release list so zero-budget cleanup cannot settle it.
        let gate = Arc::new(Gate::default());
        let retained = gate.clone();
        *h.fixture.controller.hooks.callback.lock().unwrap() = Some(Arc::new(move |point| {
            if point == Point::BeforeSpawnClaim {
                retained.wait();
            }
        }));
        assert_held(
            h.fixture.pending(Duration::from_millis(100)),
            StartReason::Deadline,
        );
        gate.entered();
        let weak = Arc::downgrade(&h.fixture.operation());
        h.fixture.cleanup_timeout = Duration::ZERO;
        let failed_cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(h)));
        assert!(
            failed_cleanup.is_err(),
            "cleanup failure must not become a passing test"
        );
        let operation = weak
            .upgrade()
            .expect("unfinished operation lost fixture custody");
        assert!(operation.hooks.fixture_retention.lock().unwrap().is_some());
        assert!(
            path.exists(),
            "TempDir was removed under unfinished owned work"
        );
        assert!(
            !operation
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished()
        );
        gate.release();
        finish_operation(&operation, Duration::from_secs(3)).unwrap();
        assert!(operation.hooks.fixture_retention.lock().unwrap().is_none());
        drop(operation);
        assert!(
            !path.exists(),
            "settled private fixture directory was not released"
        );
    }

    #[test]
    fn fixture_assertion_unwind_releases_owned_work_without_double_panic() {
        let path = Mutex::new(None);
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let h = harness(HOLD);
            *path.lock().unwrap() = Some(h.directory.path().to_owned());
            let gate = h.fixture.controller.hooks.hold(Point::Spawned);
            assert_held(
                h.fixture.pending(Duration::from_millis(100)),
                StartReason::Deadline,
            );
            gate.entered();
            panic!("owned fixture assertion unwind");
        }));
        assert!(failed.is_err());
        assert!(
            !path.lock().unwrap().as_ref().unwrap().exists(),
            "cooperatively settled fixture retained its directory"
        );
    }
    #[test]
    fn unwind_after_owner_spawn_installs_handle_before_abandoning_start_gate() {
        let h = harness("exit 0");
        *h.fixture.controller.hooks.callback.lock().unwrap() = Some(Arc::new(|point| {
            if point == Point::BeforeOwnerInstall {
                panic!("owned installation unwind");
            }
        }));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            h.fixture.pending(Duration::from_secs(1))
        }));
        assert!(result.is_err());
        let operation = h.fixture.operation();
        assert!(
            operation.owner.lock().unwrap().is_some(),
            "successfully spawned owner handle was detached"
        );
        h.fixture.finish();
        assert!(!h.directory.path().join("starts").exists());
        assert_eq!(
            h.fixture
                .controller
                .hooks
                .spawn_calls
                .load(Ordering::Acquire),
            0
        );
    }
    #[test]
    fn uncertain_wait_outcome_forbids_fixture_retry_even_when_public_reason_changed() {
        let h = harness(HOLD);
        h.fixture
            .controller
            .hooks
            .wait_unknown_failure
            .store(true, Ordering::Release);
        assert_held(h.fixture.pending(Duration::from_secs(2)), StartReason::Wait);
        let operation = h.fixture.operation();
        // Current display reason is not authority to repeat a failed wait.
        *operation.reason.lock().unwrap() = StartReason::Deadline;
        assert!(h.fixture.finish_inner().is_err());
        assert!(operation.hooks.fixture_retention.lock().unwrap().is_some());
        assert!(h.directory.path().exists());
        assert!(h.fixture.finish_inner().is_err());
        assert_eq!(
            operation.hooks.cleanup_wait_calls.load(Ordering::Acquire),
            0
        );
        assert_eq!(operation.hooks.native_wait_calls.load(Ordering::Acquire), 0);
        // Only this fixture knows the supposed native error was injected
        // before any OS wait. Explicitly prove that premise before allowing
        // the exact owned fake Child to be reaped for test teardown.
        operation.hooks.wait_failure.store(true, Ordering::Release);
        operation
            .hooks
            .fixture_wait_uncertain
            .store(false, Ordering::Release);
        h.fixture.finish();
        assert!(operation.hooks.cleanup_wait_calls.load(Ordering::Acquire) > 0);
        assert!(operation.hooks.fixture_retention.lock().unwrap().is_none());
    }
}
