//! Local daemon IPC: unix-domain sockets on unix, named pipes on Windows —
//! one seam so the daemon, control client, and `axum::serve` are
//! platform-free.
//!
//! **The endpoint is the lock.** On unix, whoever binds the socket is the
//! daemon (a socket that accepts or has a full backlog ⇒ a live daemon; only
//! a refused socket is stale and unlinked). Other probe outcomes fail startup.
//! On Windows, `first_pipe_instance(true)` gives the same semantics — the
//! first creator owns the pipe name and a second daemon gets `ACCESS_DENIED`
//! (⇒ [`BindOutcome::AlreadyRunning`]). Pipes die with their handles, so
//! there is no stale *file* to clean up — but the name stays reserved for a
//! few milliseconds after the last handle of an instance that carried a
//! connection closes, so `bind_exclusive` retries briefly before believing
//! ACCESS_DENIED (see the comment there).
//!
//! Pipe names are derived from the same per-state-dir socket *path* the unix
//! side uses (`\\.\pipe\thegn-v2-<hex(sha256(identity))[..32]>`). The
//! `thegn/daemon-pipe/v2\0` domain and 128-bit digest prefix bind the name to a
//! normalized Windows path identity (Unix callers use the socket path itself);
//! display strings are diagnostic only and are never identity keys. The
//! derivation and endpoint classification are pure and unit-tested on every
//! platform; only the syscalls are `#[cfg]`-gated.
//! A pre-upgrade Windows daemon keeps its old pipe name and is reached through
//! its registry row; after it stops, new daemons use the v2 name.
//!
//! Unix endpoint authenticity is bound to the daemon's effective uid and the
//! filesystem namespace containing its owner-only socket parent. Other
//! principals cannot replace the endpoint or lock there. A same-uid process
//! in that namespace is the same filesystem principal, so token-only
//! deployments must keep this private parent outside sandbox mounts. Migrate
//! custom token-only sockets into a private per-user runtime/state directory.
//! Lock contention and socket probes fail closed after the same bounded wait.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// OS-authenticated identity attached to an accepted local-control stream.
/// This is created by the listener, never from request headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerIdentity {
    /// Effective uid returned by the Unix peer-credential syscall.
    UnixEuid(u32),
    /// A local-only Windows named pipe (`reject_remote_clients(true)`).
    LocalPipe,
    /// A daemon-internal dispatch whose caller already authenticated it.
    TrustedInternal,
    /// The platform could not authenticate this peer. Such a peer must use a
    /// normal scoped token even when implicit local admin is enabled.
    Unavailable(String),
}

/// Connection metadata propagated to HTTP/WebSocket and gRPC request
/// extensions by `into_make_service_with_connect_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcConnectInfo {
    pub endpoint: String,
    pub peer: PeerIdentity,
}

impl IpcConnectInfo {
    /// Identity for daemon-internal calls routed through the HTTP adapters.
    pub fn trusted_internal() -> Self {
        Self {
            endpoint: "in-process".into(),
            peer: PeerIdentity::TrustedInternal,
        }
    }
}

/// The daemon's effective uid on Unix. `None` on platforms without Unix uid
/// credentials; kept here so platform-specific identity knowledge has one
/// home.
pub fn effective_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The Windows named-pipe namespace prefix.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Deterministic pipe name for a daemon socket path. Its SHA-256 digest is
/// domain-separated with `thegn/daemon-pipe/v2\0` and truncated to 128 bits.
/// Valid UTF-8 paths use normalized Windows spelling and Unicode lowercase;
/// non-UTF-8 paths retain their raw encoded bytes. Display strings are
/// diagnostic only and are never identity keys.
pub fn pipe_name_for_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"thegn/daemon-pipe/v2\0");
    hasher.update(pipe_identity_bytes(path));
    let digest = hasher.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{PIPE_PREFIX}thegn-v2-{hex}")
}

/// Lossless identity bytes with Windows path spelling normalized where the
/// encoded path is valid UTF-8. Raw encoded bytes preserve non-UTF-8 identities.
fn pipe_identity_bytes(path: &Path) -> Vec<u8> {
    let encoded = path.as_os_str().as_encoded_bytes();
    let Ok(path) = std::str::from_utf8(encoded) else {
        return encoded.to_vec();
    };

    let path = path.replace('/', "\\");
    let path = if let Some(verbatim_path) = path.strip_prefix(r"\\?\") {
        let bytes = verbatim_path.as_bytes();
        if bytes.len() >= 4 && bytes[..3].eq_ignore_ascii_case(b"UNC") && bytes[3] == b'\\' {
            format!(r"\\{}", &verbatim_path[4..])
        } else {
            verbatim_path.to_owned()
        }
    } else {
        path
    };
    path.to_lowercase().into_bytes()
}

/// Where local daemon IPC lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    /// Unix-domain socket path.
    Unix(PathBuf),
    /// Windows named pipe (`\\.\pipe\…`).
    Pipe(String),
}

impl IpcEndpoint {
    /// Classify a stored/configured "socket path" into an endpoint for this
    /// platform. A `\\.\pipe\…` string (e.g. a `DaemonRow.endpoint` written by
    /// a Windows daemon) is already a pipe name; any other path is a unix
    /// socket on unix and is *derived into* a pipe name on Windows.
    pub fn for_socket_path(path: &Path) -> Self {
        Self::classify(path, cfg!(windows))
    }

    /// [`Self::for_socket_path`] with the platform explicit — pure, so both
    /// arms are unit-tested on Linux CI.
    fn classify(path: &Path, windows: bool) -> Self {
        let s = path.to_string_lossy();
        if s.starts_with(PIPE_PREFIX) {
            IpcEndpoint::Pipe(s.into_owned())
        } else if windows {
            IpcEndpoint::Pipe(pipe_name_for_path(path))
        } else {
            IpcEndpoint::Unix(path.to_path_buf())
        }
    }

    /// The stable string form — what the daemon registry row stores and log
    /// lines print.
    pub fn display(&self) -> String {
        match self {
            IpcEndpoint::Unix(p) => p.to_string_lossy().into_owned(),
            IpcEndpoint::Pipe(name) => name.clone(),
        }
    }
}

/// One connected IPC stream (either side, either platform).
pub enum IpcStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
}

macro_rules! delegate {
    ($self:ident, $inner:ident => $e:expr) => {
        match $self.get_mut() {
            #[cfg(unix)]
            IpcStream::Unix($inner) => $e,
            #[cfg(windows)]
            IpcStream::PipeClient($inner) => $e,
            #[cfg(windows)]
            IpcStream::PipeServer($inner) => $e,
        }
    };
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_read(cx, buf))
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, s => Pin::new(s).poll_write(cx, buf))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_flush(cx))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_shutdown(cx))
    }
}

/// Connect to a (presumed live) daemon endpoint.
pub async fn connect(ep: &IpcEndpoint) -> io::Result<IpcStream> {
    match ep {
        IpcEndpoint::Unix(path) => {
            #[cfg(unix)]
            {
                connect_unix_as(
                    path,
                    effective_uid().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "cannot determine effective uid",
                        )
                    })?,
                )
                .await
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Err(unsupported("unix-socket IPC on a non-unix host"))
            }
        }
        IpcEndpoint::Pipe(name) => {
            #[cfg(windows)]
            {
                use tokio::net::windows::named_pipe::ClientOptions;
                // ERROR_PIPE_BUSY (231): instances exist but none is free —
                // the server is alive and about to create the next instance,
                // so a short bounded backoff (~127ms worst case) is correct.
                // Unknown-name (daemon gone) errors surface immediately.
                const ERROR_PIPE_BUSY: i32 = 231;
                let mut delay_ms = 1u64;
                loop {
                    match ClientOptions::new().open(name) {
                        Ok(c) => return Ok(IpcStream::PipeClient(c)),
                        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && delay_ms <= 64 => {
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            delay_ms *= 2;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = name; // best-effort: non-Result discard — param unused in this stub
                Err(unsupported("named-pipe IPC on a non-Windows host"))
            }
        }
    }
}

#[cfg(unix)]
async fn connect_unix_as(path: &Path, expected_euid: u32) -> io::Result<IpcStream> {
    verify_unix_client_parent(path)?;
    connect_unix_after_parent_check(path, expected_euid).await
}

#[cfg(unix)]
fn verify_unix_client_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::PermissionDenied, "unix socket has no parent")
    })?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symlink", parent.display()),
        ));
    }
    verify_unix_bind_parent(parent, &metadata)
}

#[cfg(unix)]
async fn connect_unix_after_parent_check(path: &Path, expected_euid: u32) -> io::Result<IpcStream> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let peer_uid = stream.peer_cred()?.uid();
    server_peer_allowed(Some(peer_uid), expected_euid)?;
    Ok(IpcStream::Unix(stream))
}

#[cfg(unix)]
fn server_peer_allowed(peer_uid: Option<u32>, euid: u32) -> io::Result<()> {
    if peer_uid == Some(euid) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "unix control server uid {:?} does not match client uid {euid}",
                peer_uid
            ),
        ))
    }
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))] // Unix-only production call sites.
struct IoPathContext {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for IoPathContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for IoPathContext {
    // `Display` already renders the cause; exposing it again as `source()`
    // makes anyhow's chain print it twice.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[cfg_attr(not(unix), allow(dead_code))] // Unix-only production call sites.
fn io_error_with_path_context(
    source: io::Error,
    operation: &'static str,
    path: &Path,
) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        IoPathContext {
            operation,
            path: path.to_path_buf(),
            source,
        },
    )
}

/// Take the advisory lock serializing [`IpcListener::bind_exclusive`]'s
/// probe→unlink→bind critical section for `sock`. The sidecar `<sock>.lock`
/// is created once and NEVER unlinked (unlinking it would resurrect the very
/// race it closes); the flock (std's `File::lock`) dies with the process, so
/// it can't go stale. Acquisition is bounded by [`BIND_LOCK_WAIT`]; a held
/// lock that exceeds the bound is a startup error, never a wait without end.
/// The caller is already on a blocking thread. Failure is fatal: probing or
/// unlinking without this lock can race another daemon endpoint.
#[cfg(unix)]
fn bind_lock(sock: &Path) -> io::Result<std::fs::File> {
    bind_lock_with_wait(sock, BIND_LOCK_WAIT)
}

#[cfg(unix)]
/// Upper bound for control socket lock contention. The host's
/// `SCOPE_PROBE_TIMEOUT` planner budget is 5 s; honest contention is measured
/// in milliseconds, so this bound leaves room for startup work without an
/// unbounded wait.
const BIND_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The listener probe shares the bind-lock budget; a healthy probe completes
/// in microseconds.
#[cfg(unix)]
const BIND_PROBE_WAIT: std::time::Duration = BIND_LOCK_WAIT;

#[cfg(unix)]
fn bind_lock_with_wait(sock: &Path, wait: std::time::Duration) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let mut path = sock.as_os_str().to_owned();
    path.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(std::path::Path::new(&path))
        .map_err(|error| io_error_with_path_context(error, "open control socket lock", sock))?;
    let mut metadata = file
        .metadata()
        .map_err(|error| io_error_with_path_context(error, "inspect control socket lock", sock))?;
    if !metadata.is_file() || metadata.uid() != effective_uid().unwrap_or_default() {
        let mode = metadata.mode() & 0o777;
        return Err(io_error_with_path_context(
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "control socket lock must be an owner-owned regular file (found uid {}, mode {mode:04o})",
                    metadata.uid()
                ),
            ),
            "verify control socket lock",
            sock,
        ));
    }
    if metadata.mode() & 0o777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                io_error_with_path_context(error, "restrict control socket lock", sock)
            })?;
        metadata = file.metadata().map_err(|error| {
            io_error_with_path_context(error, "recheck control socket lock", sock)
        })?;
        if !metadata.is_file()
            || metadata.uid() != effective_uid().unwrap_or_default()
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(io_error_with_path_context(
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "control socket lock could not be restricted to owner mode 0600",
                ),
                "verify control socket lock",
                sock,
            ));
        }
    }
    let deadline = std::time::Instant::now() + wait;
    let mut interval = thegn_proc::POLL_INTERVAL;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(io_error_with_path_context(
                    error,
                    "lock control socket",
                    sock,
                ));
            }
        }
        let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
            Some(remaining) if !remaining.is_zero() => remaining,
            _ => {
                return Err(io_error_with_path_context(
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "control socket lock is held by another process (waited {}ms)",
                            wait.as_millis()
                        ),
                    ),
                    "lock control socket",
                    sock,
                ));
            }
        };
        std::thread::sleep(interval.min(remaining));
        interval = thegn_proc::next_backoff(interval, remaining);
    }
}

#[cfg_attr(not(unix), allow(dead_code))] // Unix-only production call site.
fn require_live_endpoint(
    bind_error: io::Error,
    probe: io::Result<()>,
    sock: &Path,
) -> io::Result<()> {
    match probe {
        Ok(()) => Ok(()),
        Err(_) => Err(io_error_with_path_context(
            bind_error,
            "bind control socket",
            sock,
        )),
    }
}

/// Classification of a non-blocking probe of an existing Unix socket.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum UnixProbe {
    Live,
    Stale,
    Gone,
}

/// A probe is live when it connects or the accept backlog is full. Only
/// `ConnectionRefused` proves that the socket is stale; unclassified failures
/// carry path context and must never cause the endpoint to be unlinked.
#[cfg_attr(not(unix), allow(dead_code))]
fn classify_unix_probe(probe: io::Result<()>, sock: &Path) -> io::Result<UnixProbe> {
    match probe {
        Ok(()) => Ok(UnixProbe::Live),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(UnixProbe::Live),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Ok(UnixProbe::Stale),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UnixProbe::Gone),
        Err(error) => Err(io_error_with_path_context(
            error,
            "probe existing control socket",
            sock,
        )),
    }
}

/// Probe from the existing `spawn_blocking` worker. `Handle::block_on` is
/// legal there, and the runtime thread awaiting that worker drives the I/O
/// driver even when it uses a current-thread runtime.
#[cfg(unix)]
fn probe_unix_listener(sock: &Path) -> io::Result<()> {
    let handle = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    match handle.block_on(tokio::time::timeout(
        BIND_PROBE_WAIT,
        tokio::net::UnixStream::connect(sock),
    )) {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "existing control socket did not answer the probe within {}ms; refusing to replace it",
                BIND_PROBE_WAIT.as_millis()
            ),
        )),
    }
}

/// Outcome of [`IpcListener::bind_exclusive`]: the caller either *is* the
/// daemon or found a live one.
pub enum BindOutcome {
    Bound(IpcListener),
    AlreadyRunning,
}

/// The daemon's listening endpoint (and single-instance lock).
pub enum IpcListener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        name: String,
        /// The pre-created next server instance. Always `Some` between
        /// accepts so a client connecting concurrently never sees
        /// file-not-found; recreated after each hand-off.
        next: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    },
}

impl IpcListener {
    /// Bind the endpoint, treating it as the single-daemon lock (see the
    /// module docs). `AlreadyRunning` is the spawn-race loser's clean exit.
    pub async fn bind_exclusive(ep: &IpcEndpoint) -> io::Result<BindOutcome> {
        Self::bind_exclusive_with_hardening(ep, false).await
    }

    /// Bind the endpoint with fail-closed Unix filesystem hardening. Bearer
    /// tokens authenticate clients, so every Unix server endpoint must also
    /// be protected from replacement by an untrusted filesystem principal.
    /// The flag is retained for API compatibility; Windows pipe policy does
    /// not use it.
    pub async fn bind_exclusive_with_hardening(
        ep: &IpcEndpoint,
        _require_hardening: bool,
    ) -> io::Result<BindOutcome> {
        match ep {
            IpcEndpoint::Unix(sock) => {
                #[cfg(unix)]
                {
                    // A connectable socket ⇒ a live daemon; a stale file
                    // (bind would fail with AddrInUse) is unlinked. The whole
                    // probe→unlink→bind sequence runs under the advisory bind
                    // lock (see [`bind_lock`]) on a blocking thread — two
                    // racing binders could otherwise BOTH probe-fail the same
                    // stale socket, and the loser's `remove_file` would unlink
                    // the winner's freshly-bound live socket (unlink succeeds
                    // on bound sockets), stranding every future client.
                    // `ensure_daemon`'s lazy spawn makes concurrent binders
                    // over one stale socket the expected case, not a freak.
                    enum UnixBind {
                        Bound(std::os::unix::net::UnixListener),
                        AlreadyRunning,
                    }
                    let sock = sock.clone();
                    let decision = tokio::task::spawn_blocking(move || -> io::Result<UnixBind> {
                        let parent = sock.parent().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "control socket has no parent directory",
                            )
                        })?;
                        let parent_metadata = std::fs::symlink_metadata(parent)?;
                        if parent_metadata.file_type().is_symlink() {
                            return Err(io_error_with_path_context(
                                io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    "control socket parent must not be a symlink",
                                ),
                                "verify control socket parent",
                                parent,
                            ));
                        }
                        verify_unix_bind_parent(parent, &parent_metadata)?;
                        let _lock = bind_lock(&sock)?;
                        if sock.exists() {
                            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
                            let metadata = std::fs::symlink_metadata(&sock)?;
                            if !metadata.file_type().is_socket()
                                || metadata.uid() != effective_uid().unwrap_or_default()
                            {
                                return Err(io_error_with_path_context(
                                    io::Error::new(
                                        io::ErrorKind::PermissionDenied,
                                        "existing control endpoint is not an owner-owned socket",
                                    ),
                                    "verify existing control socket",
                                    &sock,
                                ));
                            }
                            match classify_unix_probe(probe_unix_listener(&sock), &sock)? {
                                UnixProbe::Live => return Ok(UnixBind::AlreadyRunning),
                                UnixProbe::Gone => {}
                                UnixProbe::Stale => match std::fs::remove_file(&sock) {
                                    Ok(()) => {}
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                    Err(error) => {
                                        return Err(io_error_with_path_context(
                                            error,
                                            "remove stale control socket",
                                            &sock,
                                        ));
                                    }
                                }
                            }
                        }
                        match std::os::unix::net::UnixListener::bind(&sock) {
                            Ok(l) => {
                                // Filesystem isolation is defense in depth; runtime
                                // peer credentials are the authority. When this
                                // listener may grant implicit admin, however, a
                                // hardening failure is a startup error rather than a
                                // silently weakened deployment.
                                if let Err(e) = thegn_core::fsperm::restrict_to_owner(&sock)
                                    .and_then(|()| verify_unix_owner_mode(&sock, 0o600, false))
                                {
                                    if let Err(cleanup) = std::fs::remove_file(&sock) {
                                        tracing::error!(target: "thegn::daemon", path = %sock.display(), "failed to remove unhardened control socket: {cleanup}");
                                    }
                                    return Err(e);
                                }
                                l.set_nonblocking(true)?;
                                Ok(UnixBind::Bound(l))
                            }
                            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                                let probe = match classify_unix_probe(
                                    probe_unix_listener(&sock),
                                    &sock,
                                ) {
                                    Ok(UnixProbe::Live) => Ok(()),
                                    Ok(UnixProbe::Stale | UnixProbe::Gone) => Err(io::Error::new(
                                        io::ErrorKind::ConnectionRefused,
                                        "no live listener",
                                    )),
                                    Err(error) => return Err(error),
                                };
                                match require_live_endpoint(
                                    e,
                                    probe,
                                    &sock,
                                ) {
                                    Ok(()) => Ok(UnixBind::AlreadyRunning),
                                    Err(error) => Err(error),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    })
                    .await
                    .map_err(io::Error::other)??;
                    match decision {
                        UnixBind::Bound(l) => Ok(BindOutcome::Bound(IpcListener::Unix(
                            tokio::net::UnixListener::from_std(l)?,
                        ))),
                        UnixBind::AlreadyRunning => Ok(BindOutcome::AlreadyRunning),
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = sock;
                    Err(unsupported("unix-socket IPC on a non-unix host"))
                }
            }
            IpcEndpoint::Pipe(name) => {
                #[cfg(windows)]
                {
                    use tokio::net::windows::named_pipe::ServerOptions;
                    // ERROR_ACCESS_DENIED (5): an instance of the name already
                    // exists. Usually that means a live daemon owns it — but
                    // not always, so retry briefly before concluding it.
                    //
                    // Windows keeps the name reserved for a moment after the
                    // last handle closes, if any of its instances ever carried
                    // a connection. Measured on the runner: a bind right after
                    // the previous owner drops its handles needs ONE ~5ms retry
                    // when there was a client, and zero when there wasn't. A
                    // daemon restarting into that window (`thegn daemon`
                    // straight after the old one exits) would otherwise decide
                    // a rival owned the pipe and exit, leaving no daemon at all.
                    //
                    // Deliberately NOT probed by connecting, unlike the unix
                    // arm. A unix probe connect is free — the listener's
                    // backlog absorbs it. A pipe probe is destructive: it
                    // consumes the one free instance the listener pre-created,
                    // and Windows then wants an explicit DisconnectNamedPipe
                    // before that instance can serve anyone else, so the next
                    // `accept_stream` hands out a corpse whose first read fails
                    // with "early eof". Also measured, the hard way.
                    const ERROR_ACCESS_DENIED: i32 = 5;
                    // ~122ms total. Only ever paid on daemon startup, and only
                    // when the name is taken — a losing racer is exiting anyway.
                    const REBIND_BACKOFF_MS: [u64; 5] = [2, 5, 15, 40, 60];
                    let mut backoff = REBIND_BACKOFF_MS.iter();
                    loop {
                        match ServerOptions::new()
                            .first_pipe_instance(true)
                            .reject_remote_clients(true)
                            .create(name)
                        {
                            Ok(server) => {
                                return Ok(BindOutcome::Bound(IpcListener::Pipe {
                                    name: name.clone(),
                                    next: Some(server),
                                }));
                            }
                            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                                match backoff.next() {
                                    Some(ms) => {
                                        tokio::time::sleep(std::time::Duration::from_millis(*ms))
                                            .await;
                                    }
                                    // Still held after the whole budget: a real
                                    // daemon. Refusing to start is recoverable;
                                    // stealing a live daemon's pipe is not.
                                    None => return Ok(BindOutcome::AlreadyRunning),
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = name; // best-effort: non-Result discard — param unused in this stub
                    Err(unsupported("named-pipe IPC on a non-Windows host"))
                }
            }
        }
    }

    /// Accept one stream together with listener-authenticated peer metadata.
    pub async fn accept_stream_with_info(&mut self) -> io::Result<(IpcStream, IpcConnectInfo)> {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(l) => {
                let (stream, _) = l.accept().await?;
                let peer = match stream.peer_cred() {
                    Ok(cred) => PeerIdentity::UnixEuid(cred.uid()),
                    Err(error) => PeerIdentity::Unavailable(error.to_string()),
                };
                let endpoint = l
                    .local_addr()
                    .ok()
                    .and_then(|addr| {
                        addr.as_pathname()
                            .map(|path| path.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default();
                Ok((IpcStream::Unix(stream), IpcConnectInfo { endpoint, peer }))
            }
            #[cfg(windows)]
            IpcListener::Pipe { name, next } => {
                use tokio::net::windows::named_pipe::ServerOptions;
                let server = match next.take() {
                    Some(s) => s,
                    None => ServerOptions::new()
                        .reject_remote_clients(true)
                        .create(&*name)?,
                };
                server.connect().await?;
                // Pre-create the successor before handing this one out.
                *next = ServerOptions::new()
                    .reject_remote_clients(true)
                    .create(&*name)
                    .ok();
                Ok((
                    IpcStream::PipeServer(server),
                    IpcConnectInfo {
                        endpoint: name.clone(),
                        peer: PeerIdentity::LocalPipe,
                    },
                ))
            }
        }
    }

    /// Accept one connection. (Named `accept_stream` so the inherent method
    /// doesn't shadow `axum::serve::Listener::accept`.)
    pub async fn accept_stream(&mut self) -> io::Result<IpcStream> {
        self.accept_stream_with_info()
            .await
            .map(|(stream, _)| stream)
    }

    /// The endpoint's stable string form (registry row / log lines).
    pub fn endpoint_display(&self) -> String {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(l) => l
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.to_string_lossy().into_owned()))
                .unwrap_or_default(),
            #[cfg(windows)]
            IpcListener::Pipe { name, .. } => name.clone(),
        }
    }
}

/// `axum::serve` integration: the trait's `accept` is infallible, so transient
/// accept errors are logged and retried (matching axum's own built-in
/// listener impls).
impl axum::serve::Listener for IpcListener {
    type Io = IpcStream;
    type Addr = IpcConnectInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.accept_stream_with_info().await {
                Ok(accepted) => return accepted,
                Err(e) => {
                    tracing::warn!(target: "thegn::daemon", "ipc accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(IpcConnectInfo {
            endpoint: self.endpoint_display(),
            peer: PeerIdentity::Unavailable("listener address has no peer".into()),
        })
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, IpcListener>>
    for IpcConnectInfo
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, IpcListener>) -> Self {
        stream.remote_addr().clone()
    }
}

/// Create and owner-restrict a Unix socket's parent directory. Unix endpoint
/// parents are hardened for both local-admin and token-only listeners.
pub fn prepare_control_directory(path: &Path, _require_hardening: bool) -> io::Result<()> {
    // Windows control endpoints are named pipes, so their security comes from
    // the pipe DACL and there is no filesystem parent to harden.
    if cfg!(windows) {
        return Ok(());
    }
    std::fs::create_dir_all(path).map_err(|error| {
        io_error_with_path_context(
            error,
            "prepare control socket directory; move [daemon] socket into a private directory",
            path,
        )
    })?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "control socket parent {} must not be a symlink; move [daemon] socket into a private directory",
                path.display()
            ),
        ));
    }
    let result = thegn_core::fsperm::restrict_dir_to_owner(path);
    #[cfg(unix)]
    let result = result.and_then(|()| verify_unix_owner_mode(path, 0o700, true));
    result.map_err(|error| {
        io_error_with_path_context(
            error,
            "harden control socket directory; move [daemon] socket into a private directory",
            path,
        )
    })
}

/// Read-only hardening status used by `thegn doctor`.
pub fn control_endpoint_hardening(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket has no parent directory",
        )
    })?;
    #[cfg(unix)]
    {
        verify_unix_owner_mode(parent, 0o700, true)?;
        verify_unix_owner_mode(path, 0o600, false)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

#[cfg(unix)]
fn verify_unix_bind_parent(path: &Path, metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let mode = metadata.mode() & 0o777;
    let expected_uid = effective_uid().expect("unix has an effective uid");
    if !metadata.is_dir() || metadata.uid() != expected_uid || mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be an owner-owned directory without group/other write permission (found uid {}, mode {mode:04o})",
                path.display(),
                metadata.uid(),
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_unix_owner_mode(path: &Path, expected_mode: u32, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    let metadata = std::fs::symlink_metadata(path)?;
    let correct_kind = if directory {
        metadata.is_dir()
    } else {
        metadata.file_type().is_socket()
    };
    let mode = metadata.permissions().mode() & 0o777;
    let uid = metadata.uid();
    let expected_uid = effective_uid().expect("unix has an effective uid");
    if !correct_kind || uid != expected_uid || mode != expected_mode {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be an owner-owned {} with mode {:04o} (found uid {uid}, mode {mode:04o}); set owner uid {expected_uid} and mode {:04o}",
                path.display(),
                if directory { "directory" } else { "socket" },
                expected_mode,
                expected_mode,
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_probe_classifies_every_probe_result() {
        let sock = Path::new("control.sock");
        assert_eq!(classify_unix_probe(Ok(()), sock).unwrap(), UnixProbe::Live);

        let cases = [
            (io::ErrorKind::WouldBlock, Some(UnixProbe::Live)),
            (io::ErrorKind::ConnectionRefused, Some(UnixProbe::Stale)),
            (io::ErrorKind::NotFound, Some(UnixProbe::Gone)),
            (io::ErrorKind::TimedOut, None),
            (io::ErrorKind::PermissionDenied, None),
            (io::ErrorKind::Other, None),
        ];
        for (kind, expected) in cases {
            let result = classify_unix_probe(Err(io::Error::new(kind, "probe failure")), sock);
            match expected {
                Some(expected) => assert_eq!(result.unwrap(), expected, "{kind:?}"),
                None => {
                    let error = result.expect_err("unclassified probe errors must fail closed");
                    assert!(error.to_string().contains("probe existing control socket"));
                    assert!(error.to_string().contains(&sock.display().to_string()));
                    let context = error
                        .get_ref()
                        .and_then(|source| source.downcast_ref::<IoPathContext>())
                        .expect("probe error keeps its path context");
                    assert_eq!(context.source.kind(), kind);
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_probe_treats_a_full_backlog_as_a_live_daemon() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::io::AsRawFd as _;

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("busy.sock");
        assert_socket_path_fits(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // SAFETY: listener owns a valid listening socket fd; listen updates its backlog.
        let result = unsafe { libc::listen(listener.as_raw_fd(), 0) };
        assert_eq!(result, 0, "listen must shrink the backlog");

        let mut connections = Vec::new();
        let mut backlog_full = false;
        for _ in 0..64 {
            match tokio::net::UnixStream::connect(&sock).await {
                Ok(stream) => connections.push(stream),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    backlog_full = true;
                    break;
                }
                Err(error) => panic!("unexpected backlog fill error: {error}"),
            }
        }
        if !backlog_full {
            assert!(!thegn_core::testenv::require_or_skip(
                "a unix listener whose accept backlog fills (EAGAIN)",
                false,
            ));
            return;
        }

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            IpcListener::bind_exclusive(&IpcEndpoint::for_socket_path(&sock)),
        )
        .await
        .expect("probe must not block on a full backlog")
        .unwrap();
        assert!(matches!(outcome, BindOutcome::AlreadyRunning));
        assert!(sock.exists(), "live listener socket must survive the probe");
        drop(connections);
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_probe_still_replaces_a_refused_stale_socket() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("stale.sock");
        assert_socket_path_fits(&sock);
        let endpoint = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive(&endpoint).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        drop(listener);

        assert!(matches!(
            IpcListener::bind_exclusive(&endpoint).await.unwrap(),
            BindOutcome::Bound(_)
        ));
    }

    #[cfg(unix)]
    fn open_lock_holder(path: &Path) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .unwrap()
    }

    #[cfg(unix)]
    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[cfg(unix)]
    fn assert_socket_path_fits(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;

        // Keep a NUL terminator available in sockaddr_un::sun_path on every
        // Unix target supported by libc.
        let sockaddr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        assert!(
            path.as_os_str().as_bytes().len() < sockaddr.sun_path.len(),
            "socket path exceeds sockaddr_un::sun_path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn server_peer_identity_requires_same_uid() {
        let uid = effective_uid().unwrap();
        assert!(server_peer_allowed(Some(uid), uid).is_ok());
        assert_eq!(
            server_peer_allowed(Some(uid.wrapping_add(1)), uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            server_peer_allowed(None, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_rejects_symlinked_and_shared_parents_before_connecting() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let td = tempfile::tempdir().unwrap();
        let real = td.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = td.path().join("alias");
        symlink(&real, &alias).unwrap();
        let symlink_error = match connect(&IpcEndpoint::Unix(alias.join("missing.sock"))).await {
            Err(error) => error,
            Ok(_) => panic!("symlinked socket parent connected"),
        };
        assert_eq!(symlink_error.kind(), io::ErrorKind::PermissionDenied);

        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o777)).unwrap();
        let broad_error = match connect(&IpcEndpoint::Unix(real.join("missing.sock"))).await {
            Err(error) => error,
            Ok(_) => panic!("world-writable socket parent connected"),
        };
        assert_eq!(broad_error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_rejects_a_server_before_the_server_can_read_anything() {
        use tokio::io::AsyncReadExt as _;

        let dir = private_tempdir();
        let sock = dir.path().join("fake.sock");
        assert_socket_path_fits(&sock);
        prepare_control_directory(dir.path(), false).unwrap();
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };

        verify_unix_client_parent(&sock).unwrap();
        drop(listener);
        let mut replacement = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("dropped listener must be stale"),
        };
        let expected_euid = effective_uid().unwrap().wrapping_add(1);
        let error = match connect_unix_after_parent_check(&sock, expected_euid).await {
            Err(error) => error,
            Ok(_) => panic!("server with a different uid was accepted"),
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let (mut server_side, _) = replacement.accept_stream_with_info().await.unwrap();
        let mut received = Vec::new();
        server_side.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty(), "rejected server received client bytes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_handles_leaf_replacement_before_connect() {
        let dir = private_tempdir();
        let sock = dir.path().join("replace.sock");
        assert_socket_path_fits(&sock);
        prepare_control_directory(dir.path(), false).unwrap();
        let ep = IpcEndpoint::for_socket_path(&sock);

        let first = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        drop(first);
        let mut replacement = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("dropped listener must be stale"),
        };
        let client = connect(&ep)
            .await
            .expect("connect must authenticate the listener bound at the replaced leaf");
        drop(client);
        let (accepted, _) = replacement.accept_stream_with_info().await.unwrap();
        drop(accepted);

        std::fs::remove_file(&sock).unwrap();
        std::fs::write(&sock, b"replacement leaf").unwrap();
        assert!(connect(&ep).await.is_err());
    }

    #[test]
    fn pipe_name_is_deterministic_prefixed_and_short() {
        let a = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let b = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let c = pipe_name_for_path(Path::new("/tmp/other/thegn/daemon.sock"));
        assert_eq!(a, b, "same path ⇒ same pipe name");
        assert_ne!(a, c, "different state dirs ⇒ different pipes (isolation)");
        assert!(a.starts_with(r"\\.\pipe\thegn-v2-"), "{a}");
        // prefix + "thegn-v2-" + 32 hex chars — comfortably inside the 256-char
        // pipe-name limit regardless of the input path length.
        assert_eq!(a.len(), PIPE_PREFIX.len() + "thegn-v2-".len() + 32);
    }

    #[test]
    fn pipe_name_normalizes_case_separators_and_verbatim_prefix() {
        let paths = [
            r"C:\Users\U\AppData\Local\thegn\daemon.sock",
            r"\\?\C:\Users\U\AppData\Local\thegn\daemon.sock",
            "C:/Users/U/AppData/Local/thegn/daemon.sock",
            r"c:\users\u\appdata\local\thegn\daemon.sock",
        ];
        let names: Vec<_> = paths
            .iter()
            .map(|path| pipe_name_for_path(Path::new(path)))
            .collect();
        let expected = r"\\.\pipe\thegn-v2-416b942e44eb25df62f1699da9543710";
        for name in &names {
            assert_eq!(name, expected, "normalized pipe name differs: {name}");
        }
    }

    #[test]
    fn pipe_name_unc_verbatim_prefix_matches_plain_unc() {
        let verbatim = pipe_name_for_path(Path::new(r"\\?\UNC\server\share\thegn\daemon.sock"));
        let plain = pipe_name_for_path(Path::new(r"\\server\share\thegn\daemon.sock"));
        let other = pipe_name_for_path(Path::new(r"\\server\share\other\daemon.sock"));
        assert_eq!(verbatim, plain);
        assert_ne!(plain, other);
    }

    #[test]
    fn pipe_name_tolerates_multibyte_after_verbatim_prefix() {
        for path in [r"\\?\a界\x", r"\\?\abé\x"] {
            let name = pipe_name_for_path(Path::new(path));
            assert!(name.starts_with(r"\\.\pipe\thegn-v2-"), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pipe_name_keeps_non_utf8_paths_distinct() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let first = Path::new(OsStr::from_bytes(b"/s/\xff"));
        let second = Path::new(OsStr::from_bytes(b"/s/\xfe"));
        assert_ne!(pipe_name_for_path(first), pipe_name_for_path(second));
    }

    #[test]
    fn classify_routes_by_prefix_then_platform() {
        let sock = Path::new("/run/user/1000/thegn/daemon.sock");
        assert_eq!(
            IpcEndpoint::classify(sock, false),
            IpcEndpoint::Unix(sock.to_path_buf())
        );
        assert_eq!(
            IpcEndpoint::classify(sock, true),
            IpcEndpoint::Pipe(pipe_name_for_path(sock))
        );
        // A stored pipe name (DaemonRow.endpoint from a Windows daemon) is
        // recognized as-is on either platform — discovery round-trips.
        let pipe = Path::new(r"\\.\pipe\thegn-0011223344556677");
        for windows in [false, true] {
            assert_eq!(
                IpcEndpoint::classify(pipe, windows),
                IpcEndpoint::Pipe(pipe.to_string_lossy().into_owned())
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_bind_is_the_lock_and_round_trips() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let ep = IpcEndpoint::for_socket_path(&sock);

        // First bind wins…
        let mut listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(l) => l,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        // …the second sees a live daemon.
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::AlreadyRunning
        ));
        // That liveness probe connected once; drain it from the backlog so the
        // round-trip below accepts the real client.
        let (probe, probe_info) = listener.accept_stream_with_info().await.unwrap();
        assert_eq!(
            probe_info.peer,
            PeerIdentity::UnixEuid(effective_uid().unwrap())
        );
        drop(probe);

        // Round-trip a byte each way through connect/accept.
        let client = tokio::spawn({
            let ep = ep.clone();
            async move {
                let mut c = connect(&ep).await.unwrap();
                c.write_all(b"hi").await.unwrap();
                let mut buf = [0u8; 2];
                c.read_exact(&mut buf).await.unwrap();
                buf
            }
        });
        let (mut server_side, peer_info) = listener.accept_stream_with_info().await.unwrap();
        assert_eq!(
            peer_info.peer,
            PeerIdentity::UnixEuid(effective_uid().unwrap())
        );
        let mut buf = [0u8; 2];
        server_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
        server_side.write_all(b"ok").await.unwrap();
        assert_eq!(&client.await.unwrap(), b"ok");

        // A stale file (dead daemon) is unlinked and re-bound.
        drop(server_side);
        drop(listener);
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::Bound(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "restores a private temp dir after exercising stale-socket removal"
    )]
    async fn failed_stale_socket_removal_is_a_contextual_startup_error() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        drop(listener); // leave a genuine stale socket at the endpoint

        let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(dir.path(), permissions).unwrap();
        // Root and some unusual filesystems can still unlink from a read-only
        // directory. Report that missing non-root prerequisite explicitly.
        match std::fs::remove_file(&sock) {
            Ok(()) => {
                let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
                permissions.set_readonly(false);
                std::fs::set_permissions(dir.path(), permissions).unwrap();
                assert!(!thegn_core::testenv::require_or_skip(
                    "a read-only directory that refuses unlink (non-root)",
                    false,
                ));
                return;
            }
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
        }

        let error = match IpcListener::bind_exclusive(&ep).await {
            Ok(BindOutcome::AlreadyRunning) => panic!("an unremovable path is not a live daemon"),
            Ok(BindOutcome::Bound(_)) => {
                panic!("a stale socket in a read-only directory must not bind")
            }
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("remove stale control socket"));
        assert!(error.to_string().contains(&sock.display().to_string()));
        let source = error
            .get_ref()
            .and_then(|context| context.downcast_ref::<IoPathContext>())
            .expect("context wrapper preserves the filesystem error as its source");
        assert_eq!(source.source.kind(), io::ErrorKind::PermissionDenied);

        let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(dir.path(), permissions).unwrap();
    }

    #[test]
    fn addr_in_use_without_a_connectable_listener_preserves_the_bind_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");

        assert!(
            require_live_endpoint(
                io::Error::new(io::ErrorKind::AddrInUse, "original bind failure"),
                Ok(()),
                &sock,
            )
            .is_ok()
        );

        let error = require_live_endpoint(
            io::Error::new(io::ErrorKind::AddrInUse, "original bind failure"),
            Err(io::Error::new(io::ErrorKind::NotFound, "no listener")),
            &sock,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains("bind control socket"));
        assert!(error.to_string().contains(&sock.display().to_string()));
        let source = error
            .get_ref()
            .and_then(|context| context.downcast_ref::<IoPathContext>())
            .expect("context wrapper preserves the bind error as its source");
        assert_eq!(source.source.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(source.source.to_string(), "original bind failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn strict_hardening_is_verified_and_doctor_detects_regression() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), true).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive_with_hardening(&ep, true)
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh socket must bind"),
        };
        control_endpoint_hardening(&sock).expect("strict bind leaves a hardened endpoint");

        let link = dir.path().join("link.sock");
        std::os::unix::fs::symlink(&sock, &link).unwrap();
        let symlink_error = control_endpoint_hardening(&link).unwrap_err();
        assert_eq!(symlink_error.kind(), io::ErrorKind::PermissionDenied);

        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).unwrap();
        let error = control_endpoint_hardening(&sock).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("mode 0666"));

        // Token-only mode cannot use a shared parent or a replaceable lock
        // path as a source of server identity either.
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        let shared_error = match IpcListener::bind_exclusive_with_hardening(
            &IpcEndpoint::for_socket_path(&shared.join("d.sock")),
            false,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("token-only bind must reject a shared parent"),
        };
        assert_eq!(shared_error.kind(), io::ErrorKind::PermissionDenied);

        let not_a_directory = dir.path().join("not-a-directory");
        std::fs::write(&not_a_directory, b"file").unwrap();
        let directory_error = prepare_control_directory(&not_a_directory, false).unwrap_err();
        assert!(
            directory_error
                .to_string()
                .contains("move [daemon] socket into a private directory")
        );

        let lock_sock = dir.path().join("replace.sock");
        let lock_link = dir.path().join("replace.sock.lock");
        let lock_target = dir.path().join("lock-target");
        std::fs::write(&lock_target, b"unrelated").unwrap();
        std::os::unix::fs::symlink(&lock_target, &lock_link).unwrap();
        let lock_ep = IpcEndpoint::for_socket_path(&lock_sock);
        let lock_error = match IpcListener::bind_exclusive_with_hardening(&lock_ep, false).await {
            Err(error) => error,
            Ok(_) => panic!("token-only bind must reject a replaceable lock path"),
        };
        let lock_context = lock_error
            .get_ref()
            .and_then(|error| error.downcast_ref::<IoPathContext>())
            .expect("lock open error retains path context");
        assert!(lock_error.to_string().contains("open control socket lock"));
        assert_eq!(lock_context.source.raw_os_error(), Some(libc::ELOOP));
        assert!(
            !lock_sock.exists(),
            "failed lock acquisition must not create the endpoint"
        );

        let regular_sock = dir.path().join("regular.sock");
        std::fs::write(&regular_sock, b"leave this file alone").unwrap();
        let regular_ep = IpcEndpoint::for_socket_path(&regular_sock);
        let regular_error = match IpcListener::bind_exclusive(&regular_ep).await {
            Err(error) => error,
            Ok(_) => panic!("bind must reject a regular file at the socket path"),
        };
        assert_eq!(regular_error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read(&regular_sock).unwrap(),
            b"leave this file alone"
        );

        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_restricts_an_existing_broad_control_lock() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let lock = dir.path().join("d.sock.lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

        let listener = match IpcListener::bind_exclusive(&IpcEndpoint::for_socket_path(&sock))
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600,
            "bind must tighten the preexisting lock before using it"
        );

        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_fails_closed_while_another_holder_keeps_it() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("held.sock");
        assert_socket_path_fits(&sock);
        let holder = open_lock_holder(&sock.with_extension("sock.lock"));
        holder.lock().unwrap();

        let started = std::time::Instant::now();
        let error = bind_lock_with_wait(&sock, std::time::Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("held by another process"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!sock.exists(), "failed lock acquisition created a socket");
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_acquires_once_the_holder_releases() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("held.sock");
        assert_socket_path_fits(&sock);
        let holder = open_lock_holder(&sock.with_extension("sock.lock"));
        holder.lock().unwrap();
        drop(holder);

        assert!(bind_lock_with_wait(&sock, std::time::Duration::from_secs(2)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_waits_out_a_brief_holder() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("held.sock");
        assert_socket_path_fits(&sock);
        let lock_path = sock.with_extension("sock.lock");
        let (locked_sender, locked_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let holder_thread = std::thread::spawn(move || {
            let holder = open_lock_holder(&lock_path);
            holder.lock().unwrap();
            locked_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            drop(holder);
        });
        locked_receiver.recv().unwrap();

        let (binding_sender, binding_receiver) = std::sync::mpsc::channel();
        let binding_sock = sock.clone();
        let binder_thread = std::thread::spawn(move || {
            binding_sender.send(()).unwrap();
            bind_lock_with_wait(&binding_sock, std::time::Duration::from_secs(2))
        });
        binding_receiver.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let binder_waited_for_holder = !binder_thread.is_finished();
        release_sender.send(()).unwrap();
        assert!(binder_thread.join().unwrap().is_ok());
        holder_thread.join().unwrap();
        assert!(
            binder_waited_for_holder,
            "binder finished while the lock was held"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_fails_closed_when_control_lock_cannot_be_opened() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("blocked.sock");
        assert_socket_path_fits(&sock);
        std::fs::create_dir(sock.with_extension("sock.lock")).unwrap();

        let error = match IpcListener::bind_exclusive_with_hardening(
            &IpcEndpoint::for_socket_path(&sock),
            false,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("bind must fail when lock acquisition fails"),
        };
        assert!(error.to_string().contains("open control socket lock"));
        assert!(!sock.exists(), "failed lock acquisition created a socket");
    }

    /// The stale-socket TOCTOU: N binders racing one stale socket file must
    /// elect exactly one daemon, and the losers' probe/unlink path must not
    /// strip the winner's freshly-bound socket out of the filesystem. Without
    /// the `<sock>.lock` serialization this is a race (unlink succeeds on
    /// bound sockets); with it the outcome is deterministic.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn racing_binders_on_a_stale_socket_elect_exactly_one_daemon() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let path = dir.path().join("d.sock");
        assert_socket_path_fits(&path);
        // A dead daemon's leftover: a socket file nothing is listening on.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let ep = IpcEndpoint::for_socket_path(&path);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let ep = ep.clone();
            tasks.push(tokio::spawn(async move {
                IpcListener::bind_exclusive(&ep).await
            }));
        }
        let mut bound = Vec::new();
        let mut already = 0usize;
        for t in tasks {
            match t.await.unwrap().unwrap() {
                BindOutcome::Bound(l) => bound.push(l),
                BindOutcome::AlreadyRunning => already += 1,
            }
        }
        assert_eq!(bound.len(), 1, "exactly one binder may win the socket");
        assert_eq!(already, 7);
        // The winner's socket survived the losers' stale-file handling.
        connect(&ep)
            .await
            .expect("the surviving socket accepts connections");
        drop(bound);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_bind_is_the_lock_and_round_trips() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // Unique per test process so parallel CI runs can't collide.
        let ep = IpcEndpoint::Pipe(format!(r"\\.\pipe\thegn-test-{}", std::process::id()));

        let mut listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(l) => l,
            BindOutcome::AlreadyRunning => panic!("fresh pipe must bind"),
        };
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::AlreadyRunning
        ));

        let client = tokio::spawn({
            let ep = ep.clone();
            async move {
                let mut c = connect(&ep).await.unwrap();
                c.write_all(b"hi").await.unwrap();
                let mut buf = [0u8; 2];
                c.read_exact(&mut buf).await.unwrap();
                buf
            }
        });
        let mut server_side = listener.accept_stream().await.unwrap();
        let mut buf = [0u8; 2];
        server_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
        server_side.write_all(b"ok").await.unwrap();
        assert_eq!(&client.await.unwrap(), b"ok");

        // Pipes die with their handles: dropping the listener frees the name.
        drop(server_side);
        drop(listener);
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::Bound(_)
        ));
    }
}
