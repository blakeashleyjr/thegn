//! The pane daemon (`thegn daemon`, hidden): a headless tokio process that
//! owns portable-pty sessions so they survive UI clients detaching.
//!
//! One daemon per state dir (`$XDG_STATE_HOME/thegn`) — the DB, session
//! table, and worktree registry are all per-state-dir, so `just start` /
//! smoke-test isolation gets an isolated daemon for free. **The IPC endpoint
//! is the lock** (unix socket / Windows named pipe — `thegn_svc::ipc`):
//! whoever binds it is the daemon; a second instance exits 0 and the racing
//! client just connects to the winner.
//!
//! All timers here (heartbeat, lease reaper, idle-exit) are daemon-process
//! tokio tasks — the compositor's 0%-idle event-loop contract binds the UI
//! loop, not this process, and nothing here ever ticks a UI client (clients
//! only receive frames via their own mpsc + waker path).

pub(crate) mod agent_open;
pub(crate) mod client;
pub(crate) mod service;
pub(crate) mod session;
pub(crate) mod tombstone;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc};

use thegn_core::config::Config;
use thegn_core::control::plan_leases;
use thegn_core::control_wire::{EventFrame, LeaseEventKind};
use thegn_core::db::Db;
use thegn_core::store::{ControlStore, DaemonRow};

use service::DaemonService;
use session::{IdleTransition, SessionMsg};

/// Heartbeat cadence; discovery treats rows fresher than
/// [`thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS`] as live.
const HEARTBEAT_SECS: u64 = 15;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

use thegn_core::util::hostname;

/// The daemon's scope key: the canonical state dir it serves.
pub(crate) fn scope_key() -> String {
    thegn_core::util::xdg_state_home()
        .join("thegn")
        .to_string_lossy()
        .into_owned()
}

/// Resolve the control-socket path from config + env (the pure helper lives in
/// core; this binds the ambient env).
pub(crate) fn socket_path(dcfg: &thegn_core::config::DaemonConfig) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
    let natural = dcfg.socket_path(
        runtime_dir.as_deref(),
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    // An explicit `[daemon] socket` is the user's word — never relocate it.
    // (They may be pointing two thegns at one daemon deliberately.)
    if !dcfg.socket.is_empty() {
        return natural;
    }
    let max = thegn_core::config_daemon::max_socket_path_len(cfg!(target_os = "linux"));
    thegn_core::config_daemon::resolve_socket_path(
        natural,
        short_runtime_dir().as_deref(),
        max,
        cfg!(windows),
    )
}

/// A short, private directory to host the control socket when the natural path
/// would overflow `sun_path`.
///
/// This is the hole in the fallback chain that made macOS fragile: Linux has
/// `$XDG_RUNTIME_DIR` (`/run/user/<uid>`, short and 0700 by spec), macOS has no
/// XDG runtime dir at all, so thegn fell back to the *state* dir — whose depth
/// is exactly what overflows. `$TMPDIR` on macOS is the OS-created per-user
/// `/var/folders/<…>/T` (mode 0700, owned by the login user), which is the
/// direct analogue.
///
/// Vetted, not trusted: `$TMPDIR` is attacker-settable in a hostile environment,
/// and the local control plane grants admin to any socket peer
/// ([`thegn_core::config::ServeConfig::local_admin`]) — so a world-writable or
/// foreign-owned directory here would let someone else bind the socket first and
/// impersonate the daemon. Require a real directory, owned by us, with no group
/// or other access. Anything less ⇒ `None` ⇒ no relocation, and the caller
/// degrades to in-process panes instead.
fn short_runtime_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = PathBuf::from(std::env::var_os("TMPDIR")?);
        let md = std::fs::metadata(&dir).ok()?;
        let ours = md.uid() == unsafe { libc::geteuid() };
        let private = md.permissions().mode() & 0o077 == 0;
        (md.is_dir() && ours && private).then_some(dir)
    }
    #[cfg(not(unix))]
    {
        // Windows endpoints are hashed pipe names — length is never a problem.
        None
    }
}

/// `thegn serve` options: expose the daemon to remote thin clients.
pub(crate) struct ServeOpts {
    /// TCP bind override (defaults to `[serve] bind`).
    pub bind: Option<String>,
    /// Skip minting + printing the startup pairing URL.
    pub no_pair_url: bool,
}

/// Entry point for the hidden `thegn daemon` subcommand: builds the runtime
/// and serves until shutdown. Exits 0 immediately if another daemon already
/// owns the socket.
pub(crate) fn run_blocking(cfg: &Config, socket_override: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("daemon runtime")?;
    rt.block_on(run(cfg, socket_override, None))
}

/// Entry point for `thegn serve` (foreground): the daemon runtime + a TCP
/// listener (HTTP/WS + gRPC, bearer-token auth) + a printed pairing URL.
pub(crate) fn serve_blocking(cfg: &Config, opts: ServeOpts) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("daemon runtime")?;
    rt.block_on(run(cfg, None, Some(opts)))
}

async fn run(
    cfg: &Config,
    socket_override: Option<PathBuf>,
    serve: Option<ServeOpts>,
) -> Result<()> {
    // The daemon is its own process, so it installs its own file-log subscriber
    // (opt-in via THEGN_LOG, same as the compositor) — otherwise a headless
    // daemon is unobservable. Free when THEGN_LOG is unset.
    if std::env::var_os("THEGN_LOG").is_some() {
        thegn_core::log_trace::init(
            thegn_core::log_trace::Role::Host,
            &thegn_core::config::LogConfig {
                file: true,
                ..Default::default()
            },
        );
    }
    let sock = socket_override.unwrap_or_else(|| socket_path(&cfg.daemon));
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
        // Owner-only (0700) on the run-dir holding the control socket: the
        // XDG_RUNTIME_DIR path is already 0700, but the state-dir fallback
        // (`$XDG_STATE_HOME/thegn/run`, used when XDG_RUNTIME_DIR is unset —
        // ssh-without-logind, cron, containers) inherits the umask. Best-effort.
        let _ = thegn_core::fsperm::restrict_dir_to_owner(parent);
    }
    // Pre-check the `sun_path` bound. The compositor degrades to in-process
    // panes on this (see `handlers::startup::daemon_active`), but a DIRECT
    // `thegn daemon` / `thegn serve` must fail loudly and say why: std's own
    // bind error is a bare "path must be shorter than SUN_LEN" naming neither
    // the limit nor the path.
    {
        use thegn_core::config_daemon::{check_socket_path_len, max_socket_path_len};
        let max = max_socket_path_len(cfg!(target_os = "linux"));
        if let Err(too_long) = check_socket_path_len(&sock, max, cfg!(windows)) {
            anyhow::bail!(
                "control socket path is {} bytes, over this platform's {}-byte limit: {}\n\
                 Set [daemon] socket (or --socket) to a shorter path.",
                too_long.len,
                too_long.max,
                sock.display()
            );
        }
    }
    let ep = thegn_svc::ipc::IpcEndpoint::for_socket_path(&sock);

    // The endpoint is the lock (unix socket / Windows named pipe — see
    // `thegn_svc::ipc`). A connectable endpoint ⇒ a live daemon ⇒ exit 0
    // (the spawn race's loser); a stale socket file is unlinked in the seam.
    let listener = match thegn_svc::ipc::IpcListener::bind_exclusive(&ep)
        .await
        .with_context(|| format!("bind {}", ep.display()))?
    {
        thegn_svc::ipc::BindOutcome::Bound(l) => l,
        thegn_svc::ipc::BindOutcome::AlreadyRunning => {
            // `thegn daemon` (the compositor's pane-daemon ensure): the spawn
            // race's loser exits 0 quietly — a live daemon on the socket is the
            // whole point. But `thegn serve` needs to OPEN a TCP listener, which
            // the AlreadyRunning path never reaches; returning Ok here means serve
            // silently no-ops (exit 0, no listener) whenever a pane daemon already
            // holds the socket — the common case after any TUI use. Surface it.
            if serve.is_some() {
                anyhow::bail!(
                    "a pane daemon already owns {} — stop it (or set [daemon] enabled = false) \
                     before running `thegn serve`, or point serve at a different [daemon] socket",
                    ep.display()
                );
            }
            tracing::info!(target: "thegn::daemon", "daemon already running on {}", ep.display());
            return Ok(());
        }
    };

    let db: service::SharedDb = Arc::new(Mutex::new(Db::open()?));
    let scope = scope_key();
    let daemon_id = {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).expect("csprng for daemon id");
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    };

    // Boot sweep: previous daemons for this scope whose pid is gone left
    // meaningless registry rows and leases (their PTYs died with them).
    //
    // No session-pid reaping is needed even though daemon-persistent bwrap
    // panes drop `--die-with-parent`: when a daemon process dies (graceful OR
    // SIGKILL), the kernel closes every fd it held — including each session's
    // PTY master AND the reader thread's cloned fd — which hangs up the tty and
    // delivers SIGHUP to the child. bwrap is PID 1 of its unshared namespace,
    // so its death collapses the whole namespace. The guarantee `--die-with-
    // parent` used to give (die with the forking process) is preserved by the
    // tty hangup; what it wrongly added — dying with the forking *thread* — is
    // what we shed. Per-session teardown while the daemon lives is handled by
    // the actor's explicit child-terminate (see `SessionActor::run`).
    //
    // Our registry row is kept in scope for the daemon's lifetime: serve mode
    // re-puts it with `tcp_addr` filled in, WITHOUT re-reading it from the DB
    // (a transient SQLITE_BUSY read there used to become a guaranteed panic).
    let mut daemon_row = DaemonRow {
        daemon_id: daemon_id.clone(),
        pid: std::process::id() as i64,
        scope: scope.clone(),
        // The endpoint's stable string form: the socket path on unix, the
        // `\\.\pipe\…` name on Windows. Discovery classifies by prefix.
        endpoint: ep.display(),
        tcp_addr: None,
        hostname: hostname(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: now_ms(),
        heartbeat_at: now_ms(),
    };
    {
        let db = db.lock().expect("daemon db lock");
        for stale in boot_sweep_targets(&db.daemons().unwrap_or_default(), &scope, pid_alive) {
            let _ = db.clear_daemon_leases(&stale);
            let _ = db.del_daemon(&stale);
        }
        db.put_daemon(&daemon_row)?;
    }

    let (events, _) = broadcast::channel::<Arc<EventFrame>>(1024);
    let (idle_tx, idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let svc = Arc::new(DaemonService {
        daemon_id: daemon_id.clone(),
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        tombs: Arc::new(tokio::sync::Mutex::new(
            thegn_core::graveyard::Graveyard::new(
                tombstone::MAX_TOMBSTONES,
                tombstone::TOMBSTONE_TTL_MS,
            ),
        )),
        events: events.clone(),
        db: db.clone(),
        grace_ms: (cfg.daemon.lease_grace_secs as i64).saturating_mul(1000),
        idle_tx,
        shutdown: shutdown.clone(),
        config: std::sync::Arc::new(cfg.clone()),
        endpoint: ep.display(),
    });

    // The resource ceiling has to be published *in this process*: the daemon is
    // spawned detached from `current_exe`, so `main.rs`/`run.rs` publishing it
    // in the compositor does nothing for the sessions the daemon owns. Without
    // this, a session opened straight against the control API escapes every cap.
    thegn_core::sandbox_cpucap::publish_background_limits(
        thegn_core::sandbox::SandboxLimits::from(&cfg.sandbox.limits),
    );
    // Warm the scope probe off the runtime's worker threads: it spawns a real
    // `systemd-run … true`, and no control-API request should ever pay for it.
    tokio::task::spawn_blocking(|| {
        let usable = thegn_core::sandbox_cpucap::warm_scope_probe();
        tracing::debug!(target: "thegn::daemon", scope_usable = usable, "cpu-cap probe warmed");
    });

    // SIGTERM/SIGINT (console-close on Windows) → the same graceful-shutdown
    // path as the shutdown RPC, so `kill <daemon>` still deregisters and
    // unlinks the socket.
    crate::platform::spawn_shutdown_notifier(shutdown.clone());
    // Heartbeat (registry freshness for discovery).
    tokio::spawn(heartbeat_loop(db.clone(), daemon_id.clone()));
    // Lease bookkeeping: idle/busy transitions + expiry reaping.
    tokio::spawn(lease_loop(svc.clone(), idle_rx));
    // Idle-exit: leave no orphan daemon behind an unused state dir. `thegn
    // serve` is exempt — its TCP listener exists precisely for thin clients
    // that haven't connected yet, and self-terminating would tear the control
    // plane down under them.
    if let Some(window) = idle_exit_window(cfg.daemon.idle_exit_secs, serve.is_some()) {
        tokio::spawn(idle_exit_loop(svc.clone(), shutdown.clone(), window));
    }

    let state = thegn_svc::control::http::ControlState {
        api: svc.clone(),
        store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
        local_admin: cfg.serve.local_admin,
        require_approval: cfg.serve.require_approval,
        server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
    };
    let app = thegn_svc::control::http::router(state);

    // Serve mode: a TCP listener for remote thin clients — the same HTTP/WS
    // surface merged with the gRPC service, bearer tokens REQUIRED (never
    // local_admin on TCP) — plus a startup pairing URL. v1 is plaintext:
    // bind to a trusted interface (tailscale/wireguard) or reach it over
    // `ssh -L`; every request is still token-gated.
    if let Some(opts) = serve {
        let bind = opts.bind.unwrap_or_else(|| cfg.serve.bind.clone());
        let tcp = tokio::net::TcpListener::bind(&bind)
            .await
            .with_context(|| format!("bind {bind}"))?;
        let actual = tcp.local_addr().context("serve local_addr")?;
        // Advertise the TCP address on our registry row (`put_daemon` replaces
        // by daemon_id). Surfaced with `?`: a serve invocation that can't
        // register its address is undiscoverable and should fail loudly.
        daemon_row.tcp_addr = Some(actual.to_string());
        daemon_row.heartbeat_at = now_ms();
        {
            let db = db.lock().expect("daemon db lock");
            db.put_daemon(&daemon_row)
                .context("record serve tcp_addr in the daemon registry")?;
        }
        let tcp_state = thegn_svc::control::http::ControlState {
            api: svc.clone(),
            store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: false,
            require_approval: cfg.serve.require_approval,
            server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
        };
        let grpc = thegn_svc::control::grpc::GrpcControl {
            api: svc.clone(),
            store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: false,
            server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
        };
        let tcp_app = thegn_svc::control::http::router(tcp_state).merge(
            tonic::service::Routes::new(thegn_svc::control::grpc::ControlServer::new(grpc))
                .into_axum_router(),
        );
        let shutdown_tcp = shutdown.clone();
        tokio::spawn(async move {
            let _ = axum::serve(tcp, tcp_app)
                .with_graceful_shutdown(async move { shutdown_tcp.notified().await })
                .await;
        });

        thegn_core::outln!("thegn control plane listening on {actual} (HTTP/WS + gRPC)");
        if !opts.no_pair_url {
            let now = now_ms();
            let minted = thegn_svc::control::auth::mint(
                thegn_core::control::TokenKind::PairingCode,
                thegn_core::control::ScopeSet::parse("read"),
                "serve startup",
                None,
                Some(now + 15 * 60_000),
                now,
            );
            {
                let db = db.lock().expect("daemon db lock");
                db.put_pairing(&minted.row)?;
            }
            let url = thegn_core::control::PairingUrl {
                host: hostname(),
                port: actual.port(),
                code: minted.token,
                fp: None,
            };
            thegn_core::outln!("pair a client (single-use, read scope, 15 min):");
            thegn_core::outln!("  {}", url.encode());
            thegn_core::outln!("  {}", url.web_form());
            thegn_core::outln!(
                "mint more with `thegn pair new --scope read,git` · approve/revoke with `thegn pair`"
            );
        }
    }

    tracing::info!(target: "thegn::daemon", %daemon_id, "pane daemon serving on {}", ep.display());
    let shutdown_wait = shutdown.clone();
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown_wait.notified().await });
    let result = serve.await;

    // Cleanup: registry row + socket file. Leases stay only if sessions do —
    // a graceful shutdown killed them, so sweep ours.
    {
        let db = db.lock().expect("daemon db lock");
        let _ = db.clear_daemon_leases(&daemon_id);
        let _ = db.del_daemon(&daemon_id);
    }
    // best-effort: unlink the unix socket file; on Windows `sock` is only the
    // pipe-name seed (no fs entry), so this is a harmless no-op failure.
    let _ = std::fs::remove_file(&sock);
    result.context("daemon serve")
}

fn pid_alive(pid: i64) -> bool {
    crate::platform::pid_alive(pid)
}

/// Boot-sweep decision (pure): which registry rows a starting daemon removes —
/// SAME-scope rows whose pid is gone (their PTYs died with the process, so the
/// row and its leases are meaningless). Rows for other scopes belong to other
/// state dirs' daemons and are never touched, dead or alive.
fn boot_sweep_targets(
    rows: &[DaemonRow],
    scope: &str,
    pid_alive: impl Fn(i64) -> bool,
) -> Vec<String> {
    rows.iter()
        .filter(|r| r.scope == scope && !pid_alive(r.pid))
        .map(|r| r.daemon_id.clone())
        .collect()
}

/// Idle-exit policy (pure): a plain pane daemon with a nonzero window runs the
/// janitor; `0` disables it; serve mode ALWAYS disables it — the TCP control
/// plane must outlive "no sessions yet" (`[daemon] idle_exit_secs` docs).
fn idle_exit_window(idle_exit_secs: u64, serve_mode: bool) -> Option<std::time::Duration> {
    (!serve_mode && idle_exit_secs > 0).then(|| std::time::Duration::from_secs(idle_exit_secs))
}

async fn heartbeat_loop(db: service::SharedDb, daemon_id: String) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let db = db.clone();
        let id = daemon_id.clone();
        // best-effort: a missed heartbeat only delays discovery
        let _ = tokio::task::spawn_blocking(move || {
            let db = db.lock().expect("daemon db lock");
            let _ = db.touch_daemon_heartbeat(&id, now_ms());
        })
        .await;
    }
}

/// Lease supervision: consume idle/busy transitions from session actors and
/// reap sessions whose relay grace expired. Event-driven — sleeps until the
/// earliest pending expiry or the next transition, never polls.
async fn lease_loop(svc: Arc<DaemonService>, mut idle_rx: mpsc::UnboundedReceiver<IdleTransition>) {
    loop {
        // Reclaim expired tombstones here rather than under a janitor of their
        // own. This loop already wakes on every session death — the actor sends
        // an `IdleTransition` as it tears down — so the sweep lands promptly
        // with no new timer, and without depending on `idle_exit_loop`, which
        // is not even spawned when `idle_exit_secs = 0`.
        svc.tombs.lock().await.sweep(now_ms());

        // Decide: reap what's due, then sleep until the next expiry (if any).
        let (due, next_wake_at) = {
            let db = svc.db.lock().expect("daemon db lock");
            let leases = db.leases(&svc.daemon_id).unwrap_or_default();
            let plan = plan_leases(&leases, now_ms());
            let due = if plan.reap.is_empty() {
                Vec::new()
            } else {
                db.reap_expired_leases(&svc.daemon_id, now_ms())
                    .unwrap_or_default()
            };
            (due, plan.next_wake_at)
        };
        for lease in due {
            // Reap the PTY: the grace period ended with no client returning.
            let tx = svc
                .sessions
                .lock()
                .await
                .get(&lease.session_id)
                .map(|e| e.msg_tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(SessionMsg::Kill).await;
            }
            let _ = svc.events.send(Arc::new(EventFrame::Lease {
                session: lease.session_id.clone(),
                kind: LeaseEventKind::Reaped,
                expires_at: lease.expires_at,
            }));
            tracing::info!(target: "thegn::daemon", session = %lease.session_id, "relay lease expired; session reaped");
        }

        let sleep_until = next_wake_at.map(|at| {
            let delta = (at - now_ms()).max(0) as u64;
            tokio::time::Instant::now() + std::time::Duration::from_millis(delta)
        });
        tokio::select! {
            t = idle_rx.recv() => match t {
                Some(IdleTransition { session, idle: true }) => svc.on_session_idle(&session).await,
                Some(IdleTransition { session, idle: false }) => svc.on_session_busy(&session).await,
                None => return, // service gone
            },
            _ = async {
                match sleep_until {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
}

/// Exit when the daemon has had no live sessions for `idle_exit`. Never
/// spawned in serve mode (see [`idle_exit_window`]). Coarse check (10s
/// cadence, capped at the idle window) — this is a janitor, not a hot path.
async fn idle_exit_loop(
    svc: Arc<DaemonService>,
    shutdown: Arc<tokio::sync::Notify>,
    idle_exit: std::time::Duration,
) {
    let cadence = idle_exit.min(std::time::Duration::from_secs(10));
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        tokio::time::sleep(cadence).await;
        // LIVE sessions only. Tombstones deliberately do not count: a daemon
        // holding nothing but corpses has no work left, and letting them keep
        // it alive would defeat idle-exit for the whole tombstone TTL.
        let busy = !svc.sessions.lock().await.is_empty();
        if busy {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= idle_exit {
            tracing::info!(target: "thegn::daemon", "idle for {:?}; exiting", idle_exit);
            shutdown.notify_waiters();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, scope: &str, pid: i64) -> DaemonRow {
        DaemonRow {
            daemon_id: id.into(),
            pid,
            scope: scope.into(),
            endpoint: format!("/run/{id}.sock"),
            tcp_addr: None,
            hostname: "h".into(),
            version: "0".into(),
            started_at: 0,
            heartbeat_at: 0,
        }
    }

    /// The boot sweep removes exactly the same-scope rows whose pid is dead:
    /// a live same-scope daemon and any other scope's rows (even dead ones —
    /// their own next boot sweeps them) are kept.
    #[test]
    fn boot_sweep_removes_only_same_scope_dead_daemons() {
        let rows = vec![
            row("dead-same", "/scope/a", 11),
            row("alive-same", "/scope/a", 22),
            row("dead-other", "/scope/b", 33),
        ];
        let alive = |pid: i64| pid == 22;
        assert_eq!(
            boot_sweep_targets(&rows, "/scope/a", alive),
            vec!["dead-same".to_string()]
        );
        // Nothing dead in-scope ⇒ nothing swept.
        assert!(boot_sweep_targets(&rows, "/scope/c", alive).is_empty());
    }

    /// The idle-exit janitor arms only for a plain pane daemon: `0` = never,
    /// and serve mode is always exempt (the TCP listener must keep serving
    /// thin clients that haven't connected yet).
    #[test]
    fn idle_exit_only_arms_for_a_plain_daemon() {
        assert_eq!(
            idle_exit_window(1800, false),
            Some(std::time::Duration::from_secs(1800))
        );
        assert_eq!(idle_exit_window(0, false), None, "0 = never");
        assert_eq!(idle_exit_window(1800, true), None, "serve never idle-exits");
        assert_eq!(idle_exit_window(0, true), None);
    }
}
