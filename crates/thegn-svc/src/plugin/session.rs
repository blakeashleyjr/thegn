//! Bounded resident admission. Callers never own pipes or wait for children.
use super::proc::PluginError;
use futures_util::FutureExt;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thegn_core::plugin_api::{RpcMessage, RpcResponse};
use tokio::time::Instant;

pub(super) mod admission;
mod owner;
pub use admission::SessionWriter;
use admission::Shared;

pub const MAX_RESIDENT_SESSIONS: usize = 32;
pub const MAX_QUEUED_FRAMES: usize = 32;
pub const MAX_QUEUED_BYTES: usize = 2 * super::proc::MAX_LINE_BYTES;
pub const CLOSE_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFailure {
    Full,
    TooLarge,
    Closed,
}
impl std::fmt::Display for SessionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "resident admission is full",
            Self::TooLarge => "resident frame exceeds limits",
            Self::Closed => "resident session is closed",
        })
    }
}
impl std::error::Error for SessionFailure {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    WriterClosed,
    Requested,
    StdoutClosed,
    ProcessExit,
    Protocol,
    WriteFailed,
    WriteTimeout,
    CallbackPanic,
    OwnerPanic,
    Shutdown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeGuarantee {
    Unproven,
}

/// Independent facts: leader reaping never certifies descendant containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutcome {
    pub reason: CloseReason,
    pub leader_reaped: bool,
    pub code: Option<i32>,
    pub pipes_settled: bool,
    pub tree: TreeGuarantee,
    pub termination_requested: bool,
    pub errors: Vec<String>,
}
impl SessionOutcome {
    pub fn settled(&self) -> bool {
        self.leader_reaped && self.pipes_settled
    }
}
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Message(RpcMessage),
    Response(RpcResponse),
    Junk(String),
    /// Last callback. The completion receipt carries reaping/pipe facts.
    Exit {
        code: Option<i32>,
    },
}
struct Entry {
    shared: Arc<Shared>,
    task: Arc<tokio::sync::Mutex<TaskState>>,
    // Custody survives a completed task when termination or pipe settlement failed.
    held: Arc<Mutex<Option<super::platform::Process>>>,
}
struct TaskState {
    handle: Option<tokio::task::JoinHandle<()>>,
    joined: Option<Result<(), String>>,
    failures: Vec<String>,
}
struct Registry {
    closed: bool,
    entries: BTreeMap<String, Entry>,
}
struct SupervisorInner {
    #[cfg(test)]
    after_spawn: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    runtime: tokio::runtime::Handle,
    registry: Mutex<Registry>,
}
/// One application-wide registry retained across plugin reloads.
#[derive(Clone)]
pub struct ResidentSupervisor(Arc<SupervisorInner>);
#[derive(Debug)]
pub struct ShutdownReport {
    pub outcomes: Vec<(String, SessionOutcome)>,
    pub unresolved: Vec<String>,
}
impl ShutdownReport {
    pub fn is_settled(&self) -> bool {
        self.unresolved.is_empty() && self.outcomes.iter().all(|(_, outcome)| outcome.settled())
    }
}
impl ResidentSupervisor {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self(Arc::new(SupervisorInner {
            #[cfg(test)]
            after_spawn: Mutex::new(None),
            runtime,
            registry: Mutex::new(Registry {
                closed: false,
                entries: BTreeMap::new(),
            }),
        }))
    }
    /// Background setup/restart lane only. Wait for the old closing identity
    /// before replacement; Held resources continue to block the same key.
    pub fn wait_for_release(&self, key: &str) -> Result<(), PluginError> {
        let entry = {
            let registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
            if registry.closed {
                return Err(PluginError::Spawn("resident supervisor closing".into()));
            }
            registry
                .entries
                .get(key)
                .map(|entry| (entry.shared.clone(), entry.task.clone()))
        };
        let Some((shared, task)) = entry else {
            return Ok(());
        };
        let Some((_, deadline)) = shared.closing() else {
            return Err(PluginError::Spawn(
                "resident identity already running".into(),
            ));
        };
        self.0.runtime.block_on(async {
            let mut outcome = shared.outcome.subscribe();
            while outcome.borrow().is_none() {
                if !matches!(
                    tokio::time::timeout_at(deadline, outcome.changed()).await,
                    Ok(Ok(()))
                ) {
                    return Err(PluginError::Spawn(
                        "old resident cleanup remains outstanding".into(),
                    ));
                }
            }
            if !outcome
                .borrow()
                .as_ref()
                .is_some_and(SessionOutcome::settled)
            {
                return Err(PluginError::Spawn(
                    "old resident retains unsettled resources".into(),
                ));
            }
            let mut task = tokio::time::timeout_at(deadline, task.lock())
                .await
                .map_err(|_| PluginError::Spawn("old resident join outstanding".into()))?;
            if let Some(handle) = task.handle.as_mut() {
                let result = tokio::time::timeout_at(deadline, handle)
                    .await
                    .map_err(|_| PluginError::Spawn("old resident join outstanding".into()))?;
                task.joined = Some(result.map_err(|error| error.to_string()));
                task.handle = None;
            }
            if matches!(task.joined, Some(Ok(()))) {
                Ok(())
            } else {
                Err(PluginError::Spawn("old resident owner failed".into()))
            }
        })
    }
    pub fn request_shutdown(&self, deadline: Instant) {
        let entries = {
            let mut registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.closed = true;
            registry
                .entries
                .values()
                .map(|entry| entry.shared.clone())
                .collect::<Vec<_>>()
        };
        for shared in entries {
            shared.close(CloseReason::Shutdown, deadline);
        }
    }
    /// Request all sessions first, then wait concurrently against ONE deadline.
    pub async fn shutdown_until(&self, deadline: Instant) -> ShutdownReport {
        self.request_shutdown(deadline);
        let entries = {
            let registry = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry
                .entries
                .iter()
                .map(|(key, entry)| {
                    (
                        key.clone(),
                        entry.shared.outcome.subscribe(),
                        entry.task.clone(),
                        entry.shared.clone(),
                        entry.held.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let results = futures_util::future::join_all(entries.into_iter().map(
            |(key, mut outcome, task, shared, held)| async move {
                while outcome.borrow().is_none() {
                    if !matches!(
                        tokio::time::timeout_at(deadline, outcome.changed()).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                // The handle stays inside registry-owned state even if this waiter
                // is cancelled. Concurrent shutdown callers observe the same join.
                let joined = match tokio::time::timeout_at(deadline, task.lock()).await {
                    Ok(mut task) => {
                        if let Some(handle) = task.handle.as_mut() {
                            if let Ok(result) = tokio::time::timeout_at(deadline, handle).await {
                                let result = result.map_err(|error| error.to_string());
                                if let Err(error) = &result {
                                    task.failures.push(error.clone());
                                }
                                task.joined = Some(result);
                                task.handle = None;
                            }
                        }
                        if task.handle.is_none() && Instant::now() < deadline {
                            let process = held
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .take();
                            if let Some(process) = process {
                                let custody =
                                    owner::Custody::new(process, shared.clone(), held.clone());
                                shared.outcome.send_replace(None);
                                task.handle =
                                    Some(self.0.runtime.spawn(owner::retry(custody, deadline)));
                                task.joined = None;
                                if let Ok(result) = tokio::time::timeout_at(
                                    deadline,
                                    task.handle.as_mut().expect("retry owner"),
                                )
                                .await
                                {
                                    let result = result.map_err(|error| error.to_string());
                                    if let Err(error) = &result {
                                        task.failures.push(error.clone());
                                    }
                                    task.joined = Some(result);
                                    task.handle = None;
                                }
                            }
                        }
                        if task.failures.is_empty() {
                            task.joined.clone()
                        } else {
                            Some(Err(task.failures.join("; ")))
                        }
                    }
                    Err(_) => None,
                };
                let result = outcome.borrow().clone();
                (key, result, joined)
            },
        ))
        .await;
        let mut report = ShutdownReport {
            outcomes: Vec::new(),
            unresolved: Vec::new(),
        };
        for (key, result, joined) in results {
            if !matches!(joined, Some(Ok(()))) {
                report
                    .unresolved
                    .push(format!("{key}: owner join {joined:?}"));
            }
            match result {
                Some(outcome) => report.outcomes.push((key, outcome)),
                None => report
                    .unresolved
                    .push(format!("{key}: no terminal receipt")),
            }
        }
        report
    }
}
impl Drop for SupervisorInner {
    fn drop(&mut self) {
        let registry = self.registry.get_mut().unwrap_or_else(|e| e.into_inner());
        for (key, entry) in &registry.entries {
            entry.shared.close(CloseReason::Shutdown, Instant::now());
            if entry
                .shared
                .outcome
                .borrow()
                .as_ref()
                .is_none_or(|outcome| !outcome.settled())
            {
                let held = entry
                    .held
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                tracing::error!(target: "thegn::plugin", plugin = %key, held, "final supervisor release with unresolved resources; custody cannot survive application exit");
            }
        }
    }
}
pub struct ResidentSession {
    shared: Arc<Shared>,
}
impl ResidentSession {
    pub fn spawn(
        supervisor: &ResidentSupervisor,
        key: &str,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Self, PluginError> {
        let Some((program, args)) = argv.split_first() else {
            return Err(PluginError::Spawn("empty command".into()));
        };
        if key.len() > 256 {
            return Err(PluginError::Spawn("resident identity exceeds limit".into()));
        }
        let shared = Shared::new();
        let held = Arc::new(Mutex::new(None));
        let task_state = Arc::new(tokio::sync::Mutex::new(TaskState {
            handle: None,
            joined: None,
            failures: Vec::new(),
        }));
        // Acquire registration ownership BEFORE exposing the reservation.
        // Concurrent shutdown waits on this guard under its own deadline.
        let mut registration = task_state
            .clone()
            .try_lock_owned()
            .expect("new private task state");
        {
            let mut registry = supervisor
                .0
                .registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry.entries.retain(|_, entry| {
                if !entry
                    .shared
                    .outcome
                    .borrow()
                    .as_ref()
                    .is_some_and(SessionOutcome::settled)
                {
                    return true;
                }
                if let Ok(mut task) = entry.task.try_lock() {
                    if let Some(handle) = task.handle.as_mut()
                        && handle.is_finished()
                    {
                        if let Some(result) = handle.now_or_never() {
                            task.joined = Some(result.map_err(|error| error.to_string()));
                            task.handle = None;
                        }
                    }
                    return !matches!(task.joined, Some(Ok(())));
                }
                true
            });
            if registry.closed
                || registry.entries.contains_key(key)
                || registry.entries.len() >= MAX_RESIDENT_SESSIONS
            {
                return Err(PluginError::Spawn(
                    "resident supervisor closed, full, or identity still owned".into(),
                ));
            }
            registry.entries.insert(
                key.to_string(),
                Entry {
                    shared: shared.clone(),
                    task: task_state.clone(),
                    held: held.clone(),
                },
            );
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(env);
        if let Some(path) = cwd.filter(|path| path.is_dir()) {
            command.current_dir(path);
        }
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
        ] {
            command.env_remove(name);
        }
        let entered = supervisor.0.runtime.enter();
        let spawned = super::platform::Prepared::new().and_then(|prepared| {
            if shared.closing().is_some() {
                return Err(std::io::Error::other("supervisor closing"));
            }
            prepared.spawn(command)
        });
        drop(entered);
        let process = match spawned {
            Ok(process) => process,
            Err(error) => {
                supervisor
                    .0
                    .registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entries
                    .remove(key);
                return Err(PluginError::Spawn(error.to_string()));
            }
        };
        let custody = owner::Custody::new(process, shared.clone(), held);
        #[cfg(test)]
        if let Some(hook) = supervisor.0.after_spawn.lock().unwrap().take() {
            hook();
        }
        let (start, ready) = tokio::sync::oneshot::channel();
        let task = supervisor.0.runtime.spawn(async move {
            if ready.await.is_ok() {
                owner::run(custody, on_event).await;
            }
        });
        registration.handle = Some(task);
        drop(registration);
        start.send(()).map_err(|()| {
            PluginError::Spawn("lifecycle owner stopped before registration".into())
        })?;
        Ok(Self { shared })
    }
    pub fn writer(&self) -> SessionWriter {
        SessionWriter(self.shared.clone())
    }
    pub fn request_close(&self, reason: CloseReason, deadline: Instant) {
        self.shared.close(reason, deadline);
    }
    pub fn kill(&self) {
        self.request_close(CloseReason::Requested, Instant::now() + CLOSE_BUDGET);
    }
    pub fn completion(&self) -> tokio::sync::watch::Receiver<Option<SessionOutcome>> {
        self.shared.outcome.subscribe()
    }
}
impl Drop for ResidentSession {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Classify one complete NDJSON line. Junk remains diagnostic, not executable.
fn parse_line(text: &str) -> SessionEvent {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) if value.get("method").is_some() => {
            match serde_json::from_value::<RpcMessage>(value) {
                Ok(message) => SessionEvent::Message(message),
                Err(_) => SessionEvent::Junk(text.to_string()),
            }
        }
        Ok(value) if value.get("id").is_some() => {
            match serde_json::from_value::<RpcResponse>(value) {
                Ok(response) => SessionEvent::Response(response),
                Err(_) => SessionEvent::Junk(text.to_string()),
            }
        }
        _ => SessionEvent::Junk(text.to_string()),
    }
}
#[cfg(test)]
pub(crate) mod tests;
