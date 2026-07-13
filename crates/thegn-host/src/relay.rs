//! The full-seal model relay: a per-agent unix-domain socket on the host that
//! forwards every connection to the model proxy's TCP listener.
//!
//! When a launched agent runs in a `network=none` sealed container (the default
//! `agent_profile = sealed`), it has loopback only — no route to the host proxy.
//! Its **sole** egress is this socket, bind-mounted into the container; the
//! in-sandbox pi extension dials it directly (over a unix-socket HTTP dispatcher)
//! instead of an IP. The relay is a dumb bidirectional pipe — it adds no policy
//! of its own; the proxy it forwards to is the chokepoint that enforces budgets
//! and routing. This keeps the seal honest: nothing the agent emits can reach
//! anything but the proxy.
//!
//! [`spawn`] returns a [`RelayHandle`] whose drop (or explicit [`RelayHandle::shutdown`])
//! aborts the listener — tied to the agent's ACP connection lifetime in `run.rs`.
// `shutdown` / `socket_dir_ready` are part of the relay's contract + exercised by
// the tests even where the loop relies on `Drop`; allow at module scope.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
#[cfg(unix)]
use tokio::net::{TcpStream, UnixListener};

/// A running relay. Dropping it aborts the accept loop and removes the socket
/// file; the per-connection forwarders finish on their own (EOF) shortly after.
pub struct RelayHandle {
    task: tokio::task::JoinHandle<()>,
    socket: PathBuf,
}

impl RelayHandle {
    /// Stop accepting and remove the socket file.
    pub fn shutdown(self) {
        // Drop runs the teardown.
        drop(self);
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Bind `socket` and forward every accepted connection to `tcp_addr` (the model
/// proxy's `host:port`). The socket's parent dir must exist + be bind-mountable
/// into the sealed container. Returns a handle that tears the relay down on drop.
#[cfg(unix)]
pub fn spawn(socket: PathBuf, tcp_addr: String) -> std::io::Result<RelayHandle> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A stale socket from a prior run would make `bind` fail with EADDRINUSE.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    let socket_for_handle = socket.clone();

    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let addr = tcp_addr.clone();
                    tokio::spawn(async move {
                        if let Err(e) = forward(stream, &addr).await {
                            tracing::debug!(target: "thegn::relay", "relay forward ended: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(target: "thegn::relay", "relay accept failed: {e}");
                    break;
                }
            }
        }
    });

    Ok(RelayHandle {
        task,
        socket: socket_for_handle,
    })
}

/// Windows stub: the relay's consumers are sealed *Linux* containers (the
/// socket is bind-mounted into the sandbox), which don't exist on a native
/// Windows host — there is nothing to relay for.
#[cfg(not(unix))]
pub fn spawn(_socket: PathBuf, _tcp_addr: String) -> std::io::Result<RelayHandle> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "sealed-sandbox model relay requires unix sockets (Linux containers)",
    ))
}

/// Pipe one accepted unix connection to a fresh TCP connection to the proxy,
/// copying in both directions until either side closes.
#[cfg(unix)]
async fn forward(mut unix: tokio::net::UnixStream, tcp_addr: &str) -> std::io::Result<()> {
    let mut tcp = TcpStream::connect(tcp_addr).await?;
    tokio::io::copy_bidirectional(&mut unix, &mut tcp).await?;
    Ok(())
}

/// Best-effort: does `socket`'s directory exist (so the relay can bind there)?
/// Used by callers to log a clear error before launch instead of a late bind fail.
pub fn socket_dir_ready(socket: &Path) -> bool {
    socket.parent().is_some_and(|d| d.exists())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixStream};

    /// End-to-end: a byte written into the unix socket reaches the TCP backend
    /// and the backend's reply comes back — proving the relay is a faithful pipe.
    #[tokio::test]
    async fn relays_unix_to_tcp_round_trip() {
        // A trivial TCP "proxy" that upper-cases whatever it receives.
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut sock, _) = backend.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            let up = buf[..n].to_ascii_uppercase();
            sock.write_all(&up).await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!("sz-relay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("proxy.sock");
        let handle = spawn(socket.clone(), backend_addr).unwrap();

        // Give the listener a beat to bind.
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut client = UnixStream::connect(&socket).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"PING", "relay must pipe bytes through to the proxy");

        // Dropping the handle removes the socket file.
        handle.shutdown();
        assert!(!socket.exists(), "shutdown removes the socket file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_creates_the_socket_dir() {
        let dir = std::env::temp_dir().join(format!("sz-relay-mk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("nested").join("proxy.sock");
        assert!(!socket_dir_ready(&socket));
        let handle = spawn(socket.clone(), "127.0.0.1:1".into()).unwrap();
        assert!(socket.parent().unwrap().exists(), "spawn mkdir -p the dir");
        drop(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
