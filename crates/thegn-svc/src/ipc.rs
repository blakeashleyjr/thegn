//! Local daemon IPC: unix-domain sockets on unix, named pipes on Windows —
//! one seam so the daemon, control client, and `axum::serve` are
//! platform-free.
//!
//! **The endpoint is the lock.** On unix, whoever binds the socket is the
//! daemon (a connectable socket ⇒ a live daemon; a stale file is unlinked).
//! On Windows, `first_pipe_instance(true)` gives the same semantics — the
//! first creator owns the pipe name, a second daemon gets `ACCESS_DENIED`
//! (⇒ [`BindOutcome::AlreadyRunning`]), and pipes die with the process, so
//! there is no stale-file case at all.
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
                let _ = name;
                Err(unsupported("named-pipe IPC on a non-Windows host"))
            }
        }
    }
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
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
        match ep {
            IpcEndpoint::Unix(sock) => {
                #[cfg(unix)]
                {
                    // A connectable socket ⇒ a live daemon; a stale file
                    // (bind would fail with AddrInUse) is unlinked.
                    if sock.exists() {
                        match tokio::net::UnixStream::connect(sock).await {
                            Ok(_) => return Ok(BindOutcome::AlreadyRunning),
                            Err(_) => {
                                let _ = std::fs::remove_file(sock);
                            }
                        }
                    }
                    match tokio::net::UnixListener::bind(sock) {
                        Ok(l) => Ok(BindOutcome::Bound(IpcListener::Unix(l))),
                        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                            Ok(BindOutcome::AlreadyRunning)
                        }
                        Err(e) => Err(e),
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
                    // ERROR_ACCESS_DENIED (5): another process already owns
                    // the first instance ⇒ a live daemon.
                    const ERROR_ACCESS_DENIED: i32 = 5;
                    match ServerOptions::new()
                        .first_pipe_instance(true)
                        .reject_remote_clients(true)
                        .create(name)
                    {
                        Ok(server) => Ok(BindOutcome::Bound(IpcListener::Pipe {
                            name: name.clone(),
                            next: Some(server),
                        })),
                        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                            Ok(BindOutcome::AlreadyRunning)
                        }
                        Err(e) => Err(e),
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    Err(unsupported("named-pipe IPC on a non-Windows host"))
                }
            }
        }
    }

    /// Accept one connection. (Named `accept_stream` so the inherent method
    /// doesn't shadow `axum::serve::Listener::accept`.)
    pub async fn accept_stream(&mut self) -> io::Result<IpcStream> {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(l) => Ok(IpcStream::Unix(l.accept().await?.0)),
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
                Ok(IpcStream::PipeServer(server))
            }
        }
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
    type Addr = String;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.accept_stream().await {
                Ok(stream) => return (stream, self.endpoint_display()),
                Err(e) => {
                    tracing::warn!(target: "thegn::daemon", "ipc accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.endpoint_display())
    }
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
        let _ = std::fs::remove_dir_all(&dir);
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
        drop(listener.accept_stream().await.unwrap());

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
        let mut server_side = listener.accept_stream().await.unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);
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
