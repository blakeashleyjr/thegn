//! Off-loop plugin runtime host: discovery, resident sessions, interval polls.
//!
//! Everything here runs on background `std::thread`s — plugin discovery is fs
//! I/O and plugin processes are subprocesses, so per the 0%-idle contract none
//! of it may touch the event loop. Producers send [`PluginMsg`] on the loop's
//! tokio channel **and pulse the [`TerminalWaker`]**, exactly like the metrics
//! supervisor; the loop-side state machine lives in
//! [`crate::handlers::plugins`] and applies the messages at drain time.

use std::collections::BTreeMap;
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
    LoadedPlugin, PluginError, PluginRun, ResidentSession, SessionEvent, SessionWriter,
};
use tokio::sync::mpsc as tokio_mpsc;

/// What the off-loop plugin host reports back to the event loop.
pub(crate) enum PluginMsg {
    /// Discovery finished: every enabled plugin that negotiated cleanly, in
    /// discovery order. Sent exactly once per `spawn_plugins_host`.
    Loaded(Vec<LoadedState>),
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

/// Handle to the off-loop plugin host: restart trigger + shutdown. Dropping it
/// (or calling [`PluginsHost::shutdown`]) best-effort-deactivates and kills
/// every resident session.
pub(crate) struct PluginsHost {
    stop: Arc<AtomicBool>,
    sessions: Sessions,
    disabled: Disabled,
    tx: tokio_mpsc::UnboundedSender<PluginMsg>,
    waker: TerminalWaker,
}

/// Start the plugin host. A SETUP thread does discovery (fs I/O) and spawns
/// resident sessions, then becomes the SCHEDULER servicing
/// `CadenceHint::Interval` contributions. Call this only after the first frame
/// has flushed (see the wiring in `run.rs`).
pub(crate) fn spawn_plugins_host(
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    tx: tokio_mpsc::UnboundedSender<PluginMsg>,
    waker: TerminalWaker,
) -> PluginsHost {
    let stop = Arc::new(AtomicBool::new(false));
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
    let disabled: Disabled = Arc::new(Mutex::new(Default::default()));
    let host = PluginsHost {
        stop: stop.clone(),
        sessions: sessions.clone(),
        disabled: disabled.clone(),
        tx: tx.clone(),
        waker: waker.clone(),
    };
    let spawn = std::thread::Builder::new()
        .name("thegn-plugins".into())
        .spawn(move || setup_and_schedule(specs, config_dir, sessions, disabled, stop, tx, waker));
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
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        let spawn = std::thread::Builder::new()
            .name("thegn-plugin-respawn".into())
            .spawn(move || {
                std::thread::sleep(delay);
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let writer = match spawn_session(&plugin, &tx, &waker) {
                    Ok(session) => {
                        let w = session.writer();
                        lock(&sessions).insert(id.clone(), session);
                        Some(w)
                    }
                    Err(e) => {
                        tracing::warn!(target: "thegn::plugin", plugin = %id, error = %e, "respawn failed");
                        None
                    }
                };
                // best-effort: the loop may be shutting down.
                let _ = tx.send(PluginMsg::Respawned { plugin: id, writer });
                let _ = waker.wake();
            });
        if let Err(e) = spawn {
            tracing::warn!(target: "thegn::plugin", error = %e, "respawn thread failed to start");
        }
    }

    /// Mark a plugin disabled-until-reload: the scheduler stops polling it.
    /// Run a one-shot plugin once, now, off-loop (a palette action was
    /// invoked). The result lands as [`PluginMsg::OneShot`] like a scheduled
    /// run.
    pub(crate) fn run_one_shot(&self, plugin: LoadedPlugin) {
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        let spawn = std::thread::Builder::new()
            .name("thegn-plugin-once".into())
            .spawn(move || {
                let id = plugin.spec.manifest.id.as_str().to_string();
                let run = thegn_svc::plugin::spawn_ndjson(
                    &plugin.spec.command,
                    &plugin.spec.env,
                    plugin.effective_cwd().as_deref(),
                    std::time::Duration::from_secs(plugin.spec.timeout_secs.max(1)),
                );
                // best-effort: the loop may be shutting down.
                let _ = tx.send(PluginMsg::OneShot { plugin: id, run });
                let _ = waker.wake();
            });
        if let Err(e) = spawn {
            tracing::warn!(target: "thegn::plugin", error = %e, "one-shot thread failed to start");
        }
    }

    pub(crate) fn set_disabled(&self, plugin: &str) {
        lock(&self.disabled).insert(plugin.to_string());
    }

    /// Best-effort `deactivate` + kill on every resident session. Idempotent.
    pub(crate) fn shutdown(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let sessions = std::mem::take(&mut *lock(&self.sessions));
        for (id, session) in sessions {
            // best-effort: the session may already be dead; kill() reaps it.
            let _ = session
                .writer()
                .notify(PluginCallback::Deactivate, serde_json::json!({}));
            session.kill();
            tracing::debug!(target: "thegn::plugin", plugin = %id, "session shut down");
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

/// Spawn one resident session whose events are tagged with the plugin id,
/// queued on the loop channel and waker-pulsed.
fn spawn_session(
    plugin: &LoadedPlugin,
    tx: &tokio_mpsc::UnboundedSender<PluginMsg>,
    waker: &TerminalWaker,
) -> Result<ResidentSession, PluginError> {
    let id = plugin.spec.manifest.id.as_str().to_string();
    let tx = tx.clone();
    let waker = waker.clone();
    ResidentSession::spawn(
        &plugin.spec.command,
        &plugin.spec.env,
        plugin.effective_cwd().as_deref(),
        move |event| {
            // best-effort: the loop may be gone during shutdown.
            let _ = tx.send(PluginMsg::Event {
                plugin: id.clone(),
                event,
            });
            let _ = waker.wake();
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

#[allow(clippy::too_many_arguments)]
fn setup_and_schedule(
    specs: Vec<PluginSpec>,
    config_dir: PathBuf,
    sessions: Sessions,
    disabled: Disabled,
    stop: Arc<AtomicBool>,
    tx: tokio_mpsc::UnboundedSender<PluginMsg>,
    waker: TerminalWaker,
) {
    // `discover` reads `cfg.plugins` + the plugins directory; hand it the
    // owned specs on a default config rather than cloning the whole config
    // across the thread boundary.
    let cfg = thegn_core::config::Config {
        plugins: specs,
        ..Default::default()
    };
    let discovered = thegn_svc::plugin::discover(&cfg, &config_dir);
    let mut loaded: Vec<LoadedState> = Vec::new();
    let mut sched: Vec<SchedEntry> = Vec::new();
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
            PluginMode::Resident => match spawn_session(&plugin, &tx, &waker) {
                Ok(session) => {
                    let w = session.writer();
                    lock(&sessions).insert(id.clone(), session);
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!(target: "thegn::plugin", plugin = %id, error = %e, "resident session failed to spawn");
                    None
                }
            },
            PluginMode::OneShot => None,
        };
        for c in &negotiated.accepted_contributions {
            if let CadenceHint::Interval { millis } = c.cadence {
                sched.push(SchedEntry {
                    plugin: id.clone(),
                    mode: plugin.spec.mode,
                    command: plugin.spec.command.clone(),
                    env: plugin.spec.env.clone(),
                    cwd: plugin.effective_cwd(),
                    timeout: Duration::from_secs(plugin.spec.timeout_secs.max(1)),
                    surface: c.surface.clone(),
                    contribution: c.id.clone(),
                    interval: Duration::from_millis(millis.max(1)),
                    // Due immediately: the first poll doubles as the initial view.
                    next: now,
                });
            }
        }
        loaded.push(LoadedState {
            plugin,
            negotiated,
            writer,
        });
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
    for st in &loaded {
        if st.plugin.spec.mode == PluginMode::OneShot
            && !sched
                .iter()
                .any(|e| e.plugin == st.plugin.spec.manifest.id.as_str())
        {
            initial_runs.push(InitialRun {
                plugin: st.plugin.spec.manifest.id.as_str().to_string(),
                command: st.plugin.spec.command.clone(),
                env: st.plugin.spec.env.clone(),
                cwd: st.plugin.effective_cwd(),
                timeout: Duration::from_secs(st.plugin.spec.timeout_secs.max(1)),
            });
        }
    }

    // best-effort: the loop may already be tearing down.
    let _ = tx.send(PluginMsg::Loaded(loaded));
    let _ = waker.wake();

    for r in initial_runs {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let run = thegn_svc::plugin::spawn_ndjson(&r.command, &r.env, r.cwd.as_deref(), r.timeout);
        let _ = tx.send(PluginMsg::OneShot {
            plugin: r.plugin,
            run,
        });
        let _ = waker.wake();
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
                    let _ = tx.send(PluginMsg::OneShot {
                        plugin: entry.plugin.clone(),
                        run,
                    });
                    let _ = waker.wake();
                }
            }
        }
        std::thread::sleep(granularity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_backoff_is_1_5_25_then_disabled() {
        assert_eq!(restart_delay(0), Some(Duration::from_secs(1)));
        assert_eq!(restart_delay(1), Some(Duration::from_secs(5)));
        assert_eq!(restart_delay(2), Some(Duration::from_secs(25)));
        assert_eq!(restart_delay(3), None);
        assert_eq!(restart_delay(17), None);
    }
}
