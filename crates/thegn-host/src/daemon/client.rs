//! Client side of the pane daemon: discovery / lazy spawn, and the
//! [`ExecSource`] adapter that lets a compositor pane be daemon-backed through
//! the exact machinery provider panes already use (`PaneIo::Stream` +
//! `relay_exec`'s reconnect ladder).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::config::DaemonConfig;
use thegn_core::control_wire::{EventFrame, MAX_WIRE_FRAME};
use thegn_core::db::Db;
use thegn_svc::control::client::{
    AttachControl, AttachStream, ByteBudget, ControlAddr, ControlClient, StreamEnd,
};
use thegn_svc::control::{OpenSpec, SessionInfo};
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec};

use crate::pane_source::ExecSource;

const SCOPE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// One scope-probe budget shared by every startup candidate.
const CONNECT_DAEMON_TOTAL_DEADLINE: Duration = SCOPE_PROBE_TIMEOUT;
/// The spawn wait is bounded by the 3 s already promised by its timeout error
/// (the old 60 x 50 ms loop).
pub(super) const SPAWN_WAIT_DEADLINE: Duration = Duration::from_secs(3);
/// Interval between checks while waiting for the spawned daemon to start.
const SPAWN_WAIT_POLL: Duration = Duration::from_millis(50);

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

/// Result of checking whether a stop candidate is a live Thegn daemon for one
/// of this process's state scopes.
pub(crate) enum StopProbe {
    NotRunning,
    Ours,
    Foreign(String),
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
            if error
                .downcast_ref::<thegn_svc::control::client::ControlProtocolError>()
                .is_some_and(|protocol| {
                    matches!(
                        protocol,
                        thegn_svc::control::client::ControlProtocolError::EmptySuccessBody
                            | thegn_svc::control::client::ControlProtocolError::MissingJsonContentType
                    )
                })
            {
                // A pre-route Unix daemon includes tonic's gRPC fallback,
                // which answers unknown paths with HTTP 200 and an empty,
                // non-JSON body rather than an HTTP 404.
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
    let result = match tokio::time::timeout(
        SCOPE_PROBE_TIMEOUT,
        client.call_raw("GET", super::SCOPE_PATH, None),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return ScopeCheck::NotOurs,
    };
    let check = classify_scope_reply(result, expected);
    if check == ScopeCheck::Unknown {
        tracing::debug!(target: "thegn::daemon", "daemon has no scope endpoint; accepting legacy daemon");
    }
    check
}

fn client_socket_label(client: &ControlClient) -> String {
    addr_label(client.addr())
}

fn addr_key(addr: &ControlAddr) -> String {
    match addr {
        ControlAddr::Unix(path) => format!("unix:{}", path.display()),
        ControlAddr::Tcp { addr, .. } => format!("tcp:{addr}"),
        ControlAddr::HttpOrigin { origin, .. } => format!("origin:{origin}"),
    }
}

fn addr_label(addr: &ControlAddr) -> String {
    match addr {
        ControlAddr::Unix(path) => path.display().to_string(),
        ControlAddr::Tcp { addr, .. } => addr.clone(),
        ControlAddr::HttpOrigin { origin, .. } => origin.clone(),
    }
}

struct CandidateScan {
    found: Option<ControlClient>,
    foreign: Vec<(String, String)>,
    unassessed: Vec<String>,
}

async fn scan_candidates(
    candidates: Vec<ControlAddr>,
    deadline: Duration,
    probe: impl Fn(ControlAddr) -> BoxFuture<'static, StopProbe>,
) -> CandidateScan {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if seen.insert(addr_key(&candidate)) {
            unique.push(candidate);
        }
    }

    let deadline_at = tokio::time::Instant::now() + deadline;
    let mut scan = CandidateScan {
        found: None,
        foreign: Vec::new(),
        unassessed: Vec::new(),
    };
    let mut candidates = unique.into_iter();
    while let Some(candidate) = candidates.next() {
        let label = addr_label(&candidate);
        match tokio::time::timeout_at(deadline_at, probe(candidate.clone())).await {
            Ok(StopProbe::Ours) => {
                scan.found = Some(ControlClient::new(candidate));
                return scan;
            }
            Ok(StopProbe::Foreign(scope)) => scan.foreign.push((label, scope)),
            Ok(StopProbe::NotRunning) => {}
            Err(_) => {
                scan.unassessed.push(label);
                scan.unassessed
                    .extend(candidates.map(|addr| addr_label(&addr)));
                return scan;
            }
        }
    }
    scan
}

async fn scan_startup_candidates(
    local: Option<ControlAddr>,
    candidates: Vec<ControlAddr>,
    deadline: Duration,
    probe: impl Fn(ControlAddr) -> BoxFuture<'static, StopProbe>,
) -> CandidateScan {
    let mut scan = CandidateScan {
        found: None,
        foreign: Vec::new(),
        unassessed: Vec::new(),
    };
    let mut candidates = candidates;
    if let Some(local) = local {
        // Preserve the local client's request timeout; only registry rows
        // share the startup reconciliation deadline below.
        let key = addr_key(&local);
        candidates.retain(|candidate| addr_key(candidate) != key);
        let label = addr_label(&local);
        match probe(local.clone()).await {
            StopProbe::Ours => {
                scan.found = Some(ControlClient::new(local));
                return scan;
            }
            StopProbe::Foreign(scope) => scan.foreign.push((label, scope)),
            StopProbe::NotRunning => {}
        }
    }

    let mut registry_scan = scan_candidates(candidates, deadline, probe).await;
    scan.foreign.append(&mut registry_scan.foreign);
    scan.found = registry_scan.found;
    scan.unassessed = registry_scan.unassessed;
    scan
}

async fn startup_scan(dcfg: &DaemonConfig) -> CandidateScan {
    super::reconcile_legacy_socket(dcfg);
    let scopes = super::scope_aliases();
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
    let candidates = discovered
        .into_iter()
        .filter(|addr| !is_legacy_unix_addr(addr, &legacy))
        .collect::<Vec<_>>();
    let sock = super::socket_path(dcfg);
    let local = (!dcfg.socket.is_empty() || sock != legacy).then_some(ControlAddr::Unix(sock));
    scan_startup_candidates(
        local,
        candidates,
        CONNECT_DAEMON_TOTAL_DEADLINE,
        move |addr| {
            let scopes = scopes.clone();
            Box::pin(async move { probe_stop_target(&ControlClient::new(addr), &scopes).await })
        },
    )
    .await
}

fn startup_refusal(scan: &CandidateScan, sock_label: &str) -> Option<anyhow::Error> {
    if !scan.unassessed.is_empty() {
        return Some(anyhow!(
            "could not check pane daemon endpoint(s) {} within {}s; not starting another daemon (retry, or run `thegn daemon stop`)",
            scan.unassessed.join(", "),
            CONNECT_DAEMON_TOTAL_DEADLINE.as_secs()
        ));
    }
    scan.foreign
        .iter()
        .find(|(label, _)| label == sock_label)
        .map(|(_, scope)| scope_mismatch_error(sock_label, scope))
}

pub(crate) fn socket_label(client: &ControlClient) -> String {
    client_socket_label(client)
}

pub(crate) async fn probe_stop_target(client: &ControlClient, scopes: &[String]) -> StopProbe {
    if client.health().await.is_err() {
        return StopProbe::NotRunning;
    }
    match check_daemon_scope(client, scopes).await {
        ScopeCheck::Match | ScopeCheck::Unknown => StopProbe::Ours,
        ScopeCheck::Mismatch(actual) => StopProbe::Foreign(actual),
        ScopeCheck::NotOurs => StopProbe::NotRunning,
    }
}

fn scope_mismatch_error(socket: &str, actual: &str) -> anyhow::Error {
    anyhow!(
        "pane daemon socket {socket} belongs to a different state directory (reported scope {actual})"
    )
}

enum SpawnWait {
    Up,
    Foreign(String),
    TimedOut,
}

async fn wait_for_spawned_daemon(
    deadline: Duration,
    poll: Duration,
    probe: impl Fn() -> BoxFuture<'static, StopProbe>,
) -> SpawnWait {
    let deadline_at = tokio::time::Instant::now() + deadline;
    loop {
        let now = tokio::time::Instant::now();
        let Some(remaining) = deadline_at.checked_duration_since(now) else {
            return SpawnWait::TimedOut;
        };
        if remaining.is_zero() {
            return SpawnWait::TimedOut;
        }
        tokio::time::sleep(poll.min(remaining)).await;
        match tokio::time::timeout_at(deadline_at, probe()).await {
            Ok(StopProbe::Ours) => return SpawnWait::Up,
            Ok(StopProbe::Foreign(actual)) => return SpawnWait::Foreign(actual),
            Ok(StopProbe::NotRunning) => {}
            Err(_) => return SpawnWait::TimedOut,
        }
    }
}

fn own_spawned_daemon(child: std::process::Child) {
    if let Err(error) = thegn_core::thread_name::try_spawn_named("thegn-dmn-wait", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        reap_spawned_daemon(child);
    }) {
        // The failed thread spawn drops its closure (and the Child handle), but
        // does not kill the daemon; there is no safe synchronous fallback here.
        tracing::warn!(
            target: "thegn::daemon",
            %error,
            "could not start the daemon child waiter; the daemon will not be reaped"
        );
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "blocking wait on a dedicated waiter thread, never the UI loop"
)]
fn reap_spawned_daemon(mut child: std::process::Child) {
    match child.wait() {
        Ok(status) => tracing::debug!(
            target: "thegn::daemon",
            %status,
            "spawned pane daemon exited"
        ),
        Err(error) => tracing::debug!(
            target: "thegn::daemon",
            %error,
            "could not wait for the spawned pane daemon"
        ),
    }
}

fn no_daemon_error() -> anyhow::Error {
    anyhow!(
        "no thegn pane daemon is running — start one with `thegn serve`, \
         or enable `[daemon]` in config so the compositor keeps one warm"
    )
}

/// Verify that a CLI client reached a daemon for one of this process's state
/// scopes. Unknown scope endpoints are accepted for pre-upgrade daemons.
pub(crate) async fn verify_local_daemon(
    client: ControlClient,
    scopes: &[String],
) -> Result<ControlClient> {
    if client.health().await.is_err() {
        return Err(no_daemon_error());
    }
    match check_daemon_scope(&client, scopes).await {
        ScopeCheck::Match | ScopeCheck::Unknown => Ok(client),
        ScopeCheck::Mismatch(actual) => {
            Err(scope_mismatch_error(&client_socket_label(&client), &actual))
        }
        ScopeCheck::NotOurs => Err(no_daemon_error()),
    }
}

/// Find a live daemon for this state dir WITHOUT spawning one: probe the
/// configured socket, then registry discovery. `None` = no daemon is
/// running (so there is nothing to kill/list — callers must not spawn one as
/// a side effect).
pub(crate) async fn connect_daemon(dcfg: &DaemonConfig) -> Option<ControlClient> {
    let scan = startup_scan(dcfg).await;
    report_startup_scan(&scan);
    if let Some(client) = scan.found {
        start_error_state_bridge(&client);
        return Some(client);
    }
    None
}

fn report_startup_scan(scan: &CandidateScan) {
    for (socket, actual) in &scan.foreign {
        tracing::warn!(
            target: "thegn::daemon",
            %socket,
            %actual,
            "refusing daemon from a different state directory"
        );
    }
    if !scan.unassessed.is_empty() {
        tracing::warn!(
            target: "thegn::daemon",
            candidates = ?scan.unassessed,
            "pane daemon candidates not checked within the startup deadline"
        );
    }
}

/// Find a live daemon for this state dir, or spawn one detached and wait for
/// its socket. The registry row is a hint; a successful `/health` round-trip
/// is the truth.
pub(crate) async fn ensure_daemon(dcfg: &DaemonConfig) -> Result<ControlClient> {
    let scopes = super::scope_aliases();
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    let scan = startup_scan(dcfg).await;
    report_startup_scan(&scan);
    if let Some(client) = scan.found.as_ref() {
        if let ControlAddr::Unix(path) = client.addr() {
            super::control_publish::publish_for_socket(path);
        }
        start_error_state_bridge(client);
        return Ok(client.clone());
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
    if let Some(error) = startup_refusal(&scan, &sock.display().to_string()) {
        return Err(error);
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
    let child = cmd.spawn().context("spawn pane daemon")?;
    own_spawned_daemon(child);
    let probe_dcfg = dcfg.clone();
    let last_probe_socket = std::sync::Arc::new(std::sync::Mutex::new(sock.clone()));
    let probe_socket = std::sync::Arc::clone(&last_probe_socket);
    let probe = move || {
        let socket = super::socket_path(&probe_dcfg);
        *probe_socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = socket.clone();
        let client = ControlClient::new(ControlAddr::Unix(socket));
        let scopes = scopes.clone();
        Box::pin(async move { probe_stop_target(&client, &scopes).await })
            as BoxFuture<'static, StopProbe>
    };
    let probed_socket = || {
        last_probe_socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    };
    match wait_for_spawned_daemon(SPAWN_WAIT_DEADLINE, SPAWN_WAIT_POLL, probe).await {
        SpawnWait::Up => {
            let sock = probed_socket();
            super::control_publish::publish_for_socket(&sock);
            let client = ControlClient::new(ControlAddr::Unix(sock));
            start_error_state_bridge(&client);
            Ok(client)
        }
        SpawnWait::Foreign(actual) => {
            let sock = probed_socket();
            Err(scope_mismatch_error(&sock.display().to_string(), &actual))
        }
        SpawnWait::TimedOut => {
            let sock = probed_socket();
            Err(anyhow!(
                "pane daemon did not come up on {} within {}s",
                sock.display(),
                SPAWN_WAIT_DEADLINE.as_secs()
            ))
        }
    }
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

    let key = addr_key(client.addr());
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
    /// Shared with the pane's lazy source so gap streaks survive relay reconnects.
    pub gap_tracker: GapTracker,
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

const GAP_BACKOFF_BASE: Duration = Duration::from_millis(500);
const GAP_BACKOFF_CAP: Duration = Duration::from_secs(30);
const GAP_STABLE_RESET: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub(crate) struct GapStreak {
    started: tokio::time::Instant,
    resumed_at: tokio::time::Instant,
    gaps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GapDecision {
    Backoff(Duration),
    Unstable { gaps: u32, elapsed: Duration },
}

pub(crate) type GapStreakCell = std::sync::Arc<std::sync::Mutex<Option<GapStreak>>>;

#[derive(Clone)]
pub(crate) struct GapTracker {
    window: Duration,
    streak: GapStreakCell,
}

impl GapTracker {
    fn new(window: Duration, streak: GapStreakCell) -> Self {
        Self { window, streak }
    }
}

fn on_sequence_gap(
    streak: &mut Option<GapStreak>,
    window: Duration,
    now: tokio::time::Instant,
) -> GapDecision {
    let reset = streak.is_none_or(|current| {
        now.saturating_duration_since(current.resumed_at) >= GAP_STABLE_RESET
    });
    if reset {
        *streak = Some(GapStreak {
            started: now,
            resumed_at: now,
            gaps: 0,
        });
    }
    let current = streak.as_mut().expect("gap streak initialized");
    current.gaps = current.gaps.saturating_add(1);
    let elapsed = now.saturating_duration_since(current.started);
    if !window.is_zero() && elapsed >= window {
        return GapDecision::Unstable {
            gaps: current.gaps,
            elapsed,
        };
    }
    let multiplier = 1u32 << current.gaps.saturating_sub(1).min(20);
    let backoff = GAP_BACKOFF_BASE
        .saturating_mul(multiplier)
        .min(GAP_BACKOFF_CAP);
    current.resumed_at = now + backoff;
    GapDecision::Backoff(backoff)
}

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
        Ok(adapt(session.to_string(), stream, self.gap_tracker.clone()))
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

/// Maximum bytes retained in the pane adapter's output queue.
const ADAPTER_BYTE_BUDGET: usize = 4 * MAX_WIRE_FRAME;

/// Lower a daemon exit report onto the `ExecFrame::Exit` integer contract.
fn exec_exit_code(code: Option<i32>) -> i32 {
    code.unwrap_or(EXIT_STATUS_UNKNOWN)
}

/// Bridge an [`AttachStream`] (decoded control-wire frames) to the pane
/// machinery's [`ExecSession`] shape: snapshot and deltas both become raw
/// `Stdout` bytes (the snapshot is an ANSI repaint — the emulator applies it
/// like any output), `SessionExit` becomes `Exit`, and stdin/resize/close map
/// onto the attach control channel.
fn adapt(session_id: String, stream: AttachStream, gaps: GapTracker) -> ExecSession {
    let AttachStream {
        mut frames,
        control,
        end: mut end_rx,
    } = stream;
    let (out_tx, out_rx) = tokio_mpsc::channel::<ExecFrame>(256);
    let mut budget = ByteBudget::new(ADAPTER_BYTE_BUDGET);
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
                            &mut budget,
                            &mut in_rx,
                            &control,
                        )
                        .await
                        {
                            return; // pane gone
                        }
                    }
                    Some(EventFrame::SessionExit { code, .. }) => {
                        if let Err(error) = out_tx.send(ExecFrame::Exit(exec_exit_code(code))).await {
                            tracing::debug!(target: "thegn::daemon", %error, "pane exit could not be delivered");
                        }
                        return;
                    }
                    Some(_) => {} // Hello / feed frames: not pane bytes
                    None => {
                        match tokio::time::timeout(Duration::from_secs(1), &mut end_rx).await {
                            Ok(Ok(StreamEnd::SequenceGap { expected, got })) => {
                                let now = tokio::time::Instant::now();
                                let decision = match gaps.streak.lock() {
                                    Ok(mut streak) => on_sequence_gap(&mut streak, gaps.window, now),
                                    Err(poisoned) => on_sequence_gap(
                                        &mut poisoned.into_inner(),
                                        gaps.window,
                                        now,
                                    ),
                                };
                                match decision {
                                    GapDecision::Backoff(backoff) => {
                                        tracing::debug!(target: "thegn::daemon", session = %session_id, expected, got, ?backoff, "pane attach sequence gap; backing off before reconnect");
                                        let sleep = tokio::time::sleep(backoff);
                                        tokio::pin!(sleep);
                                        loop {
                                            tokio::select! {
                                                biased;
                                                control_message = in_rx.recv() => match control_message {
                                                    Some(ExecControl::Close) | None => {
                                                        if let Err(error) = control.send(AttachControl::Close).await {
                                                            tracing::debug!(target: "thegn::daemon", %error, "attach close could not be forwarded during sequence-gap backoff");
                                                        }
                                                        return;
                                                    }
                                                    Some(ExecControl::Stdin(_)) | Some(ExecControl::Resize { .. }) => {}
                                                },
                                                () = &mut sleep => break,
                                            }
                                        }
                                    }
                                    GapDecision::Unstable { gaps, elapsed } => {
                                        tracing::warn!(target: "thegn::daemon", session = %session_id, expected, got, gaps, ?elapsed, "pane attach stream is unstable after repeated sequence gaps");
                                        let diagnostic = format!("\r\n[pane stream unstable: {gaps} sequence gaps in {}s; session {session_id} is still running in the daemon (thegn session list)]\r\n", elapsed.as_secs());
                                        if let Err(error) = out_tx.send(ExecFrame::Stdout(diagnostic.into_bytes())).await {
                                            tracing::debug!(target: "thegn::daemon", %error, "pane stream instability diagnostic could not be delivered");
                                        }
                                        if let Err(error) = out_tx.send(ExecFrame::Exit(EXIT_STATUS_UNKNOWN)).await {
                                            tracing::debug!(target: "thegn::daemon", %error, "pane stream instability exit could not be delivered");
                                        }
                                        return;
                                    }
                                }
                            }
                            Ok(Ok(end)) if end.reconnects() => {
                                tracing::debug!(target: "thegn::daemon", session = %session_id, ?end, "pane attach stream ended");
                            }
                            Ok(Ok(end @ StreamEnd::Protocol(_)))
                            | Ok(Ok(end @ StreamEnd::IdentityMismatch { .. })) => {
                                tracing::warn!(target: "thegn::daemon", session = %session_id, ?end, "pane attach stream ended permanently");
                                let reason = format!("{end:?}").replace('\r', " ").replace('\n', " ");
                                if let Err(error) = out_tx
                                    .send(ExecFrame::Stdout(format!("\r\n[pane stream failed: {reason}]\r\n").into_bytes()))
                                    .await
                                {
                                    tracing::debug!(target: "thegn::daemon", %error, "pane stream failure diagnostic could not be delivered");
                                }
                                if let Err(error) = out_tx.send(ExecFrame::Exit(EXIT_STATUS_UNKNOWN)).await {
                                    tracing::debug!(target: "thegn::daemon", %error, "pane stream failure exit could not be delivered");
                                }
                                return;
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
    budget: &mut ByteBudget,
    in_rx: &mut tokio_mpsc::Receiver<ExecControl>,
    control: &tokio_mpsc::Sender<AttachControl>,
) -> bool {
    let charge = match &frame {
        ExecFrame::Stdout(bytes) => bytes.len(),
        ExecFrame::Exit(_) => 0,
    };
    loop {
        tokio::select! {
            permit = budget.reserve(out_tx, charge) => match permit {
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
    /// Per-pane sequence-gap policy, shared with every short-lived source.
    pub gap_streak: GapStreakCell,
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
            gap_tracker: GapTracker::new(
                Duration::from_secs(self.cfg.stream_gap_window_secs),
                std::sync::Arc::clone(&self.gap_streak),
            ),
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
            gap_tracker: GapTracker::new(
                Duration::from_secs(self.cfg.stream_gap_window_secs),
                std::sync::Arc::clone(&self.gap_streak),
            ),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use thegn_core::control_wire::MAX_WIRE_PAYLOAD;

    fn gap_tracker(window: Duration) -> GapTracker {
        GapTracker::new(window, Default::default())
    }

    fn attach_stream_with_end() -> (
        tokio_mpsc::Sender<EventFrame>,
        tokio::sync::oneshot::Sender<StreamEnd>,
        AttachStream,
    ) {
        let (frame_tx, frames) = tokio_mpsc::channel(32);
        let (control, _controls) = tokio_mpsc::channel(32);
        let (end_tx, end) = tokio::sync::oneshot::channel();
        (
            frame_tx,
            end_tx,
            AttachStream {
                frames,
                control,
                end,
            },
        )
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_wait_bounds_a_probe_that_never_answers() {
        let started = tokio::time::Instant::now();
        let result = wait_for_spawned_daemon(SPAWN_WAIT_DEADLINE, SPAWN_WAIT_POLL, || {
            Box::pin(std::future::pending::<StopProbe>())
        })
        .await;
        let elapsed = tokio::time::Instant::now().duration_since(started);

        assert!(matches!(result, SpawnWait::TimedOut));
        assert!(elapsed <= SPAWN_WAIT_DEADLINE + SPAWN_WAIT_POLL);
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_wait_retries_not_running_until_up() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let result = wait_for_spawned_daemon(SPAWN_WAIT_DEADLINE, SPAWN_WAIT_POLL, move || {
            let calls = probe_calls.clone();
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                    StopProbe::NotRunning
                } else {
                    StopProbe::Ours
                }
            })
        })
        .await;

        assert!(matches!(result, SpawnWait::Up));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_wait_reports_a_foreign_scope() {
        let result = wait_for_spawned_daemon(SPAWN_WAIT_DEADLINE, SPAWN_WAIT_POLL, || {
            Box::pin(async { StopProbe::Foreign("other".into()) })
        })
        .await;

        assert!(matches!(result, SpawnWait::Foreign(actual) if actual == "other"));
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_wait_times_out_when_the_daemon_never_comes_up() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let result = wait_for_spawned_daemon(SPAWN_WAIT_DEADLINE, SPAWN_WAIT_POLL, move || {
            let calls = probe_calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                StopProbe::NotRunning
            })
        })
        .await;

        assert!(matches!(result, SpawnWait::TimedOut));
        assert!(calls.load(Ordering::SeqCst) <= 61);
    }

    #[test]
    fn spawn_daemon_child_is_reaped_after_it_exits() {
        if !crate::platform::sound_process::test_support::UNIX
            || !std::path::Path::new("/proc/self/stat").exists()
        {
            return;
        }

        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        own_spawned_daemon(child);

        let proc_entry = std::path::PathBuf::from(format!("/proc/{pid}"));
        let stat_path = proc_entry.join("stat");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while proc_entry.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if proc_entry.exists() {
            let stat = std::fs::read_to_string(stat_path).unwrap_or_else(|error| error.to_string());
            panic!("spawned child pid {pid} was not reaped; /proc/{pid}/stat: {stat}");
        }
    }

    #[test]
    fn sequence_gap_streak_backs_off_resets_and_obeys_window() {
        let start = tokio::time::Instant::now();
        let mut streak = None;
        let mut expected = Vec::new();
        for second in 0..9 {
            let decision = on_sequence_gap(
                &mut streak,
                Duration::from_secs(600),
                start + Duration::from_secs(second),
            );
            let GapDecision::Backoff(backoff) = decision else {
                panic!("gap within the window should back off");
            };
            expected.push(backoff);
        }
        assert_eq!(
            expected,
            [
                500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000
            ]
            .map(Duration::from_millis)
        );

        let resumed_at = streak.unwrap().resumed_at;
        assert_eq!(
            on_sequence_gap(
                &mut streak,
                Duration::from_secs(600),
                resumed_at + Duration::from_secs(61),
            ),
            GapDecision::Backoff(Duration::from_millis(500))
        );
        assert_eq!(streak.unwrap().gaps, 1);

        let mut window_streak = None;
        let mut decision = GapDecision::Backoff(Duration::ZERO);
        for second in 0..=600 {
            decision = on_sequence_gap(
                &mut window_streak,
                Duration::from_secs(600),
                start + Duration::from_secs(second),
            );
        }
        assert_eq!(
            decision,
            GapDecision::Unstable {
                gaps: 601,
                elapsed: Duration::from_secs(600),
            }
        );

        let mut unlimited_streak = None;
        for second in 0..=900 {
            assert!(matches!(
                on_sequence_gap(
                    &mut unlimited_streak,
                    Duration::ZERO,
                    start + Duration::from_secs(second),
                ),
                GapDecision::Backoff(_)
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_sequence_gaps_end_the_pane_as_unstable() {
        let tracker = gap_tracker(Duration::from_secs(1));
        let (frame_tx, end_tx, stream) = attach_stream_with_end();
        let mut session = adapt("session-1".into(), stream, tracker.clone());
        end_tx
            .send(StreamEnd::SequenceGap {
                expected: 4,
                got: 7,
            })
            .unwrap();
        drop(frame_tx);
        assert_eq!(session.frames.recv().await, None);

        tokio::time::advance(Duration::from_secs(2)).await;
        let (frame_tx, end_tx, stream) = attach_stream_with_end();
        let mut session = adapt("session-1".into(), stream, tracker);
        end_tx
            .send(StreamEnd::SequenceGap {
                expected: 8,
                got: 12,
            })
            .unwrap();
        drop(frame_tx);

        let Some(ExecFrame::Stdout(bytes)) = session.frames.recv().await else {
            panic!("unstable stream should emit a diagnostic");
        };
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains("pane stream unstable")
        );
        assert_eq!(session.frames.recv().await, Some(ExecFrame::Exit(254)));
        assert_eq!(session.frames.recv().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_sequence_gap_backs_off_before_reconnecting() {
        let before = tokio::time::Instant::now();
        let (frame_tx, end_tx, stream) = attach_stream_with_end();
        let mut session = adapt(
            "session-1".into(),
            stream,
            gap_tracker(Duration::from_secs(600)),
        );
        end_tx
            .send(StreamEnd::SequenceGap {
                expected: 2,
                got: 5,
            })
            .unwrap();
        drop(frame_tx);
        assert_eq!(session.frames.recv().await, None);
        assert!(tokio::time::Instant::now().duration_since(before) >= GAP_BACKOFF_BASE);
    }

    #[tokio::test]
    async fn permanent_stream_outcomes_emit_diagnostic_and_unknown_exit() {
        let cases = [
            (
                StreamEnd::IdentityMismatch {
                    expected: "session-1".into(),
                    got: "session-2".into(),
                },
                "IdentityMismatch",
            ),
            (
                StreamEnd::Protocol("duplicate Hello".into()),
                "duplicate Hello",
            ),
        ];

        for (end, reason) in cases {
            let (frame_tx, end_tx, stream) = attach_stream_with_end();
            let mut session = adapt(
                "session-1".into(),
                stream,
                gap_tracker(Duration::from_secs(600)),
            );
            end_tx.send(end).unwrap();
            drop(frame_tx);

            let diagnostic = tokio::time::timeout(Duration::from_secs(1), session.frames.recv())
                .await
                .unwrap()
                .unwrap();
            let ExecFrame::Stdout(bytes) = diagnostic else {
                panic!("expected a stream failure diagnostic");
            };
            let diagnostic = String::from_utf8(bytes).unwrap();
            assert!(diagnostic.starts_with("\r\n[pane stream failed: "));
            assert!(diagnostic.ends_with("]\r\n"));
            assert!(diagnostic.contains(reason), "{diagnostic:?}");
            assert_eq!(session.frames.recv().await, Some(ExecFrame::Exit(254)));
            assert_eq!(session.frames.recv().await, None);
        }
    }

    #[tokio::test]
    async fn transient_and_clean_stream_ends_emit_no_exit() {
        for end in [
            StreamEnd::TransportLost("socket reset".into()),
            StreamEnd::CleanClose,
        ] {
            let (frame_tx, end_tx, stream) = attach_stream_with_end();
            let mut session = adapt(
                "session-1".into(),
                stream,
                gap_tracker(Duration::from_secs(600)),
            );
            end_tx.send(end).unwrap();
            drop(frame_tx);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), session.frames.recv())
                    .await
                    .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn adapter_byte_budget_bounds_queued_stdout_and_drains_in_order() {
        let frame_size = MAX_WIRE_PAYLOAD / 2;
        let count = ADAPTER_BYTE_BUDGET / frame_size + 3;
        let (frame_tx, frames) = tokio_mpsc::channel(count);
        let (control, _controls) = tokio_mpsc::channel(1);
        let stream = AttachStream {
            frames,
            control,
            end: tokio::sync::oneshot::channel().1,
        };
        let mut session = adapt(
            "session-1".into(),
            stream,
            gap_tracker(Duration::from_secs(600)),
        );
        for seq in 0..count {
            frame_tx
                .send(EventFrame::PaneDelta {
                    session: "session-1".into(),
                    seq: seq as u64,
                    bytes: vec![b'x'; frame_size],
                })
                .await
                .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(session.frames.len() * frame_size <= ADAPTER_BYTE_BUDGET);

        for _ in 0..count {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), session.frames.recv())
                    .await
                    .unwrap(),
                Some(ExecFrame::Stdout(vec![b'x'; frame_size]))
            );
        }
    }

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
            classify_scope_reply(
                Err(anyhow::Error::new(
                    thegn_svc::control::client::ControlProtocolError::EmptySuccessBody,
                )),
                &expected
            ),
            ScopeCheck::Unknown
        );
        assert_eq!(
            classify_scope_reply(
                Err(anyhow::Error::new(
                    thegn_svc::control::client::ControlProtocolError::MissingJsonContentType,
                )),
                &expected
            ),
            ScopeCheck::Unknown
        );
        assert_eq!(
            classify_scope_reply(
                Err(anyhow::Error::new(
                    thegn_svc::control::client::ControlProtocolError::InvalidJson,
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
    async fn candidate_scan_short_circuits_on_the_first_verified_daemon() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let scan = scan_candidates(
            vec![
                ControlAddr::Unix("/a.sock".into()),
                ControlAddr::Unix("/b.sock".into()),
            ],
            Duration::from_secs(1),
            move |_| {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { StopProbe::Ours })
            },
        )
        .await;
        assert!(scan.found.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(scan.unassessed.is_empty());
    }

    #[tokio::test]
    async fn candidate_scan_deduplicates_registry_and_configured_socket() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let scan = scan_candidates(
            vec![
                ControlAddr::Unix("/a.sock".into()),
                ControlAddr::Unix("/a.sock".into()),
                ControlAddr::Tcp {
                    addr: "127.0.0.1:1".into(),
                    token: "secret".into(),
                },
            ],
            Duration::from_secs(1),
            move |_| {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { StopProbe::NotRunning })
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(scan.found.is_none());
        assert!(scan.unassessed.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn candidate_scan_reports_unassessed_candidates_at_the_shared_deadline() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let start = tokio::time::Instant::now();
        let scan = scan_candidates(
            vec![
                ControlAddr::Unix("/a.sock".into()),
                ControlAddr::Unix("/b.sock".into()),
                ControlAddr::Tcp {
                    addr: "127.0.0.1:2".into(),
                    token: "secret".into(),
                },
            ],
            Duration::from_secs(5),
            move |addr| {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                if matches!(addr, ControlAddr::Unix(path) if path == std::path::Path::new("/a.sock")) {
                    Box::pin(futures::future::pending())
                } else {
                    Box::pin(async { StopProbe::Ours })
                }
            },
        )
        .await;
        assert_eq!(scan.unassessed, ["/a.sock", "/b.sock", "127.0.0.1:2"]);
        assert!(scan.found.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let elapsed = tokio::time::Instant::now() - start;
        assert!(elapsed >= Duration::from_secs(5));
        assert!(elapsed < Duration::from_millis(5_002));
        assert!(
            scan.unassessed
                .iter()
                .all(|label| !label.contains("secret"))
        );
    }

    #[tokio::test]
    async fn candidate_scan_slow_registry_candidate_cannot_starve_local_socket() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let probe_calls = calls.clone();
        let scan = scan_startup_candidates(
            Some(ControlAddr::Unix("/configured.sock".into())),
            vec![ControlAddr::Tcp {
                addr: "127.0.0.1:2".into(),
                token: "secret".into(),
            }],
            Duration::from_millis(1),
            move |addr| {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                if matches!(addr, ControlAddr::Unix(path) if path == std::path::Path::new("/configured.sock")) {
                    Box::pin(async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        StopProbe::Ours
                    })
                } else {
                    Box::pin(futures::future::pending())
                }
            },
        )
        .await;

        assert!(scan.found.is_some());
        assert!(scan.unassessed.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn candidate_scan_startup_refusal_is_typed() {
        let unassessed = CandidateScan {
            found: None,
            foreign: Vec::new(),
            unassessed: vec!["/slow.sock".into()],
        };
        assert!(
            startup_refusal(&unassessed, "/configured.sock")
                .unwrap()
                .to_string()
                .contains("not starting another daemon")
        );

        let configured_foreign = CandidateScan {
            found: None,
            foreign: vec![("/configured.sock".into(), "other-scope".into())],
            unassessed: Vec::new(),
        };
        assert!(
            startup_refusal(&configured_foreign, "/configured.sock")
                .unwrap()
                .to_string()
                .contains("belongs to a different state directory")
        );

        let other_foreign = CandidateScan {
            found: None,
            foreign: vec![("/other.sock".into(), "other-scope".into())],
            unassessed: Vec::new(),
        };
        assert!(startup_refusal(&other_foreign, "/configured.sock").is_none());
    }

    #[tokio::test]
    async fn candidate_scan_bounds_a_listener_that_never_answers() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("d.sock");
        let endpoint = thegn_svc::ipc::IpcEndpoint::for_socket_path(&socket);
        let mut listener = match thegn_svc::ipc::IpcListener::bind_exclusive(&endpoint)
            .await
            .unwrap()
        {
            thegn_svc::ipc::BindOutcome::Bound(listener) => listener,
            thegn_svc::ipc::BindOutcome::AlreadyRunning => panic!("fresh socket must bind"),
        };
        let accept_task = tokio::spawn(async move {
            let mut streams = Vec::new();
            while let Ok(stream) = listener.accept_stream().await {
                streams.push(stream);
            }
            streams
        });
        let started = std::time::Instant::now();
        let candidate = ControlAddr::Unix(socket.clone());
        let scopes = vec!["scope".into()];
        let scan = scan_candidates(vec![candidate], Duration::from_millis(300), move |addr| {
            let scopes = scopes.clone();
            Box::pin(async move { probe_stop_target(&ControlClient::new(addr), &scopes).await })
        })
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(scan.unassessed, [socket.display().to_string()]);
        assert!(scan.found.is_none());
        accept_task.abort();
        assert!(accept_task.await.is_err());
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
        let session = adapt(
            "session-1".into(),
            stream,
            gap_tracker(Duration::from_secs(600)),
        );
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
        let mut session = adapt(
            "session-1".into(),
            stream,
            gap_tracker(Duration::from_secs(600)),
        );
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
                gap_streak: Default::default(),
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
            gap_streak: Default::default(),
            last_client: Default::default(),
        };
        assert!(source.session_absent("live").await.is_err());
    }
}
