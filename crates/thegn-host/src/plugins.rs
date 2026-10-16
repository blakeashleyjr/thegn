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
use std::sync::{Arc, Mutex};
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
    ) -> Result<(), tokio_mpsc::error::SendError<PluginMsg>> {
        match message {
            PluginMsg::Event { plugin, event } => {
                self.inbox.push(plugin, event);
                Ok(())
            }
            message => self.lifecycle.send(message),
        }
    }
}

impl PluginRx {
    pub(crate) fn try_recv(&mut self) -> Result<PluginMsg, tokio_mpsc::error::TryRecvError> {
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

    pub(crate) fn pop_rejected(&self) -> Option<inbox::RejectedRequest> {
        self.inbox.pop_rejected()
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
    Event { plugin: String, event: SessionEvent },
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OneShotStart {
    Started,
    Busy,
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

type Sessions = Arc<Mutex<BTreeMap<String, ResidentSession>>>;
type Disabled = Arc<Mutex<std::collections::BTreeSet<String>>>;

#[derive(Clone, Default)]
struct OneShotGate(Arc<Mutex<BTreeSet<String>>>);

struct OneShotTicket {
    gate: OneShotGate,
    plugin: String,
}

impl OneShotGate {
    fn try_begin(&self, plugin: &str) -> Option<OneShotTicket> {
        let mut in_flight = lock(&self.0);
        if !in_flight.insert(plugin.to_string()) {
            return None;
        }
        Some(OneShotTicket {
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
    stop: Arc<AtomicBool>,
    supervisor: ResidentSupervisor,
    sessions: Sessions,
    disabled: Disabled,
    tx: PluginTx,
    waker: TerminalWaker,
    one_shot_gate: OneShotGate,
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
    let stop = Arc::new(AtomicBool::new(false));
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
    let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
    let one_shot_gate = OneShotGate::default();
    let host = PluginsHost {
        stop: stop.clone(),
        supervisor: supervisor.clone(),
        sessions: sessions.clone(),
        disabled: disabled.clone(),
        tx: tx.clone(),
        waker: waker.clone(),
        one_shot_gate,
    };
    let spawn = std::thread::Builder::new()
        .name("thegn-plugins".into())
        .spawn(move || {
            // Utility: resident plugin contributions render as visible UI content.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            setup_and_schedule(
                supervisor, specs, config_dir, sessions, disabled, stop, tx, waker,
            )
        });
    if let Err(e) = spawn {
        tracing::warn!(target: "thegn::plugin", error = %e, "plugin host thread failed to start");
    }
    host
}

impl PluginsHost {
    /// Schedule a resident plugin respawn after `delay` on its own thread
    /// (never on the loop). The outcome lands as [`PluginMsg::Respawned`].
    pub(crate) fn respawn(&self, plugin: LoadedPlugin, delay: Duration) {
        let id = plugin.spec.manifest.id.as_str().to_string();
        let stop = self.stop.clone();
        let sessions = self.sessions.clone();
        let supervisor = self.supervisor.clone();
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        let spawn = std::thread::Builder::new()
            .name("thegn-plugin-respawn".into())
            .spawn(move || {
                // Utility: restores visibly-missing plugin content.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                std::thread::sleep(delay);
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let writer = match spawn_session(&supervisor, &plugin, &tx, &waker) {
                    Ok(session) => {
                        let w = session.writer();
                        if let Err(session) = admit_unless_stopped(&sessions, &stop, id.clone(), session) {
                            session.kill();
                            return;
                        }
                        Some(w)
                    }
                    Err(e) => {
                        tracing::warn!(target: "thegn::plugin", plugin = %id, error = %e, "respawn failed");
                        None
                    }
                };
                // best-effort: the loop may be shutting down.
                let _ = tx.send(PluginMsg::Respawned { plugin: id, writer });
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            });
        if let Err(e) = spawn {
            tracing::warn!(target: "thegn::plugin", error = %e, "respawn thread failed to start");
        }
    }

    /// Mark a plugin disabled-until-reload: the scheduler stops polling it.
    /// Run a one-shot plugin once, now, off-loop (a palette action was
    /// invoked). The result lands as [`PluginMsg::OneShot`] like a scheduled
    /// run.
    pub(crate) fn run_one_shot(&self, plugin: LoadedPlugin) -> OneShotStart {
        let id = plugin.spec.manifest.id.as_str().to_string();
        let Some(ticket) = self.one_shot_gate.try_begin(&id) else {
            return OneShotStart::Busy;
        };
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        let spawn = std::thread::Builder::new()
            .name("thegn-plugin-once".into())
            .spawn(move || {
                // Utility: the user just invoked this one-shot and is waiting on its result.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let id = plugin.spec.manifest.id.as_str().to_string();
                let run = thegn_svc::plugin::spawn_ndjson(
                    &plugin.spec.command,
                    &plugin.spec.env,
                    plugin.effective_cwd().as_deref(),
                    plugin.spec.run_timeout(),
                );
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
        if let Some(session) = lock(&self.sessions).get(plugin) {
            session.kill();
        }
    }

    /// Best-effort `deactivate` + kill on every resident session. Idempotent.
    pub(crate) fn shutdown(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
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

/// Validate the complete plugin set before setup is allowed to start any
/// plugin process. Any enabled-spec problem rejects the whole set; run
/// `thegn plugin check` for the diagnostics.
fn admitted_plugins(
    cfg: &thegn_core::config::Config,
    config_dir: &std::path::Path,
) -> Result<Vec<LoadedPlugin>, Vec<thegn_svc::plugin::loader::SpecProblem>> {
    let (loaded, broken) = thegn_svc::plugin::discover(cfg, config_dir);
    let problems = thegn_svc::plugin::loader::check_loaded(&loaded, broken);
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(loaded)
}

/// Spawn one resident session whose events are tagged with the plugin id,
/// queued on the loop channel and waker-pulsed.
fn spawn_session(
    supervisor: &ResidentSupervisor,
    plugin: &LoadedPlugin,
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
        &plugin.spec.env,
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
            });
        }
    }
    sched
}

#[allow(clippy::too_many_arguments)]
fn setup_and_schedule(
    supervisor: ResidentSupervisor,
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    sessions: Sessions,
    disabled: Disabled,
    stop: Arc<AtomicBool>,
    tx: PluginTx,
    waker: TerminalWaker,
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
    );
}

#[allow(clippy::too_many_arguments)]
fn setup_and_schedule_with_waker(
    supervisor: ResidentSupervisor,
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    sessions: Sessions,
    disabled: Disabled,
    stop: Arc<AtomicBool>,
    tx: PluginTx,
    waker: Option<TerminalWaker>,
) {
    // `discover` reads `cfg.plugins` + the plugins directory; hand it the
    // owned specs on a default config rather than cloning the whole config
    // across the thread boundary.
    let cfg = thegn_core::config::Config {
        plugins: specs,
        ..Default::default()
    };
    let discovered = match admitted_plugins(&cfg, &config_dir) {
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
    let (discovered, refusals) = partition_native_trust(discovered);
    for problem in &refusals {
        tracing::warn!(target: "thegn::plugin", error = %problem, "native plugin refused without explicit trust");
    }
    let mut loaded: Vec<LoadedState> = Vec::new();
    let now = Instant::now();
    for plugin in discovered {
        let id = plugin.spec.manifest.id.as_str().to_string();
        if !plugin.spec.enabled {
            tracing::debug!(target: "thegn::plugin", plugin = %id, "disabled in config; skipped");
            continue;
        }
        let negotiated = match thegn_svc::plugin::loader::negotiate(&plugin.spec) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(target: "thegn::plugin", plugin = %id, error = %e, "negotiate failed; plugin skipped");
                continue;
            }
        };
        let writer = match plugin.spec.mode {
            PluginMode::Resident => match spawn_session(
                &supervisor,
                &plugin,
                &tx,
                waker.as_ref().expect("runtime setup has a terminal waker"),
            ) {
                Ok(session) => {
                    let w = session.writer();
                    if let Err(session) =
                        admit_unless_stopped(&sessions, &stop, id.clone(), session)
                    {
                        session.kill();
                        None
                    } else {
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
        loaded.push(LoadedState {
            plugin,
            negotiated,
            writer,
        });
    }
    let mut sched = build_schedule(&loaded, now);

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
        if initial_run_ids.contains(st.plugin.spec.manifest.id.as_str()) {
            initial_runs.push(InitialRun {
                plugin: st.plugin.spec.manifest.id.as_str().to_string(),
                command: st.plugin.spec.command.clone(),
                env: st.plugin.spec.env.clone(),
                cwd: st.plugin.effective_cwd(),
                timeout: st.plugin.spec.run_timeout(),
            });
        }
    }

    // best-effort: the loop may already be tearing down.
    let _ = tx.send(PluginMsg::Loaded(loaded));
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
        if stop.load(Ordering::SeqCst) {
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
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        for entry in &mut sched {
            if entry.next > now || lock(&disabled).contains(&entry.plugin) {
                continue;
            }
            entry.next = now + entry.interval;
            match entry.mode {
                PluginMode::Resident => {
                    // Fetch the writer at fire time so a respawned session's
                    // fresh writer is picked up; a dead/missing session skips.
                    let writer = lock(&sessions).get(&entry.plugin).map(|s| s.writer());
                    if let Some(w) = writer {
                        // best-effort: a mid-exit session errors; Exit handles it.
                        let _ = w.notify(
                            PluginCallback::Render,
                            serde_json::json!({
                                "surface": entry.surface,
                                "contribution": entry.contribution,
                            }),
                        );
                    }
                }
                PluginMode::OneShot => {
                    let run = thegn_svc::plugin::spawn_ndjson(
                        &entry.command,
                        &entry.env,
                        entry.cwd.as_deref(),
                        entry.timeout,
                    );
                    // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    let _ = tx.send(PluginMsg::OneShot {
                        plugin: entry.plugin.clone(),
                        run,
                    });
                    if let Some(waker) = &waker {
                        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                    }
                }
            }
        }
        std::thread::sleep(granularity);
    }
}

fn partition_native_trust(
    plugins: Vec<LoadedPlugin>,
) -> (
    Vec<LoadedPlugin>,
    Vec<thegn_svc::plugin::loader::SpecProblem>,
) {
    let mut admitted = Vec::new();
    let mut refused = Vec::new();
    for plugin in plugins {
        if let Some(problem) = thegn_svc::plugin::loader::native_trust_refusal(&plugin) {
            refused.push(problem);
        } else {
            admitted.push(plugin);
        }
    }
    (admitted, refused)
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
    use thegn_core::plugin_api::{ContributionId, MAX_PLUGIN_TIMEOUT_SECS, PluginId, PluginSpec};

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

    #[test]
    fn restart_backoff_is_1_5_25_then_disabled() {
        assert_eq!(restart_delay(0), Some(Duration::from_secs(1)));
        assert_eq!(restart_delay(1), Some(Duration::from_secs(5)));
        assert_eq!(restart_delay(2), Some(Duration::from_secs(25)));
        assert_eq!(restart_delay(3), None);
        assert_eq!(restart_delay(17), None);
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
        assert!(gate.try_begin("p").is_none());
        let other = gate.try_begin("q").unwrap();
        drop(first);
        assert!(gate.try_begin("p").is_some());
        drop(other);
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
            stop: Arc::new(AtomicBool::new(false)),
            supervisor,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            disabled: Arc::new(Mutex::new(Default::default())),
            tx,
            waker,
            one_shot_gate: OneShotGate::default(),
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
    fn partition_native_trust_drops_only_untrusted_gated_plugins() {
        let mut refused = capability_spec("refused", "run:git");
        refused.mode = PluginMode::OneShot;
        let mut trusted = capability_spec("trusted", "run:git");
        trusted.mode = PluginMode::OneShot;
        trusted.trusted_native = true;
        let hello = example_spec();
        let plugins = [refused, trusted, hello]
            .into_iter()
            .map(|spec| LoadedPlugin {
                spec,
                dir: None,
                trusted_native_ignored: false,
            })
            .collect();

        let (kept, refused) = partition_native_trust(plugins);

        assert_eq!(kept.len(), 2);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].plugin, "refused");
        let states = kept
            .into_iter()
            .map(|plugin| {
                let negotiated = thegn_svc::plugin::loader::negotiate(&plugin.spec)
                    .expect("kept plugin negotiates");
                LoadedState {
                    plugin,
                    negotiated,
                    writer: None,
                }
            })
            .collect::<Vec<_>>();
        let sched = build_schedule(&states, Instant::now());
        let initial = initial_run_ids(&states, &sched);
        assert!(sched.iter().all(|entry| entry.plugin != "refused"));
        assert!(!initial.contains("refused"));
        assert!(initial.contains("trusted"));
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
        let stop = Arc::new(AtomicBool::new(false));
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
        );

        assert!(matches!(rx.try_recv().unwrap(), PluginMsg::Loaded(loaded) if loaded.is_empty()));
        assert!(lock(&sessions).is_empty());
        assert!(!marker.exists());
        assert!(rx.try_recv().is_err());
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
        let stop = Arc::new(AtomicBool::new(false));
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
}
