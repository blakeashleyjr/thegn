//! The daemon's session table — the one [`ControlApi`] implementation.
//!
//! Owns the map of live session actors, the daemon-wide event feed, and the
//! lease bookkeeping hooks (idle/busy transitions from actors land here). All
//! DB access is `spawn_blocking` (this runs on the daemon's tokio runtime;
//! there is no render loop in this process, but blocking a worker thread on
//! SQLite still starves the executor under load).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::{broadcast, mpsc, oneshot};

use thegn_core::attention::PaneAgentState;
use thegn_core::control::relay_expiry;
use thegn_core::control_wire::{EventFrame, LeaseEventKind, PairingState};
use thegn_core::db::Db;
use thegn_core::graveyard::Graveyard;
use thegn_core::store::{ControlStore, IntentStore, LeaseRow};
use thegn_svc::control::{
    AttachKind, AttachReply, BrowserCommand, ControlApi, ControlError, ControlResult, ForkSpec,
    GitFileStatus, OpenSpec, RecordSpec, RecordStatus, SessionActivityEvent, SessionInfo,
    WaitCondition, WaitOutcome,
};
use thegn_svc::git::{CliGit, CommitOps, GitBackend};

use super::session::{IdleTransition, LiveMeta, ProbeReply, SessionMeta, SessionMsg};
use super::tombstone::Tombstone;

/// One live session in the daemon's table.
pub(crate) struct SessionEntry {
    pub msg_tx: mpsc::Sender<SessionMsg>,
    pub meta: SessionMeta,
    pub live: Arc<Mutex<LiveMeta>>,
    /// The resolved source recipe used to create this live session. It is
    /// deliberately absent only on test/legacy stubs and is never persisted.
    pub recipe: Option<thegn_core::session_fork::DaemonRecipe>,
}

/// What a session id resolves to.
///
/// Every verb that used to call `entry_tx` and 404 on a miss goes through this
/// instead, because "gone" and "finished" are different answers: a supervisor
/// asking about a session that just exited wants its exit code, not a 404.
pub(crate) enum Lookup {
    Live(mpsc::Sender<SessionMsg>),
    /// Exited recently enough to still be readable.
    Dead(Box<Tombstone>),
    /// Never existed, or long gone.
    Unknown,
}

/// Shared handle to the daemon's SQLite connection (the proxy's `SharedDb`
/// pattern: one connection, short critical sections, used off-runtime via
/// `spawn_blocking`).
pub(crate) type SharedDb = Arc<Mutex<Db>>;

pub(crate) struct DaemonService {
    pub daemon_id: String,
    pub sessions: Arc<tokio::sync::Mutex<HashMap<String, SessionEntry>>>,
    /// Recently-exited sessions, kept briefly so a supervisor that polls a
    /// moment late still gets the exit code and the final screen instead of a
    /// 404. Deliberately a *separate* map from `sessions`: the idle-exit check
    /// is `!sessions.is_empty()`, and corpses in that map would keep a daemon
    /// alive forever; the lease reaper looks sessions up to send `Kill`, and a
    /// corpse has nothing to kill.
    pub tombs: Arc<tokio::sync::Mutex<Graveyard<Tombstone>>>,
    pub events: broadcast::Sender<Arc<EventFrame>>,
    pub db: SharedDb,
    /// `[daemon] lease_grace_secs`, in ms.
    pub grace_ms: i64,
    /// Actors report idle/busy transitions here; the daemon run loop's lease
    /// bookkeeping consumes it.
    pub idle_tx: mpsc::UnboundedSender<IdleTransition>,
    /// Signals the daemon run loop to exit gracefully.
    pub shutdown: Arc<tokio::sync::Notify>,
    /// The daemon's config. The merge verbs resolve `[merge_queue]` PER REPO
    /// from it (a daemon serves many repos, so a single `MergeQueueConfig`
    /// snapshot was already the wrong shape — it could not honor a
    /// `[workspace.<slug>]` refinement, or even a differing target branch).
    pub config: std::sync::Arc<thegn_core::config::Config>,
    /// The control endpoint's stable string form (the socket path on unix),
    /// exported into every session's environment as `THEGN_CONTROL_SOCKET`
    /// so a program inside a pane can reach the daemon that owns it.
    pub endpoint: String,
}

/// The identity a daemon session exports to its child: its own session id
/// and the control endpoint — enough for an agent in the pane to address
/// itself (`thegn session snapshot --session $THEGN_SESSION_ID`) or its
/// siblings. Caller-supplied pairs win over these only for unrelated keys;
/// these two are the daemon's to set.
pub(crate) fn session_identity_env(
    id: &str,
    endpoint: &str,
    env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env
        .iter()
        .filter(|(k, _)| k != "THEGN_SESSION_ID" && k != "THEGN_CONTROL_SOCKET")
        .cloned()
        .collect();
    out.push(("THEGN_SESSION_ID".into(), id.to_string()));
    out.push(("THEGN_CONTROL_SOCKET".into(), endpoint.to_string()));
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The wire word for an agent state — the same vocabulary `wait --until` takes
/// and `EventFrame::Activity` carries.
fn state_label(s: PaneAgentState) -> &'static str {
    match s {
        PaneAgentState::Blocked => "blocked",
        PaneAgentState::Working => "working",
        PaneAgentState::Done => "done",
        PaneAgentState::Idle => "idle",
    }
}

/// Whether an observed state satisfies a waited-for one.
///
/// `Idle` is deliberately loose — it means **"not working"**, so it is
/// satisfied by `done` as well, and it additionally requires that the session
/// has been busy at least once. Without that, `wait --until idle` on a
/// just-spawned agent would return instantly (a session that has never worked
/// is, literally, idle), which is never what a supervisor meant by "wait until
/// it finishes".
fn satisfied(p: &ProbeReply, want: PaneAgentState) -> bool {
    match want {
        PaneAgentState::Idle => {
            p.ever_busy && matches!(p.state, PaneAgentState::Idle | PaneAgentState::Done)
        }
        w => p.state == w,
    }
}

/// [`satisfied`] against a state word off the event feed. A *transition* into
/// `done`/`idle` can only be reached from `active`, so the "has been busy"
/// requirement is already implied here.
fn event_satisfies(observed: &str, want: PaneAgentState) -> bool {
    match want {
        PaneAgentState::Idle => matches!(observed, "idle" | "done"),
        w => observed == state_label(w),
    }
}

/// Ask a live actor what it is doing right now. `None` when the actor is gone.
async fn probe(tx: &mpsc::Sender<SessionMsg>) -> Option<ProbeReply> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(SessionMsg::Probe { reply: reply_tx }).await.ok()?;
    reply_rx.await.ok()
}

/// Block on the event feed until `session` reaches `want` (or ends).
async fn await_state(
    rx: &mut broadcast::Receiver<Arc<EventFrame>>,
    session: &str,
    want: PaneAgentState,
    label: &'static str,
) -> WaitOutcome {
    let exited = |code: Option<i32>| WaitOutcome {
        matched: true,
        condition: "exited".into(),
        exit_code: code,
    };
    loop {
        match rx.recv().await {
            Ok(frame) => match &*frame {
                EventFrame::Activity { json } => {
                    if let Ok(ev) = serde_json::from_str::<SessionActivityEvent>(json)
                        && ev.session == session
                        && event_satisfies(&ev.state, want)
                    {
                        return WaitOutcome {
                            matched: true,
                            condition: label.into(),
                            exit_code: None,
                        };
                    }
                }
                // An agent that exits has stopped working, so every state wait
                // resolves — as `exited`, so the caller can tell the difference.
                EventFrame::SessionExit { session: s, code } if s == session => {
                    return exited(*code);
                }
                _ => {}
            },
            // A lagging receiver skipped frames; the state is level-checked
            // again by the next transition, and the feed is bounded generously.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return exited(None),
        }
    }
}

/// Bound a wait on the caller's deadline. A negative or absent `timeout_ms`
/// waits forever, matching the `Exited` arm's long-standing behaviour.
async fn with_timeout(
    feed: impl std::future::Future<Output = WaitOutcome>,
    timeout_ms: Option<i64>,
    label: &'static str,
) -> WaitOutcome {
    match timeout_ms {
        Some(ms) if ms >= 0 => {
            tokio::time::timeout(std::time::Duration::from_millis(ms as u64), feed)
                .await
                .unwrap_or(WaitOutcome {
                    matched: false,
                    condition: label.into(),
                    exit_code: None,
                })
        }
        _ => feed.await,
    }
}

pub(crate) fn fresh_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("csprng for session id");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl DaemonService {
    /// Run `f` against the shared DB on a blocking thread.
    pub(crate) async fn with_db<T, F>(&self, f: F) -> ControlResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("daemon db lock");
            f(&db)
        })
        .await
        .map_err(|e| ControlError::Internal(anyhow::anyhow!("db task join: {e}")))?
        .map_err(ControlError::Internal)
    }

    async fn entry_tx(&self, session: &str) -> ControlResult<mpsc::Sender<SessionMsg>> {
        self.sessions
            .lock()
            .await
            .get(session)
            .map(|e| e.msg_tx.clone())
            .ok_or_else(|| ControlError::NotFound(format!("session {session}")))
    }

    /// Resolve a session id to a live actor, a readable corpse, or nothing.
    ///
    /// Checks the live table first: the actor buries its tombstone *before* it
    /// removes itself from `sessions`, so during the handover a session is
    /// briefly in both maps and "live" is the fresher answer.
    pub(crate) async fn lookup(&self, session: &str) -> Lookup {
        if let Some(tx) = self
            .sessions
            .lock()
            .await
            .get(session)
            .map(|e| e.msg_tx.clone())
        {
            return Lookup::Live(tx);
        }
        match self.tombs.lock().await.get(session, now_ms()) {
            Some(t) => Lookup::Dead(Box::new(t.clone())),
            None => Lookup::Unknown,
        }
    }

    /// A dead session's tombstone, read under the tombs lock — the
    /// transport-retry observer's whole view of an exited session (final
    /// screen, geometry, and who was attached at death) in one lock-scope
    /// read. `None` while the session is live (nothing to retry), unknown, or
    /// reaped past the TTL.
    pub(crate) async fn tombstone(&self, id: &str) -> Option<Tombstone> {
        self.tombs.lock().await.get(id, now_ms()).cloned()
    }

    pub(crate) fn not_found(session: &str) -> ControlError {
        ControlError::NotFound(format!("session {session}"))
    }

    /// The mailbox of a session that must be *alive* — writing to, resizing or
    /// attaching to a corpse is a conflict, not a 404: the id is real, the
    /// session simply ended, and saying so is more useful than "no such thing".
    async fn live_tx(&self, session: &str) -> ControlResult<mpsc::Sender<SessionMsg>> {
        match self.lookup(session).await {
            Lookup::Live(tx) => Ok(tx),
            Lookup::Dead(t) => Err(ControlError::Conflict(format!(
                "session {session} exited (code {:?})",
                t.exit_code
            ))),
            Lookup::Unknown => Err(Self::not_found(session)),
        }
    }

    /// The outcome every wait condition collapses to once the session is dead.
    /// Uniform on purpose: no condition ever hangs on a corpse, and the caller
    /// tells "the agent finished its turn" from "the agent died" by reading
    /// `condition`.
    fn exited_outcome(t: &Tombstone) -> WaitOutcome {
        WaitOutcome {
            matched: true,
            condition: "exited".into(),
            exit_code: t.exit_code,
        }
    }

    pub(crate) fn emit(&self, frame: EventFrame) {
        let _ = self.events.send(Arc::new(frame)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
    }

    /// Open a relay lease for a now-idle session (the actor signaled the last
    /// subscriber left). Called from the daemon run loop's idle listener.
    /// `grace_ms == 0` opens an UNTIMED lease (`expires_at: None`) — the
    /// reaper's `plan_leases` skips those, so the session lives until it is
    /// explicitly killed or reattached (the never-reap default).
    pub(crate) async fn on_session_idle(&self, session: &str) {
        let daemon_id = self.daemon_id.clone();
        let sid = session.to_string();
        let expires = (self.grace_ms > 0).then(|| relay_expiry(now_ms(), self.grace_ms));
        let put = self
            .with_db(move |db| {
                // Replace any prior lease for this session (re-detach refreshes).
                db.release_session_leases(&sid)?;
                db.put_lease(&sid, &daemon_id, None, "relay", expires, now_ms())?;
                Ok(())
            })
            .await;
        if put.is_ok() {
            self.emit(EventFrame::Lease {
                session: session.to_string(),
                kind: LeaseEventKind::Opened,
                expires_at: expires,
            });
        }
    }

    /// Confine a control-plane git/merge verb to a thegn-REGISTERED worktree.
    /// The control plane must never run git against an arbitrary caller-supplied
    /// path — a token-holding remote `serve` client is not the daemon's uid — so
    /// reject anything absent from the worktree registry (compared canonically to
    /// tolerate trailing-slash / symlink variation). NotFound, not a hard error,
    /// so an unknown path reads the same as a gone session.
    async fn confine_worktree(&self, wt: &str) -> ControlResult<()> {
        let canon =
            |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
        let want = canon(wt);
        let known = self
            .with_db(move |db| {
                use thegn_core::store::WorkspaceStore;
                Ok(db.worktrees()?.into_iter().any(|r| {
                    std::fs::canonicalize(&r.worktree)
                        .unwrap_or_else(|_| std::path::PathBuf::from(&r.worktree))
                        == want
                }))
            })
            .await?;
        if known {
            Ok(())
        } else {
            Err(ControlError::NotFound(format!(
                "worktree not registered with thegn: {wt}"
            )))
        }
    }

    /// Release a session's relay lease (a subscriber attached, or the session
    /// ended entirely).
    pub(crate) async fn on_session_busy(&self, session: &str) {
        let sid = session.to_string();
        let released = self
            .with_db(move |db| db.release_session_leases(&sid))
            .await;
        if released.is_ok() {
            self.emit(EventFrame::Lease {
                session: session.to_string(),
                kind: LeaseEventKind::Released,
                expires_at: None,
            });
        }
    }
}

impl ControlApi for DaemonService {
    fn list_sessions(&self) -> BoxFuture<'_, ControlResult<Vec<SessionInfo>>> {
        Box::pin(async move {
            let daemon_id = self.daemon_id.clone();
            let leases: Vec<LeaseRow> = self
                .with_db(move |db| db.leases(&daemon_id))
                .await
                .unwrap_or_default();
            let sessions = self.sessions.lock().await;
            let mut out: Vec<SessionInfo> = sessions
                .values()
                .map(|e| {
                    let lease = leases
                        .iter()
                        .find(|l| l.session_id == e.meta.id && l.kind == "relay")
                        .and_then(|l| l.expires_at);
                    let live = e.live.lock().expect("live meta lock");
                    e.meta.info(&live, lease)
                })
                .collect();
            drop(sessions);
            // Recently-finished sessions belong in the roster too: a supervisor
            // asking "which of my workers are done?" must not have to race the
            // moment of exit. They carry `exited_at_ms`, so a caller that wants
            // only live sessions filters on it (the split lookup below does).
            let now = now_ms();
            let mut tombs = self.tombs.lock().await;
            tombs.sweep(now);
            out.extend(tombs.iter(now).map(|(_, t)| t.info()));
            drop(tombs);
            out.sort_by_key(|s| s.created_at_ms);
            Ok(out)
        })
    }

    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        Box::pin(async move {
            // Keep only a memory-resident recipe for a later live fork. Raw
            // argv/env never enter a response, tombstone, or cache row.
            let recipe = match &spec.agent {
                Some(launch) => super::fork::agent_recipe(&self.config, launch, &spec),
                None => Some(thegn_core::session_fork::DaemonRecipe::Raw(
                    thegn_core::session_fork::RawLaunchRecipe {
                        argv: spec.argv.clone(),
                        cwd: spec.cwd.clone(),
                        env: spec.env.clone(),
                        worktree: spec.worktree.clone(),
                    },
                )),
            };
            // An agent launch resolves through the same pipeline the wizard
            // uses — sandbox, credentials, cap and all — so what runs here is
            // identical to what a TUI-launched agent runs. Blocking work
            // (SQLite, sandbox prep, a bounded direnv warm), so it goes off the
            // runtime's worker threads.
            let resolved = match &spec.agent {
                Some(launch) => {
                    let snapshot = self.config.clone();
                    let launch = launch.clone();
                    let spec2 = spec.clone();
                    Some(
                        self.with_db(move |db| {
                            // Per-request config: a `[[agents]]` entry added or
                            // retuned since the daemon started is honoured now,
                            // not after a restart. The snapshot is the fallback
                            // when the file no longer loads.
                            let fresh = crate::config_source::fresh();
                            let cfg = fresh.as_ref().unwrap_or(&snapshot);
                            super::agent_open::resolve(cfg, db, &spec2, &launch)
                        })
                        .await
                        .map_err(|e| ControlError::Conflict(e.to_string()))?,
                    )
                }
                None => None,
            };

            // The resolved argv is already sandbox-wrapped AND CPU-capped by
            // `enter_argv`; a raw caller's argv is capped here, unless it says
            // it already did so itself (the compositor does).
            let (argv, cwd_s, env_pairs, worktree) = match &resolved {
                Some(r) => (
                    r.argv.clone(),
                    r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    r.env.clone(),
                    spec.worktree.clone().or_else(|| spec.cwd.clone()),
                ),
                None => (
                    thegn_core::sandbox_cpucap::wrap_control_argv(
                        spec.argv.clone(),
                        spec.already_capped,
                    ),
                    spec.cwd.clone(),
                    Vec::new(),
                    spec.worktree.clone(),
                ),
            };
            if argv.is_empty() {
                return Err(ControlError::Conflict("empty argv".into()));
            }

            let id = fresh_id();
            // Redaction chokepoint: a pane argv can carry a token on the command
            // line (`--token …`, `FOO_TOKEN=…`). At DEBUG log only the program
            // name + argument count (never a value); the full argv is TRACE-only
            // and passes the redactor. See `thegn_core::log_redact`.
            tracing::debug!(
                target: "thegn::daemon",
                cmd = %thegn_core::log_redact::command_summary(&argv),
                cwd = ?cwd_s,
                "open session"
            );
            tracing::trace!(
                target: "thegn::daemon",
                argv = ?thegn_core::log_redact::redact_argv(&argv),
                "open session argv"
            );
            let rows = spec.rows.max(1);
            let cols = spec.cols.max(1);
            // Composition order, weakest first: what the launch resolved, then
            // the caller's explicit pairs, then the two keys that are the
            // daemon's alone.
            let mut env = env_pairs;
            env.extend(spec.env.iter().cloned());
            let env = session_identity_env(&id, &self.endpoint, &env);
            let program = match &spec.agent {
                Some(a) => a.agent.clone(),
                None => crate::pane::program_name(&argv),
            };
            let info = super::fork::spawn_session(
                self,
                super::fork::SpawnRequest {
                    id: id.clone(),
                    argv,
                    cwd: cwd_s,
                    env,
                    rows,
                    cols,
                    worktree: worktree.clone(),
                    program,
                    recipe,
                    forked_from: None,
                    handoff: None,
                },
            )
            .await?;

            // Ask a running compositor to graft this session into a real pane.
            // Best-effort by design: with no instance up, the session is simply
            // headless until someone attaches, which is a fine outcome — the
            // intent is a nudge, not a dependency.
            if spec.adopt {
                let payload = thegn_core::models::AdoptIntent {
                    session: id.clone(),
                    worktree,
                    focus: false,
                    tab: false,
                };
                if let Err(e) = self
                    .with_db(move |db| {
                        db.put_intent("adopt_session", &serde_json::to_string(&payload)?)?;
                        Ok(())
                    })
                    .await
                {
                    tracing::warn!(target: "thegn::daemon", "adopt intent for {id} failed: {e}");
                }
            }
            Ok(info)
        })
    }

    fn fork(&self, spec: ForkSpec) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        super::fork::run(self, spec)
    }

    fn attach<'a>(
        &'a self,
        client_id: &'a str,
        session: &'a str,
        kind: AttachKind,
        rows: u16,
        cols: u16,
        history: bool,
    ) -> BoxFuture<'a, ControlResult<AttachReply>> {
        Box::pin(async move {
            let tx = self.live_tx(session).await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(SessionMsg::Attach {
                client_id: client_id.to_string(),
                kind,
                rows,
                cols,
                history,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ControlError::NotFound(format!("session {session}")))?;
            let reply = reply_rx
                .await
                .map_err(|_| ControlError::NotFound(format!("session {session}")))??;
            // Attaching cancels the relay grace period — but ONLY for an
            // interactive attach. An Observer never resizes the PTY and must not
            // hold the relay lease open (the AttachKind contract), so a watcher
            // attaching/detaching must not extend a detached session's life.
            if !matches!(kind, AttachKind::Observer) {
                self.on_session_busy(session).await;
            }
            Ok(reply)
        })
    }

    fn detach<'a>(
        &'a self,
        client_id: &'a str,
        session: &'a str,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            let tx = self.entry_tx(session).await?;
            // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            let _ = tx
                .send(SessionMsg::Detach {
                    client_id: client_id.to_string(),
                })
                .await;
            Ok(())
        })
    }

    fn send_input<'a>(
        &'a self,
        session: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            let tx = self.live_tx(session).await?;
            tx.send(SessionMsg::Stdin(bytes))
                .await
                .map_err(|_| Self::not_found(session))
        })
    }

    fn resize<'a>(
        &'a self,
        session: &'a str,
        rows: u16,
        cols: u16,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            let tx = self.live_tx(session).await?;
            tx.send(SessionMsg::Resize { rows, cols })
                .await
                .map_err(|_| Self::not_found(session))
        })
    }

    fn snapshot<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<EventFrame>> {
        Box::pin(async move {
            let tx = match self.lookup(session).await {
                Lookup::Live(tx) => tx,
                // The whole point of a tombstone: reading an agent's last words
                // a moment after it exited is the common case, not an error.
                Lookup::Dead(t) => return Ok(t.final_screen.clone()),
                Lookup::Unknown => return Err(Self::not_found(session)),
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(SessionMsg::Snapshot { reply: reply_tx })
                .await
                .map_err(|_| Self::not_found(session))?;
            reply_rx.await.map_err(|_| Self::not_found(session))
        })
    }

    fn kill<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            match self.lookup(session).await {
                Lookup::Live(tx) => {
                    // best-effort: a closed mailbox means the actor is already
                    // tearing down, which is what Kill asked for.
                    let _ = tx.send(SessionMsg::Kill).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    self.on_session_busy(session).await; // drop any lease with it
                    Ok(())
                }
                // Killing something already dead is what the caller wanted.
                Lookup::Dead(_) => Ok(()),
                Lookup::Unknown => Err(Self::not_found(session)),
            }
        })
    }

    fn record_session<'a>(
        &'a self,
        session: &'a str,
        spec: RecordSpec,
    ) -> BoxFuture<'a, ControlResult<RecordStatus>> {
        Box::pin(async move {
            let tx = match self.lookup(session).await {
                Lookup::Live(tx) => tx,
                // A finished session can't record, but its tombstone still knows
                // where the finalized `.cast` was written.
                Lookup::Dead(t) => {
                    return Ok(RecordStatus {
                        recording: false,
                        path: t
                            .recording
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                        bytes: 0,
                        capped: false,
                        // A tombstone only knows where the file went; the live
                        // actor already logged/reported any finalize failure.
                        truncated: None,
                    });
                }
                Lookup::Unknown => return Err(Self::not_found(session)),
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(SessionMsg::Record {
                spec,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Self::not_found(session))?;
            reply_rx.await.map_err(|_| Self::not_found(session))?
        })
    }

    fn open_worktree<'a>(
        &'a self,
        repo: &'a str,
        _branch: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            // Same channel `thegn open` uses: the v37 intents mailbox, drained
            // by a running compositor (~1s). Branch selection is a compositor
            // concern; the intent carries the repo target.
            let repo = repo.to_string();
            self.with_db(move |db| {
                let payload = serde_json::to_string(&thegn_core::models::FocusIntent { repo })?;
                db.put_intent("focus_workspace", &payload)?;
                Ok(())
            })
            .await
        })
    }

    fn drive_browser(&self, _cmd: BrowserCommand) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async move { Err(ControlError::Unimplemented("drive-browser")) })
    }

    fn wait<'a>(
        &'a self,
        session: &'a str,
        cond: WaitCondition,
        timeout_ms: Option<i64>,
    ) -> BoxFuture<'a, ControlResult<WaitOutcome>> {
        Box::pin(async move {
            // Subscribe FIRST, always. Everything below re-checks the world
            // afterwards, so a transition that lands during the check is still
            // waiting on the feed rather than lost in the gap.
            let mut rx = self.events.subscribe();

            // A dead session answers every condition at once. No supervisor
            // should ever block on a corpse, and the exit code is the answer it
            // actually wanted.
            if let Lookup::Dead(t) = self.lookup(session).await {
                if let WaitCondition::OutputMatches { regex } = &cond {
                    // ...except a pattern that genuinely appears in the retained
                    // tail: it did match, and saying "exited" would be a lie.
                    let re = thegn_core::output_match::compile_wait_regex(regex)
                        .map_err(|e| ControlError::Conflict(e.to_string()))?;
                    if thegn_core::output_match::first_match_line(
                        &re,
                        t.history_tail.iter().map(String::as_str),
                    )
                    .is_some()
                    {
                        return Ok(WaitOutcome {
                            matched: true,
                            condition: "output_matches".into(),
                            exit_code: t.exit_code,
                        });
                    }
                }
                return Ok(Self::exited_outcome(&t));
            }

            match cond {
                // Event-driven, never polled: block on the feed until the
                // target session exits.
                WaitCondition::Exited => {
                    // best-effort: registered-send that surfaces 404 via `?`; the discard drops only the success value
                    let _ = self.entry_tx(session).await?; // 404 if already gone
                    let feed = async {
                        loop {
                            match rx.recv().await {
                                Ok(frame) => {
                                    if let EventFrame::SessionExit { session: s, code } = &*frame
                                        && s == session
                                    {
                                        return WaitOutcome {
                                            matched: true,
                                            condition: "exited".into(),
                                            exit_code: *code,
                                        };
                                    }
                                }
                                // A lagging receiver skipped events; keep waiting
                                // (an exit we missed would 404 on the next check,
                                // but the feed is bounded generously).
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => {
                                    return WaitOutcome {
                                        matched: true,
                                        condition: "exited".into(),
                                        exit_code: None,
                                    };
                                }
                            }
                        }
                    };
                    match timeout_ms {
                        Some(ms) if ms >= 0 => {
                            let dur = std::time::Duration::from_millis(ms as u64);
                            Ok(tokio::time::timeout(dur, feed)
                                .await
                                .unwrap_or(WaitOutcome {
                                    matched: false,
                                    condition: "exited".into(),
                                    exit_code: None,
                                }))
                        }
                        _ => Ok(feed.await),
                    }
                }
                // The agent-state conditions, off the daemon's per-session
                // observer of the activity FSM.
                WaitCondition::Idle | WaitCondition::Blocked | WaitCondition::Done => {
                    let want = match cond {
                        WaitCondition::Idle => PaneAgentState::Idle,
                        WaitCondition::Blocked => PaneAgentState::Blocked,
                        _ => PaneAgentState::Done,
                    };
                    let label = state_label(want);
                    let tx = self.live_tx(session).await?;

                    // Level check before blocking: a condition that is ALREADY
                    // true must resolve now, not wait for a transition that has
                    // already happened.
                    if let Some(p) = probe(&tx).await
                        && satisfied(&p, want)
                    {
                        return Ok(WaitOutcome {
                            matched: true,
                            condition: label.into(),
                            exit_code: None,
                        });
                    }

                    let feed = await_state(&mut rx, session, want, label);
                    Ok(with_timeout(feed, timeout_ms, label).await)
                }
                // Output matching lives in the actor: it owns the ANSI-stripped
                // scrollback, and the event feed carries no text.
                WaitCondition::OutputMatches { ref regex } => {
                    let re = thegn_core::output_match::compile_wait_regex(regex)
                        // Compiled here, not in the actor: a bad pattern is the
                        // caller's mistake and deserves a 4xx, not an internal
                        // error raised from a task nobody is awaiting.
                        .map_err(|e| ControlError::Conflict(e.to_string()))?;
                    let tx = self.live_tx(session).await?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    tx.send(SessionMsg::WatchOutput {
                        re: Box::new(re),
                        reply: reply_tx,
                    })
                    .await
                    .map_err(|_| Self::not_found(session))?;

                    // The actor checks the retained scrollback on receipt, so a
                    // pattern that already scrolled past resolves immediately;
                    // otherwise this waits for a future line. A closed channel
                    // means the session ended first.
                    let feed = async {
                        match reply_rx.await {
                            Ok(_) => WaitOutcome {
                                matched: true,
                                condition: "output_matches".into(),
                                exit_code: None,
                            },
                            // The session ended before the pattern appeared.
                            // Its tombstone was buried before the exit became
                            // observable, so the outcome is knowable here — and
                            // a supervisor that waited on a line it never got
                            // still has to tell a clean finish from a crash.
                            Err(_) => WaitOutcome {
                                matched: true,
                                condition: "exited".into(),
                                exit_code: self
                                    .tombs
                                    .lock()
                                    .await
                                    .get(session, now_ms())
                                    .and_then(|t| t.exit_code),
                            },
                        }
                    };
                    Ok(with_timeout(feed, timeout_ms, "output_matches").await)
                }
            }
        })
    }

    fn list_worktrees(
        &self,
    ) -> BoxFuture<'_, ControlResult<Vec<thegn_svc::control::WorktreeInfo>>> {
        Box::pin(async move {
            // The DB is the resurrection cache for worktrees (git is the source
            // of truth for their *state*); listing registrations is a cache read,
            // off the runtime's worker threads like every other DB call here.
            let rows = self
                .with_db(move |db| {
                    use thegn_core::store::WorkspaceStore;
                    db.worktrees()
                })
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| thegn_svc::control::WorktreeInfo {
                    path: r.worktree,
                    branch: r.branch,
                    repo_root: r.repo_root,
                    location: r.location,
                    created_at: r.created_at,
                })
                .collect())
        })
    }

    fn git_status<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<GitFileStatus>>> {
        Box::pin(async move {
            self.confine_worktree(worktree).await?;
            let wt = worktree.to_string();
            tokio::task::spawn_blocking(move || {
                let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
                let files = CliGit.status(&loc)?;
                Ok::<_, anyhow::Error>(
                    files
                        .into_iter()
                        .map(|f| GitFileStatus {
                            path: f.path,
                            code: format!("{}{}", f.staged, f.unstaged),
                        })
                        .collect(),
                )
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("git task join: {e}")))?
            .map_err(ControlError::Internal)
        })
    }

    fn git_stage<'a>(
        &'a self,
        worktree: &'a str,
        paths: &'a [String],
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            self.confine_worktree(worktree).await?;
            let wt = worktree.to_string();
            let paths = paths.to_vec();
            tokio::task::spawn_blocking(move || {
                let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
                for p in &paths {
                    CliGit.stage(&loc, p)?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("git task join: {e}")))?
            .map_err(ControlError::Internal)
        })
    }

    fn git_commit<'a>(
        &'a self,
        worktree: &'a str,
        message: &'a str,
    ) -> BoxFuture<'a, ControlResult<String>> {
        Box::pin(async move {
            self.confine_worktree(worktree).await?;
            let wt = worktree.to_string();
            let message = message.to_string();
            tokio::task::spawn_blocking(move || {
                let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
                CliGit.commit(&loc, &message, false, None)?;
                // The new HEAD is the commit we just made (git_cmd scrubs
                // GIT_* env; inside spawn_blocking, so the wait is off-loop).
                #[expect(
                    clippy::disallowed_methods,
                    reason = "inside spawn_blocking — off-loop child wait is the sanctioned pattern"
                )]
                let out = thegn_core::util::git_cmd(std::path::Path::new(&wt))
                    .args(["rev-parse", "HEAD"])
                    .output()?;
                anyhow::ensure!(out.status.success(), "rev-parse HEAD failed");
                Ok::<_, anyhow::Error>(String::from_utf8_lossy(&out.stdout).trim().to_string())
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("git task join: {e}")))?
            .map_err(ControlError::Internal)
        })
    }

    fn merge_add<'a>(&'a self, worktree: &'a str) -> BoxFuture<'a, ControlResult<String>> {
        Box::pin(async move {
            self.confine_worktree(worktree).await?;
            let wt = worktree.to_string();
            let cfg = self.config.clone();
            // Fresh DB handle (like the CLI) so we don't hold the daemon's shared
            // db lock across the git subprocesses `enqueue_worktree` runs.
            tokio::task::spawn_blocking(move || {
                let db = thegn_core::db::Db::open()?;
                crate::merge_ops::enqueue_worktree(&cfg, &db, std::path::Path::new(&wt))
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("merge task join: {e}")))?
            .map_err(ControlError::Internal)
        })
    }

    fn merge_clear<'a>(&'a self, worktree: &'a str) -> BoxFuture<'a, ControlResult<usize>> {
        let cfg = self.config.clone();
        Box::pin(async move {
            let wt = worktree.to_string();
            tokio::task::spawn_blocking(move || {
                let db = thegn_core::db::Db::open()?;
                let root = crate::merge_ops::repo_root_of(std::path::Path::new(&wt))
                    .ok_or_else(|| anyhow::anyhow!("{wt}: not inside a git repository"))?;
                crate::merge_ops::clear_repo(&cfg, &db, &root)
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("merge task join: {e}")))?
            .map_err(ControlError::Internal)
        })
    }

    fn merge_list<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<thegn_core::db::MergeQueueRow>>> {
        Box::pin(async move {
            let wt = worktree.to_string();
            tokio::task::spawn_blocking(move || {
                let db = thegn_core::db::Db::open()?;
                let root = crate::merge_ops::repo_root_of(std::path::Path::new(&wt))
                    .ok_or_else(|| anyhow::anyhow!("{wt}: not inside a git repository"))?;
                Ok::<_, anyhow::Error>(crate::merge_ops::rows_for_repo(&db, &root))
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("merge task join: {e}")))?
            .map_err(ControlError::Internal)
        })
    }

    /// `pr.status`: a pure projection of the `pr_cache` table (the TTL'd
    /// read-through cache the sidebar/panel hydrate from) — one row per
    /// worktree whose cached `gh pr view` JSON still parses. The forge is the
    /// source of truth; `fetched_at` carries each row's staleness. Rows whose
    /// JSON no longer parses are skipped, not errors: a cache read must never
    /// fail on one stale entry.
    fn pr_status(&self) -> BoxFuture<'_, ControlResult<Vec<thegn_svc::control::PrStatusRow>>> {
        Box::pin(async move {
            let rows = self
                .with_db(|db| {
                    use thegn_core::store::CacheStore;
                    db.list_pr_cache()
                })
                .await?;
            Ok(rows
                .into_iter()
                .filter_map(|(worktree, json, fetched_at)| {
                    let st: thegn_core::forge::model::PrStatus =
                        serde_json::from_str(&json).ok()?;
                    Some(thegn_svc::control::PrStatusRow {
                        worktree,
                        branch: st.head_ref_name,
                        number: st.number,
                        title: st.title,
                        state: st.state,
                        url: st.url,
                        is_draft: st.is_draft,
                        fetched_at,
                    })
                })
                .collect())
        })
    }

    /// `notify.push`: append a tray notification, exactly like the other
    /// producers (`thegn notify push`, the hydration diff engines) — via
    /// [`thegn_core::store::NotificationStore::put_notification`]. Urgency
    /// maps onto the built-in kinds so API pushes participate in the badge
    /// machinery: `alert`/`critical` → `agent_attention` (the red ⚑ tier),
    /// anything else → `agent_done` (the CLI default's notice tier).
    fn notify_push(
        &self,
        note: thegn_svc::control::PushedNote,
    ) -> BoxFuture<'_, ControlResult<i64>> {
        Box::pin(async move {
            self.with_db(move |db| {
                use thegn_core::store::NotificationStore;
                let kind = match note.urgency.as_deref() {
                    Some("alert") | Some("critical") => "agent_attention",
                    _ => "agent_done",
                };
                let source = note.source.as_deref().unwrap_or("api");
                let message = if note.body.is_empty() {
                    note.title.clone()
                } else {
                    format!("{} — {}", note.title, note.body)
                };
                db.put_notification(kind, source, &message, "")
            })
            .await
        })
    }

    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>> {
        Box::pin(async move {
            let daemon_id = self.daemon_id.clone();
            self.with_db(move |db| db.leases(&daemon_id)).await
        })
    }

    fn mcp_proxy_status(&self) -> BoxFuture<'_, ControlResult<thegn_svc::control::McpProxyStatus>> {
        Box::pin(async move { Ok(crate::mcp_proxy::daemon_status(&self.config)) })
    }

    fn mcp_proxy_reload(
        &self,
    ) -> BoxFuture<'_, ControlResult<thegn_svc::control::McpProxyReloadReport>> {
        let baseline = std::sync::Arc::clone(&self.config);
        Box::pin(async move {
            // Re-read config off the runtime; diff the global-scope effective
            // set against the daemon's boot snapshot.
            let report =
                tokio::task::spawn_blocking(move || crate::mcp_proxy::daemon_reload(&baseline))
                    .await
                    .map_err(|e| ControlError::Internal(anyhow::anyhow!(e)))?;
            Ok(report)
        })
    }

    // --- agent orchestration (THE-57) ---------------------------------------
    // Issue verbs route through `IssueRouter` (the same provider seam the panel
    // hydrates from), built per-call from `[issues]` config — the reqwest calls
    // are async and this is already on the daemon runtime. The dispatch verbs
    // and `worktree_create` are local DB / git, on `spawn_blocking` like the
    // merge verbs.

    fn issues_list<'a>(
        &'a self,
        filter: &'a thegn_core::issue::IssueFilter,
    ) -> BoxFuture<'a, ControlResult<Vec<thegn_core::issue::Issue>>> {
        Box::pin(async move {
            let router = thegn_svc::issue::IssueRouter::from_config(&self.config.issues);
            if !router.is_configured() {
                return Err(ControlError::Unimplemented("no issue tracker configured"));
            }
            // `list_issues` swallows every per-account error into a
            // `tracing::warn!` and always answers `Ok` — over the control API
            // that reaches a supervisor agent as "zero issues", not "your token
            // is dead" (THE-72). Report per account instead: a partial failure
            // still yields the accounts that worked, an all-failed run errors.
            let per_provider = router.list_per_provider(filter).await;
            let total = per_provider.len();
            let mut failed: Vec<String> = Vec::new();
            let mut issues = Vec::new();
            for (account, provider, result) in per_provider {
                match result {
                    Ok(mut v) => issues.append(&mut v),
                    Err(e) => {
                        tracing::warn!(account = %account, provider, error = %e, "issues.list account failed");
                        failed.push(format!("{provider}/{account}: {e}"));
                    }
                }
            }
            if total > 0 && failed.len() == total {
                return Err(ControlError::Internal(anyhow::anyhow!(
                    "issues.list: every configured account errored — {}",
                    failed.join("; ")
                )));
            }
            if filter.limit > 0 && issues.len() > filter.limit {
                issues.truncate(filter.limit);
            }
            Ok(issues)
        })
    }

    fn issues_get<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, ControlResult<thegn_core::issue::IssueDetail>> {
        Box::pin(async move {
            let router = thegn_svc::issue::IssueRouter::from_config(&self.config.issues);
            router
                .get_issue(id)
                .await
                .map_err(|e| ControlError::Internal(anyhow::anyhow!("issues.get {id}: {e}")))
        })
    }

    fn issues_update<'a>(
        &'a self,
        id: &'a str,
        patch: &'a thegn_core::issue::IssuePatch,
    ) -> BoxFuture<'a, ControlResult<thegn_core::issue::Issue>> {
        Box::pin(async move {
            let router = thegn_svc::issue::IssueRouter::from_config(&self.config.issues);
            router
                .update_issue(id, patch)
                .await
                .map_err(|e| ControlError::Internal(anyhow::anyhow!("issues.update {id}: {e}")))
        })
    }

    fn issues_comment<'a>(
        &'a self,
        id: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            let router = thegn_svc::issue::IssueRouter::from_config(&self.config.issues);
            router
                .add_comment(id, body)
                .await
                .map_err(|e| ControlError::Internal(anyhow::anyhow!("issues.comment {id}: {e}")))
        })
    }

    fn dispatches_list(
        &self,
    ) -> BoxFuture<'_, ControlResult<Vec<thegn_core::issue::AgentDispatch>>> {
        Box::pin(async move {
            self.with_db(|db| {
                use thegn_core::store::NotificationStore;
                db.list_dispatches()
            })
            .await
        })
    }

    fn dispatch_put(
        &self,
        req: thegn_svc::control::DispatchPutReq,
    ) -> BoxFuture<'_, ControlResult<thegn_core::issue::AgentDispatch>> {
        Box::pin(async move {
            self.with_db(move |db| {
                use thegn_core::store::NotificationStore;
                let id = db.put_agent_dispatch(thegn_core::issue::NewDispatch {
                    issue_id: &req.issue_id,
                    worktree_path: &req.worktree_path,
                    agent_name: &req.agent_name,
                    stage: req.stage.as_deref(),
                    parent_id: req.parent_id,
                    session_id: req.session_id.as_deref(),
                    artifact_path: req.artifact_path.as_deref(),
                    chunk_path: None,
                })?;
                db.get_dispatch(id)?
                    .ok_or_else(|| anyhow::anyhow!("dispatch {id} vanished after insert"))
            })
            .await
        })
    }

    fn dispatch_set_status(
        &self,
        id: i64,
        status: thegn_core::issue::AgentDispatchStatus,
    ) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async move {
            self.with_db(move |db| {
                use thegn_core::store::NotificationStore;
                if db.get_dispatch(id)?.is_none() {
                    anyhow::bail!("no dispatch with id {id}");
                }
                db.update_dispatch_status(id, status)
            })
            .await
        })
    }

    fn worktree_create(
        &self,
        req: thegn_svc::control::WorktreeCreateReq,
    ) -> BoxFuture<'_, ControlResult<thegn_svc::control::WorktreeInfo>> {
        let cfg = self.config.clone();
        Box::pin(async move {
            // Resolve the branch to link/create up front: an issue id needs the
            // provider's `branch_hint`, which is an async router read, so it
            // cannot happen inside `spawn_blocking`.
            let issue = req.issue.clone().filter(|s| !s.is_empty());
            let seed = match (&req.branch, &issue) {
                (Some(b), _) if !b.trim().is_empty() => b.trim().to_string(),
                (_, Some(id)) => {
                    let router = thegn_svc::issue::IssueRouter::from_config(&cfg.issues);
                    let detail = router.get_issue(id).await.map_err(|e| {
                        ControlError::Internal(anyhow::anyhow!("worktrees.create {id}: {e}"))
                    })?;
                    thegn_core::issue::issue_branch_seed(
                        detail.issue.branch_hint.as_deref(),
                        &detail.issue.number,
                    )
                }
                _ => {
                    return Err(ControlError::Conflict(
                        "worktrees.create needs a branch or an issue id".into(),
                    ));
                }
            };
            let repo_hint = req.repo.clone();
            self.with_db(move |db| {
                use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
                use thegn_core::{repo, worktree as wt};

                let root = repo_hint
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .and_then(|p| repo::main_worktree(std::path::Path::new(p)))
                    .or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .and_then(|c| repo::main_worktree(&c))
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("worktrees.create: no git repo (pass `repo`)")
                    })?;

                let base = wt::resolve_base(&root, &cfg);
                let taken = wt::BranchSet::load(&root);
                let branch = wt::dedupe(&seed, &taken);
                let path = wt::worktree_path(&root, &branch, &cfg);
                wt::add_checked(&root, &branch, &base, &path, &cfg)
                    .map_err(|e| anyhow::anyhow!("worktrees.create: {e}"))?;

                let wt_str = path.to_string_lossy().into_owned();
                let slug = repo::repo_slug(&root);
                let tab = repo::branch_tab(&slug, &branch);
                let root_s = root.to_string_lossy().into_owned();
                let _ = db.put_worktree(&tab, &root_s, &wt_str, &branch, None, None); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                if let Some(id) = &issue {
                    let _ = db.link_issue(&wt_str, id); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                }
                Ok(thegn_svc::control::WorktreeInfo {
                    path: wt_str,
                    branch,
                    repo_root: root_s,
                    location: String::new(),
                    created_at: now_ms() / 1000,
                })
            })
            .await
        })
    }

    /// `agent.sessions`: a bounded filesystem scan of each harness's local
    /// session store. Off the runtime's worker threads (`spawn_blocking`) — it
    /// reads potentially many transcript heads. The DB supplies the tracked
    /// worktree set only for the `unlinked` flag; a DB miss degrades to "all
    /// unlinked", never an error.
    fn agent_sessions<'a>(
        &'a self,
        worktree: Option<&'a str>,
        harness: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<Vec<thegn_core::harness::SessionRecord>>> {
        Box::pin(async move {
            let cfg = self.config.clone();
            let worktree = worktree.map(str::to_string);
            let harness = harness.map(str::to_string);
            let known: std::collections::HashSet<String> = self
                .with_db(|db| {
                    use thegn_core::store::WorkspaceStore;
                    db.worktrees()
                })
                .await
                .map(|rows| rows.into_iter().map(|r| r.worktree).collect())
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || {
                let filter = thegn_svc::sessions::SessionFilter {
                    worktree: worktree.as_deref(),
                    harness: harness.as_deref(),
                };
                thegn_svc::sessions::discover(&cfg, &filter, &known)
            })
            .await
            .map_err(|e| ControlError::Internal(anyhow::anyhow!("session scan join: {e}")))
        })
    }

    fn publish_pairing(&self, pairing_id: &str, label: &str, scope: &str, state: PairingState) {
        self.emit(EventFrame::Pairing {
            pairing_id: pairing_id.to_string(),
            label: label.to_string(),
            scope: scope.to_string(),
            state,
        });
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<EventFrame>> {
        self.events.subscribe()
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.shutdown.notify_waiters();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a service with an in-memory DB and no live sessions — enough to
    /// exercise the lease bookkeeping glue (`on_session_idle` / `on_session_busy`)
    /// in isolation from the PTY actors, which is exactly the untested seam.
    fn service(grace_ms: i64) -> (DaemonService, broadcast::Receiver<Arc<EventFrame>>) {
        service_with_config(grace_ms, thegn_core::config::Config::default())
    }

    /// [`service`] with a caller-supplied config — the transport-retry tests
    /// shrink the backoff so a park→re-check cycle runs in milliseconds.
    fn service_with_config(
        grace_ms: i64,
        config: thegn_core::config::Config,
    ) -> (DaemonService, broadcast::Receiver<Arc<EventFrame>>) {
        let (events, rx) = broadcast::channel(64);
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel();
        let svc = DaemonService {
            daemon_id: "d0".into(),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            tombs: Arc::new(tokio::sync::Mutex::new(Graveyard::new(
                super::super::tombstone::MAX_TOMBSTONES,
                super::super::tombstone::TOMBSTONE_TTL_MS,
            ))),
            events,
            db: Arc::new(Mutex::new(Db::open_memory().expect("in-memory db"))),
            grace_ms,
            idle_tx,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            config: Arc::new(config),
            endpoint: "/run/test.sock".into(),
        };
        (svc, rx)
    }

    fn leases(svc: &DaemonService) -> Vec<LeaseRow> {
        svc.db.lock().unwrap().leases(&svc.daemon_id).unwrap()
    }

    /// Drain one Lease frame (skip any non-lease frames like `Sessions`).
    fn next_lease(
        rx: &mut broadcast::Receiver<Arc<EventFrame>>,
    ) -> (String, LeaseEventKind, Option<i64>) {
        loop {
            let frame = rx.try_recv().expect("a lease frame was emitted");
            if let EventFrame::Lease {
                session,
                kind,
                expires_at,
            } = &*frame
            {
                return (session.clone(), *kind, *expires_at);
            }
        }
    }

    /// The roster answers "which of my workers are done?" in one call. A
    /// worker that exited moments ago must still be listed — dropping the row
    /// the instant the child dies is what would make a supervisor re-dispatch
    /// finished work — and it must be distinguishable from a live one.
    #[tokio::test]
    async fn the_roster_lists_finished_sessions_marked_as_such() {
        let (svc, _rx) = service(0);
        svc.tombs.lock().await.insert(
            "dead1".into(),
            super::super::tombstone::tests::tomb("dead1", Some(7)),
            now_ms(),
        );

        let rows = svc.list_sessions().await.expect("roster");
        let row = rows
            .iter()
            .find(|r| r.id == "dead1")
            .expect("a finished session stays on the roster");
        assert_eq!(row.exit_code, Some(7), "with its outcome");
        assert_eq!(row.final_state.as_deref(), Some("done"));
        assert!(
            row.exited_at_ms.is_some(),
            "and marked finished, so a caller can tell it from a live session"
        );
    }

    /// A supervisor waiting for a line the agent never printed still has to
    /// learn how the run ended. The actor buries the tombstone before dropping
    /// its matchers, so by the time the waiter sees a closed channel the exit
    /// code is knowable — reporting `None` would make a crash indistinguishable
    /// from a clean finish.
    #[tokio::test]
    async fn a_matcher_wait_reports_the_exit_code_of_a_session_that_died() {
        let (svc, _rx) = service(0);
        let (msg_tx, mut msg_rx) = mpsc::channel(4);
        svc.sessions.lock().await.insert(
            "s1".into(),
            SessionEntry {
                msg_tx,
                meta: SessionMeta {
                    id: "s1".into(),
                    worktree: None,
                    program: "claude".into(),
                    cwd: None,
                    created_at_ms: 0,
                    pid: None,
                    forked_from: None,
                },
                live: Arc::new(Mutex::new(LiveMeta {
                    rows: 24,
                    cols: 80,
                    attached: 0,
                    ..Default::default()
                })),
                recipe: None,
            },
        );

        // Stand in for the actor's teardown, in its real order: bury the
        // corpse, *then* drop the matcher's reply sender.
        let tombs = Arc::clone(&svc.tombs);
        tokio::spawn(async move {
            let Some(SessionMsg::WatchOutput { reply, .. }) = msg_rx.recv().await else {
                panic!("the wait must register a matcher");
            };
            tombs.lock().await.insert(
                "s1".into(),
                super::super::tombstone::tests::tomb("s1", Some(9)),
                now_ms(),
            );
            drop(reply);
        });

        let out = svc
            .wait(
                "s1",
                WaitCondition::OutputMatches {
                    regex: "NEVER".into(),
                },
                Some(5_000),
            )
            .await
            .expect("wait resolves");
        assert_eq!(out.condition, "exited", "it ended rather than matching");
        assert_eq!(out.exit_code, Some(9), "and the outcome survives");
    }

    /// An expired corpse is gone from the roster too — the graveyard's TTL is
    /// what stops a long-lived daemon accumulating stale rows.
    #[tokio::test]
    async fn an_expired_corpse_leaves_the_roster() {
        let (svc, _rx) = service(0);
        let buried = now_ms() - super::super::tombstone::TOMBSTONE_TTL_MS - 1;
        svc.tombs.lock().await.insert(
            "old".into(),
            super::super::tombstone::tests::tomb("old", Some(0)),
            buried,
        );
        let rows = svc.list_sessions().await.expect("roster");
        assert!(
            !rows.iter().any(|r| r.id == "old"),
            "expired rows are swept"
        );
    }

    // --- retroactive supervision coverage (THE-57) --------------------------
    // The substrate landed in d51ab92e; these lock the wait/tombstone contracts
    // the orchestration surface depends on, using injected corpses (no PTY).

    /// A dead session answers every activity condition uniformly as `exited`
    /// with its code — no supervisor ever blocks on a corpse, and the exit code
    /// is the answer it actually wanted.
    #[tokio::test]
    async fn every_wait_on_a_dead_session_resolves_to_its_exit_code() {
        for cond in [
            WaitCondition::Exited,
            WaitCondition::Idle,
            WaitCondition::Blocked,
            WaitCondition::Done,
        ] {
            let (svc, _rx) = service(0);
            svc.tombs.lock().await.insert(
                "gone".into(),
                super::super::tombstone::tests::tomb("gone", Some(5)),
                now_ms(),
            );
            let out = svc.wait("gone", cond.clone(), Some(1_000)).await.unwrap();
            assert!(
                out.matched,
                "a corpse never leaves a waiter hanging: {cond:?}"
            );
            assert_eq!(out.condition, "exited", "{cond:?}");
            assert_eq!(out.exit_code, Some(5), "the outcome survives: {cond:?}");
        }
    }

    /// An `OutputMatches` wait that lands after the session died still sees the
    /// pattern in the corpse's retained scrollback — "exited" would be a lie
    /// when the line the caller wanted is right there in the tail.
    #[tokio::test]
    async fn a_matcher_wait_on_a_dead_session_scans_the_retained_tail() {
        let (svc, _rx) = service(0);
        // `tomb()` seeds the tail with "one"/"two".
        svc.tombs.lock().await.insert(
            "dead".into(),
            super::super::tombstone::tests::tomb("dead", Some(0)),
            now_ms(),
        );
        // A pattern present in the tail matches (not "exited").
        let hit = svc
            .wait(
                "dead",
                WaitCondition::OutputMatches {
                    regex: "two".into(),
                },
                Some(1_000),
            )
            .await
            .unwrap();
        assert!(hit.matched);
        assert_eq!(hit.condition, "output_matches");
        // A pattern absent from the tail falls back to the exit outcome.
        let miss = svc
            .wait(
                "dead",
                WaitCondition::OutputMatches {
                    regex: "NEVER".into(),
                },
                Some(1_000),
            )
            .await
            .unwrap();
        assert_eq!(miss.condition, "exited");
        assert_eq!(miss.exit_code, Some(0));
    }

    /// `snapshot` on a corpse serves its final screen rather than 404ing — the
    /// late-poller-reads-the-corpse contract.
    #[tokio::test]
    async fn snapshot_reads_a_dead_sessions_final_screen() {
        let (svc, _rx) = service(0);
        svc.tombs.lock().await.insert(
            "corpse".into(),
            super::super::tombstone::tests::tomb("corpse", Some(0)),
            now_ms(),
        );
        let frame = svc.snapshot("corpse").await.expect("the corpse answers");
        match frame {
            EventFrame::PaneSnapshot { bytes, .. } => assert_eq!(bytes, b"final"),
            other => panic!("expected the final screen, got {other:?}"),
        }
        // An id that never existed is still a genuine not-found.
        assert!(matches!(
            svc.snapshot("nobody").await,
            Err(ControlError::NotFound(_))
        ));
    }

    /// The `idle` condition requires the session to have been busy at least
    /// once: a just-spawned agent that has done nothing is *not* "done", so a
    /// `wait --until idle` on it must not return instantly.
    #[test]
    fn idle_wait_requires_ever_busy_but_done_and_blocked_are_level_triggered() {
        use thegn_core::attention::PaneAgentState as S;
        let fresh_idle = ProbeReply {
            state: S::Idle,
            ever_busy: false,
        };
        let worked_then_idle = ProbeReply {
            state: S::Idle,
            ever_busy: true,
        };
        // Idle is "not working, and it has worked" — a fresh spawn fails it.
        assert!(!satisfied(&fresh_idle, S::Idle));
        assert!(satisfied(&worked_then_idle, S::Idle));
        // Done satisfies an idle wait too (finished ⇒ not working).
        assert!(satisfied(
            &ProbeReply {
                state: S::Done,
                ever_busy: true
            },
            S::Idle
        ));
        // Blocked/Done are exact level checks, no ever-busy gate.
        assert!(satisfied(
            &ProbeReply {
                state: S::Blocked,
                ever_busy: false
            },
            S::Blocked
        ));
    }

    /// The dispatch roster round-trips through the control plane: put appends a
    /// typed row, set_status advances it (and 404s an unknown id), list reads
    /// it back newest-first — the ledger a supervisor resumes from.
    #[tokio::test]
    async fn dispatch_roster_put_list_and_set_status_round_trip() {
        use thegn_core::issue::AgentDispatchStatus as St;
        let (svc, _rx) = service(0);
        assert!(svc.dispatches_list().await.unwrap().is_empty());
        let row = svc
            .dispatch_put(thegn_svc::control::DispatchPutReq {
                issue_id: "linear:A-1".into(),
                worktree_path: "/wt/a".into(),
                agent_name: "claude".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(row.status, St::Queued, "a fresh dispatch starts queued");
        assert_eq!(
            (row.stage.as_deref(), row.parent_id),
            (None, None),
            "a plain dispatch carries no pipeline columns"
        );

        // The v56 pipeline columns ride `put` (no second verb) and come back on
        // the row.
        let child = svc
            .dispatch_put(thegn_svc::control::DispatchPutReq {
                issue_id: "linear:A-1".into(),
                worktree_path: "/wt/a".into(),
                agent_name: "claude".into(),
                stage: Some("code".into()),
                parent_id: Some(row.id),
                session_id: Some("sess-code-1".into()),
                artifact_path: Some(".thegn/pipeline/architect/1.md".into()),
            })
            .await
            .unwrap();
        assert_eq!(child.stage.as_deref(), Some("code"));
        assert_eq!(child.parent_id, Some(row.id));
        assert_eq!(child.session_id.as_deref(), Some("sess-code-1"));
        assert_eq!(
            child.artifact_path.as_deref(),
            Some(".thegn/pipeline/architect/1.md")
        );
        svc.dispatch_set_status(child.id, St::Done).await.unwrap();
        svc.dispatch_set_status(row.id, St::Running).await.unwrap();
        let rows = svc.dispatches_list().await.unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first (id DESC breaks the same-millisecond tie).
        assert_eq!(rows[0].id, child.id);
        assert_eq!(rows[0].status, St::Done);
        assert_eq!(rows[1].status, St::Running);
        // Advancing a non-existent row is a clean error, not a silent no-op.
        assert!(svc.dispatch_set_status(9999, St::Done).await.is_err());
    }

    /// With no `[issues]` provider configured, the issue verbs answer
    /// `Unimplemented` rather than pretending — the AI-free shell contract (the
    /// row exists, the tracker simply is not wired).
    #[tokio::test]
    async fn issue_verbs_are_unimplemented_without_a_configured_tracker() {
        let (svc, _rx) = service(0);
        let filter = thegn_core::issue::IssueFilter::default();
        assert!(matches!(
            svc.issues_list(&filter).await,
            Err(ControlError::Unimplemented(_))
        ));
    }

    /// `pr.status` is an honest projection of `pr_cache`: valid rows come
    /// back with their facts + `fetched_at`, and a row whose cached JSON no
    /// longer parses is skipped, never an error.
    #[tokio::test]
    async fn pr_status_projects_the_cache_and_skips_garbage_rows() {
        let (svc, _rx) = service(0);
        {
            use thegn_core::store::CacheStore;
            let db = svc.db.lock().unwrap();
            db.put_pr_cache(
                "/w/a",
                "feat-a",
                r#"{"number":42,"title":"a change","state":"OPEN",
                    "url":"https://forge/pr/42","isDraft":true,"headRefName":"feat-a"}"#,
            )
            .unwrap();
            db.put_pr_cache("/w/b", "feat-b", "not json").unwrap();
        }
        let rows = svc.pr_status().await.unwrap();
        assert_eq!(rows.len(), 1, "the unparseable row is skipped");
        let r = &rows[0];
        assert_eq!(r.worktree, "/w/a");
        assert_eq!(r.branch, "feat-a");
        assert_eq!(r.number, 42);
        assert_eq!(r.state, "OPEN");
        assert_eq!(r.url, "https://forge/pr/42");
        assert!(r.is_draft);
        assert!(r.fetched_at > 0, "cache stamp survives the projection");
    }

    /// `notify.push` appends a tray row like any other producer: alert
    /// urgency lands on the red-flag kind, the default on the notice kind,
    /// the source falls back to "api", and the returned id is the row's.
    #[tokio::test]
    async fn notify_push_stores_a_tray_row_with_urgency_mapped_to_kind() {
        use thegn_core::store::NotificationStore;
        let (svc, _rx) = service(0);
        let id = svc
            .notify_push(thegn_svc::control::PushedNote {
                title: "build done".into(),
                body: "all green".into(),
                urgency: None,
                source: None,
            })
            .await
            .unwrap();
        let id2 = svc
            .notify_push(thegn_svc::control::PushedNote {
                title: "build broke".into(),
                body: String::new(),
                urgency: Some("alert".into()),
                source: Some("ci".into()),
            })
            .await
            .unwrap();
        assert_ne!(id, id2);

        let db = svc.db.lock().unwrap();
        let all = db.get_all_notifications(10).unwrap();
        assert_eq!(all.len(), 2);
        let normal = all.iter().find(|n| n.id == id).unwrap();
        assert_eq!(normal.kind.as_str(), "agent_done");
        assert_eq!(normal.source_ref, "api");
        assert_eq!(normal.message, "build done — all green");
        let alert = all.iter().find(|n| n.id == id2).unwrap();
        assert_eq!(alert.kind.as_str(), "agent_attention");
        assert_eq!(alert.source_ref, "ci");
        assert_eq!(alert.message, "build broke", "empty body ⇒ title only");
    }

    /// grace_ms == 0 ⇒ the never-reap default: an UNTIMED relay lease
    /// (`expires_at IS NULL`) and an `Opened` frame carrying `None`. The
    /// reaper's `plan_leases` skips null-expiry leases, so the session lives
    /// until it is explicitly killed or reattached.
    #[tokio::test]
    async fn idle_with_zero_grace_opens_untimed_lease() {
        let (svc, mut rx) = service(0);
        svc.on_session_idle("s1").await;

        let rows = leases(&svc);
        assert_eq!(rows.len(), 1, "exactly one relay lease");
        assert_eq!(rows[0].session_id, "s1");
        assert_eq!(rows[0].kind, "relay");
        assert_eq!(rows[0].expires_at, None, "grace 0 ⇒ untimed lease");

        let (session, kind, expires) = next_lease(&mut rx);
        assert_eq!(session, "s1");
        assert_eq!(kind, LeaseEventKind::Opened);
        assert_eq!(expires, None);
    }

    /// grace_ms > 0 ⇒ a timed relay lease whose `expires_at` is `now + grace`,
    /// and the `Opened` frame carries the same instant. A regression that
    /// inverts the `grace_ms > 0` guard (untimed when it should be timed, or
    /// vice versa) fails one of these two tests.
    #[tokio::test]
    async fn idle_with_positive_grace_opens_timed_lease() {
        let grace = 60_000; // 60s
        let before = now_ms();
        let (svc, mut rx) = service(grace);
        svc.on_session_idle("s1").await;
        let after = now_ms();

        let rows = leases(&svc);
        assert_eq!(rows.len(), 1);
        let exp = rows[0].expires_at.expect("grace > 0 ⇒ timed lease");
        assert!(
            exp >= before + grace && exp <= after + grace,
            "expires_at must be now+grace: {exp} not in [{}, {}]",
            before + grace,
            after + grace,
        );

        let (_, kind, frame_exp) = next_lease(&mut rx);
        assert_eq!(kind, LeaseEventKind::Opened);
        assert_eq!(frame_exp, Some(exp), "frame expiry matches the DB lease");
    }

    /// Re-detach refreshes: a second idle transition RELEASES the prior lease
    /// and PUTs a fresh one, so the session never accumulates duplicate leases
    /// (a leak that would keep resurrecting a reaped PTY).
    #[tokio::test]
    async fn re_idle_replaces_the_prior_lease() {
        let (svc, _rx) = service(60_000);
        svc.on_session_idle("s1").await;
        let first = leases(&svc);
        assert_eq!(first.len(), 1);
        svc.on_session_idle("s1").await;
        let second = leases(&svc);
        assert_eq!(second.len(), 1, "release-then-put keeps exactly one lease");
        assert_ne!(
            first[0].lease_id, second[0].lease_id,
            "the lease was replaced, not left stale"
        );
    }

    /// Attaching (or the session ending) makes it busy: the relay lease is
    /// released and a `Released` frame is emitted. This is the path that must
    /// cancel the grace period so a returning client's session is NOT reaped.
    #[tokio::test]
    async fn busy_releases_the_lease() {
        let (svc, mut rx) = service(60_000);
        svc.on_session_idle("s1").await;
        assert_eq!(leases(&svc).len(), 1);
        // best-effort: test drain: the opened frame is not what this test asserts on
        let _ = next_lease(&mut rx); // drain the Opened frame

        svc.on_session_busy("s1").await;
        assert!(leases(&svc).is_empty(), "busy releases the relay lease");

        let (session, kind, expires) = next_lease(&mut rx);
        assert_eq!(session, "s1");
        assert_eq!(kind, LeaseEventKind::Released);
        assert_eq!(expires, None);
    }

    /// The reap contract the `lease_loop` relies on, driven at the store level
    /// (the loop's DB half): a timed lease already past its expiry is returned
    /// by `reap_expired_leases` (⇒ the loop kills its PTY + emits `Reaped`),
    /// while an UNTIMED lease (grace 0) is never reaped. Inverting the grace
    /// guard would either reap sessions that should survive or leak abandoned
    /// ones — this pins the boundary.
    #[tokio::test]
    async fn expired_lease_is_reaped_untimed_is_not() {
        let (svc, _rx) = service(60_000);
        // A timed lease already expired 1s ago.
        {
            let db = svc.db.lock().unwrap();
            let past = now_ms() - 1000;
            db.put_lease("expired", &svc.daemon_id, None, "relay", Some(past), past)
                .unwrap();
            // An untimed (never-reap) lease.
            db.put_lease("forever", &svc.daemon_id, None, "relay", None, now_ms())
                .unwrap();
        }
        let reaped = {
            let db = svc.db.lock().unwrap();
            db.reap_expired_leases(&svc.daemon_id, now_ms()).unwrap()
        };
        assert_eq!(reaped.len(), 1, "only the expired timed lease is reaped");
        assert_eq!(reaped[0].session_id, "expired");
        let remaining = leases(&svc);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_id, "forever", "untimed lease survives");
    }

    /// Insert a live-looking session entry backed by a stub actor task that
    /// answers `Attach` with an empty snapshot — enough to exercise the
    /// service-level attach path (lease bookkeeping included) without a PTY.
    async fn insert_stub_session(svc: &DaemonService, id: &str) {
        let (msg_tx, mut msg_rx) = mpsc::channel(8);
        let meta = SessionMeta {
            id: id.into(),
            worktree: None,
            program: "sh".into(),
            cwd: None,
            created_at_ms: 0,
            pid: None,
            forked_from: None,
        };
        let live = Arc::new(Mutex::new(LiveMeta {
            rows: 24,
            cols: 80,
            attached: 0,
            ..Default::default()
        }));
        svc.sessions.lock().await.insert(
            id.to_string(),
            SessionEntry {
                msg_tx,
                meta,
                live,
                recipe: None,
            },
        );
        tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                if let SessionMsg::Attach { reply, .. } = msg {
                    let (_tx, rx) = mpsc::channel(1);
                    // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    let _ = reply.send(Ok(AttachReply {
                        snapshot: EventFrame::PaneSnapshot {
                            session: "stub".into(),
                            seq: 0,
                            cols: 80,
                            rows: 24,
                            bytes: vec![],
                        },
                        frames: rx,
                    }));
                }
            }
        });
    }

    /// The Observer contract at the service seam: an observer attach must NOT
    /// release (cancel) the relay lease keeping a detached session in grace;
    /// an interactive attach must.
    #[tokio::test]
    async fn observer_attach_leaves_the_relay_lease_open() {
        let (svc, _rx) = service(60_000);
        insert_stub_session(&svc, "s1").await;
        svc.on_session_idle("s1").await;
        assert_eq!(leases(&svc).len(), 1, "detached session holds its lease");

        svc.attach("obs", "s1", AttachKind::Observer, 24, 80, true)
            .await
            .expect("observer attach");
        assert_eq!(
            leases(&svc).len(),
            1,
            "an Observer must not cancel the relay grace"
        );

        svc.attach("int", "s1", AttachKind::Interactive, 24, 80, true)
            .await
            .expect("interactive attach");
        assert!(
            leases(&svc).is_empty(),
            "an interactive attach cancels the relay grace"
        );
    }

    /// `publish_pairing` lands on the broadcast feed — the producer half of
    /// the pairing lifecycle events the HTTP handlers emit.
    #[tokio::test]
    async fn publish_pairing_reaches_the_event_feed() {
        let (svc, mut rx) = service(0);
        svc.publish_pairing("p1", "phone", "read,git", PairingState::Requested);
        loop {
            let frame = rx.try_recv().expect("a pairing frame was emitted");
            if let EventFrame::Pairing {
                pairing_id,
                label,
                scope,
                state,
            } = &*frame
            {
                assert_eq!(pairing_id, "p1");
                assert_eq!(label, "phone");
                assert_eq!(scope, "read,git");
                assert_eq!(*state, PairingState::Requested);
                return;
            }
        }
    }

    /// The daemon WS warm-attach pipeline, end to end and in process: a real
    /// `DaemonService` behind the real axum router on a real unix socket,
    /// attached through the real `ControlClient` WS path. Locks the seq
    /// contract (first frame after `Hello` is the snapshot; the first delta is
    /// `snapshot.seq + 1`), input echo, and the exit frame on kill.
    #[tokio::test(flavor = "multi_thread")]
    async fn ws_warm_attach_pipeline_over_a_real_socket() {
        use thegn_svc::control::client::{AttachControl, ControlAddr, ControlClient};

        async fn next_frame(rx: &mut mpsc::Receiver<EventFrame>) -> EventFrame {
            tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("frame within 10s")
                .expect("stream open")
        }

        let dir = std::env::temp_dir().join(format!("thegn-daemon-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("d.sock");
        let ep = thegn_svc::ipc::IpcEndpoint::for_socket_path(&sock);
        let listener = match thegn_svc::ipc::IpcListener::bind_exclusive(&ep)
            .await
            .unwrap()
        {
            thegn_svc::ipc::BindOutcome::Bound(l) => l,
            thegn_svc::ipc::BindOutcome::AlreadyRunning => panic!("fresh socket must bind"),
        };

        let (svc, _events) = service(0);
        let svc = Arc::new(svc);
        let state = thegn_svc::control::http::ControlState {
            api: svc.clone(),
            store: svc.db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: true,
            require_approval: false,
            server_label: "test thegn".into(),
            cors_origins: Vec::new(),
        };
        let app = thegn_svc::control::http::router(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await; // best-effort: test scaffolding: a dead server fails the client assertions below
        });

        let client = ControlClient::new(ControlAddr::Unix(sock.clone()));
        // Resolve `cat` via PATH — `/bin/cat` doesn't exist on NixOS (no FHS
        // /bin except /bin/sh); this keeps the test portable across distros/CI.
        let cat = thegn_core::util::which_path("cat").unwrap_or_else(|| "/bin/cat".into());
        let info = client
            .open(&OpenSpec {
                argv: vec![cat],
                cwd: None,
                env: vec![],
                rows: 24,
                cols: 80,
                worktree: None,
                ..Default::default()
            })
            .await
            .expect("open a session over the socket");

        let mut stream = client
            .attach(&info.id, "itest", 24, 80, false)
            .await
            .expect("warm-attach over WS");

        // (a) Greeting, then the snapshot; the next delta continues its seq.
        match next_frame(&mut stream.frames).await {
            EventFrame::Hello(h) => {
                assert_eq!(h.proto, thegn_core::control_wire::PROTO_VERSION);
            }
            other => panic!("first frame must be Hello, got {other:?}"),
        }
        let snap_seq = match next_frame(&mut stream.frames).await {
            EventFrame::PaneSnapshot { seq, session, .. } => {
                assert_eq!(session, info.id);
                seq
            }
            other => panic!("second frame must be the warm snapshot, got {other:?}"),
        };

        // (b) Input echoes back through `cat`; the first delta is seq + 1.
        stream
            .control
            .send(AttachControl::Input(b"marker\n".to_vec()))
            .await
            .expect("control channel open");
        let mut echoed: Vec<u8> = Vec::new();
        let mut first_delta_seq = None;
        while !String::from_utf8_lossy(&echoed).contains("marker") {
            match next_frame(&mut stream.frames).await {
                EventFrame::PaneDelta { seq, bytes, .. } => {
                    if first_delta_seq.is_none() {
                        first_delta_seq = Some(seq);
                    }
                    echoed.extend_from_slice(&bytes);
                }
                other => panic!("expected deltas, got {other:?}"),
            }
        }
        assert_eq!(
            first_delta_seq,
            Some(snap_seq + 1),
            "the first live delta continues the snapshot's sequence"
        );

        // (c) Kill ends the session; the attach stream reports the exit.
        client.kill(&info.id).await.expect("kill over the socket");
        loop {
            match next_frame(&mut stream.frames).await {
                EventFrame::SessionExit { session, .. } => {
                    assert_eq!(session, info.id);
                    break;
                }
                EventFrame::PaneDelta { .. } | EventFrame::PaneSnapshot { .. } => continue,
                other => panic!("unexpected frame while awaiting exit: {other:?}"),
            }
        }

        server.abort();
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// Exercise the actual control-plane fork through `ControlApi`. This keeps
    /// the source alive while checking that resize state reaches the child,
    /// identity and handoff data cross only through the child environment,
    /// adopt placement is recorded, and validation/dead-session failures do
    /// not create another daemon entry.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn fork_control_path_inherits_geometry_and_cleans_handoff() {
        use std::os::unix::fs::PermissionsExt;
        use thegn_core::store::IntentStore;

        let state = tempfile::tempdir().expect("isolated state dir");
        let _env = crate::testenv::EnvVarGuard::set(&[(
            "XDG_STATE_HOME",
            state.path().to_str().expect("state path"),
        )]);
        let (svc, _events) = service(0);
        let sh = thegn_core::util::which_path("sh").unwrap_or_else(|| "/bin/sh".into());
        let script = "i=0; while [ $i -lt 2005 ]; do printf 'source-line-%s\\n' $i; i=$((i+1)); done; if [ -n \"$THEGN_FORKED_FROM\" ]; then printf 'forked=%s\\nscrollback=%s\\n' \"$THEGN_FORKED_FROM\" \"$THEGN_FORK_SCROLLBACK\"; fi; while :; do sleep 1; done";
        let source = svc
            .open(OpenSpec {
                argv: vec![sh, "-c".into(), script.into()],
                rows: 24,
                cols: 80,
                ..Default::default()
            })
            .await
            .expect("open source");
        svc.resize(&source.id, 41, 137)
            .await
            .expect("resize source");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snapshot = svc.snapshot(&source.id).await.expect("source lives");
            if let EventFrame::PaneSnapshot {
                rows, cols, bytes, ..
            } = snapshot
                && rows == 41
                && cols == 137
                && String::from_utf8_lossy(&bytes).contains("source-line-2004")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "source output/resize arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let child = svc
            .fork(ForkSpec {
                session: source.id.clone(),
                scrollback: true,
                adopt: true,
                tab: true,
                ..Default::default()
            })
            .await
            .expect("fork source");
        assert_ne!(child.id, source.id, "fork allocates a new daemon id");
        assert_ne!(child.pid, source.pid, "fork allocates a new child pid");
        assert_eq!((child.rows, child.cols), (41, 137));
        assert_eq!(child.forked_from.as_deref(), Some(source.id.as_str()));

        let handoff = state
            .path()
            .join("thegn/forks")
            .join(format!("{}.txt", child.id));
        let history = std::fs::read_to_string(&handoff).expect("handoff exists");
        assert!(history.lines().count() <= 2_000, "snapshot bound is shared");
        assert!(history.contains("source-line-2004"));
        assert_eq!(
            std::fs::metadata(&handoff).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let intents = svc
            .db
            .lock()
            .unwrap()
            .take_intents("adopt_session")
            .unwrap();
        assert_eq!(intents.len(), 1);
        assert!(intents[0].payload.contains(&child.id));
        assert!(intents[0].payload.contains("\"tab\":true"));

        let mut child_text = String::new();
        let child_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !child_text.contains(&format!("forked={}", source.id)) {
            if let EventFrame::PaneSnapshot { bytes, .. } =
                svc.snapshot(&child.id).await.expect("child lives")
            {
                child_text = String::from_utf8_lossy(&bytes).into_owned();
            }
            assert!(
                std::time::Instant::now() < child_deadline,
                "child identity output arrived"
            );
        }
        assert!(child_text.contains(&format!("scrollback={}", handoff.display())));
        svc.kill(&child.id).await.expect("kill child");
        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while handoff.exists() {
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "handoff cleaned on exit"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let before = svc
            .list_sessions()
            .await
            .expect("list after child exit")
            .len();
        let invalid = svc
            .fork(ForkSpec {
                session: "native-id".into(),
                harness: Some("pi".into()),
                ..Default::default()
            })
            .await
            .expect_err("reserved harness must not spawn");
        assert!(invalid.to_string().contains("reserved"));
        assert_eq!(
            svc.list_sessions()
                .await
                .expect("list after validation")
                .len(),
            before
        );

        svc.kill(&source.id).await.expect("kill source");
        let dead_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if svc
                .list_sessions()
                .await
                .expect("list after source exit")
                .iter()
                .any(|row| row.id == source.id && row.exited_at_ms.is_some())
            {
                break;
            }
            assert!(
                std::time::Instant::now() < dead_deadline,
                "source tombstone published"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let dead = svc
            .fork(ForkSpec {
                session: source.id,
                ..Default::default()
            })
            .await
            .expect_err("dead session must not fork");
        assert!(dead.to_string().contains("sessions.open"));
        // `EnvVarGuard` restores the caller's state root after all daemon work
        // has stopped, so this test never touches the normal profile.
    }

    /// The transport-retry observer's contract (THE-86), driven as a stub: a
    /// synthetic nonzero exit on a headless row's session, classified through
    /// the real tombstone + db + pure core, with NO harness and NO relaunch.
    /// Every outcome stamps `waiting_human` + a note; the observer NEVER
    /// writes `done`/`failed` (it can park a row but never finish one).
    #[tokio::test]
    async fn transport_retry_observer_stamps_waiting_human_and_note_never_terminal() {
        use crate::daemon::pipeline_retry;
        use thegn_core::control_wire::EventFrame;
        use thegn_core::issue::{AgentDispatchStatus as St, NewDispatch};
        use thegn_core::store::NotificationStore;

        let (svc, _rx) = service(0);
        // A headless pipeline row mid-flight, run by session "s-retry".
        let row_id = {
            let db = svc.db.lock().unwrap();
            db.put_agent_dispatch(NewDispatch {
                session_id: Some("s-retry"),
                stage: Some("code"),
                ..NewDispatch::new("linear:THE-86", "/wt/86", "aider")
            })
            .unwrap()
        };

        // The corpse: a transport-failure final screen, nobody attached. (The
        // agent is aider — no CONTINUE cap — so even a retry decision cannot
        // reach the relaunch path; the stamps are what this test pins.)
        let tomb = super::super::tombstone::Tombstone {
            attached: 0,
            final_screen: EventFrame::PaneSnapshot {
                session: "s-retry".into(),
                seq: 0,
                cols: 80,
                rows: 24,
                bytes: b"Connection error. SDK retry budget exhausted".to_vec(),
            },
            ..super::super::tombstone::tests::tomb("s-retry", Some(1))
        };
        svc.tombs
            .lock()
            .await
            .insert("s-retry".into(), tomb, now_ms());

        // A LIMIT exit parks immediately, at any attempt — never relaunches.
        let tomb = super::super::tombstone::Tombstone {
            final_screen: EventFrame::PaneSnapshot {
                session: "s-retry".into(),
                seq: 0,
                cols: 80,
                rows: 24,
                bytes: b"You have hit your weekly limit".to_vec(),
            },
            ..super::super::tombstone::tests::tomb("s-limit", Some(1))
        };
        let limit_row = {
            let db = svc.db.lock().unwrap();
            db.put_agent_dispatch(NewDispatch {
                session_id: Some("s-limit"),
                stage: Some("code"),
                ..NewDispatch::new("linear:THE-86", "/wt/86", "aider")
            })
            .unwrap()
        };
        svc.tombs
            .lock()
            .await
            .insert("s-limit".into(), tomb, now_ms());

        // Drive both synthetic exits through the observer's real path.
        let mut attempts = std::collections::HashMap::new();
        pipeline_retry::handle_exit(&svc, "s-retry", 1, &mut attempts)
            .await
            .expect("transport exit handled");
        pipeline_retry::handle_exit(&svc, "s-limit", 1, &mut attempts)
            .await
            .expect("limit exit handled");

        let (row, limit) = {
            let db = svc.db.lock().unwrap();
            (
                db.get_dispatch(row_id).unwrap().unwrap(),
                db.get_dispatch(limit_row).unwrap().unwrap(),
            )
        };
        assert_eq!(row.status, St::WaitingHuman);
        assert!(row.note.is_some(), "the attempt note is the durable ledger");
        assert_ne!(row.status, St::Done, "the observer never finishes a row");
        assert_ne!(row.status, St::Failed, "the observer never fails a row");
        assert_eq!(limit.status, St::WaitingHuman);
        assert!(
            limit
                .note
                .as_deref()
                .unwrap_or_default()
                .starts_with("limit: ")
        );
    }

    /// A verdict the Lead writes on the row DURING the backoff sleep is newer
    /// than the retry plan and must win: the observer re-reads the row after
    /// the sleep, skips the relaunch, and never forces the row back to
    /// `running` (THE-86 review fix — the pre-fix race let a `done` stamped in
    /// the backoff window be clobbered into a second agent).
    #[tokio::test]
    async fn transport_retry_relaunch_skips_a_row_re_driven_during_backoff() {
        use crate::daemon::pipeline_retry;
        use thegn_core::control_wire::EventFrame;
        use thegn_core::issue::{AgentDispatchStatus as St, NewDispatch};
        use thegn_core::store::NotificationStore;

        let mut cfg = thegn_core::config::Config::default();
        cfg.pipeline.transport_retry.backoff_ms = 150;
        let (svc, _rx) = service_with_config(0, cfg);
        // claude carries the CONTINUE cap, so a transport exit reaches the
        // Retry arm (park → sleep → re-check → relaunch) for real.
        let row_id = {
            let db = svc.db.lock().unwrap();
            db.put_agent_dispatch(NewDispatch {
                session_id: Some("s-race"),
                stage: Some("code"),
                ..NewDispatch::new("linear:THE-86", "/wt/86", "claude")
            })
            .unwrap()
        };
        let tomb = super::super::tombstone::Tombstone {
            attached: 0,
            final_screen: EventFrame::PaneSnapshot {
                session: "s-race".into(),
                seq: 0,
                cols: 80,
                rows: 24,
                bytes: b"Connection error. SDK retry budget exhausted".to_vec(),
            },
            ..super::super::tombstone::tests::tomb("s-race", Some(1))
        };
        svc.tombs
            .lock()
            .await
            .insert("s-race".into(), tomb, now_ms());

        // The concurrent verdict: the instant the observer parks the row, the
        // Lead closes it `done`. The park IS the signal the backoff sleep has
        // begun.
        let flipper = {
            let db_handle = svc.db.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    {
                        let db = db_handle.lock().unwrap();
                        if db.get_dispatch(row_id).unwrap().unwrap().status == St::WaitingHuman {
                            db.update_dispatch_status(row_id, St::Done).unwrap();
                            break;
                        }
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the observer never parked the row"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
        };

        let mut attempts = std::collections::HashMap::new();
        pipeline_retry::handle_exit(&svc, "s-race", 1, &mut attempts)
            .await
            .expect("transport exit handled");
        flipper.await.unwrap();

        let row = {
            let db = svc.db.lock().unwrap();
            db.get_dispatch(row_id).unwrap().unwrap()
        };
        assert_eq!(row.status, St::Done, "the Lead's verdict must survive");
        assert!(
            attempts.is_empty(),
            "a skipped relaunch holds no retry budget"
        );
        let note = row.note.unwrap_or_default();
        assert!(note.starts_with("transport: "), "{note}");
        assert!(!note.contains("relaunch failed"), "{note}");
    }
}

#[cfg(test)]
mod identity_env_tests {
    use super::session_identity_env;

    #[test]
    fn session_env_carries_id_and_endpoint_and_wins_over_caller() {
        let env = vec![
            ("FOO".to_string(), "bar".to_string()),
            ("THEGN_SESSION_ID".to_string(), "spoofed".to_string()),
        ];
        let out = session_identity_env("abc123", "/run/x/daemon.sock", &env);
        assert!(out.contains(&("FOO".into(), "bar".into())));
        assert!(out.contains(&("THEGN_SESSION_ID".into(), "abc123".into())));
        assert!(out.contains(&("THEGN_CONTROL_SOCKET".into(), "/run/x/daemon.sock".into())));
        assert_eq!(
            out.iter().filter(|(k, _)| k == "THEGN_SESSION_ID").count(),
            1
        );
    }
}
