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

use superzej_core::control::relay_expiry;
use superzej_core::control_wire::{EventFrame, LeaseEventKind};
use superzej_core::db::Db;
use superzej_core::store::{ControlStore, IntentStore, LeaseRow};
use superzej_svc::control::{
    AttachKind, AttachReply, BrowserCommand, ControlApi, ControlError, ControlResult,
    GitFileStatus, OpenSpec, SessionInfo,
};
use superzej_svc::git::{CliGit, CommitOps, GitBackend};

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
    pub(crate) async fn on_session_idle(&self, session: &str) {
        let daemon_id = self.daemon_id.clone();
        let sid = session.to_string();
        let expires = relay_expiry(now_ms(), self.grace_ms);
        let put = self
            .with_db(move |db| {
                // Replace any prior lease for this session (re-detach refreshes).
                db.release_session_leases(&sid)?;
                db.put_lease(&sid, &daemon_id, None, "relay", Some(expires), now_ms())?;
                Ok(())
            })
            .await;
        if put.is_ok() {
            self.emit(EventFrame::Lease {
                session: session.to_string(),
                kind: LeaseEventKind::Opened,
                expires_at: Some(expires),
            });
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
            tracing::debug!(target: "szhost::daemon", argv = ?spec.argv, cwd = ?spec.cwd, "open session");
            let rows = spec.rows.max(1);
            let cols = spec.cols.max(1);
            let (pane_tx, pane_rx) = mpsc::channel(256);
            let cwd = spec.cwd.as_ref().map(std::path::PathBuf::from);
            let pty = crate::pane_pty::open_pty(
                0, // per-session channel: the id tag is unused
                &spec.argv,
                cwd.as_deref(),
                &spec.env,
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
            tokio::spawn(actor.run(pane_rx, msg_rx));

            let info = {
                let live = live.lock().expect("live meta lock");
                meta.info(&live, None)
            };
            self.sessions
                .lock()
                .await
                .insert(id, SessionEntry { msg_tx, meta, live });
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
    ) -> BoxFuture<'a, ControlResult<AttachReply>> {
        Box::pin(async move {
            let tx = self.entry_tx(session).await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(SessionMsg::Attach {
                client_id: client_id.to_string(),
                kind,
                rows,
                cols,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ControlError::NotFound(format!("session {session}")))?;
            let reply = reply_rx
                .await
                .map_err(|_| ControlError::NotFound(format!("session {session}")))??;
            // Attaching cancels the relay grace period.
            self.on_session_busy(session).await;
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
            // Same channel `szhost open` uses: the v37 intents mailbox, drained
            // by a running compositor (~1s). Branch selection is a compositor
            // concern; the intent carries the repo target.
            let repo = repo.to_string();
            self.with_db(move |db| {
                let payload = serde_json::to_string(&superzej_core::models::FocusIntent { repo })?;
                db.put_intent("focus_workspace", &payload)?;
                Ok(())
            })
            .await
        })
    }

    fn drive_browser(&self, _cmd: BrowserCommand) -> BoxFuture<'_, ControlResult<()>> {
        Box::pin(async move { Err(ControlError::Unimplemented("drive-browser")) })
    }

    fn git_status<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<GitFileStatus>>> {
        Box::pin(async move {
            let wt = worktree.to_string();
            tokio::task::spawn_blocking(move || {
                let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
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
            let wt = worktree.to_string();
            let paths = paths.to_vec();
            tokio::task::spawn_blocking(move || {
                let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
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
            let wt = worktree.to_string();
            let message = message.to_string();
            tokio::task::spawn_blocking(move || {
                let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
                CliGit.commit(&loc, &message, false, None)?;
                // The new HEAD is the commit we just made (git_cmd scrubs
                // GIT_* env; inside spawn_blocking, so the wait is off-loop).
                #[expect(
                    clippy::disallowed_methods,
                    reason = "inside spawn_blocking — off-loop child wait is the sanctioned pattern"
                )]
                let out = superzej_core::util::git_cmd(std::path::Path::new(&wt))
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

    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>> {
        Box::pin(async move {
            let daemon_id = self.daemon_id.clone();
            self.with_db(move |db| db.leases(&daemon_id)).await
        })
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
