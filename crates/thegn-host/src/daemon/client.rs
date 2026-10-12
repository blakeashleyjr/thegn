//! Client side of the pane daemon: discovery / lazy spawn, and the
//! [`ExecSource`] adapter that lets a compositor pane be daemon-backed through
//! the exact machinery provider panes already use (`PaneIo::Stream` +
//! `relay_exec`'s reconnect ladder).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::future::BoxFuture;
use serde_json::Value;
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
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
        .unwrap_or(0)
}

fn is_legacy_unix_addr(addr: &ControlAddr, legacy: &std::path::Path) -> bool {
    matches!(addr, ControlAddr::Unix(path) if path == legacy)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScopeCheck {
    Match,
    Mismatch(String),
    Unknown,
    NotOurs,
}

fn classify_scope_reply(reply: Result<Value>, expected: &[String]) -> ScopeCheck {
    let value = match reply {
        Ok(value) => value,
        Err(error) => {
            if error
                .downcast_ref::<thegn_svc::control::client::ControlRequestError>()
                .is_some_and(|request| request.status() == 404)
            {
                return ScopeCheck::Unknown;
            }
            return ScopeCheck::NotOurs;
        }
    };
    let Some(scope) = value.get("scope").and_then(Value::as_str) else {
        return ScopeCheck::NotOurs;
    };
    if expected.iter().any(|candidate| candidate == scope) {
        ScopeCheck::Match
    } else {
        ScopeCheck::Mismatch(scope.to_owned())
    }
}

pub(super) async fn check_daemon_scope(client: &ControlClient, expected: &[String]) -> ScopeCheck {
    let result = client.call_raw("GET", super::SCOPE_PATH, None).await;
    let check = classify_scope_reply(result, expected);
    if check == ScopeCheck::Unknown {
        tracing::debug!(target: "thegn::daemon", "daemon has no scope endpoint; accepting legacy daemon");
    }
    check
}

fn client_socket_label(client: &ControlClient) -> String {
    match client.addr() {
        ControlAddr::Unix(path) => path.display().to_string(),
        ControlAddr::Tcp { addr, .. } => addr.clone(),
        ControlAddr::HttpOrigin { origin, .. } => origin.clone(),
    }
}

fn scope_mismatch_error(socket: &std::path::Path, actual: &str) -> anyhow::Error {
    anyhow!(
        "pane daemon socket {} belongs to a different state directory (reported scope {})",
        socket.display(),
        actual
    )
}

/// Find a live daemon for this state dir WITHOUT spawning one: registry
/// discovery, then a probe of the configured socket. `None` = no daemon is
/// running (so there is nothing to kill/list — callers must not spawn one as
/// a side effect).
pub(crate) async fn connect_daemon(dcfg: &DaemonConfig) -> Option<ControlClient> {
    super::reconcile_legacy_socket(dcfg);
    let scopes = super::scope_aliases();
    // 1. Registry discovery (freshest live heartbeat), verified by connect.
    let discovered = tokio::task::spawn_blocking({
        let scopes = scopes.clone();
        move || {
            let db = Db::open().ok()?;
            Some(
                scopes
                    .iter()
                    .filter_map(|scope| thegn_svc::control::client::discover(&db, scope, now_ms()))
                    .collect::<Vec<_>>(),
            )
        }
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    for addr in discovered
        .into_iter()
        .filter(|addr| !is_legacy_unix_addr(addr, &legacy))
    {
        let client = ControlClient::new(addr);
        if client.health().await.is_ok() {
            match check_daemon_scope(&client, &scopes).await {
                ScopeCheck::Match | ScopeCheck::Unknown => {
                    start_error_state_bridge(&client);
                    return Some(client);
                }
                ScopeCheck::Mismatch(actual) => {
                    tracing::warn!(
                        target: "thegn::daemon",
                        socket = %client_socket_label(&client),
                        %actual,
                        "refusing daemon from a different state directory"
                    );
                }
                ScopeCheck::NotOurs => {}
            }
        }
    }

    // 2. The configured socket may host a daemon the registry missed (e.g. a
    //    fresh DB): probe it before giving up.
    let sock = super::socket_path(dcfg);
    if !dcfg.socket.is_empty() || sock != legacy {
        let client = ControlClient::new(ControlAddr::Unix(sock));
        if client.health().await.is_ok() {
            match check_daemon_scope(&client, &scopes).await {
                ScopeCheck::Match | ScopeCheck::Unknown => {
                    start_error_state_bridge(&client);
                    return Some(client);
                }
                ScopeCheck::Mismatch(actual) => {
                    tracing::warn!(
                        target: "thegn::daemon",
                        socket = %client_socket_label(&client),
                        %actual,
                        "refusing daemon from a different state directory"
                    );
                }
                ScopeCheck::NotOurs => {}
            }
        }
    }
    None
}

/// Find a live daemon for this state dir, or spawn one detached and wait for
/// its socket. The registry row is a hint; a successful `/health` round-trip
/// is the truth.
pub(crate) async fn ensure_daemon(dcfg: &DaemonConfig) -> Result<ControlClient> {
    let scopes = super::scope_aliases();
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    if let Some(client) = connect_daemon(dcfg).await {
        return Ok(client);
    }
    if super::legacy_socket_state() == super::LegacySocketState::Live {
        let legacy = thegn_core::config_daemon::legacy_socket_path(
            &thegn_core::util::xdg_state_home().join("thegn"),
        );
        return Err(anyhow!(
            "legacy daemon is still live at {}; detach its panes, then run `thegn daemon stop` before starting a new daemon",
            legacy.display()
        ));
    }
    if dcfg.socket.is_empty() && super::socket_path(dcfg) == legacy {
        return Err(anyhow!(
            "no secure private runtime directory is available for the pane daemon; set XDG_RUNTIME_DIR to an owner-only directory"
        ));
    }
    let sock = super::socket_path(dcfg);
    let client = ControlClient::new(ControlAddr::Unix(sock.clone()));

    // `connect_daemon` skips foreign daemons so callers that only need to
    // discover one can degrade gracefully. Startup must stop here instead of
    // racing a spawn against a foreign process that already owns the socket.
    if client.health().await.is_ok()
        && let ScopeCheck::Mismatch(actual) = check_daemon_scope(&client, &scopes).await
    {
        return Err(scope_mismatch_error(&sock, &actual));
    }

    // 3. Spawn detached (own process group, null stdio — the compositor must
    //    not adopt the daemon on its tty) and wait for the socket. The daemon
    //    binds the socket as its lock, so a spawn race resolves itself: the
    //    loser exits 0 and both clients connect to the winner.
    // Not `current_exe()` directly: after a rebuild-in-place it names a deleted
    // file, and the spawn below then ENOENTs on every pane open. `self_exe`
    // re-execs this same build through `/proc/self/exe`, which also keeps the
    // daemon's schema/wire version matched to ours.
    let exe = thegn_core::util::self_exe()
        .context("resolving this executable's path for daemon spawn")?;
    let mut cmd = thegn_core::util::detached(&exe.to_string_lossy());
    cmd.arg("daemon").arg("--socket").arg(&sock);
    cmd.spawn().context("spawn pane daemon")?;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if client.health().await.is_ok() {
            match check_daemon_scope(&client, &scopes).await {
                ScopeCheck::Match | ScopeCheck::Unknown => {
                    start_error_state_bridge(&client);
                    return Ok(client);
                }
                ScopeCheck::Mismatch(actual) => {
                    return Err(scope_mismatch_error(&sock, &actual));
                }
                ScopeCheck::NotOurs => {}
            }
        }
    }
    Err(anyhow!(
        "pane daemon did not come up on {} within 3s",
        sock.display()
    ))
}

/// Keep the compositor's attention cache synchronized with the daemon's
/// process-wide activity feed. The pane attach stream carries activity frames
/// only for the attached pane; this subscription also covers daemon sessions
/// that are currently detached from a pane.
fn start_error_state_bridge(client: &ControlClient) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn started() -> &'static Mutex<HashMap<String, String>> {
        static STARTED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        STARTED.get_or_init(|| Mutex::new(HashMap::new()))
    }
    fn next_generation() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    let key = match client.addr() {
        ControlAddr::Unix(path) => format!("unix:{}", path.display()),
        ControlAddr::Tcp { addr, .. } => format!("tcp:{addr}"),
        ControlAddr::HttpOrigin { origin, .. } => format!("origin:{origin}"),
    };
    let owner = format!("{key}#{}", next_generation());
    if let Ok(mut bridges) = started().lock() {
        if bridges.contains_key(&key) {
            return;
        }
        bridges.insert(key.clone(), owner.clone());
    } else {
        return;
    }

    let client = client.clone();
    let owner_for_task = owner.clone();
    tokio::spawn(async move {
        // best-effort: the bridge is an ambient cache feed; a daemon disconnect
        // must not affect the compositor or its pane relay.
        let _ = async {
            let stream = client.subscribe_events().await?;
            let mut frames = stream.frames;
            // Keep the sender alive for the lifetime of the websocket pump.
            let _control = stream.control;

            // The event feed starts with Hello and then only carries deltas.
            // Fetch the authoritative roster after subscribing, while frames
            // remain buffered, so the snapshot is applied before any future
            // Activity or SessionExit delta.
            let sessions = client.sessions().await?;
            super::agent_error_cache::replace_owner(
                &owner_for_task,
                sessions
                    .into_iter()
                    .map(|session| (session.id, session.worktree, session.error_active)),
            );
            while let Some(frame) = frames.recv().await {
                match frame {
                    EventFrame::Activity { json } => {
                        if let Ok(event) =
                            serde_json::from_str::<thegn_svc::control::SessionActivityEvent>(&json)
                        {
                            super::agent_error_cache::set_for(
                                &owner_for_task,
                                &event.session,
                                event.worktree,
                                event.error_active,
                            );
                        }
                    }
                    EventFrame::SessionExit { session, .. } => {
                        super::agent_error_cache::clear_for(&owner_for_task, &session);
                    }
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        super::agent_error_cache::clear_owner(&owner_for_task);
        if let Ok(mut keys) = started().lock()
            && keys
                .get(&key)
                .is_some_and(|current| current == &owner_for_task)
        {
            keys.remove(&key);
        }
    });
}

/// A daemon-backed exec source for one worktree's panes. `sandbox_id` on the
/// [`ExecSource`] contract maps to the daemon session id; persistence reuses
/// the existing `pane_sessions` capture verbatim with `provider = "daemon"`.
pub(crate) struct DaemonSource {
    pub client: ControlClient,
    /// Worktree hint recorded on opened sessions (listing/grouping).
    pub worktree: Option<String>,
    /// Shared with the [`LazyDaemonSource`] that built this (see [`HistoryOnce`]).
    pub attached_once: HistoryOnce,
}

/// Whether THIS PANE has already been fed a session's screen.
///
/// The first attach feeds a fresh client emulator, so its snapshot should carry
/// the scrollback history tail; every later attach on the same pane is
/// `relay_exec`'s reconnect ladder re-feeding an emulator that already holds
/// that history — replaying the tail would append up to 2000 duplicate lines to
/// scrollback on every transient drop.
///
/// The distinguishing thing is therefore the **pane**, not the session: this
/// used to be a process-global set keyed by session id, which meant the SECOND
/// pane in a process to attach a given session got no history — and that pane is
/// precisely the one that needs it, being a brand-new `PtyPane` with an empty
/// emulator (a re-materialize after a workspace eviction, or a terminal restored
/// from its persisted layout). Its screen came up blank, which reads exactly
/// like the shell having restarted. One cell per pane: [`LazyDaemonSource`] is
/// built per pane in `Panes::spawn_daemon_backed` and hands a clone to each
/// short-lived [`DaemonSource`] it makes.
pub(crate) type HistoryOnce = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// Claim the history tail for this pane: true exactly once per cell.
fn claim_history(once: &HistoryOnce) -> bool {
    !once.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// A cancelled/failed first attach did not deliver history. Release its claim
/// so the next real attach still restores this pane's empty emulator.
struct HistoryClaim {
    once: HistoryOnce,
    include: bool,
    delivered: bool,
}

impl HistoryClaim {
    fn new(once: &HistoryOnce) -> Self {
        // Production constructs one source per pane and its one relay awaits
        // attach serially. This claim is not a multi-consumer history lock.
        Self {
            once: once.clone(),
            include: claim_history(once),
            delivered: false,
        }
    }
}

impl Drop for HistoryClaim {
    fn drop(&mut self) {
        if self.include && !self.delivered {
            self.once.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
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
                // The compositor composed this argv through
                // `sandbox::enter_argv`, which already applied the pane CPU
                // cap. Without this the daemon would wrap it a second time.
                already_capped: true,
                ..Default::default()
            })
            .await?;
        // A just-opened session has no history yet; claim the cell so a later
        // reconnect on this pane counts as a re-attach.
        let session = self
            .attach_session(&info.id, spec.cols, spec.rows, true)
            .await?;
        self.attached_once
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(session)
    }

    async fn attach_session(
        &self,
        session: &str,
        cols: u16,
        rows: u16,
        include_history: bool,
    ) -> Result<ExecSession> {
        let client_id = format!("compositor-{}", std::process::id());
        let stream = self
            .client
            .attach_opts(session, &client_id, rows, cols, false, include_history)
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
        Box::pin(async move {
            // First attach on this pane (resurrect warm attach) restores the
            // scrollback context; its reconnects repaint without it.
            let mut history = HistoryClaim::new(&self.attached_once);
            let result = self
                .attach_session(session, cols, rows, history.include)
                .await?;
            history.delivered = true;
            Ok(result)
        })
    }

    fn session_absent<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(!self
                .client
                .sessions()
                .await?
                .iter()
                .any(|s| s.id == session))
        })
    }

    fn kill_session<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.client.kill(session))
    }

    fn session_pid<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Option<u32>> {
        Box::pin(self.lookup_pid(session))
    }
}

/// Exit code reported when the daemon couldn't reap a real status
/// (`SessionExit { code: None }`: the child was killed, reaped out-of-band,
/// or lost). A deliberate non-zero sentinel — mapping unknown to 0 would let
/// a killed session masquerade as success to anything keying off the pane's
/// exit code. 254 avoids the shell's 126/127/128+N conventions.
const EXIT_STATUS_UNKNOWN: i32 = 254;

/// Lower a daemon exit report onto the `ExecFrame::Exit` integer contract.
fn exec_exit_code(code: Option<i32>) -> i32 {
    code.unwrap_or(EXIT_STATUS_UNKNOWN)
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
        end: mut end_rx,
    } = stream;
    let (out_tx, out_rx) = tokio_mpsc::channel::<ExecFrame>(256);
    let (in_tx, mut in_rx) = tokio_mpsc::channel::<ExecControl>(64);
    let (sid_tx, sid_rx) = tokio::sync::watch::channel(Some(session_id.clone()));

    tokio::spawn(async move {
        let _sid_tx = sid_tx; // keep the watch alive for the session's lifetime
        loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(EventFrame::PaneSnapshot { session, bytes, .. })
                    | Some(EventFrame::PaneDelta { session, bytes, .. }) => {
                        if session != session_id {
                            return;
                        }
                        if !forward_frame_while_servicing_control(
                            &out_tx,
                            ExecFrame::Stdout(bytes),
                            &mut in_rx,
                            &control,
                        )
                        .await
                        {
                            return; // pane gone
                        }
                    }
                    Some(EventFrame::SessionExit { code, .. }) => {
                        let _ = out_tx.send(ExecFrame::Exit(exec_exit_code(code))).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                        return;
                    }
                    Some(_) => {} // Hello / feed frames: not pane bytes
                    None => {
                        match tokio::time::timeout(Duration::from_secs(1), &mut end_rx).await {
                            Ok(Ok(end)) if end.reconnects() => {
                                tracing::debug!(target: "thegn::daemon", session = %session_id, ?end, "pane attach stream ended");
                            }
                            Ok(Ok(end)) => {
                                tracing::warn!(target: "thegn::daemon", session = %session_id, ?end, "pane attach stream ended permanently");
                            }
                            Ok(Err(error)) => {
                                tracing::debug!(target: "thegn::daemon", session = %session_id, %error, "pane attach stream ended without an outcome");
                            }
                            Err(_) => {
                                tracing::debug!(target: "thegn::daemon", session = %session_id, "pane attach terminal outcome timed out");
                            }
                        }
                        return; // transport dropped ⇒ relay reconnects
                    }
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
                        let _ = control.send(AttachControl::Close).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
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

/// Forward a pane frame without preventing Close from reaching the attach
/// stream while the pane's bounded output queue is full.
async fn forward_frame_while_servicing_control(
    out_tx: &tokio_mpsc::Sender<ExecFrame>,
    frame: ExecFrame,
    in_rx: &mut tokio_mpsc::Receiver<ExecControl>,
    control: &tokio_mpsc::Sender<AttachControl>,
) -> bool {
    loop {
        tokio::select! {
            permit = out_tx.reserve() => match permit {
                Ok(permit) => {
                    permit.send(frame);
                    return true;
                }
                Err(_) => return false,
            },
            message = in_rx.recv() => match message {
                Some(ExecControl::Stdin(bytes)) => {
                    if control.send(AttachControl::Input(bytes)).await.is_err() {
                        return false;
                    }
                }
                Some(ExecControl::Resize { cols, rows }) => {
                    if control.send(AttachControl::Resize { rows, cols }).await.is_err() {
                        return false;
                    }
                }
                Some(ExecControl::Close) | None => {
                    match control.send(AttachControl::Close).await {
                        Ok(()) | Err(_) => return false,
                    }
                }
            }
        }
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
    /// This pane's history-tail cell — see [`HistoryOnce`]. One source is built
    /// per pane, so `Default` (unclaimed) is right for every new pane.
    pub attached_once: HistoryOnce,
    /// The endpoint pinned by this pane's first open/attach. An
    /// absence check must not discover/start a different daemon with an empty
    /// roster and mistake that for proof that the original shell disappeared.
    pub last_client: std::sync::Mutex<Option<ControlClient>>,
}

impl LazyDaemonSource {
    async fn source(&self) -> Result<DaemonSource> {
        let recorded = self
            .last_client
            .lock()
            .map_err(|_| anyhow!("daemon client lock poisoned"))?
            .is_some();
        if recorded {
            return self.recorded_source();
        }
        let client = ensure_daemon(&self.cfg).await?;
        *self
            .last_client
            .lock()
            .map_err(|_| anyhow!("daemon client lock poisoned"))? = Some(client.clone());
        Ok(DaemonSource {
            client,
            worktree: self.worktree.clone(),
            // Cloned, not fresh: `source()` runs per open/attach, so a
            // per-`DaemonSource` cell would read as "first attach" every time
            // and replay the tail on every reconnect.
            attached_once: std::sync::Arc::clone(&self.attached_once),
        })
    }

    fn recorded_source(&self) -> Result<DaemonSource> {
        let client = self
            .last_client
            .lock()
            .map_err(|_| anyhow!("daemon client lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("no recorded daemon session endpoint"))?;
        Ok(DaemonSource {
            client,
            worktree: self.worktree.clone(),
            attached_once: self.attached_once.clone(),
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
            let source = self.source().await?;
            source.attach(session, cols, rows).await
        })
    }

    fn kill_session<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.recorded_source()?.client.kill(session).await })
    }

    fn session_absent<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { self.recorded_source()?.session_absent(session).await })
    }

    fn session_pid<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move { self.recorded_source().ok()?.lookup_pid(session).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_reply_classification_is_fail_closed_and_accepts_legacy_daemons() {
        let expected = vec!["v1-current".to_owned(), "/old/state/thegn".to_owned()];
        assert_eq!(
            classify_scope_reply(Ok(serde_json::json!({ "scope": "v1-current" })), &expected),
            ScopeCheck::Match
        );
        assert_eq!(
            classify_scope_reply(
                Ok(serde_json::json!({ "scope": "/old/state/thegn" })),
                &expected
            ),
            ScopeCheck::Match
        );
        assert_eq!(
            classify_scope_reply(Ok(serde_json::json!({ "scope": "v1-other" })), &expected),
            ScopeCheck::Mismatch("v1-other".to_owned())
        );
        assert_eq!(
            classify_scope_reply(
                Err(anyhow::Error::new(
                    thegn_svc::control::client::ControlRequestError::new(404, ""),
                )),
                &expected
            ),
            ScopeCheck::Unknown
        );
        assert_eq!(
            classify_scope_reply(
                Err(anyhow::Error::new(
                    thegn_svc::control::client::ControlRequestError::new(500, ""),
                )),
                &expected
            ),
            ScopeCheck::NotOurs
        );
        assert_eq!(
            classify_scope_reply(Ok(serde_json::json!({})), &expected),
            ScopeCheck::NotOurs
        );
        assert_eq!(
            classify_scope_reply(Ok(serde_json::json!({ "scope": 42 })), &expected),
            ScopeCheck::NotOurs
        );
    }

    #[test]
    fn registry_discovery_does_not_adopt_legacy_unix_socket() {
        let legacy = std::path::Path::new("/state/thegn/run/daemon.sock");
        assert!(is_legacy_unix_addr(
            &ControlAddr::Unix(legacy.to_path_buf()),
            legacy
        ));
        assert!(!is_legacy_unix_addr(
            &ControlAddr::Unix("/run/user/7/thegn/daemon.sock".into()),
            legacy
        ));
        assert!(!is_legacy_unix_addr(
            &ControlAddr::Tcp {
                addr: "127.0.0.1:1".into(),
                token: "secret".into(),
            },
            legacy
        ));
    }

    #[tokio::test]
    async fn attach_close_is_forwarded_when_pane_output_queue_is_full() {
        let (frame_tx, frame_rx) = tokio_mpsc::channel(257);
        let (attach_control_tx, mut attach_control_rx) = tokio_mpsc::channel(1);
        let stream = AttachStream {
            frames: frame_rx,
            control: attach_control_tx,
            end: tokio::sync::oneshot::channel().1,
        };
        let session = adapt("session-1".into(), stream);
        let snapshot = || EventFrame::PaneSnapshot {
            session: "session-1".into(),
            seq: 0,
            cols: 80,
            rows: 24,
            bytes: Vec::new(),
        };
        for _ in 0..257 {
            frame_tx.send(snapshot()).await.unwrap();
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            while session.frames.len() != 256 || frame_tx.capacity() != 257 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        session.control.send(ExecControl::Close).await.unwrap();

        let close = tokio::time::timeout(Duration::from_secs(2), attach_control_rx.recv())
            .await
            .unwrap();
        assert!(matches!(close, Some(AttachControl::Close)));
    }

    #[tokio::test]
    async fn wrong_session_pane_frame_is_not_forwarded() {
        let (frame_tx, frame_rx) = tokio_mpsc::channel(1);
        let (attach_control_tx, _attach_control_rx) = tokio_mpsc::channel(1);
        let stream = AttachStream {
            frames: frame_rx,
            control: attach_control_tx,
            end: tokio::sync::oneshot::channel().1,
        };
        let mut session = adapt("session-1".into(), stream);
        frame_tx
            .send(EventFrame::PaneSnapshot {
                session: "session-2".into(),
                seq: 0,
                cols: 80,
                rows: 24,
                bytes: b"wrong".to_vec(),
            })
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), session.frames.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The daemon reports `code: None` exactly when the exit is unreapable
    /// (killed / reaped out-of-band). That must surface as a distinct
    /// non-zero sentinel, never as success.
    #[test]
    fn unknown_exit_maps_to_the_nonzero_sentinel() {
        assert_eq!(exec_exit_code(Some(0)), 0);
        assert_eq!(exec_exit_code(Some(1)), 1);
        assert_eq!(exec_exit_code(Some(137)), 137);
        assert_eq!(exec_exit_code(None), EXIT_STATUS_UNKNOWN);
        assert_ne!(
            exec_exit_code(None),
            0,
            "an unknown exit must not read as success"
        );
    }

    /// Only a pane's FIRST attach requests the history tail; its reconnects do
    /// not, or every transient drop would append the tail again.
    #[test]
    fn only_the_first_attach_on_a_pane_requests_history() {
        let pane: HistoryOnce = Default::default();
        assert!(claim_history(&pane), "fresh pane: history wanted");
        assert!(
            !claim_history(&pane),
            "this pane's reconnects must skip the history tail"
        );
    }

    /// The cell is per PANE, not per session. A second pane attaching a session
    /// this process already showed is a brand-new `PtyPane` with an empty
    /// emulator (a re-materialize after an eviction, or a terminal restored from
    /// its persisted layout) — it must get the tail, or it comes up blank and
    /// reads exactly like the shell having restarted.
    #[test]
    fn a_second_pane_on_the_same_session_still_gets_the_history() {
        let first: HistoryOnce = Default::default();
        let second: HistoryOnce = Default::default();
        assert!(claim_history(&first));
        assert!(
            claim_history(&second),
            "a fresh pane's empty emulator needs the tail"
        );
    }

    /// Every short-lived `DaemonSource` a pane's `LazyDaemonSource` builds
    /// shares the one cell — a per-`DaemonSource` cell would read as "first
    /// attach" on every reconnect.
    #[test]
    fn cloned_cells_share_the_claim() {
        let pane: HistoryOnce = Default::default();
        let per_call = std::sync::Arc::clone(&pane);
        assert!(claim_history(&per_call));
        assert!(!claim_history(&pane), "the clone consumed the claim");
    }

    #[test]
    fn failed_or_cancelled_attach_releases_its_history_claim() {
        let pane = HistoryOnce::default();
        drop(HistoryClaim::new(&pane));
        let mut next = HistoryClaim::new(&pane);
        assert!(next.include);
        next.delivered = true;
        drop(next);
        assert!(!HistoryClaim::new(&pane).include);
    }

    #[tokio::test]
    async fn lazy_absence_queries_only_the_last_attach_endpoint_and_decodes_strictly() {
        use axum::{Router, body::Body, http::header, response::Response, routing::get};
        use serde_json::json;
        let cases: Vec<(Vec<u8>, Option<&'static str>, Option<bool>)> = vec![
            (
                serde_json::to_vec(&json!({"sessions": []})).unwrap(),
                Some("application/json"),
                Some(true),
            ),
            (
                serde_json::to_vec(
                    &json!({"sessions": [SessionInfo { id: "live".into(), ..Default::default() }]}),
                )
                .unwrap(),
                Some("application/json"),
                Some(false),
            ),
            (
                serde_json::to_vec(&json!({})).unwrap(),
                Some("application/json"),
                None,
            ),
            (
                serde_json::to_vec(&json!({"sessions": null})).unwrap(),
                Some("application/json"),
                None,
            ),
            (
                serde_json::to_vec(&json!({"sessions": [SessionInfo::default()]})).unwrap(),
                Some("application/json"),
                None,
            ),
            (b"{truncated".to_vec(), Some("application/json"), None),
            (
                b"<html>proxy failure</html>".to_vec(),
                Some("text/html"),
                None,
            ),
            (Vec::new(), Some("application/json"), None),
        ];
        for (body, content_type, expected) in cases {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().route(
                        "/v1/sessions",
                        get(move || async move {
                            let mut response = Response::new(Body::from(body));
                            if let Some(content_type) = content_type {
                                response
                                    .headers_mut()
                                    .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
                            }
                            response
                        }),
                    ),
                )
                .await
                .unwrap();
            });
            let source = LazyDaemonSource {
                cfg: DaemonConfig::default(),
                worktree: None,
                attached_once: Default::default(),
                last_client: std::sync::Mutex::new(Some(ControlClient::new(
                    ControlAddr::HttpOrigin {
                        origin: format!("http://{address}"),
                        token: "fixture".into(),
                    },
                ))),
            };
            // No health endpoint exists. This must use the recorded client,
            // never discovery/ensure_daemon (or the user's configured socket).
            assert_eq!(source.session_absent("live").await.ok(), expected);
            server.abort();
        }
        let source = LazyDaemonSource {
            cfg: DaemonConfig::default(),
            worktree: None,
            attached_once: Default::default(),
            last_client: Default::default(),
        };
        assert!(source.session_absent("live").await.is_err());
    }
}
