//! One admitted devcontainer startup. Caller deadlines do not surrender child,
//! pipe or possible resource ownership. No signals or automatic reconciliation.

use super::{DevcontainerProvider, DevcontainerSession, PreparedStartup};
use std::any::Any;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const STDERR_LIMIT: usize = 2 * 1024 * 1024;
const PENDING: u8 = 0;
const READY: u8 = 1;
const PUBLISHING: u8 = 2;
const ACCEPTED: u8 = 3;
const HELD: u8 = 4;
const NOT_STARTED: u8 = 5;
// Resource effects are held already; wait only for bounded diagnostics.
const HELD_DRAINING: u8 = 6;
const SPAWNING: u8 = 7;
const RUNNING: u8 = 8;

type PanicPayload = Box<dyn Any + Send>;
type Factory = dyn Fn() -> Arc<dyn DevcontainerProvider> + Send + Sync;
type SessionMap = Mutex<HashMap<String, DevcontainerSession>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartReason {
    Busy,
    Deadline,
    OwnerUnavailable,
    ProbeUnavailable,
    Preparation,
    Spawn,
    ReaderUnavailable,
    Read,
    Overflow,
    Wait,
    Nonzero,
    Abandoned,
    Publication,
    Panic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartFailure {
    NotStarted(StartReason),
    HeldUnknown(StartReason, Arc<Diagnostic>),
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted(reason) => write!(f, "devcontainer was not started ({reason:?})"),
            Self::HeldUnknown(reason, diagnostic) => {
                write!(
                    f,
                    "devcontainer startup outcome unknown ({reason:?}); automatic fallback/retry held; work may still be running"
                )?;
                if *reason == StartReason::Nonzero {
                    write!(f, "; up failed with exit code {:?}", diagnostic.exit_code)?;
                }
                if diagnostic.len > 0 {
                    write!(
                        f,
                        ": {}",
                        String::from_utf8_lossy(&diagnostic.bytes[..diagnostic.len])
                    )?;
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for StartFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    bytes: [u8; 512],
    len: usize,
    exit_code: Option<i32>,
}
impl Default for Diagnostic {
    fn default() -> Self {
        Self {
            bytes: [0; 512],
            len: 0,
            exit_code: None,
        }
    }
}
impl Diagnostic {
    fn first_line(&mut self, bytes: &[u8]) {
        let prefix = &bytes[..bytes.len().min(self.bytes.len())];
        let first = prefix
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        self.len = first.len();
        self.bytes[..self.len].copy_from_slice(first);
    }
}
impl StartFailure {
    fn held(reason: StartReason) -> Self {
        Self::HeldUnknown(reason, Arc::new(Diagnostic::default()))
    }
}

/// Borrowed input only: refusal cannot destroy an unadmitted provider/factory.
pub(crate) struct StartInputs<'a> {
    pub workspace: &'a Path,
    pub config_path: &'a Path,
    pub digest: &'a [u8; 32],
    pub content: &'a [u8],
    pub env: &'a [(String, String)],
}

struct Request {
    workspace: PathBuf,
    config_path: PathBuf,
    digest: [u8; 32],
    content: Vec<u8>,
    env: Vec<(String, String)>,
    factory: Arc<Factory>,
}

#[derive(Default)]
struct Resources {
    request: Option<Request>,
    provider: Option<Arc<dyn DevcontainerProvider>>,
    prepared: Option<PreparedStartup>,
    child: Option<Child>,
    wait_uncertain: bool,
    reader: Option<JoinHandle<ReadCompletion>>,
    read: Option<ReadCompletion>,
    panic: Option<PanicPayload>,
}

struct ReadCompletion {
    // Exactly one fixed-size allocation; even a refused stream cannot grow it.
    bytes: Box<[u8]>,
    len: usize,
    failure: Option<StartReason>,
    _panic: Option<PanicPayload>,
    _session: DevcontainerSession,
}

struct Operation {
    deadline: Instant,
    state: AtomicU8,
    reason: Mutex<StartReason>,
    diagnostic: Mutex<Diagnostic>,
    clean: AtomicBool,
    notice: mpsc::SyncSender<()>,
    ack: mpsc::SyncSender<()>,
    initial: Mutex<Option<Request>>,
    owner: Mutex<Option<JoinHandle<()>>>,
    // Only the owner writes this once, after its catch boundary. No backref.
    parked: Mutex<Option<Box<Resources>>>,
    unexpected_panic: Mutex<Option<PanicPayload>>,
    retained: Mutex<Option<DevcontainerSession>>,
    retired: Mutex<Option<DevcontainerSession>>,
    #[cfg(test)]
    hooks: Arc<tests::Hooks>,
    #[cfg(test)]
    test_sessions: Option<Arc<SessionMap>>,
}

impl Operation {
    fn notify(&self) {
        // The state is authoritative; full or disconnected notifications need
        // no retry, allocation, logging or wake timer.
        self.notice.try_send(()).unwrap_or(());
        self.ack.try_send(()).unwrap_or(());
    }

    fn held(&self, reason: StartReason) {
        let mut retained_reason = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.state.load(Ordering::Acquire);
        while matches!(state, PENDING | SPAWNING | RUNNING | READY | HELD_DRAINING) {
            match self
                .state
                .compare_exchange_weak(state, HELD, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    *retained_reason = reason;
                    drop(retained_reason);
                    self.notify();
                    return;
                }
                Err(next) => state = next,
            }
        }
    }

    fn failure(&self) -> StartFailure {
        let reason = *self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if self.state.load(Ordering::Acquire) == NOT_STARTED {
            StartFailure::NotStarted(reason)
        } else {
            StartFailure::HeldUnknown(
                reason,
                Arc::new(*self.diagnostic.lock().unwrap_or_else(|e| e.into_inner())),
            )
        }
    }

    fn not_started(&self, reason: StartReason) {
        let mut retained_reason = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        *retained_reason = reason;
        self.state.store(NOT_STARTED, Ordering::Release);
        drop(retained_reason);
        self.notify();
    }
}

pub(super) struct StartupController {
    slot: Mutex<Option<Arc<Operation>>>,
    factory: Arc<Factory>,
    #[cfg(test)]
    hooks: Arc<tests::Hooks>,
    #[cfg(test)]
    test_sessions: Option<Arc<SessionMap>>,
}

impl StartupController {
    fn new(factory: Arc<Factory>) -> Self {
        Self {
            slot: Mutex::new(None),
            factory,
            #[cfg(test)]
            hooks: Arc::new(tests::Hooks::default()),
            #[cfg(test)]
            test_sessions: None,
        }
    }

    fn start(
        &self,
        inputs: StartInputs<'_>,
        timeout: Duration,
    ) -> Result<PendingSession, StartFailure> {
        let Some(deadline) = Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
        else {
            return Err(StartFailure::NotStarted(StartReason::Deadline));
        };
        let mut slot = self
            .slot
            .try_lock()
            .map_err(|_| StartFailure::held(StartReason::Busy))?;
        if let Some(previous) = slot.as_ref() {
            if !previous.clean.load(Ordering::Acquire) {
                return Err(StartFailure::held(StartReason::Busy));
            }
            let mut owner = previous
                .owner
                .try_lock()
                .map_err(|_| StartFailure::held(StartReason::Busy))?;
            if !owner.as_ref().is_some_and(JoinHandle::is_finished) {
                return Err(StartFailure::held(StartReason::Busy));
            }
            if let Err(payload) = owner.take().expect("finished owner retained").join() {
                *previous
                    .unexpected_panic
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(payload);
                previous.clean.store(false, Ordering::Release);
                return Err(StartFailure::held(StartReason::Panic));
            }
        }
        let (notice, events) = mpsc::sync_channel(1);
        let (ack, accepted) = mpsc::sync_channel(1);
        let operation = Arc::new(Operation {
            deadline,
            state: AtomicU8::new(PENDING),
            reason: Mutex::new(StartReason::Abandoned),
            diagnostic: Mutex::new(Diagnostic::default()),
            clean: AtomicBool::new(false),
            notice,
            ack,
            initial: Mutex::new(None),
            owner: Mutex::new(None),
            parked: Mutex::new(None),
            unexpected_panic: Mutex::new(None),
            retained: Mutex::new(None),
            retired: Mutex::new(None),
            #[cfg(test)]
            hooks: self.hooks.clone(),
            #[cfg(test)]
            test_sessions: self.test_sessions.clone(),
        });
        // Reserving precedes copies and all factory/provider work. No old
        // operation is removed unless its off-caller disposal actually ended.
        *slot = Some(operation.clone());
        drop(slot);
        let mut cancellation = CallerGuard {
            operation: operation.clone(),
            armed: true,
        };
        *operation.initial.lock().unwrap_or_else(|e| e.into_inner()) = Some(Request {
            workspace: inputs.workspace.to_path_buf(),
            config_path: inputs.config_path.to_path_buf(),
            digest: *inputs.digest,
            content: inputs.content.to_vec(),
            env: inputs.env.to_vec(),
            factory: self.factory.clone(),
        });
        let (open, gate) = mpsc::sync_channel(1);
        let handle = spawn_owner(operation.clone(), accepted, gate);
        let handle = match handle {
            Ok(handle) => handle,
            Err(_) => {
                // The closure owned only Arcs. Retain the request in the fixed
                // entry when no owner exists for potentially effectful disposal.
                operation.not_started(StartReason::OwnerUnavailable);
                cancellation.armed = false;
                return Err(operation.failure());
            }
        };
        // First action after successful spawn: guard the retained JoinHandle.
        let mut installation = OwnerInstall {
            operation: &operation,
            handle: Some(handle),
        };
        #[cfg(test)]
        operation.hooks.at(tests::Point::BeforeOwnerInstall);
        installation.install();
        drop(installation);
        if open.send(()).is_err() {
            operation.held(StartReason::OwnerUnavailable);
        }
        loop {
            match operation.state.load(Ordering::Acquire) {
                READY => {
                    cancellation.armed = false;
                    return Ok(PendingSession { operation });
                }
                HELD | NOT_STARTED => {
                    cancellation.armed = false;
                    return Err(operation.failure());
                }
                _ => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || events.recv_timeout(remaining).is_err() {
                operation.held(StartReason::Deadline);
                return Err(operation.failure());
            }
        }
    }
}

fn spawn_owner(
    operation: Arc<Operation>,
    accepted: mpsc::Receiver<()>,
    gate: mpsc::Receiver<()>,
) -> std::io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if operation.hooks.owner_failure.load(Ordering::Acquire) {
        return Err(std::io::Error::other("owned fixture owner launch failure"));
    }
    std::thread::Builder::new()
        .name("devcontainer-startup".into())
        .spawn(move || {
            if gate.recv().is_err() {
                operation.not_started(StartReason::Abandoned);
                return;
            }
            own_startup(operation, accepted);
        })
}

struct OwnerInstall<'a> {
    operation: &'a Operation,
    handle: Option<JoinHandle<()>>,
}
impl OwnerInstall<'_> {
    fn install(&mut self) {
        if let Some(handle) = self.handle.take() {
            // One installation guard exists per newly reserved generation.
            *self
                .operation
                .owner
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(handle);
        }
    }
}
impl Drop for OwnerInstall<'_> {
    fn drop(&mut self) {
        self.install();
    }
}

struct CallerGuard {
    operation: Arc<Operation>,
    armed: bool,
}
impl Drop for CallerGuard {
    fn drop(&mut self) {
        if self.armed {
            self.operation.held(StartReason::Abandoned);
        }
    }
}

fn own_startup(operation: Arc<Operation>, accepted: mpsc::Receiver<()>) {
    // Custody is outside the unwind boundary. A provider/panic destructor never
    // runs on the caller and cannot silently release the startup slot.
    let mut resources = Box::<Resources>::default();
    resources.request = operation
        .initial
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        #[cfg(test)]
        operation.hooks.at(tests::Point::OwnerStarted);
        run_startup(&operation, &mut resources);
        if matches!(operation.state.load(Ordering::Acquire), READY | PUBLISHING) {
            while operation.state.load(Ordering::Acquire) == READY
                || operation.state.load(Ordering::Acquire) == PUBLISHING
            {
                if accepted.recv().is_err() {
                    operation.held(StartReason::Abandoned);
                    break;
                }
            }
        }
        if matches!(
            operation.state.load(Ordering::Acquire),
            NOT_STARTED | ACCEPTED
        ) {
            let retained = operation
                .retained
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let retired = operation
                .retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            drop(retained);
            drop(retired);
            drop(std::mem::take(&mut *resources));
            operation.clean.store(true, Ordering::Release);
        }
    }));
    if let Err(payload) = result {
        *operation.reason.lock().unwrap_or_else(|e| e.into_inner()) = StartReason::Panic;
        operation.state.store(HELD, Ordering::Release);
        operation.notify();
        resources.panic = Some(payload);
        operation.clean.store(false, Ordering::Release);
    }
    if !operation.clean.load(Ordering::Acquire) {
        // This sole owner reaches this write exactly once. Readers and caller
        // never write the parking cell; it cannot replace older custody.
        *operation.parked.lock().unwrap_or_else(|e| e.into_inner()) = Some(resources);
    }
}

fn run_startup(operation: &Arc<Operation>, resources: &mut Resources) {
    if operation.state.load(Ordering::Acquire) != PENDING {
        operation.not_started(StartReason::Abandoned);
        return;
    }
    #[cfg(test)]
    operation.hooks.at(tests::Point::BeforePrepare);
    let request = resources.request.as_ref().expect("owned request");
    resources.provider = Some((request.factory)());
    let provider = resources.provider.as_ref().expect("prepared provider");
    if !provider.probe().ready() {
        operation.not_started(StartReason::ProbeUnavailable);
        return;
    }
    resources.prepared = match provider.prepare_start(
        &request.workspace,
        &request.config_path,
        &request.digest,
        &request.content,
        &request.env,
    ) {
        Ok(prepared) => Some(prepared),
        Err(_) => {
            operation.not_started(StartReason::Preparation);
            return;
        }
    };
    let prepared = resources.prepared.as_mut().expect("prepared command");
    let session = DevcontainerSession {
        provider: provider.clone(),
        handle: prepared.handle.clone(),
    };
    *operation.retained.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
    let bytes = vec![0; STDERR_LIMIT + 1].into_boxed_slice();
    #[cfg(test)]
    operation.hooks.at(tests::Point::BeforeSpawnClaim);
    if super::verify_config_digest(&request.config_path, &request.digest).is_err() {
        operation.not_started(StartReason::Preparation);
        return;
    }
    if Instant::now() >= operation.deadline
        || operation
            .state
            .compare_exchange(PENDING, SPAWNING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        operation.not_started(StartReason::Deadline);
        return;
    }
    #[cfg(test)]
    {
        operation.hooks.at(tests::Point::SpawnClaimed);
        operation.hooks.spawn_calls.fetch_add(1, Ordering::AcqRel);
    }
    // A caller timeout during OS spawn is Held. Only an actual spawn Err can
    // subsequently establish no child; no post-spawn outcome falls back.
    resources.child = match prepared.command.spawn() {
        Ok(child) => Some(child),
        Err(_) => {
            operation.not_started(StartReason::Spawn);
            return;
        }
    };
    // Cancellation during the admitted spawn cannot be overwritten by its
    // completion. A successful child remains retained under Held.
    operation
        .state
        .compare_exchange(SPAWNING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
        .unwrap_or_else(|held| held);
    #[cfg(test)]
    operation.hooks.at(tests::Point::Spawned);
    let Some(pipe) = resources.child.as_mut().expect("owned child").stderr.take() else {
        operation.held(StartReason::ReaderUnavailable);
        return;
    };
    resources.reader = match spawn_reader(operation.clone(), pipe, bytes, session) {
        Ok(reader) => Some(reader),
        Err(_) => {
            operation.held(StartReason::ReaderUnavailable);
            return;
        }
    };
    let status = wait_child(
        operation,
        resources.child.as_mut().expect("owned child"),
        &mut resources.wait_uncertain,
    );
    let status = match status {
        Ok(status) => status,
        Err(_) => {
            resources.wait_uncertain = true;
            operation.held(StartReason::Wait);
            return;
        }
    };
    if !status.success() {
        // Disposition is already held before any inherited-pipe join. Only
        // notification waits for EOF/diagnostics; the caller deadline still
        // changes HELD_DRAINING to HELD and returns without joining anything.
        operation
            .diagnostic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exit_code = status.code();
        let mut reason = operation.reason.lock().unwrap_or_else(|e| e.into_inner());
        if operation
            .state
            .compare_exchange(RUNNING, HELD_DRAINING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            *reason = StartReason::Nonzero;
        }
    }
    resources.read = match resources.reader.take().expect("retained reader").join() {
        Ok(read) => Some(read),
        Err(payload) => {
            resources.panic = Some(payload);
            operation.held(StartReason::Panic);
            return;
        }
    };
    let read = resources.read.as_ref().expect("reader result");
    if let Some(reason) = read.failure {
        operation.held(reason);
        return;
    }
    if Instant::now() >= operation.deadline {
        operation.held(StartReason::Deadline);
        return;
    }
    if !status.success() {
        operation.held(StartReason::Nonzero);
        return;
    }
    #[cfg(test)]
    operation.hooks.at(tests::Point::BeforeReady);
    if operation
        .state
        .compare_exchange(RUNNING, READY, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        operation.notify();
    }
}

fn spawn_reader(
    operation: Arc<Operation>,
    pipe: ChildStderr,
    bytes: Box<[u8]>,
    session: DevcontainerSession,
) -> std::io::Result<JoinHandle<ReadCompletion>> {
    #[cfg(test)]
    if operation.hooks.reader_failure.load(Ordering::Acquire) {
        return Err(std::io::Error::other("owned fixture reader launch failure"));
    }
    std::thread::Builder::new()
        .name("devcontainer-stderr".into())
        .spawn(move || read_stderr(operation, pipe, bytes, session))
}

fn wait_child(
    _operation: &Operation,
    child: &mut Child,
    uncertain: &mut bool,
) -> std::io::Result<std::process::ExitStatus> {
    if *uncertain {
        return Err(std::io::Error::other(
            "startup wait identity already uncertain",
        ));
    }
    // A wait error or unwind revokes even a future accidental repeat.
    *uncertain = true;
    #[cfg(test)]
    {
        _operation.hooks.wait_calls.fetch_add(1, Ordering::AcqRel);
        if _operation.hooks.wait_failure.load(Ordering::Acquire)
            || _operation
                .hooks
                .wait_unknown_failure
                .load(Ordering::Acquire)
        {
            return Err(std::io::Error::other("owned fixture wait failure"));
        }
    }
    #[cfg(test)]
    _operation
        .hooks
        .native_wait_calls
        .fetch_add(1, Ordering::AcqRel);
    #[expect(
        clippy::disallowed_methods,
        reason = "single retained off-compositor startup owner; caller never waits on child"
    )]
    let result = child.wait();
    if result.is_ok() {
        *uncertain = false;
    }
    result
}

fn read_stderr(
    operation: Arc<Operation>,
    mut pipe: ChildStderr,
    bytes: Box<[u8]>,
    session: DevcontainerSession,
) -> ReadCompletion {
    let mut completion = ReadCompletion {
        bytes,
        len: 0,
        failure: None,
        _panic: None,
        _session: session,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        #[cfg(test)]
        {
            operation.hooks.at(tests::Point::ReaderStarted);
            if operation.hooks.read_failure.load(Ordering::Acquire) {
                completion.failure = Some(StartReason::Read);
                return;
            }
        }
        loop {
            match pipe.read(&mut completion.bytes[completion.len..]) {
                Ok(0) => break,
                Ok(n) => {
                    completion.len += n;
                    operation
                        .diagnostic
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .first_line(&completion.bytes[..completion.len]);
                    if completion.len > STDERR_LIMIT {
                        completion.failure = Some(StartReason::Overflow);
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    completion.failure = Some(StartReason::Read);
                    break;
                }
            }
        }
    }));
    if let Err(payload) = result {
        completion._panic = Some(payload);
        completion.failure = Some(StartReason::Panic);
    }
    if let Some(reason) = completion.failure {
        operation.held(reason);
    }
    completion
}

#[must_use = "a startup success must be published or retained as an abandoned operation"]
pub(crate) struct PendingSession {
    operation: Arc<Operation>,
}
impl PendingSession {
    pub(crate) fn publish(self, worktree: &str) -> Result<(), StartFailure> {
        #[cfg(test)]
        if let Some(sessions) = self.operation.test_sessions.clone() {
            return self.publish_into(worktree, &sessions);
        }
        self.publish_into(worktree, super::sessions())
    }

    fn publish_into(self, worktree: &str, sessions: &SessionMap) -> Result<(), StartFailure> {
        let operation = &self.operation;
        let key = worktree.to_owned();
        let session = operation
            .retained
            .try_lock()
            .map_err(|_| StartFailure::held(StartReason::Publication))?
            .as_ref()
            .ok_or(StartFailure::held(StartReason::Publication))?
            .clone();
        let mut retired = operation
            .retired
            .try_lock()
            .map_err(|_| StartFailure::held(StartReason::Publication))?;
        if retired.is_some() {
            return Err(StartFailure::held(StartReason::Publication));
        }
        let mut map = sessions
            .try_lock()
            .map_err(|_| StartFailure::held(StartReason::Publication))?;
        map.try_reserve(1)
            .map_err(|_| StartFailure::held(StartReason::Publication))?;
        if Instant::now() >= operation.deadline
            || operation
                .state
                .compare_exchange(READY, PUBLISHING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(StartFailure::held(StartReason::Deadline));
        }
        let mut publication = PublicationGuard {
            operation,
            accepted: false,
        };
        // The pre-admitted empty cell receives displaced custody directly.
        // No local old session, fallible transfer or lock acquisition follows.
        *retired = map.insert(key, session);
        #[cfg(test)]
        assert!(
            !operation.hooks.panic_after_insert.load(Ordering::Acquire),
            "owned fixture immediate post-insert unwind"
        );
        drop(map);
        drop(retired);
        operation.state.store(ACCEPTED, Ordering::Release);
        publication.accepted = true;
        operation.notify();
        Ok(())
    }
}
impl Drop for PendingSession {
    fn drop(&mut self) {
        self.operation.held(StartReason::Abandoned);
    }
}

struct PublicationGuard<'a> {
    operation: &'a Operation,
    accepted: bool,
}
impl Drop for PublicationGuard<'_> {
    fn drop(&mut self) {
        if !self.accepted {
            *self
                .operation
                .reason
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = StartReason::Publication;
            self.operation.state.store(HELD, Ordering::Release);
            self.operation.notify();
        }
    }
}

pub(super) fn start(inputs: StartInputs<'_>) -> Result<PendingSession, StartFailure> {
    #[cfg(test)]
    if let Some((controller, timeout)) = tests::override_for(inputs.workspace) {
        return controller.start(inputs, timeout);
    }
    static CONTROLLER: OnceLock<StartupController> = OnceLock::new();
    CONTROLLER
        .get_or_init(|| StartupController::new(Arc::new(super::provider)))
        .start(inputs, super::START_TIMEOUT)
}

#[cfg(test)]
#[path = "platform/devcontainer_startup_tests.rs"]
pub(crate) mod tests;
