//! Local daemon IPC: unix-domain sockets on unix, named pipes on Windows —
//! one seam so the daemon, control client, and `axum::serve` are
//! platform-free.
//!
//! **The endpoint is the lock.** On unix, whoever binds the socket is the
//! daemon (a connectable socket ⇒ a live daemon; a stale file is unlinked).
//! On Windows, `first_pipe_instance(true)` gives the same semantics — the
//! first creator owns the pipe name and a second daemon gets `ACCESS_DENIED`
//! (⇒ [`BindOutcome::AlreadyRunning`]). Pipes die with their handles, so
//! there is no stale *file* to clean up — but the name stays reserved for a
//! few milliseconds after the last handle of an instance that carried a
//! connection closes, so `bind_exclusive` retries briefly before believing
//! ACCESS_DENIED (see the comment there).
//!
//! Pipe names are derived from the same per-state-dir socket *path* the unix
//! side uses (`\\.\pipe\thegn-<hex(sha256(path))[..16]>`), so the
//! one-daemon-per-`$XDG_STATE_HOME` isolation (`just start`, smoke tests, the
//! "this shell runs inside a live thegn" gotcha) carries over unchanged. The
//! derivation and endpoint classification are pure and unit-tested on every
//! platform; only the syscalls are `#[cfg]`-gated.

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

/// Deterministic pipe name for a daemon socket path. Hashed (not sanitized)
/// so arbitrarily long/exotic state-dir paths always yield a valid, collision-
/// resistant pipe name.
pub fn pipe_name_for_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{PIPE_PREFIX}thegn-{hex}")
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
                Ok(IpcStream::Unix(
                    tokio::net::UnixStream::connect(path).await?,
                ))
            }
            #[cfg(not(unix))]
            {
                drop(path);
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

fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

/// Take the advisory lock serializing [`IpcListener::bind_exclusive`]'s
/// probe→unlink→bind critical section for `sock`. The sidecar `<sock>.lock`
/// is created once and NEVER unlinked (unlinking it would resurrect the very
/// race it closes); the flock (std's `File::lock`) dies with the process, so
/// it can't go stale. Contention lasts a probe + a bind (milliseconds), and
/// the caller is already on a blocking thread. Best-effort: `None` (exotic
/// fs, permissions) degrades to the old raced path rather than refusing to
/// serve.
#[cfg(unix)]
fn bind_lock(sock: &Path) -> Option<std::fs::File> {
    let mut path = sock.as_os_str().to_owned();
    path.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(std::path::Path::new(&path))
        .ok()?;
    file.lock().ok()?;
    Some(file)
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

    /// Bind the endpoint and require owner-only endpoint hardening when
    /// `require_hardening` is true. The strict form is used whenever the
    /// listener can grant implicit local admin; token-only listeners retain
    /// the portable best-effort permission behavior.
    pub async fn bind_exclusive_with_hardening(
        ep: &IpcEndpoint,
        require_hardening: bool,
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
                        let _lock = bind_lock(&sock);
                        if sock.exists() {
                            match std::os::unix::net::UnixStream::connect(&sock) {
                                Ok(_) => return Ok(UnixBind::AlreadyRunning),
                                Err(_) => {
                                    let _ = std::fs::remove_file(&sock); // best-effort: stale-socket cleanup; next bind re-reports
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
                                    if require_hardening {
                                        let _ = std::fs::remove_file(&sock);
                                        return Err(e);
                                    }
                                    tracing::warn!(target: "thegn::daemon", path = %sock.display(), "could not harden control socket: {e}");
                                }
                                l.set_nonblocking(true)?;
                                Ok(UnixBind::Bound(l))
                            }
                            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                                Ok(UnixBind::AlreadyRunning)
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
                    drop(sock);
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

/// Create and owner-restrict a Unix socket's parent directory. This is strict
/// when implicit local admin is requested, and otherwise retains the existing
/// best-effort portability behavior.
pub fn prepare_control_directory(path: &Path, require_hardening: bool) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let result = thegn_core::fsperm::restrict_dir_to_owner(path);
    #[cfg(unix)]
    let result = result.and_then(|()| verify_unix_owner_mode(path, 0o700, true));
    if require_hardening {
        result
    } else {
        if let Err(error) = result {
            tracing::warn!(target: "thegn::daemon", path = %path.display(), "could not harden control directory: {error}");
        }
        Ok(())
    }
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
fn verify_unix_owner_mode(path: &Path, expected_mode: u32, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    let metadata = std::fs::metadata(path)?;
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
                "{} must be an owner-owned {} with mode {:04o} (found uid {uid}, mode {mode:04o}); set owner uid {expected_uid} and mode {:04o}, or set [serve] local_admin = false to require tokens",
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
    fn pipe_name_is_deterministic_prefixed_and_short() {
        let a = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let b = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let c = pipe_name_for_path(Path::new("/tmp/other/thegn/daemon.sock"));
        assert_eq!(a, b, "same path ⇒ same pipe name");
        assert_ne!(a, c, "different state dirs ⇒ different pipes (isolation)");
        assert!(a.starts_with(r"\\.\pipe\thegn-"), "{a}");
        // prefix + "thegn-" + 16 hex chars — comfortably inside the 256-char
        // pipe-name limit regardless of the input path length.
        assert_eq!(a.len(), PIPE_PREFIX.len() + "thegn-".len() + 16);
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
        let dir = std::env::temp_dir().join(format!("thegn-ipc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&dir).unwrap();
        let ep = IpcEndpoint::for_socket_path(&dir.join("d.sock"));

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
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn strict_hardening_is_verified_and_doctor_detects_regression() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("thegn-ipc-hardening-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        prepare_control_directory(&dir, true).unwrap();
        let sock = dir.join("d.sock");
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive_with_hardening(&ep, true)
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh socket must bind"),
        };
        control_endpoint_hardening(&sock).expect("strict bind leaves a hardened endpoint");

        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).unwrap();
        let error = control_endpoint_hardening(&sock).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("mode 0666"));

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stale-socket TOCTOU: N binders racing one stale socket file must
    /// elect exactly one daemon, and the losers' probe/unlink path must not
    /// strip the winner's freshly-bound socket out of the filesystem. Without
    /// the `<sock>.lock` serialization this is a race (unlink succeeds on
    /// bound sockets); with it the outcome is deterministic.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn racing_binders_on_a_stale_socket_elect_exactly_one_daemon() {
        let dir = std::env::temp_dir().join(format!("thegn-ipc-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.sock");
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
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
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
