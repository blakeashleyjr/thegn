//! In-process supervisor for ingress shares (`[share]`).
//!
//! It manages per-worktree `bore` (or future) tunnel-client children as
//! **background subprocesses** (not PTY panes). Each share runs on its own OS thread that drives
//! [`thegn_svc::share`] — spawn the client, wait for its URL, then block on
//! the child — and reports state back over a tokio mpsc channel, pulsing the
//! `TerminalWaker` exactly as the config/LSP/refresh producers do.
//!
//! The supervisor itself is pure bookkeeping (no DB, no rendering). Its event
//! persistence projection is generation-fenced; the loop re-syncs the
//! [`crate::chrome::FrameModel`] from [`ShareSupervisor::views`].

use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use termwiz::terminal::TerminalWaker;
use thegn_core::config::ShareConfig;
use thegn_core::share::build_share_spec;
use thegn_core::store::WorktreeAuxStore;
use thegn_svc::share::PublicEndpoint;
use thegn_svc::share::ShareProvider;
use tokio::sync::mpsc::UnboundedSender;

/// A status update from a share's supervisor thread back to the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareEvent {
    /// The tunnel reported its public URL.
    Up {
        worktree: String,
        port: u16,
        generation: u64,
        provider: &'static str,
        url: PublicEndpoint,
    },
    /// The tunnel client exited (clean stop, crash, or `stop()` kill).
    Down {
        worktree: String,
        port: u16,
        generation: u64,
    },
    /// Bring-up failed (no URL within the timeout, spawn error, …).
    Failed {
        worktree: String,
        port: u16,
        generation: u64,
        provider: &'static str,
        error: String,
        teardown_unverified: bool,
    },
}

pub(crate) enum SharePersist {
    MarkFailed {
        worktree: String,
        port: u16,
        generation: u64,
        provider: String,
    },
    Upsert {
        worktree: String,
        port: u16,
        generation: u64,
        provider: String,
        url: String,
    },
    DeleteIfOwned {
        worktree: String,
        port: u16,
        generation: u64,
    },
}

/// Pure projection of a supervisor event into its durable operation.
pub(crate) fn share_persist_for(ev: &ShareEvent) -> SharePersist {
    match ev {
        ShareEvent::Up {
            worktree,
            port,
            generation,
            provider,
            url,
        } => SharePersist::Upsert {
            worktree: worktree.clone(),
            port: *port,
            generation: *generation,
            provider: (*provider).to_string(),
            url: url.canonical(),
        },
        ShareEvent::Down {
            worktree,
            port,
            generation,
        } => SharePersist::DeleteIfOwned {
            worktree: worktree.clone(),
            port: *port,
            generation: *generation,
        },
        ShareEvent::Failed {
            worktree,
            port,
            generation,
            provider,
            teardown_unverified: true,
            ..
        } => SharePersist::MarkFailed {
            worktree: worktree.clone(),
            port: *port,
            generation: *generation,
            provider: (*provider).to_string(),
        },
        ShareEvent::Failed {
            worktree,
            port,
            generation,
            ..
        } => SharePersist::DeleteIfOwned {
            worktree: worktree.clone(),
            port: *port,
            generation: *generation,
        },
    }
}

pub(crate) fn apply_share_persist(db: &thegn_core::db::Db, op: SharePersist) {
    match op {
        SharePersist::MarkFailed {
            worktree,
            port,
            generation,
            provider,
        } => {
            if let Err(error) =
                db.upsert_share(&worktree, port, generation, &provider, None, "failed")
            {
                tracing::debug!(target: "thegn::db", %error, "share failure persist failed");
            }
        }
        SharePersist::Upsert {
            worktree,
            port,
            generation,
            provider,
            url,
        } => {
            if let Err(error) =
                db.upsert_share(&worktree, port, generation, &provider, Some(&url), "up")
            {
                tracing::debug!(target: "thegn::db", %error, "share persist failed");
            }
        }
        SharePersist::DeleteIfOwned {
            worktree,
            port,
            generation,
        } => match db.delete_share_generation(&worktree, port, generation) {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(target: "thegn::db", %worktree, port, generation, "stale share delete left another generation intact")
            }
            Err(error) => {
                tracing::debug!(target: "thegn::db", %error, "share delete persist failed")
            }
        },
    }
}

/// The lifecycle state of one share, as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareState {
    Starting,
    Up(PublicEndpoint),
    Failed(String),
    Stopping,
}

/// A small render-facing snapshot of one share (mirrored into `FrameModel`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShareView {
    pub worktree: String,
    pub port: u16,
    /// The canonical public URL, socket authority, or opaque ticket.
    /// `None` while starting/stopping or on failure.
    pub url: Option<String>,
    /// Typed endpoint used by actions that need to distinguish a browser URL
    /// from an opaque ticket or socket authority.
    pub endpoint: Option<PublicEndpoint>,
    pub failed: bool,
    pub stopping: bool,
    /// The failure reason when `failed` — surfaced in the panel row; without it
    /// the user saw the word "failed" and nothing else.
    pub error: Option<String>,
    /// Provider id (`bore`/`frp`/`tailscale`/`iroh`) — drives the panel glyph.
    pub provider: &'static str,
    /// Whether the share is reachable from the public internet (safety badge).
    pub public: bool,
}

impl ShareView {
    /// The reach glyph: 🌐 public, 👥 team (tailscale serve), 🔗 peer (iroh).
    pub fn reach_glyph(&self) -> char {
        if self.public {
            '\u{1f310}' // 🌐
        } else if self.provider == "iroh" {
            '\u{1f517}' // 🔗
        } else {
            '\u{1f465}' // 👥
        }
    }

    pub fn copy_value(&self) -> Option<String> {
        self.endpoint.as_ref().map(PublicEndpoint::canonical)
    }

    pub fn browser_url(&self) -> Option<&str> {
        self.endpoint.as_ref().and_then(PublicEndpoint::as_url)
    }
}

/// How to tear a sidecar-serve share down (tailscale): the sidecar container +
/// the plan whose `down_argv` removes the serve. Set by the serve thread.
#[derive(Clone)]
struct ServeTeardown {
    sidecar: String,
    serve: thegn_svc::share::ServePlan,
}

/// Cross-thread handle to one running share: lets [`ShareSupervisor::stop`] /
/// shutdown signal and tear it down. A `Process` share carries a `pid` to
/// SIGTERM; a `SidecarServe` share carries a `teardown` to run `down_argv`.
struct Shared {
    shutdown: AtomicBool,
    endpoint_conflict: AtomicBool,
    stop: Mutex<Option<thegn_svc::share::StopHandle>>,
    teardown: Mutex<Option<ServeTeardown>>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Shared {
            shutdown: AtomicBool::new(false),
            endpoint_conflict: AtomicBool::new(false),
            stop: Mutex::new(None),
            teardown: Mutex::new(None),
        })
    }
}

struct ShareInstance {
    worktree: String,
    port: u16,
    generation: u64,
    provider: &'static str,
    public: bool,
    state: ShareState,
    shared: Arc<Shared>,
    event_tx: Option<UnboundedSender<ShareEvent>>,
    event_waker: Option<TerminalWaker>,
}

impl ShareInstance {
    fn kill(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.shared.stop.lock().unwrap().as_ref() {
            match handle.stop() {
                thegn_svc::share::StopOutcome::Failed(error) => {
                    tracing::warn!(%error, worktree = %self.worktree, port = self.port, "share child termination failed");
                }
                thegn_svc::share::StopOutcome::Terminated
                | thegn_svc::share::StopOutcome::AlreadyExited => {}
            }
        }
        // Sidecar-serve teardown shells into the VPN sidecar; do it off the loop.
        if let Some(td) = self.shared.teardown.lock().unwrap().clone() {
            let tx = self.event_tx.clone();
            let waker = self.event_waker.clone();
            let worker_tx = tx.clone();
            let worker_waker = waker.clone();
            let worktree = self.worktree.clone();
            let port = self.port;
            let generation = self.generation;
            let provider = self.provider;
            let spawn = std::thread::Builder::new()
                .name("tgshare-down".into())
                .spawn(move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                    let event = match thegn_svc::share::serve_down(&td.sidecar, &td.serve) {
                        Ok(()) => ShareEvent::Down {
                            worktree,
                            port,
                            generation,
                        },
                        Err(error) => ShareEvent::Failed {
                            worktree,
                            port,
                            generation,
                            provider,
                            error: format!("{error}; stop it again to retry"),
                            teardown_unverified: true,
                        },
                    };
                    if let Some(tx) = worker_tx
                        && let Err(error) = tx.send(event)
                    {
                        tracing::debug!(%error, "share teardown acknowledgement receiver is gone");
                    }
                    if let Some(waker) = worker_waker
                        && let Err(error) = waker.wake()
                    {
                        tracing::debug!(%error, "share teardown waker is unavailable");
                    }
                });
            if let Err(error) = spawn {
                tracing::warn!(%error, "could not spawn share teardown worker");
                if let Some(tx) = tx
                    && let Err(send_error) = tx.send(ShareEvent::Failed {
                        worktree: self.worktree.clone(),
                        port: self.port,
                        generation: self.generation,
                        provider: self.provider,
                        error: format!(
                            "share: teardown unverified; could not start teardown worker: {error}; stop it again to retry"
                        ),
                        teardown_unverified: true,
                    })
                {
                    tracing::debug!(%send_error, "share teardown failure receiver is gone");
                }
                if let Some(waker) = waker
                    && let Err(wake_error) = waker.wake()
                {
                    tracing::debug!(%wake_error, "share teardown waker is unavailable");
                }
            }
        }
    }
}

/// Tracks the live ingress shares. An event-loop local, mirroring
/// [`crate::pins::PinSupervisor`].
#[derive(Default)]
pub struct ShareSupervisor {
    instances: Vec<ShareInstance>,
}

pub(crate) struct StopShares {
    pub(crate) removed: Vec<(u16, u64)>,
    pub(crate) pending: Vec<(u16, u64)>,
}

static NEXT_SHARE_GENERATION: AtomicU64 = AtomicU64::new(1);
static SHARE_PROCESS_NONCE: OnceLock<u64> = OnceLock::new();

impl ShareSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a share already exists for `(worktree, port)`.
    pub fn has(&self, worktree: &str, port: u16) -> bool {
        self.instances.iter().any(|i| {
            i.worktree == worktree && i.port == port && !matches!(i.state, ShareState::Stopping)
        })
    }

    pub fn has_other_generation(&self, worktree: &str, port: u16, generation: u64) -> bool {
        self.instances
            .iter()
            .any(|i| i.worktree == worktree && i.port == port && i.generation != generation)
    }

    /// Generation of the live, non-stopping share at `(worktree, port)`.
    pub fn generation_of(&self, worktree: &str, port: u16) -> Option<u64> {
        self.instances
            .iter()
            .find(|i| {
                i.worktree == worktree && i.port == port && !matches!(i.state, ShareState::Stopping)
            })
            .map(|i| i.generation)
    }

    /// Render-facing snapshot of all shares for `worktree` (badge + panel).
    pub fn views(&self, worktree: &str) -> Vec<ShareView> {
        self.instances
            .iter()
            .filter(|i| i.worktree == worktree)
            .map(|i| ShareView {
                worktree: i.worktree.clone(),
                port: i.port,
                url: match &i.state {
                    ShareState::Up(u) => Some(u.canonical()),
                    _ => None,
                },
                endpoint: match &i.state {
                    ShareState::Up(endpoint) => Some(endpoint.clone()),
                    _ => None,
                },
                failed: matches!(i.state, ShareState::Failed(_)),
                stopping: matches!(i.state, ShareState::Stopping),
                error: match &i.state {
                    ShareState::Failed(e) => Some(e.clone()),
                    _ => None,
                },
                provider: i.provider,
                public: i.public,
            })
            .collect()
    }

    /// Start a share for `port` on `worktree`. Returns a human error if sharing
    /// is disabled or a share already runs for this `(worktree, port)`.
    pub fn start(
        &mut self,
        cfg: &ShareConfig,
        worktree: &str,
        port: u16,
        reach: Option<thegn_core::config::ShareReach>,
        tx: &UnboundedSender<ShareEvent>,
        waker: &TerminalWaker,
    ) -> Result<(), String> {
        // A FAILED instance must not block a retry: the failed supervisor
        // thread is gone (it never emits `Down`), so without this eviction the
        // port stayed "already sharing" for the rest of the session.
        self.instances.retain(|i| {
            !(i.worktree == worktree
                && i.port == port
                && matches!(i.state, ShareState::Failed(_))
                && i.shared.teardown.lock().unwrap().is_none())
        });
        if self.has(worktree, port) {
            if self.instances.iter().any(|instance| {
                instance.worktree == worktree
                    && instance.port == port
                    && matches!(instance.state, ShareState::Failed(_))
                    && instance.shared.teardown.lock().unwrap().is_some()
            }) {
                return Err(format!(
                    "teardown of :{port} is unverified; stop it to retry"
                ));
            }
            return Err(format!("already sharing port {port}"));
        }
        let label = thegn_core::share::label_for(worktree);
        let Some(spec) = build_share_spec(cfg, &label, port, reach) else {
            return Err("sharing is disabled (set [share] provider, or that reach)".into());
        };
        let public = spec.visibility == thegn_core::config::ShareVisibility::Public;
        // Safety guard: never expose to the public internet unless opted in.
        if public && !cfg.allow_public {
            return Err(
                "public sharing is disabled (set [share] allow_public = true to enable)".into(),
            );
        }
        let provider = thegn_svc::share::for_provider(&spec).kind();
        let sequence = NEXT_SHARE_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| "share generation counter exhausted".to_string())?;
        let process_nonce = *SHARE_PROCESS_NONCE.get_or_init(|| {
            let started = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            started ^ u64::from(std::process::id()).rotate_left(17)
        });
        let generation = process_nonce.wrapping_add(sequence);
        let shared = Shared::new();
        let t_shared = shared.clone();
        let t_tx = tx.clone();
        let t_waker = waker.clone();
        let wt = worktree.to_string();
        std::thread::Builder::new()
            .name("tgshare".into())
            .spawn(move || {
                // Utility: the user started this share and is watching for its URL.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                supervise(spec, wt, port, generation, t_tx, t_waker, t_shared)
            })
            .map_err(|e| format!("could not spawn share supervisor: {e}"))?;
        self.instances.push(ShareInstance {
            worktree: worktree.to_string(),
            port,
            generation,
            provider,
            public,
            state: ShareState::Starting,
            shared,
            event_tx: Some(tx.clone()),
            event_waker: Some(waker.clone()),
        });
        Ok(())
    }

    /// Apply an event from a supervisor thread. Returns `true` if UI state
    /// changed (so the loop can flip the chrome `dirty` flag).
    pub fn on_event(&mut self, ev: ShareEvent) -> bool {
        match ev {
            ShareEvent::Up {
                worktree,
                port,
                generation,
                url,
                ..
            } => match self.find_mut(&worktree, port, generation) {
                Some(i) if matches!(i.state, ShareState::Starting) => {
                    i.state = ShareState::Up(url);
                    true
                }
                _ => {
                    tracing::debug!(%worktree, port, generation, "dropped stale share Up event");
                    false
                }
            },
            ShareEvent::Failed {
                worktree,
                port,
                generation,
                error,
                teardown_unverified,
                ..
            } => match self
                .instances
                .iter()
                .find(|i| i.worktree == worktree && i.port == port && i.generation == generation)
                .map(|i| i.state.clone())
            {
                Some(_) if teardown_unverified => {
                    let Some(i) = self.find_mut(&worktree, port, generation) else {
                        return false;
                    };
                    i.state = ShareState::Failed(error);
                    true
                }
                Some(ShareState::Starting | ShareState::Up(_)) => {
                    let Some(i) = self.find_mut(&worktree, port, generation) else {
                        return false;
                    };
                    i.state = ShareState::Failed(error);
                    true
                }
                Some(ShareState::Stopping) => {
                    self.instances.retain(|i| {
                        !(i.worktree == worktree && i.port == port && i.generation == generation)
                    });
                    true
                }
                _ => {
                    tracing::debug!(%worktree, port, generation, "dropped stale share Failed event");
                    false
                }
            },
            ShareEvent::Down {
                worktree,
                port,
                generation,
            } => {
                let before = self.instances.len();
                self.instances.retain(|i| {
                    !(i.worktree == worktree && i.port == port && i.generation == generation)
                });
                let changed = before != self.instances.len();
                if !changed {
                    tracing::debug!(%worktree, port, generation, "dropped stale share Down event");
                }
                changed
            }
        }
    }

    /// Stop shares on `worktree`: a specific `port`, or all (`None`). `removed`
    /// pairs can be deleted immediately; `pending` pairs have sidecar teardown
    /// workers whose terminal events own persistence.
    pub(crate) fn stop(&mut self, worktree: &str, port: Option<u16>) -> StopShares {
        self.stop_with(worktree, port, ShareInstance::kill)
    }

    fn stop_with(
        &mut self,
        worktree: &str,
        port: Option<u16>,
        mut kill: impl FnMut(&ShareInstance),
    ) -> StopShares {
        let mut stopped = StopShares {
            removed: Vec::new(),
            pending: Vec::new(),
        };
        // A failed instance without teardown has no worker left to await.
        self.instances.retain(|i| {
            let matched = i.worktree == worktree && port.is_none_or(|p| p == i.port);
            if matched
                && matches!(i.state, ShareState::Failed(_))
                && i.shared.teardown.lock().unwrap().is_none()
            {
                stopped.removed.push((i.port, i.generation));
                false
            } else {
                true
            }
        });
        for i in &mut self.instances {
            let matched = i.worktree == worktree && port.is_none_or(|p| p == i.port);
            if matched && !matches!(i.state, ShareState::Stopping) {
                let teardown_pending = i.shared.teardown.lock().unwrap().is_some();
                let pair = (i.port, i.generation);
                if teardown_pending {
                    stopped.pending.push(pair);
                } else {
                    stopped.removed.push(pair);
                }
                kill(i);
                // Retain this generation until its worker acknowledges teardown.
                i.state = ShareState::Stopping;
            }
        }
        stopped
    }

    /// SIGTERM every child (teardown on quit).
    pub fn shutdown_all(&mut self) {
        for i in &self.instances {
            i.kill();
        }
        self.instances.clear();
    }

    fn find_mut(
        &mut self,
        worktree: &str,
        port: u16,
        generation: u64,
    ) -> Option<&mut ShareInstance> {
        self.instances
            .iter_mut()
            .find(|i| i.worktree == worktree && i.port == port && i.generation == generation)
    }
}

/// Kill every tunnel child when the supervisor is dropped (audit run.rs:15915):
/// the only explicit teardown (`shutdown_all`) was reached solely on the
/// signal-shutdown path, so `q`/palette-quit/QuitKill leaked the tunnel
/// children as orphans (the local port stayed publicly exposed, and the next
/// launch's resurrection spawned a *second* tunnel for the same port). Dropping
/// `event_loop`'s supervisor now reaps them on every exit path, including
/// panics. `ShareInstance::kill` is idempotent, so an explicit `shutdown_all`
/// before drop is harmless.
impl Drop for ShareSupervisor {
    fn drop(&mut self) {
        for i in &self.instances {
            i.kill();
        }
    }
}

/// The per-share supervisor thread: build the plan, bring the tunnel up, report
/// its URL, then block on the child and report when it goes down.
// off-loop: runs on the per-share supervisor std::thread (see the spawn above).
fn supervise(
    spec: thegn_core::share::ShareSpec,
    worktree: String,
    port: u16,
    generation: u64,
    tx: UnboundedSender<ShareEvent>,
    waker: TerminalWaker,
    shared: Arc<Shared>,
) {
    use thegn_svc::share::{self, ShareLaunch};

    let provider = share::for_provider(&spec).kind();
    let emit = |ev: ShareEvent| {
        if let Err(error) = tx.send(ev) {
            tracing::debug!(%error, "share event receiver is gone");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(%error, "share waker is unavailable");
        }
    };
    let fail = |e: String| {
        if shared.shutdown.load(Ordering::SeqCst) {
            if let Err(error) = tx.send(ShareEvent::Down {
                worktree: worktree.clone(),
                port,
                generation,
            }) {
                tracing::debug!(%error, "share shutdown acknowledgement receiver is gone");
            }
            if let Err(error) = waker.wake() {
                tracing::debug!(%error, "share shutdown waker is unavailable");
            }
            return;
        }
        if let Err(error) = tx.send(ShareEvent::Failed {
            worktree: worktree.clone(),
            port,
            generation,
            provider,
            error: e,
            teardown_unverified: false,
        }) {
            tracing::debug!(%error, "share failure receiver is gone");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(%error, "share failure waker is unavailable");
        }
    };

    let launch = match share::for_provider(&spec).launch() {
        Ok(l) => l,
        Err(e) => return fail(e.to_string()),
    };

    match launch {
        // Process providers (bore/frp/dumbpipe): spawn the child, report its URL,
        // then block on it so we emit `Down` when it exits.
        ShareLaunch::Process(plan) => {
            let statedir = match share::share_state_dir(
                std::path::Path::new(&worktree),
                provider,
                port,
                generation,
            ) {
                Ok(path) => path,
                Err(e) => return fail(e.to_string()),
            };
            match share::start_cancellable(&plan, &statedir, spec.ready_timeout, &shared.shutdown) {
                Ok(running) => {
                    let stop_handle = running.stop_handle();
                    *shared.stop.lock().unwrap() = Some(stop_handle.clone());
                    let conflict_shared = shared.clone();
                    running.on_endpoint_conflict(Box::new(move || {
                        conflict_shared
                            .endpoint_conflict
                            .store(true, Ordering::Release);
                        if let thegn_svc::share::StopOutcome::Failed(error) = stop_handle.stop() {
                            tracing::warn!(%error, "share endpoint-conflict termination failed");
                        }
                    }));
                    if shared.shutdown.load(Ordering::SeqCst) {
                        running.stop();
                        emit(ShareEvent::Down {
                            worktree,
                            port,
                            generation,
                        });
                        return;
                    }
                    emit(ShareEvent::Up {
                        worktree: worktree.clone(),
                        port,
                        generation,
                        provider,
                        url: running.public_url.clone(),
                    });
                    running.wait();
                    *shared.stop.lock().unwrap() = None;
                    if shared.endpoint_conflict.load(Ordering::Acquire) {
                        fail(
                            "provider announced a different public endpoint; share stopped".into(),
                        );
                    } else {
                        emit(ShareEvent::Down {
                            worktree,
                            port,
                            generation,
                        });
                    }
                }
                Err(e) if e.downcast_ref::<share::ShareStartCancelled>().is_some() => {
                    emit(ShareEvent::Down {
                        worktree,
                        port,
                        generation,
                    });
                }
                Err(e) => fail(e.to_string()),
            }
        }
        // tailscale: drive `serve`/`funnel` inside the worktree's VPN sidecar.
        // There is no child to wait on; the serve persists until `stop`/shutdown
        // runs `down_argv` (held in `shared.teardown`) or the sidecar dies.
        ShareLaunch::SidecarServe(serve) => {
            // Profile-aware — the sidecar rides the container's real name.
            let sidecar = thegn_core::sandbox::vpn_sidecar_name(
                &thegn_core::sandbox::container_name_with_profile(
                    &worktree,
                    Some(&thegn_core::profile::name()),
                ),
            );
            match share::serve_up(&sidecar, &serve) {
                Ok(url) => {
                    *shared.teardown.lock().unwrap() = Some(ServeTeardown {
                        sidecar,
                        serve: serve.clone(),
                    });
                    if shared.shutdown.load(Ordering::SeqCst) {
                        match share::serve_down(
                            &shared.teardown.lock().unwrap().as_ref().unwrap().sidecar,
                            &serve,
                        ) {
                            Ok(()) => emit(ShareEvent::Down {
                                worktree,
                                port,
                                generation,
                            }),
                            Err(error) => emit(ShareEvent::Failed {
                                worktree,
                                port,
                                generation,
                                provider,
                                error: format!("{error}; stop it again to retry"),
                                teardown_unverified: true,
                            }),
                        }
                        return;
                    }
                    emit(ShareEvent::Up {
                        worktree,
                        port,
                        generation,
                        provider,
                        url,
                    });
                }
                Err(e) => fail(e.to_string()),
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn seed_failed_instance_for_test(
    supervisor: &mut ShareSupervisor,
    worktree: &str,
    port: u16,
    generation: u64,
) {
    supervisor.instances.push(ShareInstance {
        worktree: worktree.into(),
        port,
        generation,
        provider: "bore",
        public: true,
        state: ShareState::Failed("test failure".into()),
        shared: Shared::new(),
        event_tx: None,
        event_waker: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_during_bring_up_sets_the_cancellation_flag() {
        let shared = Shared::new();
        let instance = ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 41,
            provider: "bore",
            public: true,
            state: ShareState::Starting,
            shared: shared.clone(),
            event_tx: None,
            event_waker: None,
        };
        let state = tempfile::tempdir().expect("private temporary state");
        let plan = thegn_svc::share::SharePlan {
            program: if cfg!(windows) {
                "powershell.exe"
            } else {
                "sh"
            }
            .into(),
            args: if cfg!(windows) {
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    "Start-Sleep -Seconds 30".into(),
                ]
            } else {
                vec!["-c".into(), "sleep 30".into()]
            },
            env: Vec::new(),
            files: Vec::new(),
            url_rule: thegn_svc::share::UrlRule::AfterMarker {
                marker: "public: ".into(),
                scheme: "https".into(),
            },
        };
        let startup_shared = shared.clone();
        let state_path = state.path().to_path_buf();
        let starter = std::thread::spawn(move || {
            thegn_svc::share::start_cancellable(
                &plan,
                &state_path,
                std::time::Duration::from_secs(10),
                &startup_shared.shutdown,
            )
        });
        instance.kill();
        let result = starter.join().expect("startup thread");
        assert!(result.is_err_and(|error| {
            error
                .downcast_ref::<thegn_svc::share::ShareStartCancelled>()
                .is_some()
        }));
    }

    #[test]
    fn kill_with_a_reaped_child_handle_does_not_signal_a_pid() {
        let state = tempfile::tempdir().expect("private temporary state");
        let plan = thegn_svc::share::SharePlan {
            program: if cfg!(windows) {
                "powershell.exe"
            } else {
                "sh"
            }
            .into(),
            args: if cfg!(windows) {
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    "Start-Sleep -Seconds 2".into(),
                ]
            } else {
                vec!["-c".into(), "sleep 1.7".into()]
            },
            env: Vec::new(),
            files: Vec::new(),
            url_rule: thegn_svc::share::UrlRule::Fixed("http://fixed.example".into()),
        };
        let running =
            thegn_svc::share::start(&plan, state.path(), std::time::Duration::from_secs(2))
                .expect("start short-lived share");
        let handle = running.stop_handle();
        running.wait();
        let shared = Shared::new();
        *shared.stop.lock().unwrap() = Some(handle.clone());
        let instance = ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 42,
            provider: "bore",
            public: true,
            state: ShareState::Up(
                thegn_svc::share::PublicEndpoint::parse_url("http://fixed.example")
                    .expect("valid URL"),
            ),
            shared,
            event_tx: None,
            event_waker: None,
        };
        let started = std::time::Instant::now();
        instance.kill();
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(handle.stop(), thegn_svc::share::StopOutcome::AlreadyExited);
    }

    #[test]
    fn on_event_transitions_and_views() {
        let mut sup = ShareSupervisor::new();
        // Seed an instance without spawning a thread (test the bookkeeping).
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 7,
            provider: "bore",
            public: true,
            state: ShareState::Starting,
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        assert!(sup.views("/wt")[0].url.is_none());
        assert!(sup.on_event(ShareEvent::Up {
            worktree: "/wt".into(),
            port: 3000,
            generation: 7,
            provider: "bore",
            url: PublicEndpoint::parse_url("http://bore.pub:1").expect("valid URL"),
        }));
        let v = sup.views("/wt");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].port, 3000);
        assert_eq!(v[0].url.as_deref(), Some("http://bore.pub:1"));
        assert!(!v[0].failed);

        // An event for an unknown share is a no-op.
        assert!(!sup.on_event(ShareEvent::Up {
            worktree: "/other".into(),
            port: 9,
            generation: 7,
            provider: "bore",
            url: PublicEndpoint::parse_url("https://other.test").expect("valid URL"),
        }));

        // Down removes it.
        assert!(sup.on_event(ShareEvent::Down {
            worktree: "/wt".into(),
            port: 3000,
            generation: 7,
        }));
        assert!(sup.views("/wt").is_empty());
    }

    #[test]
    fn failed_event_marks_view() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 8080,
            generation: 8,
            provider: "bore",
            public: true,
            state: ShareState::Starting,
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        assert!(sup.on_event(ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 8080,
            generation: 8,
            provider: "bore",
            error: "boom".into(),
            teardown_unverified: false,
        }));
        let v = sup.views("/wt");
        assert!(v[0].failed);
        assert!(v[0].url.is_none());
    }

    #[test]
    fn unverified_teardown_keeps_a_visible_failed_share() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 443,
            generation: 9,
            provider: "tailscale",
            public: true,
            state: ShareState::Stopping,
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        let reason = "teardown unverified: still served; stop it again to retry";
        assert!(sup.on_event(ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 443,
            generation: 9,
            provider: "tailscale",
            error: reason.into(),
            teardown_unverified: true,
        }));
        assert_eq!(sup.instances.len(), 1);
        assert_eq!(sup.instances[0].state, ShareState::Failed(reason.into()));
        let views = sup.views("/wt");
        assert_eq!(views.len(), 1);
        assert!(views[0].failed);
        assert_eq!(views[0].error.as_deref(), Some(reason));
    }

    #[test]
    fn unverified_teardown_keeps_the_saved_row() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let up = ShareEvent::Up {
            worktree: "/wt".into(),
            port: 443,
            generation: 9,
            provider: "tailscale",
            url: PublicEndpoint::parse_url("https://host.ts.net").unwrap(),
        };
        apply_share_persist(&db, share_persist_for(&up));
        let failed = ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 443,
            generation: 9,
            provider: "tailscale",
            error: "teardown unverified: still served; stop it again to retry".into(),
            teardown_unverified: true,
        };
        assert!(matches!(
            share_persist_for(&failed),
            SharePersist::MarkFailed { .. }
        ));
        apply_share_persist(&db, share_persist_for(&failed));
        let rows = db.list_shares().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_port, 443);
        assert_eq!(rows[0].generation, 9);
        assert_eq!(rows[0].state, "failed");
    }

    #[test]
    fn stopping_an_unverified_share_retries_the_teardown() {
        let mut sup = ShareSupervisor::new();
        let shared = Shared::new();
        *shared.teardown.lock().unwrap() = Some(ServeTeardown {
            sidecar: "sidecar".into(),
            serve: thegn_svc::share::ServePlan {
                up_argv: vec!["tailscale".into(), "serve".into()],
                down_argv: vec!["tailscale".into(), "serve".into(), "off".into()],
                scheme: "https".into(),
                port: 443,
            },
        });
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 443,
            generation: 9,
            provider: "tailscale",
            public: true,
            state: ShareState::Failed("teardown unverified".into()),
            shared: shared.clone(),
            event_tx: None,
            event_waker: None,
        });
        let mut retries = 0;
        let stopped = sup.stop_with("/wt", Some(443), |instance| {
            assert!(instance.shared.teardown.lock().unwrap().is_some());
            retries += 1;
        });
        assert_eq!(retries, 1);
        assert!(stopped.removed.is_empty());
        assert_eq!(stopped.pending, vec![(443, 9)]);
        assert!(matches!(sup.instances[0].state, ShareState::Stopping));
    }

    #[test]
    fn stale_generation_cannot_change_or_remove_restarted_share() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 22,
            provider: "bore",
            public: true,
            state: ShareState::Up(
                PublicEndpoint::parse_url("https://new.test").expect("valid URL"),
            ),
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        assert!(!sup.on_event(ShareEvent::Up {
            worktree: "/wt".into(),
            port: 3000,
            generation: 21,
            provider: "bore",
            url: PublicEndpoint::parse_url("https://old.test").expect("valid URL"),
        }));
        assert!(!sup.on_event(ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 3000,
            generation: 21,
            provider: "bore",
            error: "late failure".into(),
            teardown_unverified: false,
        }));
        assert!(!sup.on_event(ShareEvent::Down {
            worktree: "/wt".into(),
            port: 3000,
            generation: 21
        }));
        assert_eq!(sup.views("/wt")[0].url.as_deref(), Some("https://new.test"));
    }

    #[test]
    fn matching_failure_terminates_a_live_share_view() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 22,
            provider: "bore",
            public: true,
            state: ShareState::Up(
                PublicEndpoint::parse_url("https://new.test").expect("valid URL"),
            ),
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        assert!(sup.on_event(ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 3000,
            generation: 22,
            provider: "bore",
            error: "provider announced a different public endpoint; share stopped".into(),
            teardown_unverified: false,
        }));
        let views = sup.views("/wt");
        let view = &views[0];
        assert!(view.failed);
        assert!(view.url.is_none());
    }

    #[test]
    fn stop_retains_stopping_generation_until_down_acknowledgement() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 31,
            provider: "bore",
            public: true,
            state: ShareState::Starting,
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        assert_eq!(sup.stop("/wt", Some(3000)).removed, vec![(3000, 31)]);
        assert_eq!(sup.generation_of("/wt", 3000), None);
        assert_eq!(sup.instances.len(), 1);
        assert!(matches!(sup.instances[0].state, ShareState::Stopping));
        assert!(sup.views("/wt")[0].stopping);
        assert!(sup.on_event(ShareEvent::Down {
            worktree: "/wt".into(),
            port: 3000,
            generation: 31
        }));
        assert!(sup.instances.is_empty());
    }

    #[test]
    fn failed_acknowledges_a_stopped_starting_generation() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation: 51,
            provider: "bore",
            public: true,
            state: ShareState::Starting,
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        });
        assert_eq!(sup.stop("/wt", Some(3000)).removed, vec![(3000, 51)]);
        assert!(sup.on_event(ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 3000,
            generation: 51,
            provider: "bore",
            error: "startup failed during stop".into(),
            teardown_unverified: false,
        }));
        assert!(sup.instances.is_empty());
    }

    #[test]
    fn ticket_is_copied_as_data_and_never_opened_as_a_url() {
        let endpoint = PublicEndpoint::parse_ticket("opaque-ticket_7").expect("valid ticket");
        let view = ShareView {
            worktree: "/wt".into(),
            port: 3000,
            url: Some(endpoint.canonical()),
            endpoint: Some(endpoint),
            failed: false,
            stopping: false,
            error: None,
            provider: "iroh",
            public: false,
        };
        assert_eq!(view.copy_value().as_deref(), Some("opaque-ticket_7"));
        assert!(view.browser_url().is_none());
        assert_eq!(
            thegn_svc::lsp::sanitize_for_terminal(view.copy_value().as_deref().unwrap()),
            "opaque-ticket_7"
        );
    }

    #[test]
    fn url_and_socket_endpoints_project_to_copy_and_browser_values() {
        let url = thegn_svc::share::UrlRule::Fixed("https://Example.com:443".into())
            .fixed()
            .expect("valid URL endpoint");
        let url_view = ShareView {
            worktree: "/wt".into(),
            port: 3000,
            url: Some(url.canonical()),
            endpoint: Some(url),
            failed: false,
            stopping: false,
            error: None,
            provider: "bore",
            public: true,
        };
        assert_eq!(
            url_view.copy_value().as_deref(),
            Some("https://example.com")
        );
        assert_eq!(url_view.browser_url(), Some("https://example.com"));

        let socket = thegn_svc::share::UrlRule::Fixed("bore.pub:4123".into())
            .fixed()
            .expect("valid socket endpoint");
        let socket_view = ShareView {
            worktree: "/wt".into(),
            port: 3000,
            url: Some(socket.canonical()),
            endpoint: Some(socket),
            failed: false,
            stopping: false,
            error: None,
            provider: "bore",
            public: true,
        };
        assert_eq!(socket_view.copy_value().as_deref(), Some("bore.pub:4123"));
        assert!(socket_view.browser_url().is_none());
    }

    #[test]
    fn fenced_persistence_keeps_new_up_after_old_down() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let up = ShareEvent::Up {
            worktree: "/wt".into(),
            port: 3000,
            generation: 2,
            provider: "bore",
            url: PublicEndpoint::parse_url("https://new.test").unwrap(),
        };
        apply_share_persist(&db, share_persist_for(&up));
        let old_down = ShareEvent::Down {
            worktree: "/wt".into(),
            port: 3000,
            generation: 1,
        };
        apply_share_persist(&db, share_persist_for(&old_down));
        assert_eq!(db.list_shares().unwrap()[0].generation, 2);

        let failed = ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 3000,
            generation: 2,
            provider: "bore",
            error: "provider failed".into(),
            teardown_unverified: false,
        };
        apply_share_persist(&db, share_persist_for(&failed));
        assert!(db.list_shares().unwrap().is_empty());
    }

    #[test]
    fn stop_start_storm_returns_each_generation_and_live_generation_only() {
        let mut sup = ShareSupervisor::new();
        let instance = |generation| ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            generation,
            provider: "bore",
            public: true,
            state: ShareState::Starting,
            shared: Shared::new(),
            event_tx: None,
            event_waker: None,
        };
        sup.instances.push(instance(61));
        assert_eq!(sup.stop("/wt", Some(3000)).removed, vec![(3000, 61)]);
        sup.instances.push(instance(62));
        assert_eq!(sup.generation_of("/wt", 3000), Some(62));
        assert_eq!(sup.stop("/wt", Some(3000)).removed, vec![(3000, 62)]);
        assert_eq!(sup.generation_of("/wt", 3000), None);
        assert_eq!(
            sup.instances.len(),
            2,
            "both stopped generations await terminal ack"
        );
        for generation in [61, 62] {
            assert!(sup.on_event(ShareEvent::Down {
                worktree: "/wt".into(),
                port: 3000,
                generation,
            }));
        }
        assert!(sup.instances.is_empty());
    }

    #[test]
    fn fenced_deletes_are_scoped_to_worktree_even_when_ports_match() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let first = ShareEvent::Up {
            worktree: "/wt/a".into(),
            port: 3000,
            generation: 10,
            provider: "bore",
            url: PublicEndpoint::parse_url("https://a.test").unwrap(),
        };
        let second = ShareEvent::Up {
            worktree: "/wt/b".into(),
            port: 3000,
            generation: 20,
            provider: "bore",
            url: PublicEndpoint::parse_url("https://b.test").unwrap(),
        };
        apply_share_persist(&db, share_persist_for(&first));
        apply_share_persist(&db, share_persist_for(&second));
        let down = ShareEvent::Down {
            worktree: "/wt/a".into(),
            port: 3000,
            generation: 10,
        };
        apply_share_persist(&db, share_persist_for(&down));
        let rows = db.list_shares().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].worktree, "/wt/b");
        assert_eq!(rows[0].generation, 20);
    }
}
