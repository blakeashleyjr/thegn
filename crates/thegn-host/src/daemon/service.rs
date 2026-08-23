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

use thegn_core::control::relay_expiry;
use thegn_core::control_wire::{EventFrame, LeaseEventKind, PairingState};
use thegn_core::db::Db;
use thegn_core::store::{ControlStore, IntentStore, LeaseRow};
use thegn_svc::control::{
    AttachKind, AttachReply, BrowserCommand, ControlApi, ControlError, ControlResult,
    GitFileStatus, OpenSpec, SessionInfo, WaitCondition, WaitOutcome,
};
use thegn_svc::git::{CliGit, CommitOps, GitBackend};

use super::session::{IdleTransition, LiveMeta, SessionActor, SessionMeta, SessionMsg};

/// One live session in the daemon's table.
pub(crate) struct SessionEntry {
    pub msg_tx: mpsc::Sender<SessionMsg>,
    pub meta: SessionMeta,
    pub live: Arc<Mutex<LiveMeta>>,
}

/// Shared handle to the daemon's SQLite connection (the proxy's `SharedDb`
/// pattern: one connection, short critical sections, used off-runtime via
/// `spawn_blocking`).
pub(crate) type SharedDb = Arc<Mutex<Db>>;

pub(crate) struct DaemonService {
    pub daemon_id: String,
    pub sessions: Arc<tokio::sync::Mutex<HashMap<String, SessionEntry>>>,
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

fn fresh_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("csprng for session id");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl DaemonService {
    /// Run `f` against the shared DB on a blocking thread.
    async fn with_db<T, F>(&self, f: F) -> ControlResult<T>
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

    fn emit(&self, frame: EventFrame) {
        let _ = self.events.send(Arc::new(frame));
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
            out.sort_by_key(|s| s.created_at_ms);
            Ok(out)
        })
    }

    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        Box::pin(async move {
            if spec.argv.is_empty() {
                return Err(ControlError::Conflict("empty argv".into()));
            }
            let id = fresh_id();
            tracing::debug!(target: "thegn::daemon", argv = ?spec.argv, cwd = ?spec.cwd, "open session");
            let rows = spec.rows.max(1);
            let cols = spec.cols.max(1);
            let (pane_tx, pane_rx) = mpsc::channel(256);
            let cwd = spec.cwd.as_ref().map(std::path::PathBuf::from);
            let env = session_identity_env(&id, &self.endpoint, &spec.env);
            let pty = crate::pane_pty::open_pty(
                0, // per-session channel: the id tag is unused
                &spec.argv,
                cwd.as_deref(),
                &env,
                rows,
                cols,
                pane_tx,
                None, // a daemon has no render loop to wake
                None, // ...and no grid — no off-thread feed sink
            )
            .map_err(ControlError::Internal)?;

            let meta = SessionMeta {
                id: id.clone(),
                worktree: spec.worktree.clone(),
                program: crate::pane::program_name(&spec.argv),
                cwd: spec.cwd.clone(),
                created_at_ms: now_ms(),
                pid: pty.pid,
            };
            let live = Arc::new(Mutex::new(LiveMeta {
                rows,
                cols,
                attached: 0,
            }));
            let (msg_tx, msg_rx) = mpsc::channel(64);
            let actor = SessionActor::new(
                meta.clone(),
                live.clone(),
                pty,
                rows,
                cols,
                self.events.clone(),
                self.idle_tx.clone(),
                self.sessions.clone(),
            );
            let info = {
                let live = live.lock().expect("live meta lock");
                meta.info(&live, None)
            };
            // Insert the entry BEFORE spawning the actor. The actor's teardown
            // removes its own entry (session.rs) — if the child exits instantly
            // (exec failure / `sh -c true`) and the actor is scheduled first, a
            // spawn-then-insert order runs the remove before the insert, leaving
            // a PHANTOM entry for a dead actor: listed forever, `kill` no-ops, and
            // idle-exit (busy = sessions non-empty) never fires. Inserting first
            // guarantees the teardown removal always observes the entry.
            self.sessions
                .lock()
                .await
                .insert(id, SessionEntry { msg_tx, meta, live });
            tokio::spawn(actor.run(pane_rx, msg_rx));
            self.emit(EventFrame::Sessions);
            Ok(info)
        })
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
            let tx = self.entry_tx(session).await?;
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
            let tx = self.entry_tx(session).await?;
            tx.send(SessionMsg::Stdin(bytes))
                .await
                .map_err(|_| ControlError::NotFound(format!("session {session}")))
        })
    }

    fn resize<'a>(
        &'a self,
        session: &'a str,
        rows: u16,
        cols: u16,
    ) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            let tx = self.entry_tx(session).await?;
            tx.send(SessionMsg::Resize { rows, cols })
                .await
                .map_err(|_| ControlError::NotFound(format!("session {session}")))
        })
    }

    fn snapshot<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<EventFrame>> {
        Box::pin(async move {
            let tx = self.entry_tx(session).await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(SessionMsg::Snapshot { reply: reply_tx })
                .await
                .map_err(|_| ControlError::NotFound(format!("session {session}")))?;
            reply_rx
                .await
                .map_err(|_| ControlError::NotFound(format!("session {session}")))
        })
    }

    fn kill<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<()>> {
        Box::pin(async move {
            let tx = self.entry_tx(session).await?;
            let _ = tx.send(SessionMsg::Kill).await;
            self.on_session_busy(session).await; // drop any lease with it
            Ok(())
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
            match cond {
                // Event-driven, never polled: subscribe BEFORE confirming the
                // session is live so no exit event is missed in the gap, then
                // block on the feed until the target session exits.
                WaitCondition::Exited => {
                    let mut rx = self.events.subscribe();
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
                // Activity-derived + output-match conditions need the per-pane
                // state feed (B‑3 exposure) / attach delta stream — staged.
                WaitCondition::Idle
                | WaitCondition::Blocked
                | WaitCondition::Done
                | WaitCondition::OutputMatches { .. } => Err(ControlError::Unimplemented(
                    "wait on activity/output condition",
                )),
            }
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

    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>> {
        Box::pin(async move {
            let daemon_id = self.daemon_id.clone();
            self.with_db(move |db| db.leases(&daemon_id)).await
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
        let (events, rx) = broadcast::channel(64);
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel();
        let svc = DaemonService {
            daemon_id: "d0".into(),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            events,
            db: Arc::new(Mutex::new(Db::open_memory().expect("in-memory db"))),
            grace_ms,
            idle_tx,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            config: std::sync::Arc::new(thegn_core::config::Config::default()),
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
        };
        let live = Arc::new(Mutex::new(LiveMeta {
            rows: 24,
            cols: 80,
            attached: 0,
        }));
        svc.sessions
            .lock()
            .await
            .insert(id.to_string(), SessionEntry { msg_tx, meta, live });
        tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                if let SessionMsg::Attach { reply, .. } = msg {
                    let (_tx, rx) = mpsc::channel(1);
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
        let _ = std::fs::remove_dir_all(&dir);
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
        };
        let app = thegn_svc::control::http::router(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = ControlClient::new(ControlAddr::Unix(sock.clone()));
        // The child is only an echo target. On unix that is `cat`, resolved as
        // a POSIX utility rather than off bare `PATH` (`/bin/cat` doesn't
        // exist on NixOS — no FHS /bin except /bin/sh). Not on Windows: the
        // MSYS `cat.exe` git ships never echoes under ConPTY (cygwin drives
        // the console itself), while the native shell does.
        let echoer = if cfg!(windows) {
            "cmd.exe".to_string()
        } else {
            thegn_core::util::posix_util("cat").expect("a cat to pipe through")
        };
        let info = client
            .open(&OpenSpec {
                argv: vec![echoer],
                cwd: None,
                env: vec![],
                rows: 24,
                cols: 80,
                worktree: None,
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

        // (b) Input echoes back through the child; the first delta is seq + 1.
        //
        // Enter is CR on the wire, not LF: a unix pty maps CR→NL for the
        // reader (ICRNL), and ConPTY recognises only CR as the Return key.
        //
        // ConPTY also greets every session with a handshake ending in a DSR
        // cursor query (`ESC[6n`) and stalls the child until a terminal
        // answers. In the product that answer comes from the attached pane's
        // emulator; this raw client has to play terminal itself, or nothing it
        // types is ever delivered.
        let mut echoed: Vec<u8> = Vec::new();
        let mut first_delta_seq = None;
        let mut typed = false;
        loop {
            let unblocked = !cfg!(windows) || echoed.windows(4).any(|w| w == b"\x1b[6n");
            if !typed && unblocked {
                if cfg!(windows) {
                    stream
                        .control
                        .send(AttachControl::Input(b"\x1b[1;1R".to_vec()))
                        .await
                        .expect("control channel open");
                }
                stream
                    .control
                    .send(AttachControl::Input(b"marker\r".to_vec()))
                    .await
                    .expect("control channel open");
                typed = true;
            }
            if typed && String::from_utf8_lossy(&echoed).contains("marker") {
                break;
            }
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
        let _ = std::fs::remove_dir_all(&dir);
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
