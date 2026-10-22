//! Off-loop plugin runtime host: discovery, resident sessions, interval polls.
//!
//! Everything here runs on background `std::thread`s — plugin discovery is fs
//! I/O and plugin processes are subprocesses, so per the 0%-idle contract none
//! of it may touch the event loop. Producers send [`PluginMsg`] on the loop's
//! tokio channel **and pulse the [`TerminalWaker`]**, exactly like the metrics
//! supervisor; the loop-side state machine lives in
//! [`crate::handlers::plugins`] and applies the messages at drain time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;
use thegn_core::plugin_api::{
    CadenceHint, ContributionId, NegotiatedManifest, PluginCallback, PluginMode, PluginSpec,
    SurfaceId,
};
use thegn_svc::plugin::{
    LoadedPlugin, PluginError, PluginRun, ResidentSession, ResidentSupervisor, SessionEvent,
    SessionWriter,
};
use tokio::sync::mpsc as tokio_mpsc;

pub(crate) mod inbox;
pub(crate) mod respawn;
use inbox::PluginInbox;

#[derive(Clone)]
pub(crate) struct PluginTx {
    lifecycle: tokio_mpsc::UnboundedSender<PluginMsg>,
    inbox: Arc<PluginInbox>,
}

pub(crate) struct PluginRx {
    lifecycle: tokio_mpsc::UnboundedReceiver<PluginMsg>,
    inbox: Arc<PluginInbox>,
}

pub(crate) fn plugin_channel() -> (PluginTx, PluginRx) {
    let (lifecycle_tx, lifecycle_rx) = tokio_mpsc::unbounded_channel();
    let inbox = Arc::new(PluginInbox::default());
    (
        PluginTx {
            lifecycle: lifecycle_tx,
            inbox: inbox.clone(),
        },
        PluginRx {
            lifecycle: lifecycle_rx,
            inbox,
        },
    )
}

impl PluginTx {
    pub(crate) fn send(
        &self,
        message: PluginMsg,
    ) -> Result<(), Box<tokio_mpsc::error::SendError<PluginMsg>>> {
        match message {
            PluginMsg::Event { plugin, event } => {
                self.inbox.push(plugin, event);
                Ok(())
            }
            message => self.lifecycle.send(message).map_err(Box::new),
        }
    }
}

impl PluginRx {
    pub(crate) fn try_recv(&mut self) -> Result<PluginMsg, tokio_mpsc::error::TryRecvError> {
        if let Some((plugin, event)) = self.inbox.pop_exit() {
            return Ok(PluginMsg::Event { plugin, event });
        }
        match self.lifecycle.try_recv() {
            Ok(message) => Ok(message),
            Err(tokio_mpsc::error::TryRecvError::Disconnected) => self
                .inbox
                .pop()
                .map(|(plugin, event)| PluginMsg::Event { plugin, event })
                .ok_or(tokio_mpsc::error::TryRecvError::Disconnected),
            Err(tokio_mpsc::error::TryRecvError::Empty) => self
                .inbox
                .pop()
                .map(|(plugin, event)| PluginMsg::Event { plugin, event })
                .ok_or(tokio_mpsc::error::TryRecvError::Empty),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lifecycle.is_empty() && self.inbox.is_empty()
    }

    pub(crate) fn pop_rejected_if_lifecycle_empty(&self) -> Option<inbox::RejectedRequest> {
        if self.lifecycle.is_empty() {
            self.inbox.pop_rejected()
        } else {
            None
        }
    }

    pub(crate) fn inbox(&self) -> Arc<PluginInbox> {
        self.inbox.clone()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lifecycle.len() + self.inbox.len()
    }

    #[cfg(test)]
    pub(crate) fn blocking_recv(&mut self) -> Option<PluginMsg> {
        match self.lifecycle.blocking_recv() {
            Some(message) => Some(message),
            None => self
                .inbox
                .pop()
                .map(|(plugin, event)| PluginMsg::Event { plugin, event }),
        }
    }
}

/// What the off-loop plugin host reports back to the event loop.
pub(crate) enum PluginMsg {
    /// Discovery finished: every enabled plugin that negotiated cleanly, in
    /// discovery order. Sent exactly once per `spawn_plugins_host`.
    Loaded(Vec<LoadedState>),
    /// Enabled plugins refused because declared protocol grants require
    /// explicit trust for their unsandboxed native execution.
    Refused(Vec<thegn_svc::plugin::loader::SpecProblem>),
    /// One resident session's stdout produced a line (or the session exited).
    Event {
        plugin: String,
        event: SessionEvent,
    },
    /// One completed `spawn_ndjson` run of a one-shot plugin.
    OneShot {
        plugin: String,
        run: Result<PluginRun, PluginError>,
    },
    /// A backoff-scheduled restart finished; `writer` is `None` when the
    /// respawn itself failed (the loop counts it like another crash).
    Respawned {
        plugin: String,
        writer: Option<SessionWriter>,
    },
    RespawnWorkerStartFailed {
        plugin: String,
    },
    RetryRespawn {
        plugin: LoadedPlugin,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OneShotStart {
    Started,
    Busy,
    Overloaded,
    Failed,
}

/// One discovered + negotiated plugin, ready for the loop to build state from.
pub(crate) struct LoadedState {
    pub plugin: LoadedPlugin,
    pub negotiated: NegotiatedManifest,
    /// `Some` for resident plugins whose session spawned; `None` for one-shot
    /// plugins and for residents that failed to start.
    pub writer: Option<SessionWriter>,
}

/// Restart backoff: 1s / 5s / 25s, then give up (max 3 restarts). Pure so the
/// schedule is unit-testable.
pub(crate) fn restart_delay(restarts: u32) -> Option<Duration> {
    match restarts {
        0 => Some(Duration::from_secs(1)),
        1 => Some(Duration::from_secs(5)),
        2 => Some(Duration::from_secs(25)),
        _ => None,
    }
}

fn report_host_start_failure(tx: &PluginTx, waker: &TerminalWaker, error: &std::io::Error) {
    if let Err(send_error) = tx.send(PluginMsg::Loaded(Vec::new())) {
        tracing::debug!(target: "thegn::plugin", %send_error, "plugin load message consumer is gone");
    }
    if let Err(send_error) = tx.send(PluginMsg::Refused(vec![
        thegn_svc::plugin::loader::SpecProblem {
            plugin: "plugins".into(),
            problem: format!("plugin host thread failed to start: {error}; no plugin is running"),
        },
    ])) {
        tracing::debug!(target: "thegn::plugin", %send_error, "plugin start failure consumer is gone");
    }
    if let Err(wake_error) = waker.wake() {
        tracing::debug!(target: "thegn::plugin", %wake_error, "plugin start failure waker pulse failed");
    }
}

fn report_poll_start_failure(
    failed: &mut BTreeSet<String>,
    plugin: &str,
    error: &std::io::Error,
    tx: &PluginTx,
    waker: Option<&TerminalWaker>,
) -> bool {
    if !failed.insert(plugin.to_string()) {
        return false;
    }
    if let Err(send_error) = tx.send(PluginMsg::Refused(vec![
        thegn_svc::plugin::loader::SpecProblem {
            plugin: plugin.into(),
            problem: format!("scheduled run failed to start: {error}"),
        },
    ])) {
        tracing::debug!(target: "thegn::plugin", %send_error, "scheduled run failure consumer is gone");
    }
    if let Some(waker) = waker
        && let Err(wake_error) = waker.wake()
    {
        tracing::debug!(target: "thegn::plugin", %wake_error, "scheduled run failure waker pulse failed");
    }
    true
}

type Sessions = Arc<Mutex<BTreeMap<String, ResidentSession>>>;
type Disabled = Arc<Mutex<std::collections::BTreeSet<String>>>;

const RESPAWN_IDLE_WAIT: Duration = Duration::from_secs(60);

struct RespawnState<P> {
    queue: respawn::RespawnQueue<P>,
    worker: bool,
    closed: bool,
}

struct RespawnTimers<P> {
    state: Mutex<RespawnState<P>>,
    cv: Condvar,
}

impl<P> Default for RespawnTimers<P> {
    fn default() -> Self {
        Self {
            state: Mutex::new(RespawnState {
                queue: respawn::RespawnQueue::default(),
                worker: false,
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }
}

impl<P> RespawnTimers<P> {
    fn cancel(&self, id: &str) {
        lock(&self.state).queue.cancel(id);
        self.cv.notify_all();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.queue.clear();
        self.cv.notify_all();
    }
}

fn run_respawn_worker<P>(
    timers: &RespawnTimers<P>,
    stop: &StopSignal,
    mut fire: impl FnMut(String, P),
) {
    let mut state = lock(&timers.state);
    loop {
        if stop.is_stopped() || state.closed {
            state.queue.clear();
            state.worker = false;
            return;
        }
        if let Some((id, payload)) = state.queue.pop_due(Instant::now()) {
            drop(state);
            fire(id, payload);
            state = lock(&timers.state);
            continue;
        }
        let wait = state
            .queue
            .next_wait(Instant::now())
            .unwrap_or(RESPAWN_IDLE_WAIT);
        state = timers
            .cv
            .wait_timeout(state, wait)
            .unwrap_or_else(|error| error.into_inner())
            .0;
    }
}

struct RespawnCtx {
    stop: Arc<StopSignal>,
    sessions: Sessions,
    disabled: Disabled,
    supervisor: ResidentSupervisor,
    tx: PluginTx,
    waker: TerminalWaker,
    state_anchor: PathBuf,
}

fn respawn_now(plugin: LoadedPlugin, ctx: &RespawnCtx) {
    let id = plugin.spec.manifest.id.as_str().to_string();
    let writer = match thegn_svc::plugin::resolve_launch_env(&plugin, &ctx.state_anchor) {
        Ok(env) => match spawn_session(&ctx.supervisor, &plugin, &env, &ctx.tx, &ctx.waker) {
            Ok(session) => {
                let writer = session.writer();
                if let Err(session) =
                    admit_unless_stopped(&ctx.sessions, &ctx.stop.flag, id.clone(), session)
                {
                    session.kill();
                    return;
                }
                let disabled = lock(&ctx.disabled).contains(&id);
                if disabled {
                    if let Some(session) = lock(&ctx.sessions).remove(&id) {
                        session.kill();
                    }
                    return;
                }
                Some(writer)
            }
            Err(error) => {
                tracing::warn!(target: "thegn::plugin", plugin = %id, error = %error, "respawn failed");
                None
            }
        },
        Err(problem) => {
            if let Err(error) = ctx.tx.send(PluginMsg::Refused(vec![problem])) {
                tracing::debug!(target: "thegn::plugin", %error, "refusal message consumer is gone");
            }
            if let Err(error) = ctx.waker.wake() {
                tracing::debug!(target: "thegn::plugin", %error, "refusal message waker pulse failed");
            }
            None
        }
    };
    let admitted = writer.is_some();
    if let Err(error) = ctx.tx.send(PluginMsg::Respawned {
        plugin: id.clone(),
        writer,
    }) {
        tracing::debug!(target: "thegn::plugin", plugin = %id, %error, "plugin loop is gone before respawn publish; closing the respawned session");
        if admitted && let Some(session) = lock(&ctx.sessions).remove(&id) {
            session.kill();
        }
        return;
    }
    // best-effort: the loop may be shutting down.
    if let Err(error) = ctx.waker.wake() {
        tracing::debug!(target: "thegn::plugin", %error, "respawn message waker pulse failed");
    }
}

fn fire_if_enabled(disabled: &Disabled, id: &str, fire: impl FnOnce()) {
    let enabled = !lock(disabled).contains(id);
    if enabled {
        fire();
    }
}

#[derive(Default)]
struct StopSignal {
    flag: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl StopSignal {
    fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn stop(&self) -> bool {
        let was_stopped = self.flag.swap(true, Ordering::SeqCst);
        let _guard = lock(&self.lock);
        self.cv.notify_all();
        was_stopped
    }

    fn wait_timeout(&self, duration: Duration) -> bool {
        let guard = lock(&self.lock);
        let (_guard, _) = self
            .cv
            .wait_timeout_while(guard, duration, |_| !self.is_stopped())
            .unwrap_or_else(|error| error.into_inner());
        self.is_stopped()
    }
}

pub(crate) const MAX_INFLIGHT_ONE_SHOTS: usize = 4; // planner call (batch 67): global one-shot run cap; owner may override

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateRefusal {
    Busy,
    Overloaded,
}

#[derive(Clone, Default)]
struct OneShotGate(Arc<Mutex<BTreeSet<String>>>);

struct OneShotTicket {
    gate: OneShotGate,
    plugin: String,
}

impl OneShotGate {
    fn try_begin(&self, plugin: &str) -> Result<OneShotTicket, GateRefusal> {
        let mut in_flight = lock(&self.0);
        if in_flight.contains(plugin) {
            return Err(GateRefusal::Busy);
        }
        if in_flight.len() >= MAX_INFLIGHT_ONE_SHOTS {
            return Err(GateRefusal::Overloaded);
        }
        in_flight.insert(plugin.to_string());
        Ok(OneShotTicket {
            gate: self.clone(),
            plugin: plugin.to_string(),
        })
    }
}

impl Drop for OneShotTicket {
    fn drop(&mut self) {
        lock(&self.gate.0).remove(&self.plugin);
    }
}

/// Admit a live session only while shutdown has not begun. Checking under the
/// same lock used by shutdown makes insertion atomic with the final drain.
fn admit_unless_stopped<T>(
    map: &Mutex<BTreeMap<String, T>>,
    stop: &AtomicBool,
    id: String,
    value: T,
) -> Result<(), T> {
    let mut map = lock(map);
    if stop.load(Ordering::SeqCst) {
        Err(value)
    } else {
        map.insert(id, value);
        Ok(())
    }
}

/// Handle to the off-loop plugin host: restart trigger + shutdown. Dropping it
/// (or calling [`PluginsHost::shutdown`]) best-effort-deactivates and kills
/// every resident session.
pub(crate) struct PluginsHost {
    stop: Arc<StopSignal>,
    supervisor: ResidentSupervisor,
    sessions: Sessions,
    disabled: Disabled,
    tx: PluginTx,
    waker: TerminalWaker,
    one_shot_gate: OneShotGate,
    state_anchor: PathBuf,
    respawns: Arc<RespawnTimers<LoadedPlugin>>,
    retry_runtime: tokio::runtime::Handle,
}

/// Start the plugin host. A SETUP thread does discovery (fs I/O) and spawns
/// resident sessions, then becomes the SCHEDULER servicing
/// `CadenceHint::Interval` contributions. Call this only after the first frame
/// has flushed (see the wiring in `run.rs`).
pub(crate) fn spawn_plugins_host(
    supervisor: ResidentSupervisor,
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    tx: PluginTx,
    waker: TerminalWaker,
) -> PluginsHost {
    let stop = Arc::new(StopSignal::default());
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
    let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
    let one_shot_gate = OneShotGate::default();
    let respawns = Arc::new(RespawnTimers::default());
    let state_anchor = thegn_core::util::xdg_state_home().join("thegn");
    let host = PluginsHost {
        stop: stop.clone(),
        supervisor: supervisor.clone(),
        sessions: sessions.clone(),
        disabled: disabled.clone(),
        tx: tx.clone(),
        waker: waker.clone(),
        one_shot_gate: one_shot_gate.clone(),
        state_anchor: state_anchor.clone(),
        respawns,
        retry_runtime: tokio::runtime::Handle::current(),
    };
    let spawn = std::thread::Builder::new()
        .name("thegn-plugins".into())
        .spawn(move || {
            // Utility: resident plugin contributions render as visible UI content.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            setup_and_schedule(
                supervisor,
                specs,
                config_dir,
                sessions,
                disabled,
                stop,
                tx,
                waker,
                one_shot_gate.clone(),
                state_anchor,
            )
        });
    if let Err(e) = spawn {
        tracing::warn!(target: "thegn::plugin", error = %e, "plugin host thread failed to start");
        report_host_start_failure(&host.tx, &host.waker, &e);
        host.stop.stop();
    }
    host
}

impl PluginsHost {
    fn respawn_worker_failed(&self, id: &str, error: &std::io::Error) {
        {
            let mut state = lock(&self.respawns.state);
            state.queue.cancel(id);
            state.worker = false;
            self.respawns.cv.notify_all();
        }
        if let Err(send_error) = self.tx.send(PluginMsg::Refused(vec![
            thegn_svc::plugin::loader::SpecProblem {
                plugin: id.into(),
                problem: format!("restart worker failed to start: {error}"),
            },
        ])) {
            tracing::debug!(target: "thegn::plugin", plugin = %id, %send_error, "restart failure consumer is gone");
        }
        if let Err(send_error) = self
            .tx
            .send(PluginMsg::RespawnWorkerStartFailed { plugin: id.into() })
        {
            tracing::debug!(target: "thegn::plugin", plugin = %id, %send_error, "restart failure consumer is gone");
        }
        if let Err(wake_error) = self.waker.wake() {
            tracing::debug!(target: "thegn::plugin", plugin = %id, %wake_error, "restart failure waker pulse failed");
        }
    }

    pub(crate) fn retry_respawn_after_worker_start_failure(
        &self,
        plugin: LoadedPlugin,
        delay: Duration,
    ) {
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        self.retry_runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(send_error) = tx.send(PluginMsg::RetryRespawn { plugin }) {
                tracing::debug!(target: "thegn::plugin", %send_error, "delayed restart consumer is gone");
            }
            if let Err(wake_error) = waker.wake() {
                tracing::debug!(target: "thegn::plugin", %wake_error, "delayed restart waker pulse failed");
            }
        });
    }

    /// Schedule a resident plugin respawn after `delay` on the shared timer
    /// worker (never on the loop). The outcome lands as [`PluginMsg::Respawned`].
    pub(crate) fn respawn(&self, plugin: LoadedPlugin, delay: Duration) {
        let id = plugin.spec.manifest.id.as_str().to_string();
        if lock(&self.disabled).contains(&id) || self.stop.is_stopped() {
            tracing::debug!(target: "thegn::plugin", plugin = %id, "ignoring respawn for disabled or stopped plugin");
            return;
        }
        let mut state = lock(&self.respawns.state);
        if state.closed || self.stop.is_stopped() || lock(&self.disabled).contains(&id) {
            tracing::debug!(target: "thegn::plugin", plugin = %id, "ignoring respawn for disabled or stopped plugin");
            return;
        }
        state
            .queue
            .schedule(id.clone(), Instant::now() + delay, plugin);
        self.respawns.cv.notify_all();
        if state.worker {
            return;
        }
        state.worker = true;
        drop(state);

        let timers = self.respawns.clone();
        let stop = self.stop.clone();
        let disabled = self.disabled.clone();
        let ctx = RespawnCtx {
            stop: self.stop.clone(),
            sessions: self.sessions.clone(),
            disabled: disabled.clone(),
            supervisor: self.supervisor.clone(),
            tx: self.tx.clone(),
            waker: self.waker.clone(),
            state_anchor: self.state_anchor.clone(),
        };
        if let Err(error) = thegn_core::thread_name::try_spawn_named("thegn-plug-resp", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            run_respawn_worker(&timers, &stop, move |id, plugin| {
                fire_if_enabled(&disabled, &id, || respawn_now(plugin, &ctx));
            });
        }) {
            tracing::warn!(target: "thegn::plugin", error = %error, "respawn worker failed to start");
            self.respawn_worker_failed(&id, &error);
        }
    }

    /// Mark a plugin disabled-until-reload: the scheduler stops polling it.
    /// Run a one-shot plugin once, now, off-loop (a palette action was
    /// invoked). The result lands as [`PluginMsg::OneShot`] like a scheduled
    /// run.
    pub(crate) fn run_one_shot(&self, plugin: LoadedPlugin) -> OneShotStart {
        let id = plugin.spec.manifest.id.as_str().to_string();
        let ticket = match self.one_shot_gate.try_begin(&id) {
            Ok(ticket) => ticket,
            Err(GateRefusal::Busy) => return OneShotStart::Busy,
            Err(GateRefusal::Overloaded) => return OneShotStart::Overloaded,
        };
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        let state_anchor = self.state_anchor.clone();
        let spawn = std::thread::Builder::new()
            .name("thegn-plugin-once".into())
            .spawn(move || {
                // Utility: the user just invoked this one-shot and is waiting on its result.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let id = plugin.spec.manifest.id.as_str().to_string();
                let run = match thegn_svc::plugin::resolve_launch_env(&plugin, &state_anchor) {
                    Ok(env) => thegn_svc::plugin::spawn_ndjson(
                        &plugin.spec.command,
                        &env,
                        plugin.effective_cwd().as_deref(),
                        plugin.spec.run_timeout(),
                    ),
                    Err(problem) => {
                        drop(ticket);
                        if let Err(error) = tx.send(PluginMsg::Refused(vec![problem])) {
                            tracing::debug!(target: "thegn::plugin", %error, "refusal message consumer is gone");
                        }
                        if let Err(error) = waker.wake() {
                            tracing::debug!(target: "thegn::plugin", %error, "refusal message waker pulse failed");
                        }
                        return;
                    }
                };
                drop(ticket);
                // best-effort: the loop may be shutting down.
                let _ = tx.send(PluginMsg::OneShot { plugin: id, run });
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            });
        match spawn {
            Ok(_) => OneShotStart::Started,
            Err(e) => {
                tracing::warn!(target: "thegn::plugin", error = %e, "one-shot thread failed to start");
                OneShotStart::Failed
            }
        }
    }

    pub(crate) fn set_disabled(&self, plugin: &str) {
        lock(&self.disabled).insert(plugin.to_string());
        self.respawns.cancel(plugin);
        if let Some(session) = lock(&self.sessions).get(plugin) {
            session.kill();
        }
    }

    /// Best-effort `deactivate` + kill on every resident session. Idempotent.
    pub(crate) fn shutdown(&self) {
        self.respawns.close();
        if self.stop.stop() {
            return;
        }
        let sessions = std::mem::take(&mut *lock(&self.sessions));
        for (id, session) in sessions {
            // Admission only: the supervisor owns termination and reaping.
            let _ = session
                .writer()
                .notify(PluginCallback::Deactivate, serde_json::json!({}));
            session.kill();
            tracing::debug!(target: "thegn::plugin", plugin = %id, "session close requested; awaiting owned lifecycle receipt");
        }
    }
}

impl Drop for PluginsHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Test helper that projects the production admission result to owned plugins.
#[cfg(test)]
fn admitted_plugins(
    cfg: &thegn_core::config::Config,
    config_dir: &std::path::Path,
) -> Result<Vec<LoadedPlugin>, Vec<thegn_svc::plugin::loader::SpecProblem>> {
    thegn_svc::plugin::loader::admit(cfg, config_dir)
        .startable()
        .map(|plugins| {
            plugins
                .into_iter()
                .map(|(plugin, _)| plugin.clone())
                .collect()
        })
}

/// Spawn one resident session whose events are tagged with the plugin id,
/// queued on the loop channel and waker-pulsed.
fn spawn_session(
    supervisor: &ResidentSupervisor,
    plugin: &LoadedPlugin,
    env: &BTreeMap<String, String>,
    tx: &PluginTx,
    waker: &TerminalWaker,
) -> Result<ResidentSession, PluginError> {
    let id = plugin.spec.manifest.id.as_str().to_string();
    let tx = tx.clone();
    let waker = waker.clone();
    supervisor.wait_for_release(&id)?;
    ResidentSession::spawn(
        supervisor,
        &id.clone(),
        &plugin.spec.command,
        env,
        plugin.effective_cwd().as_deref(),
        move |event| {
            // best-effort: the loop may be gone during shutdown.
            let (_, should_wake) = tx.inbox.push_with_wake(id.clone(), event);
            if should_wake && let Err(error) = waker.wake() {
                tracing::debug!(target: "thegn::plugin", plugin = %id, %error, "session event waker pulse failed");
            }
        },
    )
}

/// One `Interval` contribution the scheduler services.
struct SchedEntry {
    plugin: String,
    mode: PluginMode,
    /// Spawn recipe for one-shot polls.
    command: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    timeout: Duration,
    surface: Option<SurfaceId>,
    contribution: ContributionId,
    interval: Duration,
    next: Instant,
    skipped: u64,
    overloaded: u64,
}

/// Build cadence entries from negotiated contributions. Resident callbacks
/// remain contribution-scoped; a one-shot command has only plugin-level
/// result attribution, so its interval contributions share one poll at the
/// shortest requested cadence.
fn build_schedule(loaded: &[LoadedState], now: Instant) -> Vec<SchedEntry> {
    let mut sched: Vec<SchedEntry> = Vec::new();
    for state in loaded {
        let plugin = &state.plugin;
        let id = plugin.spec.manifest.id.as_str().to_string();
        for contribution in &state.negotiated.accepted_contributions {
            let CadenceHint::Interval { millis } = contribution.cadence else {
                continue;
            };
            let interval = Duration::from_millis(millis.max(1));
            if plugin.spec.mode == PluginMode::OneShot
                && let Some(entry) = sched
                    .iter_mut()
                    .find(|entry| entry.plugin == id && entry.mode == PluginMode::OneShot)
            {
                entry.interval = entry.interval.min(interval);
                continue;
            }
            sched.push(SchedEntry {
                plugin: id.clone(),
                mode: plugin.spec.mode,
                command: plugin.spec.command.clone(),
                env: plugin.spec.env.clone(),
                cwd: plugin.effective_cwd(),
                timeout: plugin.spec.run_timeout(),
                surface: contribution.surface.clone(),
                contribution: contribution.id.clone(),
                interval,
                // Due immediately: the first poll doubles as the initial view.
                next: now,
                skipped: 0,
                overloaded: 0,
            });
        }
    }
    sched
}

fn run_due(
    sched: &mut [SchedEntry],
    now: Instant,
    disabled: &Disabled,
    sessions: &Sessions,
    gate: &OneShotGate,
    spawn_poll: &mut dyn FnMut(OneShotTicket, &SchedEntry),
) {
    for entry in sched {
        if entry.next > now || lock(disabled).contains(&entry.plugin) {
            continue;
        }
        entry.next = now + entry.interval;
        match entry.mode {
            PluginMode::Resident => {
                // Fetch the writer at fire time so a respawned session's
                // fresh writer is picked up; a dead/missing session skips.
                let writer = lock(sessions)
                    .get(&entry.plugin)
                    .map(|session| session.writer());
                if let Some(writer) = writer {
                    // best-effort: a mid-exit session errors; Exit handles it.
                    let _ = writer.notify(
                        PluginCallback::Render,
                        serde_json::json!({
                            "surface": entry.surface,
                            "contribution": entry.contribution,
                        }),
                    );
                }
            }
            PluginMode::OneShot => match gate.try_begin(&entry.plugin) {
                Ok(ticket) => spawn_poll(ticket, entry),
                Err(GateRefusal::Busy) => {
                    entry.skipped += 1;
                    tracing::debug!(
                        target: "thegn::plugin",
                        plugin = %entry.plugin,
                        skipped = entry.skipped,
                        "one-shot poll skipped: previous run still in flight"
                    );
                }
                Err(GateRefusal::Overloaded) => {
                    entry.overloaded += 1;
                    tracing::debug!(
                        target: "thegn::plugin",
                        plugin = %entry.plugin,
                        overloaded = entry.overloaded,
                        cap = MAX_INFLIGHT_ONE_SHOTS,
                        "one-shot poll deferred: global in-flight cap reached"
                    );
                }
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn setup_and_schedule(
    supervisor: ResidentSupervisor,
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    sessions: Sessions,
    disabled: Disabled,
    stop: Arc<StopSignal>,
    tx: PluginTx,
    waker: TerminalWaker,
    gate: OneShotGate,
    state_anchor: PathBuf,
) {
    setup_and_schedule_with_waker(
        supervisor,
        specs,
        config_dir,
        sessions,
        disabled,
        stop,
        tx,
        Some(waker),
        gate,
        state_anchor,
    );
}

#[allow(clippy::too_many_arguments)]
fn setup_and_schedule_with_waker(
    supervisor: ResidentSupervisor,
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    sessions: Sessions,
    disabled: Disabled,
    stop: Arc<StopSignal>,
    tx: PluginTx,
    waker: Option<TerminalWaker>,
    gate: OneShotGate,
    state_anchor: PathBuf,
) {
    // `discover` reads `cfg.plugins` + the plugins directory; hand it the
    // owned specs on a default config rather than cloning the whole config
    // across the thread boundary.
    let cfg = thegn_core::config::Config {
        plugins: specs,
        ..Default::default()
    };
    let admitted = thegn_svc::plugin::loader::admit(&cfg, &config_dir);
    tracing::debug!(target: "thegn::plugin", digest = %admitted.digest, "plugin set admitted");
    let discovered = match admitted.startable() {
        Ok(discovered) => discovered,
        Err(problems) => {
            for problem in problems {
                tracing::warn!(target: "thegn::plugin", error = %problem, "plugin set rejected before startup");
            }
            // Publish an empty set for this failed admission. Crucially, this
            // return precedes the resident and one-shot process spawn paths.
            let _ = tx.send(PluginMsg::Loaded(Vec::new()));
            if let Some(waker) = &waker {
                let _ = waker.wake(); // best-effort: an input nudge must never fail this path
            }
            return;
        }
    };
    let mut refusals = admitted
        .plugins
        .iter()
        .filter_map(|entry| {
            if let thegn_svc::plugin::loader::PluginDecision::RefusedUntrusted(problem) =
                &entry.decision
            {
                Some(problem.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for problem in &refusals {
        tracing::warn!(target: "thegn::plugin", error = %problem, "native plugin refused without explicit trust");
    }
    let mut loaded: Vec<LoadedState> = Vec::new();
    let mut registered: Vec<String> = Vec::new();
    let mut resolved_envs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let now = Instant::now();
    for (plugin, negotiated) in discovered {
        let plugin = plugin.clone();
        let negotiated = negotiated.clone();
        let id = plugin.spec.manifest.id.as_str().to_string();
        let env = match thegn_svc::plugin::resolve_launch_env(&plugin, &state_anchor) {
            Ok(env) => env,
            Err(problem) => {
                tracing::warn!(target: "thegn::plugin", error = %problem, "plugin refused: env secret reference");
                refusals.push(problem);
                continue;
            }
        };
        let writer = match plugin.spec.mode {
            PluginMode::Resident => match spawn_session(
                &supervisor,
                &plugin,
                &env,
                &tx,
                waker.as_ref().expect("runtime setup has a terminal waker"),
            ) {
                Ok(session) => {
                    let w = session.writer();
                    if let Err(session) =
                        admit_unless_stopped(&sessions, &stop.flag, id.clone(), session)
                    {
                        session.kill();
                        None
                    } else {
                        registered.push(id.clone());
                        Some(w)
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "thegn::plugin", plugin = %id, error = %e, "resident session failed to spawn");
                    None
                }
            },
            PluginMode::OneShot => None,
        };
        resolved_envs.insert(id.clone(), env);
        loaded.push(LoadedState {
            plugin,
            negotiated,
            writer,
        });
    }
    let mut sched = build_schedule(&loaded, now);
    for entry in &mut sched {
        match resolved_envs.get(&entry.plugin) {
            Some(env) => entry.env.clone_from(env),
            None => {
                tracing::warn!(target: "thegn::plugin", plugin = %entry.plugin, "scheduled plugin has no resolved environment; clearing child environment");
                entry.env.clear();
            }
        }
    }

    // One-shot plugins with no interval contribution still get exactly one
    // run, so their register/update messages produce an initial view.
    struct InitialRun {
        plugin: String,
        command: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
        timeout: Duration,
    }
    let mut initial_runs: Vec<InitialRun> = Vec::new();
    let initial_run_ids = initial_run_ids(&loaded, &sched);
    for st in &loaded {
        let id = st.plugin.spec.manifest.id.as_str();
        if initial_run_ids.contains(id) {
            if let Some(env) = resolved_envs.get(id) {
                initial_runs.push(InitialRun {
                    plugin: id.to_string(),
                    command: st.plugin.spec.command.clone(),
                    env: env.clone(),
                    cwd: st.plugin.effective_cwd(),
                    timeout: st.plugin.spec.run_timeout(),
                });
            } else {
                tracing::warn!(target: "thegn::plugin", plugin = %id, "initial run skipped because plugin environment is unresolved");
            }
        }
    }
    drop(resolved_envs);

    if let Err(error) = tx.send(PluginMsg::Loaded(loaded)) {
        tracing::warn!(target: "thegn::plugin", %error, closing = registered.len(), "plugin loop is gone before startup publish; closing the sessions this setup started");
        let mut map = lock(&sessions);
        for id in &registered {
            if let Some(session) = map.remove(id) {
                session.kill();
            }
        }
        return;
    }
    if let Some(waker) = &waker {
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    }

    if !refusals.is_empty() {
        if let Err(error) = tx.send(PluginMsg::Refused(refusals)) {
            tracing::debug!(target: "thegn::plugin", %error, "refusal message consumer is gone");
        }
        if let Some(waker) = &waker
            && let Err(error) = waker.wake()
        {
            tracing::debug!(target: "thegn::plugin", %error, "refusal message waker pulse failed");
        }
    }

    for r in initial_runs {
        if stop.is_stopped() {
            return;
        }
        let run = thegn_svc::plugin::spawn_ndjson(&r.command, &r.env, r.cwd.as_deref(), r.timeout);
        // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = tx.send(PluginMsg::OneShot {
            plugin: r.plugin,
            run,
        });
        if let Some(waker) = &waker {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    }

    if sched.is_empty() {
        return;
    }
    // Sleep granularity: the smallest interval, floored at 1s so a plugin
    // asking for a 50ms cadence cannot spin this thread.
    let granularity = sched
        .iter()
        .map(|e| e.interval)
        .min()
        .unwrap_or(Duration::from_secs(1))
        .max(Duration::from_secs(1));
    let mut failed_poll_starts = BTreeSet::new();
    loop {
        if stop.is_stopped() {
            return;
        }
        run_due(
            &mut sched,
            Instant::now(),
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, entry| {
                let plugin = entry.plugin.clone();
                let command = entry.command.clone();
                let env = entry.env.clone();
                let cwd = entry.cwd.clone();
                let timeout = entry.timeout;
                let worker_tx = tx.clone();
                let worker_waker = waker.clone();
                match thegn_core::thread_name::try_spawn_named("thegn-plug-poll", move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                    let run =
                        thegn_svc::plugin::spawn_ndjson(&command, &env, cwd.as_deref(), timeout);
                    drop(ticket);
                    // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    let _ = worker_tx.send(PluginMsg::OneShot { plugin, run });
                    if let Some(waker) = &worker_waker {
                        let _ = waker.wake(); // best-effort: an input nudge must never fail the calling path
                    }
                }) {
                    Ok(_handle) => {
                        failed_poll_starts.remove(&entry.plugin);
                    }
                    Err(error) => {
                        tracing::warn!(target: "thegn::plugin", plugin = %entry.plugin, error = %error, "scheduled one-shot thread failed to start");
                        report_poll_start_failure(
                            &mut failed_poll_starts,
                            &entry.plugin,
                            &error,
                            &tx,
                            waker.as_ref(),
                        );
                    }
                }
            },
        );
        if stop.wait_timeout(granularity) {
            return;
        }
    }
}

fn initial_run_ids(loaded: &[LoadedState], sched: &[SchedEntry]) -> BTreeSet<String> {
    loaded
        .iter()
        .filter(|state| {
            state.plugin.spec.mode == PluginMode::OneShot
                && !sched
                    .iter()
                    .any(|entry| entry.plugin == state.plugin.spec.manifest.id.as_str())
        })
        .map(|state| state.plugin.spec.manifest.id.as_str().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::plugin_api::{
        ApiVersion, ContributionId, MAX_PLUGIN_TIMEOUT_SECS, PluginId, PluginSpec,
    };

    #[test]
    fn plugin_send_error_keeps_result_variant_small() {
        let (tx, rx) = plugin_channel();
        drop(rx);

        let error = tx.send(PluginMsg::Loaded(Vec::new())).unwrap_err();

        assert!(std::mem::size_of_val(&error) < 128);
    }

    #[test]
    fn start_failure_host_thread_publishes_empty_load_then_refusal() {
        let (tx, mut rx) = plugin_channel();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();

        report_host_start_failure(&tx, &waker, &std::io::Error::other("boom"));

        assert!(matches!(rx.try_recv(), Ok(PluginMsg::Loaded(loaded)) if loaded.is_empty()));
        match rx.try_recv() {
            Ok(PluginMsg::Refused(problems)) => {
                assert_eq!(problems.len(), 1);
                assert_eq!(problems[0].plugin, "plugins");
                assert!(
                    problems[0]
                        .problem
                        .contains("plugin host thread failed to start")
                );
                assert!(problems[0].problem.contains("boom"));
                assert_eq!(
                    problems[0].problem,
                    "plugin host thread failed to start: boom; no plugin is running"
                );
            }
            _ => panic!("expected host start refusal"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn start_failure_respawn_worker_reports_a_failed_restart() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, mut rx) = plugin_channel();
        let host = PluginsHost {
            stop: Arc::new(StopSignal::default()),
            supervisor: ResidentSupervisor::new(runtime.handle().clone()),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            disabled: Arc::new(Mutex::new(Default::default())),
            tx,
            waker,
            one_shot_gate: OneShotGate::default(),
            state_anchor: PathBuf::new(),
            respawns: Arc::new(RespawnTimers::default()),
            retry_runtime: runtime.handle().clone(),
        };
        let mut spec = example_spec();
        spec.manifest.id = PluginId::new("p");
        let plugin = LoadedPlugin {
            spec,
            dir: None,
            trusted_native_ignored: false,
        };
        {
            let mut state = lock(&host.respawns.state);
            state
                .queue
                .schedule("p".into(), Instant::now() + Duration::from_secs(60), plugin);
            state.worker = true;
        }

        host.respawn_worker_failed("p", &std::io::Error::other("boom"));

        {
            let state = lock(&host.respawns.state);
            assert!(state.queue.is_empty());
            assert!(!state.worker);
        }
        let mut saw_refusal = false;
        let mut saw_start_failure = false;
        let mut message_order = Vec::new();
        for _ in 0..8 {
            match rx.try_recv() {
                Ok(PluginMsg::Refused(problems)) => {
                    assert_eq!(problems.len(), 1);
                    assert_eq!(problems[0].plugin, "p");
                    assert_eq!(problems[0].problem, "restart worker failed to start: boom");
                    saw_refusal = true;
                    message_order.push("refused");
                }
                Ok(PluginMsg::RespawnWorkerStartFailed { plugin }) => {
                    assert_eq!(plugin, "p");
                    saw_start_failure = true;
                    message_order.push("start-failed");
                }
                Err(_) => break,
                Ok(_) => panic!("unexpected plugin message"),
            }
        }
        assert!(saw_refusal);
        assert!(saw_start_failure);
        assert_eq!(message_order, ["refused", "start-failed"]);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn start_failure_respawn_worker_retry_waits_for_backoff() {
        let runtime = tokio::runtime::Handle::current();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, mut rx) = plugin_channel();
        let host = PluginsHost {
            stop: Arc::new(StopSignal::default()),
            supervisor: ResidentSupervisor::new(runtime.clone()),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            disabled: Arc::new(Mutex::new(Default::default())),
            tx,
            waker,
            one_shot_gate: OneShotGate::default(),
            state_anchor: PathBuf::new(),
            respawns: Arc::new(RespawnTimers::default()),
            retry_runtime: runtime,
        };
        let mut spec = example_spec();
        spec.manifest.id = PluginId::new("p");
        let plugin = LoadedPlugin {
            spec,
            dir: None,
            trusted_native_ignored: false,
        };

        host.retry_respawn_after_worker_start_failure(plugin, Duration::from_secs(5));
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());

        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(matches!(rx.try_recv(), Ok(PluginMsg::RetryRespawn { .. })));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn start_failure_scheduled_poll_reports_once_until_it_starts() {
        let (tx, mut rx) = plugin_channel();
        let mut failed = BTreeSet::new();
        let error = std::io::Error::other("boom");

        assert!(report_poll_start_failure(
            &mut failed,
            "p",
            &error,
            &tx,
            None
        ));
        assert!(!report_poll_start_failure(
            &mut failed,
            "p",
            &error,
            &tx,
            None
        ));
        match rx.try_recv() {
            Ok(PluginMsg::Refused(problems)) => {
                assert_eq!(problems.len(), 1);
                assert_eq!(problems[0].plugin, "p");
                assert_eq!(problems[0].problem, "scheduled run failed to start: boom");
            }
            _ => panic!("expected scheduled run refusal"),
        }
        assert!(rx.try_recv().is_err());

        failed.remove("p");
        assert!(report_poll_start_failure(
            &mut failed,
            "p",
            &error,
            &tx,
            None
        ));
        assert!(matches!(rx.try_recv(), Ok(PluginMsg::Refused(problems)) if problems.len() == 1));
        assert!(rx.try_recv().is_err());
    }

    fn example_spec() -> PluginSpec {
        toml::from_str(include_str!("../../../examples/plugins/hello/plugin.toml")).unwrap()
    }

    fn capability_spec(id: &str, capability: &str) -> PluginSpec {
        let mut spec = example_spec();
        spec.manifest.id = PluginId::new(id);
        let (kind, target) = capability.split_once(':').expect("kind:target fixture");
        spec.manifest.capabilities = vec![thegn_core::plugin_api::Capability::new(kind, target)];
        spec.manifest.contributions.clear();
        spec
    }

    fn loaded_state(spec: PluginSpec) -> LoadedState {
        let accepted_contributions = spec.manifest.contributions.clone();
        LoadedState {
            plugin: LoadedPlugin {
                spec,
                dir: None,
                trusted_native_ignored: false,
            },
            negotiated: NegotiatedManifest {
                accepted_contributions,
                ..Default::default()
            },
            writer: None,
        }
    }

    fn spec_with_two_intervals(mode: PluginMode) -> PluginSpec {
        let mut spec = example_spec();
        spec.manifest.id = PluginId::new("cadenced");
        spec.mode = mode;
        let mut second = spec.manifest.contributions[0].clone();
        second.id = ContributionId::new("hello.other");
        spec.manifest.contributions[0].cadence = CadenceHint::Interval { millis: 800 };
        second.cadence = CadenceHint::Interval { millis: 300 };
        spec.manifest.contributions.push(second);
        spec
    }

    fn resident_pidfile_spec(pidfile: &std::path::Path) -> PluginSpec {
        let mut spec = example_spec();
        spec.mode = PluginMode::Resident;
        spec.manifest.contributions.clear();
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            "echo $$ > \"$1\"; exec sleep 30".into(),
            "plugin".into(),
            pidfile.display().to_string(),
        ];
        spec
    }

    fn wait_for_pidfile_and_exit(pidfile: &std::path::Path) {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);
        let pid = loop {
            match std::fs::read_to_string(pidfile) {
                Ok(contents) if contents.trim().is_empty() => return,
                Ok(contents) => match contents.trim().parse::<i32>() {
                    Ok(pid) => break pid,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => panic!(
                        "pid file remained unparsable after {:?}: {contents:?}",
                        started.elapsed()
                    ),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) if Instant::now() < deadline => {
                    tracing::debug!(%error, "retrying resident pid file read");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!(
                    "could not read pid file after {:?}: {error}",
                    started.elapsed()
                ),
            }
        };
        while !crate::platform::sound_process::test_support::pid_gone(pid)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let elapsed = started.elapsed();
        assert!(
            crate::platform::sound_process::test_support::pid_gone(pid),
            "pid {pid} was still alive after {elapsed:?}"
        );
    }

    #[test]
    fn restart_backoff_is_1_5_25_then_disabled() {
        assert_eq!(restart_delay(0), Some(Duration::from_secs(1)));
        assert_eq!(restart_delay(1), Some(Duration::from_secs(5)));
        assert_eq!(restart_delay(2), Some(Duration::from_secs(25)));
        assert_eq!(restart_delay(3), None);
        assert_eq!(restart_delay(17), None);
    }

    #[test]
    fn respawn_worker_shutdown_cancels_a_pending_respawn_promptly() {
        let timers = Arc::new(RespawnTimers::<u32>::default());
        {
            let mut state = lock(&timers.state);
            state.queue.schedule("ready".into(), Instant::now(), 0);
            state
                .queue
                .schedule("late".into(), Instant::now() + Duration::from_secs(60), 1);
        }
        let stop = Arc::new(StopSignal::default());
        let (fired_tx, fired_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_timers = timers.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            run_respawn_worker(&worker_timers, &worker_stop, move |id, payload| {
                fired_tx.send((id, payload)).unwrap();
            });
            done_tx.send(()).unwrap();
        });

        assert_eq!(
            fired_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("ready".into(), 0)
        );
        timers.close();
        stop.stop();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert!(fired_rx.try_recv().is_err());
        assert!(lock(&timers.state).queue.is_empty());
    }

    #[test]
    fn respawn_worker_cancelled_entry_never_fires() {
        let timers = Arc::new(RespawnTimers::<u32>::default());
        {
            let mut state = lock(&timers.state);
            state
                .queue
                .schedule("a".into(), Instant::now() + Duration::from_millis(200), 1);
            state.queue.schedule("ready".into(), Instant::now(), 0);
        }
        let stop = Arc::new(StopSignal::default());
        let (fired_tx, fired_rx) = std::sync::mpsc::channel();
        let worker_timers = timers.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            run_respawn_worker(&worker_timers, &worker_stop, move |id, payload| {
                fired_tx.send((id, payload)).unwrap();
            });
        });

        assert_eq!(
            fired_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("ready".into(), 0)
        );
        timers.cancel("a");
        {
            let mut state = lock(&timers.state);
            state.queue.schedule("b".into(), Instant::now(), 2);
            timers.cv.notify_all();
        }
        assert_eq!(
            fired_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("b".into(), 2)
        );
        assert!(fired_rx.recv_timeout(Duration::from_millis(500)).is_err());
        timers.close();
        stop.stop();
        worker.join().unwrap();
    }

    #[test]
    fn respawn_worker_fires_a_due_entry_exactly_once() {
        let timers = Arc::new(RespawnTimers::<u32>::default());
        {
            let mut state = lock(&timers.state);
            let now = Instant::now();
            state.queue.schedule("a".into(), now, 1);
            assert!(state.queue.schedule("a".into(), now, 2));
        }
        let stop = Arc::new(StopSignal::default());
        let (fired_tx, fired_rx) = std::sync::mpsc::channel();
        let worker_timers = timers.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            run_respawn_worker(&worker_timers, &worker_stop, move |id, payload| {
                fired_tx.send((id, payload)).unwrap();
            });
        });

        assert_eq!(
            fired_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            ("a".into(), 2)
        );
        assert!(fired_rx.recv_timeout(Duration::from_millis(500)).is_err());
        timers.close();
        stop.stop();
        worker.join().unwrap();
    }

    #[test]
    fn set_disabled_does_not_wait_for_a_slow_respawn() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let disabled = Arc::new(Mutex::new(Default::default()));
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, _rx) = plugin_channel();
        let host = Arc::new(PluginsHost {
            stop: Arc::new(StopSignal::default()),
            supervisor: ResidentSupervisor::new(runtime.handle().clone()),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            disabled: disabled.clone(),
            tx,
            waker,
            one_shot_gate: OneShotGate::default(),
            state_anchor: PathBuf::new(),
            respawns: Arc::new(RespawnTimers::default()),
            retry_runtime: runtime.handle().clone(),
        });

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let slow_disabled = disabled.clone();
        let slow = std::thread::spawn(move || {
            fire_if_enabled(&slow_disabled, "slow", || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let setter_host = host.clone();
        let setter = std::thread::spawn(move || {
            setter_host.set_disabled("slow");
            done_tx.send(()).unwrap();
        });
        let disabled_promptly = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        release_tx.send(()).unwrap();
        slow.join().unwrap();
        setter.join().unwrap();
        assert!(disabled_promptly, "set_disabled waited for respawn work");
    }

    #[test]
    fn stopped_host_rejects_session_admission() {
        let sessions = Mutex::new(BTreeMap::new());
        let stop = AtomicBool::new(true);
        let result = admit_unless_stopped(&sessions, &stop, "p".into(), 7_u32);

        assert_eq!(result, Err(7));
        assert!(lock(&sessions).is_empty());
    }

    #[test]
    fn one_shot_interval_contributions_share_the_shortest_plugin_poll() {
        let now = Instant::now();
        let one_shot = build_schedule(
            &[loaded_state(spec_with_two_intervals(PluginMode::OneShot))],
            now,
        );
        assert_eq!(one_shot.len(), 1);
        assert_eq!(one_shot[0].plugin, "cadenced");
        assert_eq!(one_shot[0].interval, Duration::from_millis(300));

        let mut unbounded_timeout = spec_with_two_intervals(PluginMode::OneShot);
        unbounded_timeout.timeout_secs = u64::MAX;
        let bounded = build_schedule(&[loaded_state(unbounded_timeout)], now);
        assert_eq!(
            bounded[0].timeout,
            Duration::from_secs(MAX_PLUGIN_TIMEOUT_SECS)
        );

        let resident = build_schedule(
            &[loaded_state(spec_with_two_intervals(PluginMode::Resident))],
            now,
        );
        assert_eq!(resident.len(), 2);
        assert!(
            resident
                .iter()
                .all(|entry| entry.mode == PluginMode::Resident)
        );
    }

    #[test]
    fn one_shot_gate_admits_one_run_per_plugin() {
        let gate = OneShotGate::default();
        let first = gate.try_begin("p").unwrap();
        assert!(matches!(gate.try_begin("p"), Err(GateRefusal::Busy)));
        let other = gate.try_begin("q").unwrap();
        drop(first);
        assert!(gate.try_begin("p").is_ok());
        drop(other);
    }

    #[test]
    fn one_shot_gate_refuses_past_the_global_cap() {
        let gate = OneShotGate::default();
        let mut tickets: Vec<_> = (0..MAX_INFLIGHT_ONE_SHOTS)
            .map(|index| gate.try_begin(&format!("p{index}")).unwrap())
            .collect();

        assert!(matches!(
            gate.try_begin("next"),
            Err(GateRefusal::Overloaded)
        ));
        assert!(matches!(gate.try_begin("p0"), Err(GateRefusal::Busy)));

        drop(tickets.pop());
        assert!(gate.try_begin("next").is_ok());
    }

    #[test]
    fn sched_busy_one_shot_tick_is_skipped_and_counted() {
        let now = Instant::now();
        let mut schedule = build_schedule(
            &[loaded_state(spec_with_two_intervals(PluginMode::OneShot))],
            now,
        );
        let interval = schedule[0].interval;
        let gate = OneShotGate::default();
        let ticket = gate.try_begin("cadenced").unwrap();
        let disabled = Arc::new(Mutex::new(BTreeSet::new()));
        let sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let mut spawned = 0;

        run_due(
            &mut schedule,
            now,
            &disabled,
            &sessions,
            &gate,
            &mut |_, _| spawned += 1,
        );

        assert_eq!(spawned, 0);
        assert_eq!(schedule[0].skipped, 1);
        assert_eq!(schedule[0].overloaded, 0);
        assert_eq!(schedule[0].next, now + interval);

        drop(ticket);
        run_due(
            &mut schedule,
            now + interval,
            &disabled,
            &sessions,
            &gate,
            &mut |_, _| spawned += 1,
        );

        assert_eq!(spawned, 1);
        assert_eq!(schedule[0].skipped, 1);
        assert_eq!(schedule[0].overloaded, 0);
    }

    #[test]
    fn sched_overloaded_tick_is_counted_apart_from_busy() {
        let now = Instant::now();
        let mut schedule = build_schedule(
            &[loaded_state(spec_with_two_intervals(PluginMode::OneShot))],
            now,
        );
        let interval = schedule[0].interval;
        let gate = OneShotGate::default();
        let mut tickets: Vec<_> = (0..MAX_INFLIGHT_ONE_SHOTS)
            .map(|index| gate.try_begin(&format!("holder-{index}")).unwrap())
            .collect();
        let disabled = Arc::new(Mutex::new(BTreeSet::new()));
        let sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let mut spawned = 0;

        run_due(
            &mut schedule,
            now,
            &disabled,
            &sessions,
            &gate,
            &mut |_, _| spawned += 1,
        );

        assert_eq!(spawned, 0);
        assert_eq!(schedule[0].overloaded, 1);
        assert_eq!(schedule[0].skipped, 0);
        assert_eq!(schedule[0].next, now + interval);

        drop(tickets.pop());
        run_due(
            &mut schedule,
            now + interval,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, _| {
                spawned += 1;
                drop(ticket);
            },
        );
        assert_eq!(spawned, 1);
    }

    #[test]
    fn sched_in_flight_poll_blocks_the_next_tick() {
        let now = Instant::now();
        let mut schedule = build_schedule(
            &[loaded_state(spec_with_two_intervals(PluginMode::OneShot))],
            now,
        );
        let interval = schedule[0].interval;
        let gate = OneShotGate::default();
        let disabled = Arc::new(Mutex::new(BTreeSet::new()));
        let sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let mut held_ticket = None;
        let mut spawned = 0;

        run_due(
            &mut schedule,
            now,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, _| {
                spawned += 1;
                held_ticket = Some(ticket);
            },
        );
        run_due(
            &mut schedule,
            now + interval,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, _| {
                spawned += 1;
                drop(ticket);
            },
        );

        assert_eq!(spawned, 1);
        assert_eq!(schedule[0].skipped, 1);
        assert_eq!(schedule[0].overloaded, 0);
        drop(held_ticket.take());
        run_due(
            &mut schedule,
            now + interval * 2,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, _| {
                spawned += 1;
                drop(ticket);
            },
        );
        assert_eq!(spawned, 2);
        assert_eq!(schedule[0].skipped, 1);
        assert_eq!(schedule[0].overloaded, 0);
    }

    #[test]
    fn sched_resident_entry_without_a_session_is_a_noop() {
        let now = Instant::now();
        let mut schedule = build_schedule(
            &[loaded_state(spec_with_two_intervals(PluginMode::Resident))],
            now,
        );
        let gate = OneShotGate::default();
        let disabled = Arc::new(Mutex::new(BTreeSet::new()));
        let sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let mut spawned = 0;

        run_due(
            &mut schedule,
            now,
            &disabled,
            &sessions,
            &gate,
            &mut |_, _| spawned += 1,
        );

        assert_eq!(spawned, 0);
        assert!(schedule.iter().all(|entry| entry.skipped == 0));
    }

    #[test]
    fn sched_stop_signal_wakes_a_long_wait() {
        let stop = Arc::new(StopSignal::default());
        let worker_stop = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(worker_stop.wait_timeout(Duration::from_secs(60)))
                .unwrap();
        });

        std::thread::sleep(Duration::from_millis(50));
        assert!(!stop.stop());
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn sched_stop_signal_wait_after_stop_returns_immediately() {
        let stop = StopSignal::default();

        assert!(!stop.stop());
        assert!(stop.stop());
        assert!(stop.wait_timeout(Duration::from_secs(60)));
    }

    #[test]
    fn palette_run_is_single_flight() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(tmp.path()).unwrap();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let (tx, mut rx) = plugin_channel();
        let host = PluginsHost {
            stop: Arc::new(StopSignal::default()),
            supervisor,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            disabled: Arc::new(Mutex::new(Default::default())),
            tx,
            waker,
            one_shot_gate: OneShotGate::default(),
            state_anchor: tmp.path().join("state"),
            respawns: Arc::new(RespawnTimers::default()),
            retry_runtime: runtime.handle().clone(),
        };
        let mut spec = example_spec();
        spec.manifest.id = PluginId::new("single-flight");
        spec.command = vec!["sh".into(), "-c".into(), "sleep 0.2".into()];
        spec.timeout_secs = 2;
        let plugin = LoadedPlugin {
            spec,
            dir: Some(tmp.path().to_path_buf()),
            trusted_native_ignored: false,
        };

        assert_eq!(host.run_one_shot(plugin.clone()), OneShotStart::Started);
        assert_eq!(host.run_one_shot(plugin.clone()), OneShotStart::Busy);
        assert!(
            matches!(rx.blocking_recv(), Some(PluginMsg::OneShot { plugin, .. }) if plugin == "single-flight")
        );
        assert_eq!(host.run_one_shot(plugin), OneShotStart::Started);
        assert!(
            matches!(rx.blocking_recv(), Some(PluginMsg::OneShot { plugin, .. }) if plugin == "single-flight")
        );
    }

    #[test]
    fn run_one_shot_reports_overloaded_when_the_cap_is_full() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(tmp.path()).unwrap();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let (tx, mut rx) = plugin_channel();
        let gate = OneShotGate::default();
        let host = PluginsHost {
            stop: Arc::new(StopSignal::default()),
            supervisor,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            disabled: Arc::new(Mutex::new(Default::default())),
            tx,
            waker,
            one_shot_gate: gate.clone(),
            state_anchor: tmp.path().join("state"),
            respawns: Arc::new(RespawnTimers::default()),
            retry_runtime: runtime.handle().clone(),
        };
        let tickets: Vec<_> = (0..MAX_INFLIGHT_ONE_SHOTS)
            .map(|index| gate.try_begin(&format!("holder-{index}")).unwrap())
            .collect();
        let mut spec = example_spec();
        spec.manifest.id = PluginId::new("overloaded");
        spec.command = vec!["sh".into(), "-c".into(), "sleep 0.2".into()];
        spec.timeout_secs = 2;
        let plugin = LoadedPlugin {
            spec,
            dir: Some(tmp.path().to_path_buf()),
            trusted_native_ignored: false,
        };

        assert_eq!(host.run_one_shot(plugin), OneShotStart::Overloaded);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio_mpsc::error::TryRecvError::Empty)
        ));
        drop(tickets);
    }

    #[test]
    fn duplicate_ids_reject_the_entire_set_before_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut first = example_spec();
        first.manifest.id = PluginId::new("duplicate");
        first.command = vec!["true".into()];
        let mut second = first.clone();
        second.command = vec!["false".into()];
        let cfg = thegn_core::config::Config {
            plugins: vec![first, second],
            ..Default::default()
        };

        let problems = admitted_plugins(&cfg, tmp.path()).unwrap_err();

        assert!(problems.iter().any(|problem| {
            problem.plugin == "duplicate" && problem.problem == "duplicate plugin id"
        }));
    }

    #[test]
    fn invalid_enabled_spec_rejects_the_entire_set_before_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut valid = example_spec();
        valid.command = vec!["true".into()];
        let mut invalid = example_spec();
        invalid.manifest.id = PluginId::new("invalid-command");
        invalid.command.clear();
        let cfg = thegn_core::config::Config {
            plugins: vec![valid, invalid],
            ..Default::default()
        };

        let problems = admitted_plugins(&cfg, tmp.path()).unwrap_err();

        assert!(problems.iter().any(|problem| {
            problem.plugin == "invalid-command" && problem.problem == "empty command"
        }));
    }

    #[test]
    fn startup_publishes_empty_set_and_does_not_run_duplicate_one_shots() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];
        let mut first = example_spec();
        first.manifest.id = PluginId::new("duplicate");
        first.manifest.contributions.clear();
        first.command = command;
        let second = first.clone();
        let cfg_specs = vec![first, second];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();

        setup_and_schedule_with_waker(
            supervisor,
            cfg_specs,
            tmp.path().to_path_buf(),
            sessions.clone(),
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(matches!(rx.try_recv().unwrap(), PluginMsg::Loaded(loaded) if loaded.is_empty()));
        assert!(lock(&sessions).is_empty());
        assert!(!marker.exists());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn startup_uses_the_admitted_set_and_spawns_nothing_on_a_negotiate_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let mut invalid = example_spec();
        invalid.manifest.id = PluginId::new("incompatible-api");
        invalid.manifest.api = ApiVersion::new(9, 0, 0);
        invalid.manifest.contributions.clear();
        invalid.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();

        setup_and_schedule_with_waker(
            supervisor,
            vec![invalid],
            tmp.path().to_path_buf(),
            sessions.clone(),
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(matches!(rx.try_recv().unwrap(), PluginMsg::Loaded(loaded) if loaded.is_empty()));
        assert!(lock(&sessions).is_empty());
        assert!(!marker.exists());
        assert!(rx.try_recv().is_err());
    }

    // Startup stages: admit is covered by startup_publishes_empty_set_and_does_not_run_duplicate_one_shots
    // and startup_uses_the_admitted_set_and_spawns_nothing_on_a_negotiate_failure; env is covered by
    // startup_refuses_unset_plugin_env_without_running_one_shot. The tests below cover spawn, register,
    // and publish with real failures.
    #[test]
    fn startup_publish_failure_closes_registered_resident_sessions() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("plugin.pid");
        let spec = resident_pidfile_spec(&pidfile);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let probe = supervisor.clone();
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, rx) = plugin_channel();
        drop(rx);

        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions.clone(),
            disabled,
            stop,
            tx,
            Some(waker),
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(probe.wait_for_release("hello").is_ok());
        assert!(lock(&sessions).is_empty());
        wait_for_pidfile_and_exit(&pidfile);
    }

    #[test]
    fn respawn_publish_failure_kills_the_respawned_session() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("plugin.pid");
        let spec = resident_pidfile_spec(&pidfile);
        let plugin = admitted_plugins(
            &thegn_core::config::Config {
                plugins: vec![spec],
                ..Default::default()
            },
            tmp.path(),
        )
        .unwrap()
        .remove(0);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let probe = supervisor.clone();
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let stop = Arc::new(StopSignal::default());
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, rx) = plugin_channel();
        drop(rx);
        let ctx = RespawnCtx {
            stop,
            sessions: sessions.clone(),
            disabled: Arc::new(Mutex::new(Default::default())),
            supervisor,
            tx,
            waker,
            state_anchor: tmp.path().join("state"),
        };

        respawn_now(plugin, &ctx);

        assert!(lock(&sessions).is_empty());
        assert!(probe.wait_for_release("hello").is_ok());
        wait_for_pidfile_and_exit(&pidfile);
    }

    #[test]
    fn startup_publish_failure_runs_no_initial_one_shot() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let mut spec = example_spec();
        spec.mode = PluginMode::OneShot;
        spec.manifest.contributions.clear();
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, rx) = plugin_channel();
        drop(rx);

        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions.clone(),
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(lock(&sessions).is_empty());
        assert!(!marker.exists());
    }

    #[test]
    fn startup_publish_failure_does_not_start_the_scheduler() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let mut spec = example_spec();
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let worker_stop = stop.clone();
        let (tx, rx) = plugin_channel();
        drop(rx);
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let worker_sessions = sessions.clone();
        let worker_disabled = disabled.clone();
        let worker_tmp = tmp.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            setup_and_schedule_with_waker(
                supervisor,
                vec![spec],
                worker_tmp.clone(),
                worker_sessions,
                worker_disabled,
                worker_stop,
                tx,
                None,
                OneShotGate::default(),
                worker_tmp.join("state"),
            );
            returned_tx.send(()).unwrap();
        });

        let returned = returned_rx.recv_timeout(Duration::from_secs(5)).is_ok();
        stop.stop();
        worker.join().unwrap();
        assert!(
            returned,
            "startup stayed in the scheduler after publish failed"
        );
        assert!(lock(&sessions).is_empty());
        assert!(!marker.exists());
    }

    #[test]
    fn startup_resident_spawn_failure_leaves_no_session() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = example_spec();
        spec.mode = PluginMode::Resident;
        spec.manifest.contributions.clear();
        spec.command = vec!["/nonexistent/thegn-plugin-test".into()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, mut rx) = plugin_channel();

        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions.clone(),
            disabled,
            stop,
            tx,
            Some(waker),
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let loaded = loop {
            assert!(
                Instant::now() < deadline,
                "startup did not publish Loaded before timeout"
            );
            match rx.try_recv() {
                Ok(PluginMsg::Loaded(loaded)) => break loaded,
                Ok(_) => {}
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        // A plugin whose resident spawn failed publishes no live writer
        // (it may be absent from Loaded entirely) and leaves no session.
        assert!(loaded.iter().all(|state| state.writer.is_none()));
        assert!(lock(&sessions).is_empty());
    }

    #[test]
    fn startup_stopped_host_kills_the_resident_it_just_spawned() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("plugin.pid");
        let spec = resident_pidfile_spec(&pidfile);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let probe = supervisor.clone();
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        stop.stop();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (tx, mut rx) = plugin_channel();

        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions.clone(),
            disabled,
            stop,
            tx,
            Some(waker),
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        // Startup may publish other messages first; drain (bounded) to Loaded.
        let deadline = Instant::now() + Duration::from_secs(5);
        let loaded = loop {
            assert!(Instant::now() < deadline, "startup did not publish Loaded");
            match rx.try_recv() {
                Ok(PluginMsg::Loaded(loaded)) => break loaded,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        assert_eq!(loaded.len(), 1);
        assert!(probe.wait_for_release("hello").is_ok());
        assert!(lock(&sessions).is_empty());
        wait_for_pidfile_and_exit(&pidfile);
    }

    #[test]
    fn startup_refuses_untrusted_gated_one_shot_without_running_it() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let mut refused = capability_spec("untrusted-runner", "run:git");
        refused.mode = PluginMode::OneShot;
        refused.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];
        refused.manifest.contributions.clear();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();

        setup_and_schedule_with_waker(
            supervisor,
            vec![refused],
            tmp.path().to_path_buf(),
            sessions,
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::Loaded(loaded)
                if loaded.iter().all(|state| state.plugin.spec.manifest.id.as_str() != "untrusted-runner")
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::Refused(problems)
                if problems.iter().any(|problem| problem.plugin == "untrusted-runner")
        ));
        assert!(rx.try_recv().is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn startup_resolves_plugin_env_before_running_one_shot() {
        let _env = thegn_core::testenv::EnvGuard::set(&[("THEGN_B42_PLUGIN_SECRET", "s3cr3t")]);
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("resolved-env");
        let mut spec = example_spec();
        spec.mode = PluginMode::OneShot;
        spec.manifest.contributions.clear();
        spec.env
            .insert("X".into(), "env:THEGN_B42_PLUGIN_SECRET".into());
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            "printf %s \"$X\" > \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();
        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions,
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(matches!(rx.try_recv().unwrap(), PluginMsg::Loaded(_)));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::OneShot { plugin, run: Ok(_) } if plugin == "hello"
        ));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "s3cr3t");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn startup_state_dir_reaches_the_one_shot_plugin() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path().join("state");
        let marker = tmp.path().join("state-dir");
        let mut spec = example_spec();
        spec.mode = PluginMode::OneShot;
        spec.manifest.contributions.clear();
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            "printf %s \"$THEGN_PLUGIN_STATE_DIR\" > \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();
        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions,
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            anchor.clone(),
        );

        assert!(matches!(rx.try_recv().unwrap(), PluginMsg::Loaded(_)));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::OneShot { plugin, run: Ok(_) } if plugin == "hello"
        ));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            anchor.join("plugins/hello").to_str().unwrap()
        );
        assert!(anchor.join("plugins/hello").is_dir());
    }

    #[test]
    fn startup_state_dir_refusal_skips_only_that_plugin() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path().join("state");
        let plugins_dir = anchor.join("plugins");
        let other_dir = tmp.path().join("other");
        let refused_marker = tmp.path().join("refused-ran");
        let other_marker = tmp.path().join("other-ran");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::create_dir(&other_dir).unwrap();
        if let Err(error) =
            thegn_core::fsperm::create_symlink(&other_dir, &plugins_dir.join("hello"), true)
        {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create symlink fixture: {error}");
        }
        let mut refused = example_spec();
        refused.mode = PluginMode::OneShot;
        refused.manifest.contributions.clear();
        refused.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            refused_marker.display().to_string(),
        ];
        let mut survivor = example_spec();
        survivor.manifest.id = PluginId::new("survivor");
        survivor.mode = PluginMode::OneShot;
        survivor.manifest.contributions.clear();
        survivor.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            other_marker.display().to_string(),
        ];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();
        setup_and_schedule_with_waker(
            supervisor,
            vec![refused, survivor],
            tmp.path().to_path_buf(),
            sessions,
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            anchor,
        );

        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::Loaded(loaded)
                if loaded.iter().any(|state| state.plugin.spec.manifest.id.as_str() == "survivor")
                    && loaded.iter().all(|state| state.plugin.spec.manifest.id.as_str() != "hello")
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::Refused(problems)
                if problems.iter().any(|problem| {
                    problem.plugin == "hello" && problem.problem.contains("private state directory")
                })
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::OneShot { plugin, run: Ok(_) } if plugin == "survivor"
        ));
        assert!(!refused_marker.exists());
        assert!(other_marker.exists());
        assert!(std::fs::read_dir(&other_dir).unwrap().next().is_none());
    }

    #[test]
    fn startup_refuses_unset_plugin_env_without_running_one_shot() {
        let _env = thegn_core::testenv::EnvGuard::unset(&["THEGN_B42_PLUGIN_SECRET"]);
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("must-not-run");
        let mut spec = example_spec();
        spec.mode = PluginMode::OneShot;
        spec.manifest.contributions.clear();
        spec.env
            .insert("X".into(), "env:THEGN_B42_PLUGIN_SECRET".into());
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "plugin".into(),
            marker.display().to_string(),
        ];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let supervisor = ResidentSupervisor::new(runtime.handle().clone());
        let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(StopSignal::default());
        let (tx, mut rx) = plugin_channel();
        setup_and_schedule_with_waker(
            supervisor,
            vec![spec],
            tmp.path().to_path_buf(),
            sessions,
            disabled,
            stop,
            tx,
            None,
            OneShotGate::default(),
            tmp.path().join("state"),
        );

        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::Loaded(loaded) if loaded.is_empty()
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PluginMsg::Refused(problems)
                if problems.iter().any(|problem| {
                    problem.plugin == "hello" && problem.problem.contains("env X")
                })
        ));
        assert!(rx.try_recv().is_err());
        assert!(!marker.exists());
    }
}
