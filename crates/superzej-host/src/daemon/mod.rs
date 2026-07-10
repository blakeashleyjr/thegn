//! The pane daemon (`szhost daemon`, hidden): a headless tokio process that
//! owns portable-pty sessions so they survive UI clients detaching.
//!
//! One daemon per state dir (`$XDG_STATE_HOME/superzej`) — the DB, session
//! table, and worktree registry are all per-state-dir, so `just start` /
//! smoke-test isolation gets an isolated daemon for free. **The unix socket is
//! the lock**: whoever binds it is the daemon; a second instance exits 0 and
//! the racing client just connects to the winner.
//!
//! All timers here (heartbeat, lease reaper, idle-exit) are daemon-process
//! tokio tasks — the compositor's 0%-idle event-loop contract binds the UI
//! loop, not this process, and nothing here ever ticks a UI client (clients
//! only receive frames via their own mpsc + waker path).

pub(crate) mod client;
pub(crate) mod service;
pub(crate) mod session;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc};

use superzej_core::config::Config;
use superzej_core::control::plan_leases;
use superzej_core::control_wire::{EventFrame, LeaseEventKind};
use superzej_core::db::Db;
use superzej_core::store::{ControlStore, DaemonRow};

use service::DaemonService;
use session::{IdleTransition, SessionMsg};

/// Heartbeat cadence; discovery treats rows fresher than
/// [`superzej_svc::control::client::DAEMON_HEARTBEAT_TTL_MS`] as live.
const HEARTBEAT_SECS: u64 = 15;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".into())
}

/// The daemon's scope key: the canonical state dir it serves.
pub(crate) fn scope_key() -> String {
    superzej_core::util::xdg_state_home()
        .join("superzej")
        .to_string_lossy()
        .into_owned()
}

/// Resolve the control-socket path from config + env (the pure helper lives in
/// core; this binds the ambient env).
pub(crate) fn socket_path(dcfg: &superzej_core::config::DaemonConfig) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
    dcfg.socket_path(
        runtime_dir.as_deref(),
        &superzej_core::util::xdg_state_home().join("superzej"),
    )
}

/// `szhost serve` options: expose the daemon to remote thin clients.
pub(crate) struct ServeOpts {
    /// TCP bind override (defaults to `[serve] bind`).
    pub bind: Option<String>,
    /// Skip minting + printing the startup pairing URL.
    pub no_pair_url: bool,
}

/// Entry point for the hidden `szhost daemon` subcommand: builds the runtime
/// and serves until shutdown. Exits 0 immediately if another daemon already
/// owns the socket.
pub(crate) fn run_blocking(cfg: &Config, socket_override: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("daemon runtime")?;
    rt.block_on(run(cfg, socket_override, None))
}

/// Entry point for `szhost serve` (foreground): the daemon runtime + a TCP
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
    // (opt-in via SUPERZEJ_LOG, same as the compositor) — otherwise a headless
    // daemon is unobservable. Free when SUPERZEJ_LOG is unset.
    if std::env::var_os("SUPERZEJ_LOG").is_some() {
        superzej_core::log_trace::init(
            superzej_core::log_trace::Role::Host,
            &superzej_core::config::LogConfig {
                file: true,
                ..Default::default()
            },
        );
    }
    let sock = socket_override.unwrap_or_else(|| socket_path(&cfg.daemon));
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // The socket is the lock. A connectable socket ⇒ a live daemon ⇒ exit 0
    // (the spawn race's loser); a stale file (bind would fail) is unlinked.
    if sock.exists() {
        match tokio::net::UnixStream::connect(&sock).await {
            Ok(_) => {
                tracing::info!(target: "szhost::daemon", "daemon already running on {}", sock.display());
                return Ok(());
            }
            Err(_) => {
                let _ = std::fs::remove_file(&sock);
            }
        }
    }
    let listener = match tokio::net::UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::info!(target: "szhost::daemon", "lost the daemon bind race; exiting");
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("bind {}", sock.display())),
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
    {
        let db = db.lock().expect("daemon db lock");
        for row in db.daemons().unwrap_or_default() {
            if row.scope == scope && !pid_alive(row.pid) {
                let _ = db.clear_daemon_leases(&row.daemon_id);
                let _ = db.del_daemon(&row.daemon_id);
            }
        }
        db.put_daemon(&DaemonRow {
            daemon_id: daemon_id.clone(),
            pid: std::process::id() as i64,
            scope: scope.clone(),
            endpoint: sock.to_string_lossy().into_owned(),
            tcp_addr: None,
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: now_ms(),
            heartbeat_at: now_ms(),
        })?;
    }

    let (events, _) = broadcast::channel::<Arc<EventFrame>>(1024);
    let (idle_tx, idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let svc = Arc::new(DaemonService {
        daemon_id: daemon_id.clone(),
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        events: events.clone(),
        db: db.clone(),
        grace_ms: (cfg.daemon.lease_grace_secs as i64).saturating_mul(1000),
        idle_tx,
        shutdown: shutdown.clone(),
        merge_queue: cfg.merge_queue.clone(),
    });

    // SIGTERM/SIGINT → the same graceful-shutdown path as the shutdown RPC,
    // so `kill <daemon>` still deregisters and unlinks the socket.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            shutdown.notify_waiters();
        });
    }
    // Heartbeat (registry freshness for discovery).
    tokio::spawn(heartbeat_loop(db.clone(), daemon_id.clone()));
    // Lease bookkeeping: idle/busy transitions + expiry reaping.
    tokio::spawn(lease_loop(svc.clone(), idle_rx));
    // Idle-exit: leave no orphan daemon behind an unused state dir.
    if cfg.daemon.idle_exit_secs > 0 {
        tokio::spawn(idle_exit_loop(
            svc.clone(),
            shutdown.clone(),
            std::time::Duration::from_secs(cfg.daemon.idle_exit_secs),
        ));
    }

    let state = superzej_svc::control::http::ControlState {
        api: svc.clone(),
        store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
        local_admin: cfg.serve.local_admin,
        require_approval: cfg.serve.require_approval,
        server_label: format!("{} superzej {}", hostname(), env!("CARGO_PKG_VERSION")),
    };
    let app = superzej_svc::control::http::router(state);

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
        {
            let db = db.lock().expect("daemon db lock");
            let mut row = db
                .daemons()
                .unwrap_or_default()
                .into_iter()
                .find(|d| d.daemon_id == daemon_id)
                .expect("own daemon row");
            row.tcp_addr = Some(actual.to_string());
            let _ = db.put_daemon(&row);
        }
        let tcp_state = superzej_svc::control::http::ControlState {
            api: svc.clone(),
            store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: false,
            require_approval: cfg.serve.require_approval,
            server_label: format!("{} superzej {}", hostname(), env!("CARGO_PKG_VERSION")),
        };
        let grpc = superzej_svc::control::grpc::GrpcControl {
            api: svc.clone(),
            store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: false,
            server_label: format!("{} superzej {}", hostname(), env!("CARGO_PKG_VERSION")),
        };
        let tcp_app = superzej_svc::control::http::router(tcp_state).merge(
            tonic::service::Routes::new(superzej_svc::control::grpc::ControlServer::new(grpc))
                .into_axum_router(),
        );
        let shutdown_tcp = shutdown.clone();
        tokio::spawn(async move {
            let _ = axum::serve(tcp, tcp_app)
                .with_graceful_shutdown(async move { shutdown_tcp.notified().await })
                .await;
        });

        superzej_core::outln!("superzej control plane listening on {actual} (HTTP/WS + gRPC)");
        if !opts.no_pair_url {
            let now = now_ms();
            let minted = superzej_svc::control::auth::mint(
                superzej_core::control::TokenKind::PairingCode,
                superzej_core::control::ScopeSet::parse("read"),
                "serve startup",
                None,
                Some(now + 15 * 60_000),
                now,
            );
            {
                let db = db.lock().expect("daemon db lock");
                db.put_pairing(&minted.row)?;
            }
            let url = superzej_core::control::PairingUrl {
                host: hostname(),
                port: actual.port(),
                code: minted.token,
                fp: None,
            };
            superzej_core::outln!("pair a client (single-use, read scope, 15 min):");
            superzej_core::outln!("  {}", url.encode());
            superzej_core::outln!("  {}", url.web_form());
            superzej_core::outln!(
                "mint more with `szhost pair new --scope read,git` · approve/revoke with `szhost pair`"
            );
        }
    }

    tracing::info!(target: "szhost::daemon", %daemon_id, "pane daemon serving on {}", sock.display());
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
    let _ = std::fs::remove_file(&sock);
    result.context("daemon serve")
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
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
            tracing::info!(target: "szhost::daemon", session = %lease.session_id, "relay lease expired; session reaped");
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

/// Exit when the daemon has had no sessions and no attached clients for
/// `idle_exit`. Coarse check (10s cadence, capped at the idle window) — this
/// is a janitor, not a hot path.
async fn idle_exit_loop(
    svc: Arc<DaemonService>,
    shutdown: Arc<tokio::sync::Notify>,
    idle_exit: std::time::Duration,
) {
    let cadence = idle_exit.min(std::time::Duration::from_secs(10));
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        tokio::time::sleep(cadence).await;
        let busy = !svc.sessions.lock().await.is_empty();
        if busy {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= idle_exit {
            tracing::info!(target: "szhost::daemon", "idle for {:?}; exiting", idle_exit);
            shutdown.notify_waiters();
            return;
        }
    }
}
