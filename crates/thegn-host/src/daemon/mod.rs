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

pub(crate) mod agent_error_cache;
pub(crate) mod agent_open;
pub(crate) mod calendar_api;
pub(crate) mod client;
pub(crate) mod fork;
pub(crate) mod inbox;
pub(crate) mod pipeline_reaper;
pub(crate) mod pipeline_retry;
mod pty_diagnostics;
pub(crate) mod record;
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

/// Registry capability marker for daemons that participate in the THE-55
/// per-worktree open fence. A migration refuses an older daemon: it could
/// otherwise accept a matching session after migration's initial listing.
pub(crate) const SESSION_MIGRATION_FENCE_VERSION_SUFFIX: &str = "+session-migration-fence-v1";

/// Heartbeat cadence; discovery treats rows fresher than
/// [`thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS`] as live.
const HEARTBEAT_SECS: u64 = 15;

/// Local-only endpoint used to confirm that a Unix socket belongs to this
/// state directory. This is intentionally absent from the TCP control API.
pub(crate) const SCOPE_PATH: &str = "/daemon/scope";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
        .unwrap_or(0)
}

use thegn_core::util::hostname;

/// The daemon's scope key: the canonical state dir it serves.
pub(crate) fn scope_key() -> String {
    scope_key_for(&thegn_core::util::xdg_state_home().join("thegn"))
}

/// Current and pre-upgrade registry scopes for this state dir. The legacy
/// spelling is available only for UTF-8 paths; those paths were lossless under
/// the old scheme, so it cannot alias a distinct state dir.
pub(crate) fn scope_aliases() -> Vec<String> {
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    let mut aliases = vec![scope_key_for(&state_dir)];
    if let Some(legacy) = state_dir.to_str() {
        aliases.push(legacy.to_owned());
    }
    aliases
}

fn scope_key_for(state_dir: &std::path::Path) -> String {
    thegn_core::config_daemon::state_scope_identity(state_dir)
}

fn scope_router(scope: String) -> axum::Router {
    axum::Router::new().route(
        SCOPE_PATH,
        axum::routing::get(move || {
            let scope = scope.clone();
            async move { axum::Json(serde_json::json!({ "scope": scope })) }
        }),
    )
}

/// Resolve the control-socket path from config + env (the pure helper lives in
/// core; this binds the ambient env).
pub(crate) fn socket_path(dcfg: &thegn_core::config::DaemonConfig) -> PathBuf {
    socket_path_for(
        dcfg,
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        &thegn_core::util::xdg_state_home().join("thegn"),
        effective_uid(),
        short_runtime_dir(),
    )
}

/// Whether pane startup can safely route to a daemon endpoint. This deliberately
/// performs only metadata checks and a legacy connect probe; it never creates a
/// directory on the UI thread.
pub(crate) fn daemon_socket_usable(dcfg: &thegn_core::config::DaemonConfig) -> bool {
    let socket = socket_path(dcfg);
    static PROBES: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let probes = PROBES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut probes = probes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(usable) = probes.get(&socket) {
        return *usable;
    }
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    let usable = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            socket_usable_decision(
                legacy_socket_state(),
                &socket,
                &legacy,
                &private_tmp_socket_path(
                    &thegn_core::util::xdg_state_home().join("thegn"),
                    effective_uid(),
                ),
                !dcfg.socket.is_empty(),
                &|path| {
                    let metadata = if path == std::path::Path::new("/tmp") {
                        std::fs::metadata(path).ok()?
                    } else {
                        std::fs::symlink_metadata(path).ok()?
                    };
                    Some(DirMeta {
                        is_dir: metadata.is_dir(),
                        is_symlink: metadata.file_type().is_symlink(),
                        owned: metadata.uid() == effective_uid(),
                        mode: metadata.permissions().mode(),
                    })
                },
            )
        }
        #[cfg(not(unix))]
        {
            legacy_socket_state() != LegacySocketState::Live && socket != legacy
        }
    };
    probes.insert(socket, usable);
    usable
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct DirMeta {
    is_dir: bool,
    is_symlink: bool,
    owned: bool,
    mode: u32,
}

#[cfg(unix)]
fn socket_usable_decision(
    legacy_state: LegacySocketState,
    socket: &std::path::Path,
    legacy: &std::path::Path,
    private_tmp: &std::path::Path,
    explicit: bool,
    probe: &dyn Fn(&std::path::Path) -> Option<DirMeta>,
) -> bool {
    if legacy_state == LegacySocketState::Live || socket == legacy {
        return false;
    }
    if explicit {
        return true;
    }
    let Some(parent) = socket.parent() else {
        return false;
    };
    if parent
        .parent()
        .and_then(std::path::Path::file_name)
        .is_some_and(|name| name == "thegn")
    {
        return runtime_socket_parent_usable(parent, probe);
    }
    if let Some(metadata) = probe(parent) {
        return metadata.is_dir
            && !metadata.is_symlink
            && metadata.owned
            && metadata.mode & 0o777 == 0o700;
    }
    if socket == private_tmp {
        return probe(std::path::Path::new("/tmp")).is_some_and(|metadata| {
            metadata.is_dir && !metadata.is_symlink && metadata.mode & 0o1000 != 0
        });
    }
    false
}

#[cfg(unix)]
fn runtime_socket_parent_usable(
    parent: &std::path::Path,
    probe: &dyn Fn(&std::path::Path) -> Option<DirMeta>,
) -> bool {
    // Runtime sockets have three directory levels below XDG_RUNTIME_DIR:
    // `thegn/<scope-key>/daemon.sock`. Check every existing ancestor in that
    // layout, even when the private per-scope leaf already exists.
    let mut ancestor = Some(parent);
    let mut found = false;
    let mut has_private_anchor = false;
    for depth in 0..3 {
        let Some(path) = ancestor else { break };
        if let Some(metadata) = probe(path) {
            found = true;
            if !metadata.is_dir || metadata.is_symlink || !metadata.owned {
                return false;
            }
            if depth == 0 || depth == 2 {
                if metadata.mode & 0o777 != 0o700 {
                    return false;
                }
                has_private_anchor = true;
            } else if metadata.mode & 0o022 != 0 {
                return false;
            }
        }
        ancestor = path.parent();
    }
    found && has_private_anchor
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn private_tmp_socket_path(state_dir: &std::path::Path, euid: u32) -> PathBuf {
    std::path::Path::new("/tmp")
        .join(format!(
            "thegn-{euid}-{}",
            thegn_core::config_daemon::state_endpoint_key(state_dir)
        ))
        .join("daemon.sock")
}

fn socket_path_for(
    dcfg: &thegn_core::config::DaemonConfig,
    runtime_dir: Option<&str>,
    state_dir: &std::path::Path,
    euid: u32,
    private_runtime_dir: Option<PathBuf>,
) -> PathBuf {
    let natural = dcfg.socket_path(runtime_dir, state_dir);
    // An explicit `[daemon] socket` is the user's word — never relocate it.
    // (They may be pointing two thegns at one daemon deliberately.)
    if !dcfg.socket.is_empty() {
        return natural;
    }
    let natural = if runtime_dir.is_none_or(str::is_empty) {
        match private_runtime_dir {
            Some(private) => thegn_core::config_daemon::short_socket_path(&private, state_dir),
            None => private_tmp_socket_path(state_dir, euid),
        }
    } else {
        natural
    };
    let max = thegn_core::config_daemon::max_socket_path_len(cfg!(target_os = "linux"));
    thegn_core::config_daemon::resolve_socket_path(
        natural,
        short_runtime_dir().as_deref(),
        max,
        cfg!(windows),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacySocketState {
    Absent,
    Stale,
    Live,
}

pub(crate) fn legacy_socket_decision(
    exists: bool,
    error: Option<std::io::ErrorKind>,
) -> LegacySocketState {
    if !exists {
        LegacySocketState::Absent
    } else {
        match error {
            None => LegacySocketState::Live,
            Some(std::io::ErrorKind::ConnectionRefused) => LegacySocketState::Stale,
            Some(_) => LegacySocketState::Live,
        }
    }
}

#[cfg(unix)]
fn legacy_runtime_socket_should_remove(
    exists: bool,
    is_socket: bool,
    error: Option<std::io::ErrorKind>,
) -> bool {
    exists && is_socket && legacy_socket_decision(exists, error) == LegacySocketState::Stale
}

pub(crate) fn legacy_socket_state() -> LegacySocketState {
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    let exists = std::fs::symlink_metadata(&legacy).is_ok();
    #[cfg(unix)]
    let connect_error = std::os::unix::net::UnixStream::connect(&legacy)
        .err()
        .map(|e| e.kind());
    #[cfg(not(unix))]
    let connect_error = Some(std::io::ErrorKind::Other);
    legacy_socket_decision(exists, connect_error)
}

#[cfg(unix)]
fn validate_private_fallback_parent(socket: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(parent) = socket.parent() else {
        anyhow::bail!("private fallback socket has no parent directory");
    };
    match std::fs::symlink_metadata(parent) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == effective_uid()
                && metadata.permissions().mode() & 0o777 == 0o700 =>
        {
            Ok(())
        }
        Ok(_) => anyhow::bail!(
            "private fallback directory {} already exists with the wrong owner or mode; move it aside before starting the daemon",
            parent.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspect private fallback directory {}", parent.display())),
    }
}

pub(crate) fn reconcile_legacy_socket(_dcfg: &thegn_core::config::DaemonConfig) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        // The shared runtime endpoint belongs to no single state scope. Clean
        // it independently so state-dir legacy handling cannot skip it.
        reconcile_legacy_runtime_socket();
        let legacy = thegn_core::config_daemon::legacy_socket_path(
            &thegn_core::util::xdg_state_home().join("thegn"),
        );
        if (!_dcfg.socket.is_empty() && socket_path(_dcfg) == legacy)
            || legacy_socket_state() != LegacySocketState::Stale
        {
            return;
        }
        match std::fs::symlink_metadata(&legacy) {
            Ok(md) if md.file_type().is_socket() => {
                if let Err(error) = std::fs::remove_file(&legacy) {
                    tracing::warn!(path = %legacy.display(), %error, "could not remove stale legacy daemon socket");
                }
            }
            Ok(_) => {
                tracing::warn!(path = %legacy.display(), "legacy daemon endpoint is not a socket; leaving it untouched")
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %legacy.display(), %error, "could not inspect legacy daemon endpoint");
            }
            Err(_) => {}
        }
    }
}

#[cfg(unix)]
fn reconcile_legacy_runtime_socket() {
    use std::os::unix::fs::FileTypeExt as _;
    // A live socket may belong to another scope, so only remove it when a
    // connect proves that it is stale; this cleanup never blocks this daemon.
    if let Some(runtime_legacy) = thegn_core::config_daemon::legacy_runtime_socket_path(
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    ) {
        let metadata = std::fs::symlink_metadata(&runtime_legacy);
        let exists = metadata.is_ok();
        let is_socket = metadata.as_ref().is_ok_and(|md| md.file_type().is_socket());
        let connect_error = if is_socket {
            std::os::unix::net::UnixStream::connect(&runtime_legacy)
                .err()
                .map(|error| error.kind())
        } else {
            None
        };
        if legacy_runtime_socket_should_remove(exists, is_socket, connect_error) {
            if let Err(error) = std::fs::remove_file(&runtime_legacy) {
                tracing::warn!(path = %runtime_legacy.display(), %error, "could not remove stale legacy runtime daemon socket");
            }
        } else if exists && !is_socket {
            tracing::warn!(path = %runtime_legacy.display(), "legacy runtime daemon endpoint is not a socket; leaving it untouched");
        }
    }
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
/// and the local control plane can grant same-EUID peers implicit admin
/// ([`thegn_core::config::ServeConfig::local_admin`]). A world-writable or
/// foreign-owned directory would still permit endpoint replacement or daemon
/// impersonation. Require a real directory, owned by us, with no group or other
/// access. Anything less ⇒ `None` ⇒ no relocation, and the caller degrades to
/// in-process panes instead.
fn short_runtime_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mut candidates = Vec::new();
        let uid = unsafe { libc::geteuid() };
        if cfg!(target_os = "linux") {
            candidates.push(PathBuf::from(format!("/run/user/{uid}")));
        } else if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            candidates.push(PathBuf::from(tmpdir));
        }
        candidates.into_iter().find(|dir| {
            let Ok(md) = std::fs::symlink_metadata(dir) else {
                return false;
            };
            md.is_dir()
                && !md.file_type().is_symlink()
                && md.uid() == uid
                && md.permissions().mode() & 0o777 == 0o700
        })
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
    /// Dedicated trusted CLI escape hatch for plaintext non-loopback serving.
    pub unsafe_allow_plaintext_non_loopback: bool,
    /// Trusted CLI override for the client-facing host.
    pub advertise_host: Option<String>,
    /// Trusted CLI override for the client-facing port.
    pub advertise_port: Option<u16>,
}

fn registry_control_origin(
    transport: &thegn_core::config_daemon::ServeTransportPolicy,
    actual_bind_port: u16,
) -> String {
    transport.advertised_origin(actual_bind_port)
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
    // The daemon is its own process: it writes its OWN file (`thegn-daemon.log`,
    // via `Role::Daemon`) so it never shares one rotation state machine with the
    // compositor, and it honors `[log]` + `THEGN_LOG_*` like the host. It loads
    // config first, so dir/rotation come straight from config (env still wins) —
    // fixing the old pinned-5MB×5 default. The WARN+ in-memory ring is always on
    // (zero I/O until a crash) so a headless daemon panic (`proc=daemon`) leaves
    // a crash report even though its stdio is nulled and `THEGN_LOG` is unset.
    thegn_core::diagnostics::set_ring_capacity(cfg.diagnostics.ring_size);
    thegn_core::diagnostics::configure_crash(
        cfg.diagnostics.crash_reports,
        cfg.diagnostics.crash_retention,
    );
    crate::diag::register_identity(crate::channel_state::current().as_str());
    {
        let mut log = cfg.log.clone();
        log.file = cfg.log.file || std::env::var_os("THEGN_LOG").is_some();
        thegn_core::log_trace::install(thegn_core::log_trace::Role::Daemon, &log);
    }
    let serve_transport = serve
        .as_ref()
        .map(|options| {
            let mut effective = cfg.serve.clone();
            if let Some(host) = &options.advertise_host {
                effective.advertise_host = host.clone();
            }
            if let Some(port) = options.advertise_port {
                effective.advertise_port = port;
            }
            effective.resolve_transport(
                options.bind.as_deref(),
                options.unsafe_allow_plaintext_non_loopback,
            )
        })
        .transpose()
        .map_err(anyhow::Error::msg)
        .context("invalid remote control transport")?;
    let db: service::SharedDb = Arc::new(Mutex::new(Db::open()?));
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    let scope = scope_key_for(&state_dir);
    let legacy_scope = state_dir.to_str();
    {
        let db = db.lock().expect("daemon db lock");
        if live_legacy_scope_daemon(&db.daemons()?, legacy_scope, pid_alive) {
            anyhow::bail!(
                "a pre-upgrade daemon is still registered for this state directory (pid alive); stop it with `thegn daemon stop` before starting a new daemon"
            );
        }
    }
    let has_socket_override = socket_override.is_some();
    let sock = socket_override.unwrap_or_else(|| socket_path(&cfg.daemon));
    #[cfg(unix)]
    if sock
        == private_tmp_socket_path(
            &thegn_core::util::xdg_state_home().join("thegn"),
            effective_uid(),
        )
    {
        validate_private_fallback_parent(&sock)?;
    }
    if !has_socket_override && cfg.daemon.socket.is_empty() {
        reconcile_legacy_socket(&cfg.daemon);
        if legacy_socket_state() == LegacySocketState::Live {
            let legacy = thegn_core::config_daemon::legacy_socket_path(
                &thegn_core::util::xdg_state_home().join("thegn"),
            );
            anyhow::bail!(
                "legacy daemon is still live at {}; detach its panes, then run `thegn daemon stop` before starting a new daemon",
                legacy.display()
            );
        }
    }
    let legacy = thegn_core::config_daemon::legacy_socket_path(
        &thegn_core::util::xdg_state_home().join("thegn"),
    );
    if !has_socket_override
        && cfg.daemon.socket.is_empty()
        && std::env::var("XDG_RUNTIME_DIR")
            .map(|value| value.is_empty())
            .unwrap_or(true)
        && sock == legacy
    {
        anyhow::bail!(
            "no secure private runtime directory is available for the control socket; set XDG_RUNTIME_DIR or an explicit [daemon] socket"
        );
    }
    if let Some(parent) = sock.parent() {
        thegn_svc::ipc::prepare_control_directory(parent, cfg.serve.local_admin)
            .with_context(|| format!("harden control directory {}", parent.display()))?;
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
    let listener = match thegn_svc::ipc::IpcListener::bind_exclusive_with_hardening(
        &ep,
        cfg.serve.local_admin,
    )
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
        control_origin: None,
        hostname: hostname(),
        version: format!(
            "{}{}",
            env!("CARGO_PKG_VERSION"),
            SESSION_MIGRATION_FENCE_VERSION_SUFFIX
        ),
        started_at: now_ms(),
        heartbeat_at: now_ms(),
    };
    {
        let db = db.lock().expect("daemon db lock");
        for stale in boot_sweep_targets(
            &db.daemons().unwrap_or_default(),
            &scope,
            legacy_scope,
            pid_alive,
        ) {
            let _ = db.clear_daemon_leases(&stale); // best-effort: cache write: the daemon row is bookkeeping; the sweep below derives from live state
            let _ = db.del_daemon(&stale); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
        db.put_daemon(&daemon_row)?;
        // The session map below is created empty, so no hand raised by a
        // previous incarnation can still be up (THE-68).
        // best-effort: the table is disposable derived state.
        let _ = thegn_core::store::NotificationStore::clear_all_session_attention(&*db);
    }

    let (events, _) = broadcast::channel::<Arc<EventFrame>>(1024);
    let (idle_tx, idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let svc = Arc::new(DaemonService {
        daemon_id: daemon_id.clone(),
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        actor_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        accepting: std::sync::atomic::AtomicBool::new(true),
        tombs: Arc::new(tokio::sync::Mutex::new(
            thegn_core::graveyard::Graveyard::new(
                tombstone::MAX_TOMBSTONES,
                tombstone::TOMBSTONE_TTL_MS,
            ),
        )),
        native_ids: std::sync::Mutex::new(std::collections::HashMap::new()),
        spawning_relaunches: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        events: events.clone(),
        db: db.clone(),
        grace_ms: thegn_core::time_policy::duration_millis(cfg.daemon.lease_grace_secs),
        idle_tx,
        shutdown: shutdown.clone(),
        config: std::sync::Arc::new(cfg.clone()),
        profile_root: thegn_core::profile::active().root,
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

    // The push command inbox (off by default): a phone-initiated command
    // surface hosted here so it survives UI detach. A no-op unless
    // `[notifications.push.inbox]` is enabled and validly configured.
    inbox::spawn(
        cfg,
        svc.clone(),
        db.clone(),
        shutdown.clone(),
        format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
    );
    // Transport-error retry observer (THE-86): relaunches headless pipeline
    // workers killed by a transport failure. Event-driven; zero timers while
    // idle.
    pipeline_retry::spawn(svc.clone(), svc.events.subscribe());
    // Roster self-heal: a worker that committed its handoff and died before
    // filing its report used to leave a `running` row nobody would close until
    // a human ran `dispatch reap`. Slow timer, blocking work off the runtime.
    pipeline_reaper::spawn(svc.clone());
    crate::automation_runtime::subscribe_daemon_events(svc.events.subscribe(), cfg);

    let state = thegn_svc::control::http::ControlState {
        api: svc.clone(),
        store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
        local_admin: cfg.serve.local_admin,
        daemon_euid: thegn_svc::ipc::effective_uid(),
        require_approval: cfg.serve.require_approval,
        server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
        // The unix-socket listener is local-only; browsers never dial it, so no
        // CORS is applied here (the allowlist rides the TCP listener below).
        cors_origins: Vec::new(),
        event_streams: Default::default(),
    };
    let local_grpc = thegn_svc::control::grpc::GrpcControl {
        api: svc.clone(),
        store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
        local_admin: cfg.serve.local_admin,
        daemon_euid: thegn_svc::ipc::effective_uid(),
        server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
    };
    let app = thegn_svc::control::http::router(state)
        .merge(scope_router(scope_key()))
        .merge(
            tonic::service::Routes::new(thegn_svc::control::grpc::ControlServer::new(local_grpc))
                .into_axum_router(),
        );

    // Serve mode: one policy-controlled TCP listener for HTTP/WS/SSE + gRPC,
    // with bearer tokens REQUIRED (never local_admin on TCP). Native transport
    // is plaintext: direct mode is loopback-only; declared TLS termination and
    // tunnel modes also keep the backend loopback and advertise their actual
    // public scheme. Non-loopback plaintext requires the explicit unsafe flag.
    if let Some(opts) = serve {
        let transport = serve_transport.expect("serve transport resolved with serve options");
        let bind = transport.bind;
        let tcp = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind {bind}"))?;
        let actual = tcp.local_addr().context("serve local_addr")?;
        // Preserve the actual bound socket for status/discovery contracts, and
        // persist the exact client-facing origin after every CLI override has
        // been applied. Route-to-host provisioning runs in another process and
        // must not reconstruct this from its static config (which would lose
        // `serve --advertise-host/--advertise-port`).
        daemon_row.tcp_addr = Some(actual.to_string());
        daemon_row.control_origin = Some(registry_control_origin(&transport, actual.port()));
        daemon_row.heartbeat_at = now_ms();
        {
            let db = db.lock().expect("daemon db lock");
            db.put_daemon(&daemon_row)
                .context("record serve addresses in the daemon registry")?;
        }
        let tcp_state = thegn_svc::control::http::ControlState {
            api: svc.clone(),
            store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: false,
            daemon_euid: None,
            require_approval: cfg.serve.require_approval,
            server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
            // Browser-hosted thin clients opt in per `[serve] cors_origins`
            // (empty = no cross-origin access; a wildcard is refused at config
            // validation).
            cors_origins: cfg.serve.cors_origins.clone(),
            event_streams: Default::default(),
        };
        let grpc = thegn_svc::control::grpc::GrpcControl {
            api: svc.clone(),
            store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
            local_admin: false,
            daemon_euid: None,
            server_label: format!("{} thegn {}", hostname(), env!("CARGO_PKG_VERSION")),
        };
        let tcp_app = thegn_svc::control::http::router(tcp_state).merge(
            tonic::service::Routes::new(thegn_svc::control::grpc::ControlServer::new(grpc))
                .into_axum_router(),
        );
        let shutdown_tcp = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(tcp, tcp_app)
                .with_graceful_shutdown(async move { shutdown_tcp.notified().await })
                .await
            {
                tracing::warn!(target: "thegn::daemon", error = %e, "control-plane TCP server exited");
            }
        });

        let advertised = transport.advertised_origin(actual.port());
        thegn_core::outln!(
            "thegn control backend listening on {actual} (HTTP/WS + gRPC; scoped tokens required)"
        );
        thegn_core::outln!(
            "remote transport: {} · advertised {advertised} · WebSocket {} · gRPC {}",
            transport.exposure.as_str(),
            transport.websocket_scheme(),
            transport.grpc_scheme(),
        );
        if transport.exposure == thegn_core::config::ServeExposure::UnsafePlaintext {
            thegn_core::outln!(
                "WARNING: explicit unsafe plaintext non-loopback exposure is enabled; bearer credentials and terminal traffic are observable in transit"
            );
        }
        if !opts.no_pair_url {
            let now = now_ms();
            let minted = thegn_svc::control::auth::mint(
                thegn_core::control::TokenKind::PairingCode,
                thegn_core::control::ScopeSet::parse("read"),
                "serve startup",
                None,
                Some(now.saturating_add(15 * 60_000)),
                now,
            );
            {
                let db = db.lock().expect("daemon db lock");
                db.put_pairing(&minted.row)?;
            }
            let url = thegn_core::control::PairingUrl {
                host: transport.advertise_host.clone(),
                port: transport.advertise_port(actual.port()),
                code: minted.token,
                secure: transport.http_scheme() == "https",
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
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<thegn_svc::ipc::IpcConnectInfo>(),
    )
    .with_graceful_shutdown(async move { shutdown_wait.notified().await });
    let result = serve.await;

    // Signal and idle-exit paths notify the listener directly; this final
    // coordinator call ensures they share the same actor/process teardown as
    // the shutdown RPC before registry/lease state is retired.
    svc.shutdown_sessions().await;

    // Cleanup: registry row + socket file. Leases stay only if sessions do —
    // a graceful shutdown killed them, so sweep ours.
    {
        let db = db.lock().expect("daemon db lock");
        let _ = db.clear_daemon_leases(&daemon_id); // best-effort: cache write: the daemon row is bookkeeping; the sweep below derives from live state
        let _ = db.del_daemon(&daemon_id); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    // best-effort: unlink the unix socket file; on Windows `sock` is only the
    // pipe-name seed (no fs entry), so this is a harmless no-op failure.
    let _ = std::fs::remove_file(&sock); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
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
    legacy_scope: Option<&str>,
    pid_alive: impl Fn(i64) -> bool,
) -> Vec<String> {
    rows.iter()
        .filter(|r| {
            (r.scope == scope || Some(r.scope.as_str()) == legacy_scope) && !pid_alive(r.pid)
        })
        .map(|r| r.daemon_id.clone())
        .collect()
}

fn live_legacy_scope_daemon(
    rows: &[DaemonRow],
    legacy_scope: Option<&str>,
    pid_alive: impl Fn(i64) -> bool,
) -> bool {
    legacy_scope.is_some_and(|legacy_scope| {
        rows.iter()
            .any(|row| row.scope == legacy_scope && pid_alive(row.pid))
    })
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
            let _ = db.touch_daemon_heartbeat(&id, now_ms()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
                let _ = tx.send(SessionMsg::Kill).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            }
            // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
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

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_scope_endpoint_matches_and_refuses_foreign_state_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        use thegn_svc::control::client::{ControlAddr, ControlClient};

        let temp = tempfile::Builder::new()
            .prefix("thegn-daemon-scope-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let socket = temp.path().join("daemon.sock");
        let listener =
            thegn_svc::ipc::IpcListener::Unix(tokio::net::UnixListener::bind(&socket).unwrap());
        let app = scope_router("v1-aaa".into());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<thegn_svc::ipc::IpcConnectInfo>(),
            )
            .await
            .unwrap();
        });
        let client = ControlClient::new(ControlAddr::Unix(socket));

        assert_eq!(
            client::check_daemon_scope(&client, &["v1-aaa".into()]).await,
            client::ScopeCheck::Match
        );
        assert_eq!(
            client::check_daemon_scope(&client, &["v1-bbb".into()]).await,
            client::ScopeCheck::Mismatch("v1-aaa".into())
        );

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[cfg(unix)]
    #[test]
    fn raw_unix_state_paths_keep_endpoint_and_scope_identity_distinct() {
        use std::os::unix::ffi::OsStrExt as _;

        let first = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/s/\xff"));
        let second = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/s/\xfe"));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(scope_key_for(first), scope_key_for(second));
        assert_ne!(
            private_tmp_socket_path(first, 1000),
            private_tmp_socket_path(second, 1000)
        );
        let short_dir = std::path::Path::new("/tmp");
        assert_ne!(
            thegn_core::config_daemon::short_socket_path(short_dir, first),
            thegn_core::config_daemon::short_socket_path(short_dir, second)
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_tmp_socket_names_include_uid_and_state_hash_without_state_dir() {
        let a = private_tmp_socket_path(std::path::Path::new("/state/one"), 1000);
        let b = private_tmp_socket_path(std::path::Path::new("/state/two"), 1000);
        let other_uid = private_tmp_socket_path(std::path::Path::new("/state/one"), 1001);
        assert!(a.starts_with("/tmp"));
        assert!(a.to_string_lossy().contains("thegn-1000-"));
        assert_ne!(
            a, b,
            "distinct state dirs must not share a control directory"
        );
        assert_ne!(
            a, other_uid,
            "distinct effective uids must not share a control directory"
        );
        assert!(!a.starts_with("/state/one"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_private_runtime_uses_uid_scoped_tmp_socket() {
        let state = std::path::Path::new("/isolated/state/thegn");
        let socket = socket_path_for(
            &thegn_core::config::DaemonConfig::default(),
            None,
            state,
            4242,
            None,
        );
        assert_eq!(socket, private_tmp_socket_path(state, 4242));
        assert!(socket.to_string_lossy().starts_with("/tmp/thegn-4242-"));
        assert!(!socket.starts_with(state));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_sockets_are_scoped_to_the_state_root() {
        let config = thegn_core::config::DaemonConfig::default();
        let first = socket_path_for(
            &config,
            Some("/run/user/1000"),
            std::path::Path::new("/state/one"),
            1000,
            None,
        );
        let second = socket_path_for(
            &config,
            Some("/run/user/1000"),
            std::path::Path::new("/state/two"),
            1000,
            None,
        );
        let legacy =
            thegn_core::config_daemon::legacy_runtime_socket_path(Some("/run/user/1000")).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, legacy);
        assert_ne!(second, legacy);
    }

    #[cfg(unix)]
    #[test]
    fn existing_private_fallback_directory_must_have_private_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let td = tempfile::tempdir().unwrap();
        let parent = td.path().join("thegn-fallback");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = parent.join("daemon.sock");
        assert!(validate_private_fallback_parent(&socket).is_err());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_private_fallback_parent(&socket).is_ok());
    }

    #[test]
    fn legacy_socket_decision_covers_absent_stale_and_live() {
        assert_eq!(
            legacy_socket_decision(true, Some(std::io::ErrorKind::ConnectionRefused)),
            LegacySocketState::Stale
        );
        assert_eq!(legacy_socket_decision(true, None), LegacySocketState::Live);
        assert_eq!(
            legacy_socket_decision(true, Some(std::io::ErrorKind::PermissionDenied)),
            LegacySocketState::Live
        );
        assert_eq!(
            legacy_socket_decision(true, Some(std::io::ErrorKind::WouldBlock)),
            LegacySocketState::Live
        );
        assert_eq!(
            legacy_socket_decision(false, Some(std::io::ErrorKind::ConnectionRefused)),
            LegacySocketState::Absent
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_runtime_socket_cleanup_decision_only_removes_absent_or_stale_sockets() {
        assert!(!legacy_runtime_socket_should_remove(
            false,
            false,
            Some(std::io::ErrorKind::NotFound)
        ));
        assert!(legacy_runtime_socket_should_remove(
            true,
            true,
            Some(std::io::ErrorKind::ConnectionRefused)
        ));
        assert!(!legacy_runtime_socket_should_remove(true, true, None));
        assert!(!legacy_runtime_socket_should_remove(
            true,
            false,
            Some(std::io::ErrorKind::ConnectionRefused)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn socket_usable_decision_checks_legacy_private_tmp_and_runtime_paths() {
        let legacy = std::path::Path::new("/state/thegn/run/daemon.sock");
        let state = std::path::Path::new("/home/blake/.local/state/thegn");
        let tmp_socket = private_tmp_socket_path(state, 1000);
        let hash = tmp_socket
            .parent()
            .and_then(std::path::Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash))
            .unwrap();
        assert_eq!(hash.len(), 32);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let runtime_socket = std::path::Path::new(
            "/run/user/1000/thegn/0123456789abcdef0123456789abcdef/daemon.sock",
        );
        let absent = |_: &std::path::Path| None;
        assert!(!socket_usable_decision(
            LegacySocketState::Live,
            std::path::Path::new("/run/user/1000/thegn/daemon.sock"),
            legacy,
            &tmp_socket,
            false,
            &absent,
        ));
        assert!(!socket_usable_decision(
            LegacySocketState::Absent,
            legacy,
            legacy,
            &tmp_socket,
            false,
            &absent,
        ));

        let tmp_probe = |path: &std::path::Path| match path {
            p if p == std::path::Path::new("/tmp") => Some(DirMeta {
                is_dir: true,
                is_symlink: false,
                owned: false,
                mode: 0o1777,
            }),
            _ => None,
        };
        assert!(socket_usable_decision(
            LegacySocketState::Absent,
            &tmp_socket,
            legacy,
            &tmp_socket,
            false,
            &tmp_probe,
        ));

        let bad_mode_probe = |path: &std::path::Path| {
            (path == tmp_socket.parent().unwrap()).then_some(DirMeta {
                is_dir: true,
                is_symlink: false,
                owned: true,
                mode: 0o755,
            })
        };
        assert!(!socket_usable_decision(
            LegacySocketState::Absent,
            &tmp_socket,
            legacy,
            &tmp_socket,
            false,
            &bad_mode_probe,
        ));
        let foreign_probe = |path: &std::path::Path| {
            (path == tmp_socket.parent().unwrap()).then_some(DirMeta {
                is_dir: true,
                is_symlink: false,
                owned: false,
                mode: 0o700,
            })
        };
        assert!(!socket_usable_decision(
            LegacySocketState::Absent,
            &tmp_socket,
            legacy,
            &tmp_socket,
            false,
            &foreign_probe,
        ));

        let runtime_dir = std::path::Path::new("/run/user/1000");
        let runtime_parent = runtime_socket.parent().unwrap();
        let runtime_shared = runtime_parent.parent().unwrap();
        let runtime_probe = |path: &std::path::Path| {
            (path == runtime_dir).then_some(DirMeta {
                is_dir: true,
                is_symlink: false,
                owned: true,
                mode: 0o700,
            })
        };
        assert!(socket_usable_decision(
            LegacySocketState::Absent,
            runtime_socket,
            legacy,
            &tmp_socket,
            false,
            &runtime_probe,
        ));

        let secure_runtime_tree = |path: &std::path::Path| {
            [runtime_parent, runtime_shared, runtime_dir]
                .contains(&path)
                .then_some(DirMeta {
                    is_dir: true,
                    is_symlink: false,
                    owned: true,
                    mode: 0o700,
                })
        };
        assert!(socket_usable_decision(
            LegacySocketState::Absent,
            runtime_socket,
            legacy,
            &tmp_socket,
            false,
            &secure_runtime_tree,
        ));
        let public_but_not_writable_thegn = |path: &std::path::Path| {
            if path == runtime_shared {
                Some(DirMeta {
                    is_dir: true,
                    is_symlink: false,
                    owned: true,
                    mode: 0o755,
                })
            } else if path == runtime_dir {
                Some(DirMeta {
                    is_dir: true,
                    is_symlink: false,
                    owned: true,
                    mode: 0o700,
                })
            } else {
                None
            }
        };
        assert!(socket_usable_decision(
            LegacySocketState::Absent,
            runtime_socket,
            legacy,
            &tmp_socket,
            false,
            &public_but_not_writable_thegn,
        ));
        let group_writable_thegn = |path: &std::path::Path| {
            if path == runtime_shared {
                Some(DirMeta {
                    is_dir: true,
                    is_symlink: false,
                    owned: true,
                    mode: 0o775,
                })
            } else if path == runtime_dir {
                Some(DirMeta {
                    is_dir: true,
                    is_symlink: false,
                    owned: true,
                    mode: 0o700,
                })
            } else {
                None
            }
        };
        assert!(!socket_usable_decision(
            LegacySocketState::Absent,
            runtime_socket,
            legacy,
            &tmp_socket,
            false,
            &group_writable_thegn,
        ));
        let foreign_thegn = |path: &std::path::Path| {
            if path == runtime_shared {
                Some(DirMeta {
                    is_dir: true,
                    is_symlink: false,
                    owned: false,
                    mode: 0o700,
                })
            } else {
                secure_runtime_tree(path)
            }
        };
        assert!(!socket_usable_decision(
            LegacySocketState::Absent,
            runtime_socket,
            legacy,
            &tmp_socket,
            false,
            &foreign_thegn,
        ));
        let symlinked_thegn = |path: &std::path::Path| {
            if path == runtime_shared {
                Some(DirMeta {
                    is_dir: true,
                    is_symlink: true,
                    owned: true,
                    mode: 0o700,
                })
            } else {
                secure_runtime_tree(path)
            }
        };
        assert!(!socket_usable_decision(
            LegacySocketState::Absent,
            runtime_socket,
            legacy,
            &tmp_socket,
            false,
            &symlinked_thegn,
        ));
    }

    fn row(id: &str, scope: &str, pid: i64) -> DaemonRow {
        DaemonRow {
            daemon_id: id.into(),
            pid,
            scope: scope.into(),
            endpoint: format!("/run/{id}.sock"),
            tcp_addr: None,
            control_origin: None,
            hostname: "h".into(),
            version: "0".into(),
            started_at: 0,
            heartbeat_at: 0,
        }
    }

    #[test]
    fn daemon_registry_persists_effective_advertised_origin() {
        let serve = thegn_core::config::ServeConfig {
            topology: thegn_core::config::ServeTopology::TlsTerminated,
            advertise_host: "cli-override.example".into(),
            advertise_port: 8443,
            ..Default::default()
        };
        let transport = serve.resolve_transport(None, false).unwrap();
        assert_eq!(
            registry_control_origin(&transport, 5380),
            "https://cli-override.example:8443"
        );
    }

    /// The boot sweep removes exactly the same-scope rows whose pid is dead:
    /// a live same-scope daemon and any other scope's rows (even dead ones —
    /// their own next boot sweeps them) are kept.
    #[test]
    fn boot_sweep_removes_only_same_scope_dead_daemons() {
        let rows = vec![
            row("dead-same", "v1-scope-a", 11),
            row("alive-same", "v1-scope-a", 22),
            row("dead-legacy", "/scope/a", 44),
            row("dead-other", "/scope/b", 33),
        ];
        let alive = |pid: i64| pid == 22;
        assert_eq!(
            boot_sweep_targets(&rows, "v1-scope-a", Some("/scope/a"), alive),
            vec!["dead-same".to_string(), "dead-legacy".to_string()]
        );
        // Nothing dead in-scope ⇒ nothing swept.
        assert!(boot_sweep_targets(&rows, "v1-scope-c", Some("/scope/c"), alive).is_empty());
        // Invalid UTF-8 state paths cannot match rows from the lossy legacy scheme.
        assert!(boot_sweep_targets(&rows, "v1-scope-c", None, alive).is_empty());
    }

    #[test]
    fn live_legacy_scope_row_blocks_startup() {
        let rows = vec![
            row("old-live", "/state/a", 22),
            row("old-dead", "/state/a", 11),
        ];
        let alive = |pid: i64| pid == 22;
        assert!(live_legacy_scope_daemon(&rows, Some("/state/a"), alive));
        assert!(!live_legacy_scope_daemon(&rows, Some("/state/b"), alive));
        assert!(!live_legacy_scope_daemon(&rows, None, alive));
    }

    #[test]
    fn scope_aliases_include_legacy_utf8_path() {
        let state_dir = thegn_core::util::xdg_state_home().join("thegn");
        let aliases = scope_aliases();
        assert_eq!(aliases[0], scope_key_for(&state_dir));
        assert_eq!(aliases.get(1).map(String::as_str), state_dir.to_str());
        assert_eq!(
            aliases.len(),
            if state_dir.to_str().is_some() { 2 } else { 1 }
        );
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
