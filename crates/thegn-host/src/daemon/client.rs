//! Client side of the pane daemon: discovery / lazy spawn, and the
//! [`ExecSource`] adapter that lets a compositor pane be daemon-backed through
//! the exact machinery provider panes already use (`PaneIo::Stream` +
//! `relay_exec`'s reconnect ladder).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::future::BoxFuture;
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::config::DaemonConfig;
use thegn_core::control_wire::EventFrame;
use thegn_core::db::Db;
use thegn_svc::control::client::{AttachControl, AttachStream, ControlAddr, ControlClient};
use thegn_svc::control::{OpenSpec, SessionInfo};
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec};

use crate::pane_source::ExecSource;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Find a live daemon for this state dir WITHOUT spawning one: registry
/// discovery, then a probe of the configured socket. `None` = no daemon is
/// running (so there is nothing to kill/list — callers must not spawn one as
/// a side effect).
pub(crate) async fn connect_daemon(dcfg: &DaemonConfig) -> Option<ControlClient> {
    let scope = super::scope_key();
    // 1. Registry discovery (freshest live heartbeat), verified by connect.
    let discovered = tokio::task::spawn_blocking({
        let scope = scope.clone();
        move || {
            let db = Db::open().ok()?;
            thegn_svc::control::client::discover(&db, &scope, now_ms())
        }
    })
    .await
    .ok()
    .flatten();
    if let Some(addr) = discovered {
        let client = ControlClient::new(addr);
        if client.health().await.is_ok() {
            return Some(client);
        }
    }

    // 2. The configured socket may host a daemon the registry missed (e.g. a
    //    fresh DB): probe it before giving up.
    let sock = super::socket_path(dcfg);
    let client = ControlClient::new(ControlAddr::Unix(sock));
    if client.health().await.is_ok() {
        return Some(client);
    }
    None
}

/// Find a live daemon for this state dir, or spawn one detached and wait for
/// its socket. The registry row is a hint; a successful `/health` round-trip
/// is the truth.
pub(crate) async fn ensure_daemon(dcfg: &DaemonConfig) -> Result<ControlClient> {
    if let Some(client) = connect_daemon(dcfg).await {
        return Ok(client);
    }
    let sock = super::socket_path(dcfg);
    let client = ControlClient::new(ControlAddr::Unix(sock.clone()));

    // 3. Spawn detached (own process group, null stdio — the compositor must
    //    not adopt the daemon on its tty) and wait for the socket. The daemon
    //    binds the socket as its lock, so a spawn race resolves itself: the
    //    loser exits 0 and both clients connect to the winner.
    let exe = std::env::current_exe().context("current_exe for daemon spawn")?;
    let mut cmd = thegn_core::util::detached(&exe.to_string_lossy());
    cmd.arg("daemon").arg("--socket").arg(&sock);
    cmd.spawn().context("spawn pane daemon")?;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if client.health().await.is_ok() {
            return Ok(client);
        }
    }
    Err(anyhow!(
        "pane daemon did not come up on {} within 3s",
        sock.display()
    ))
}

/// A daemon-backed exec source for one worktree's panes. `sandbox_id` on the
/// [`ExecSource`] contract maps to the daemon session id; persistence reuses
/// the existing `pane_sessions` capture verbatim with `provider = "daemon"`.
pub(crate) struct DaemonSource {
    pub client: ControlClient,
    /// Worktree hint recorded on opened sessions (listing/grouping).
    pub worktree: Option<String>,
}

impl DaemonSource {
    async fn open_and_attach(&self, spec: &ExecSpec) -> Result<ExecSession> {
        let info: SessionInfo = self
            .client
            .open(&OpenSpec {
                argv: spec.argv.clone(),
                cwd: spec.cwd.clone(),
                env: spec.env.clone(),
                rows: spec.rows,
                cols: spec.cols,
                worktree: self.worktree.clone(),
            })
            .await?;
        self.attach_session(&info.id, spec.cols, spec.rows).await
    }

    async fn attach_session(&self, session: &str, cols: u16, rows: u16) -> Result<ExecSession> {
        let client_id = format!("compositor-{}", std::process::id());
        let stream = self
            .client
            .attach(session, &client_id, rows, cols, false)
            .await?;
        Ok(adapt(session.to_string(), stream))
    }
}

impl DaemonSource {
    /// The session's PTY child pid from the daemon's listing. One extra local
    /// HTTP round-trip per (re)connect — attach/open are rare, and the pid is
    /// what makes `/proc`-based cwd/cmd capture work for daemon panes.
    async fn lookup_pid(&self, session: &str) -> Option<u32> {
        let sessions = self.client.sessions().await.ok()?;
        sessions.iter().find(|s| s.id == session)?.pid
    }
}

impl ExecSource for DaemonSource {
    fn open<'a>(&'a self, spec: &'a ExecSpec) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(self.open_and_attach(spec))
    }

    fn attach<'a>(
        &'a self,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(self.attach_session(session, cols, rows))
    }

    fn kill_session<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.client.kill(session))
    }

    fn session_pid<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Option<u32>> {
        Box::pin(self.lookup_pid(session))
    }
}

/// Bridge an [`AttachStream`] (decoded control-wire frames) to the pane
/// machinery's [`ExecSession`] shape: snapshot and deltas both become raw
/// `Stdout` bytes (the snapshot is an ANSI repaint — the emulator applies it
/// like any output), `SessionExit` becomes `Exit`, and stdin/resize/close map
/// onto the attach control channel.
fn adapt(session_id: String, stream: AttachStream) -> ExecSession {
    let AttachStream {
        mut frames,
        control,
    } = stream;
    let (out_tx, out_rx) = tokio_mpsc::channel::<ExecFrame>(256);
    let (in_tx, mut in_rx) = tokio_mpsc::channel::<ExecControl>(64);
    let (sid_tx, sid_rx) = tokio::sync::watch::channel(Some(session_id));

    tokio::spawn(async move {
        let _sid_tx = sid_tx; // keep the watch alive for the session's lifetime
        loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(EventFrame::PaneSnapshot { bytes, .. })
                    | Some(EventFrame::PaneDelta { bytes, .. }) => {
                        if out_tx.send(ExecFrame::Stdout(bytes)).await.is_err() {
                            return; // pane gone
                        }
                    }
                    Some(EventFrame::SessionExit { code, .. }) => {
                        let _ = out_tx.send(ExecFrame::Exit(code.unwrap_or(0))).await;
                        return;
                    }
                    Some(_) => {} // Hello / feed frames: not pane bytes
                    None => return, // transport dropped ⇒ relay reconnects
                },
                c = in_rx.recv() => match c {
                    Some(ExecControl::Stdin(bytes)) => {
                        if control.send(AttachControl::Input(bytes)).await.is_err() {
                            return;
                        }
                    }
                    Some(ExecControl::Resize { cols, rows }) => {
                        if control.send(AttachControl::Resize { rows, cols }).await.is_err() {
                            return;
                        }
                    }
                    Some(ExecControl::Close) | None => {
                        let _ = control.send(AttachControl::Close).await;
                        return;
                    }
                },
            }
        }
    });

    ExecSession {
        frames: out_rx,
        control: in_tx,
        session_id: sid_rx,
    }
}

/// A lazily-connecting daemon source: `ensure_daemon` runs inside `open`/
/// `attach` on the relay task, so pane spawn never blocks the event loop on
/// daemon startup (a connect/spawn failure surfaces asynchronously as the
/// pane's error husk, exactly like a provider exec failure).
pub(crate) struct LazyDaemonSource {
    pub cfg: DaemonConfig,
    /// Worktree hint recorded on opened sessions (listing/grouping).
    pub worktree: Option<String>,
}

impl LazyDaemonSource {
    async fn source(&self) -> Result<DaemonSource> {
        let client = ensure_daemon(&self.cfg).await?;
        Ok(DaemonSource {
            client,
            worktree: self.worktree.clone(),
        })
    }
}

impl ExecSource for LazyDaemonSource {
    fn open<'a>(&'a self, spec: &'a ExecSpec) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(async move { self.source().await?.open_and_attach(spec).await })
    }

    fn attach<'a>(
        &'a self,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(async move {
            self.source()
                .await?
                .attach_session(session, cols, rows)
                .await
        })
    }

    fn kill_session<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.source().await?.client.kill(session).await })
    }

    fn session_pid<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move { self.source().await.ok()?.lookup_pid(session).await })
    }
}
