//! The compositor's home side of the iroh call-home reach.
//!
//! [`IrohHome`] owns a persistent iroh `Endpoint` with a **stable** EndpointId
//! (its secret key is persisted by the host in the OS keyring), runs an accept
//! loop, and keeps a registry of live per-sandbox connections. Sandboxes dial in
//! (the `thegn-agent` binary), authenticate with a per-sandbox token, and
//! then the compositor opens an exec bi-stream per shell — bridged to the same
//! transport-blind [`ExecSession`] channels the pane machinery already consumes,
//! so no pane code changes.
//!
//! Security: iroh is E2E-encrypted (QUIC+TLS by pubkey). The sandbox pins the
//! home EndpointId (can't be MITM'd); the compositor gates every incoming
//! connection on a [`TokenVerifier`] (unminted/unknown tokens are rejected).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use thegn_core::iroh_wire::{ALPN, ExecReq, Hello, Wire, WireDecoder, encode};
use tokio::sync::{mpsc, watch};

use crate::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec};

/// Bound on how long an accepted-but-unauthenticated connection may take to send
/// its [`Hello`]. A peer that completes the QUIC handshake (any holder of the
/// home EndpointId, which is injected into every sandbox env) but then goes
/// silent would otherwise pin a task + Connection forever (pre-auth resource
/// hold). On expiry we close the connection and drop the task.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Authorizes an incoming sandbox connection. Returns the sandbox id the caller
/// is authorized to serve (the registry key), or `None` to reject the connection.
/// The production impl checks the minted-token store; tests use a stub.
pub trait TokenVerifier: Send + Sync + 'static {
    fn verify(&self, hello: &Hello) -> Option<String>;
}

/// A [`TokenVerifier`] backed by a simple closure.
pub struct FnVerifier<F>(pub F);

impl<F> TokenVerifier for FnVerifier<F>
where
    F: Fn(&Hello) -> Option<String> + Send + Sync + 'static,
{
    fn verify(&self, hello: &Hello) -> Option<String> {
        (self.0)(hello)
    }
}

type Registry = Arc<Mutex<HashMap<String, Connection>>>;

/// The compositor's home endpoint + connection registry.
pub struct IrohHome {
    endpoint: iroh::Endpoint,
    conns: Registry,
    /// Emitted (sandbox id) whenever a sandbox registers, so the host can wake
    /// its loop and mark the sandbox ready (replaces the sshd-reachability poll).
    registered_tx: mpsc::UnboundedSender<String>,
}

impl IrohHome {
    /// Bind the home endpoint. Pass `secret` to pin a stable EndpointId across
    /// restarts (the host loads it from the keyring); `None` ⇒ ephemeral (tests).
    /// Returns the home plus a receiver that fires each time a sandbox registers.
    pub async fn bind(
        secret: Option<iroh::SecretKey>,
        verifier: Arc<dyn TokenVerifier>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<String>)> {
        let mut builder =
            iroh::Endpoint::builder(iroh::endpoint::presets::N0).alpns(vec![ALPN.to_vec()]);
        if let Some(sk) = secret {
            builder = builder.secret_key(sk);
        }
        let endpoint = builder.bind().await.context("bind home endpoint")?;
        Ok(Self::serve(endpoint, verifier))
    }

    /// Start serving on an already-bound endpoint. The production path uses
    /// [`bind`](Self::bind); tests inject a `presets::Minimal` endpoint so two
    /// local endpoints connect directly (offline, no relay).
    pub fn serve(
        endpoint: iroh::Endpoint,
        verifier: Arc<dyn TokenVerifier>,
    ) -> (Self, mpsc::UnboundedReceiver<String>) {
        Self::serve_with_handshake_timeout(endpoint, verifier, HANDSHAKE_TIMEOUT)
    }

    /// [`serve`](Self::serve) with an explicit pre-auth handshake timeout. Only
    /// the default is used in production; tests inject a short timeout to prove
    /// the timeout path without a multi-second wait.
    fn serve_with_handshake_timeout(
        endpoint: iroh::Endpoint,
        verifier: Arc<dyn TokenVerifier>,
        handshake_timeout: Duration,
    ) -> (Self, mpsc::UnboundedReceiver<String>) {
        let conns: Registry = Arc::new(Mutex::new(HashMap::new()));
        let (registered_tx, registered_rx) = mpsc::unbounded_channel();

        tokio::spawn(accept_loop(
            endpoint.clone(),
            verifier,
            conns.clone(),
            registered_tx.clone(),
            handshake_timeout,
        ));

        (
            Self {
                endpoint,
                conns,
                registered_tx,
            },
            registered_rx,
        )
    }

    /// This compositor's stable home EndpointId — the value injected into a
    /// sandbox as `THEGN_HOME_NODE`.
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.endpoint.id()
    }

    /// The full addr (id + direct/relay transport addrs). Used offline in tests to
    /// dial without discovery; production sandboxes dial by id alone.
    pub fn addr(&self) -> iroh::EndpointAddr {
        self.endpoint.addr()
    }

    /// Whether a given sandbox currently has a live home connection. Sync (std
    /// Mutex) so the sandbox-provider factory can consult it off the async path.
    pub fn is_connected(&self, sandbox: &str) -> bool {
        self.conns
            .lock()
            .map(|m| m.contains_key(sandbox))
            .unwrap_or(false)
    }

    /// Open an interactive exec session (PTY) in a connected sandbox over iroh,
    /// returning the same channel-based [`ExecSession`] the pane machinery drives.
    pub async fn open_exec(&self, sandbox: &str, spec: ExecSpec) -> Result<ExecSession> {
        // Clone the connection out under the (fast, std) lock, then drop the guard
        // before any await.
        let conn = {
            let guard = self
                .conns
                .lock()
                .map_err(|_| anyhow!("registry poisoned"))?;
            guard
                .get(sandbox)
                .cloned()
                .ok_or_else(|| anyhow!("sandbox '{sandbox}' is not connected home"))?
        };

        let (mut send, recv) = conn.open_bi().await.context("open exec stream")?;
        let req = ExecReq {
            argv: spec.argv,
            tty: spec.tty,
            cols: spec.cols,
            rows: spec.rows,
            env: spec.env,
            cwd: spec.cwd,
        };
        send.write_all(&encode(&Wire::Exec(req)))
            .await
            .context("send exec request")?;

        let (frames_tx, frames_rx) = mpsc::channel::<ExecFrame>(256);
        let (control_tx, control_rx) = mpsc::channel::<ExecControl>(256);
        // Session id is a native-provider concept (reattach); iroh has no server
        // session id, so it stays `None`.
        let (_sid_tx, session_id) = watch::channel::<Option<String>>(None);

        tokio::spawn(drive_frames(recv, frames_tx));
        tokio::spawn(drive_control(send, control_rx));

        Ok(ExecSession {
            frames: frames_rx,
            control: control_tx,
            session_id,
        })
    }

    /// Drop a sandbox's connection from the registry (on teardown).
    pub fn forget(&self, sandbox: &str) {
        let removed = self.conns.lock().ok().and_then(|mut m| m.remove(sandbox));
        if let Some(conn) = removed {
            conn.close(0u32.into(), b"forgotten");
        }
    }

    /// Handle for other subsystems to observe registrations without owning the home.
    pub fn registered_sender(&self) -> mpsc::UnboundedSender<String> {
        self.registered_tx.clone()
    }
}

/// Accept incoming sandbox connections, authenticate, and register them.
async fn accept_loop(
    endpoint: iroh::Endpoint,
    verifier: Arc<dyn TokenVerifier>,
    conns: Registry,
    registered_tx: mpsc::UnboundedSender<String>,
    handshake_timeout: Duration,
) {
    while let Some(incoming) = endpoint.accept().await {
        let verifier = verifier.clone();
        let conns = conns.clone();
        let registered_tx = registered_tx.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("home: incoming failed: {e}");
                    return;
                }
            };
            // The agent's first bi-stream carries the Hello handshake. Bound the
            // whole pre-auth exchange (accept_bi + Hello read) so a peer that
            // completes the QUIC handshake and then stalls can't pin this task +
            // connection indefinitely.
            let hello = match tokio::time::timeout(handshake_timeout, async {
                let mut recv = match conn.accept_bi().await {
                    Ok((_send, recv)) => recv,
                    Err(_) => return None,
                };
                let mut dec = WireDecoder::new();
                match read_frame(&mut recv, &mut dec).await {
                    Ok(Some(Wire::Hello(h))) => Some(h),
                    _ => None,
                }
            })
            .await
            {
                Ok(Some(h)) => h,
                Ok(None) => {
                    conn.close(1u32.into(), b"no hello");
                    return;
                }
                Err(_) => {
                    tracing::debug!("home: handshake timed out; closing pre-auth connection");
                    conn.close(1u32.into(), b"handshake timeout");
                    return;
                }
            };
            match verifier.verify(&hello) {
                Some(sandbox) => {
                    tracing::info!("home: sandbox '{sandbox}' registered over iroh");
                    if let Ok(mut m) = conns.lock() {
                        m.insert(sandbox.clone(), conn.clone());
                    }
                    let _ = registered_tx.send(sandbox.clone());
                    // Evict the registry entry when the connection dies (agent
                    // crash, machine stopped, path lost) so `is_connected` stops
                    // reporting a dead sandbox reachable and the pane path falls
                    // back to ssh. Guard on connection identity so a replacement
                    // from a later re-register isn't evicted by the old close.
                    let watch_conns = conns.clone();
                    let watch_conn = conn.clone();
                    tokio::spawn(async move {
                        watch_conn.closed().await;
                        if let Ok(mut m) = watch_conns.lock()
                            && m.get(&sandbox)
                                .is_some_and(|cur| cur.stable_id() == watch_conn.stable_id())
                        {
                            m.remove(&sandbox);
                        }
                    });
                }
                None => {
                    conn.close(2u32.into(), b"unauthorized");
                }
            }
        });
    }
}

/// Bridge the sandbox→compositor half of an exec stream into [`ExecFrame`]s.
async fn drive_frames(mut recv: RecvStream, frames_tx: mpsc::Sender<ExecFrame>) {
    let mut dec = WireDecoder::new();
    loop {
        match read_frame(&mut recv, &mut dec).await {
            Ok(Some(Wire::Stdout(b))) => {
                if frames_tx.send(ExecFrame::Stdout(b)).await.is_err() {
                    break;
                }
            }
            Ok(Some(Wire::Exit(code))) => {
                let _ = frames_tx.send(ExecFrame::Exit(code)).await;
                break;
            }
            Ok(Some(_)) => {} // ignore stray control frames on this half
            Ok(None) | Err(_) => break,
        }
    }
}

/// Bridge the compositor→sandbox half: [`ExecControl`] messages → wire frames.
async fn drive_control(mut send: SendStream, mut control_rx: mpsc::Receiver<ExecControl>) {
    while let Some(ctl) = control_rx.recv().await {
        let w = match ctl {
            ExecControl::Stdin(b) => Wire::Stdin(b),
            ExecControl::Resize { cols, rows } => Wire::Resize { cols, rows },
            ExecControl::Close => Wire::Close,
        };
        let closing = matches!(w, Wire::Close);
        if send.write_all(&encode(&w)).await.is_err() {
            break;
        }
        if closing {
            let _ = send.finish();
            break;
        }
    }
}

/// Read the next [`Wire`] frame from an iroh recv stream, or `None` at end.
async fn read_frame(recv: &mut RecvStream, dec: &mut WireDecoder) -> Result<Option<Wire>> {
    loop {
        if let Some(w) = dec.next_frame()? {
            return Ok(Some(w));
        }
        let mut buf = [0u8; 16 * 1024];
        match recv.read(&mut buf).await.context("read frame")? {
            None => return Ok(dec.next_frame()?),
            Some(0) => continue,
            Some(n) => dec.push(&buf[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Bind a local, relay-free iroh endpoint for the test (offline loopback).
    async fn local_endpoint() -> iroh::Endpoint {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .expect("bind local endpoint")
    }

    /// Poll `f` until it returns true or the deadline passes.
    async fn wait_until(mut f: impl FnMut() -> bool, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        f()
    }

    /// Regression: a peer that completes the QUIC handshake but never sends its
    /// `Hello` must NOT pin the accept task + connection forever. With a short
    /// injected handshake timeout the home closes the pre-auth connection, which
    /// the dialer observes via `closed()`. The sandbox never registers.
    #[tokio::test]
    async fn handshake_timeout_closes_stalled_pre_auth_connection() {
        // Verifier would accept, but the dialer never gets far enough to be checked.
        let verifier: Arc<dyn TokenVerifier> =
            Arc::new(FnVerifier(|_h: &Hello| Some("wt-stall".to_string())));
        let (home, mut registered) = IrohHome::serve_with_handshake_timeout(
            local_endpoint().await,
            verifier,
            Duration::from_millis(300),
        );
        let home_addr = home.addr();

        // Dial and complete the QUIC handshake, then open the handshake stream but
        // deliberately never write a Hello.
        let dialer = local_endpoint().await;
        let conn = dialer
            .connect(home_addr, ALPN)
            .await
            .expect("dial home");
        let (_stalled_send, _stalled_recv) = conn.open_bi().await.expect("open bi");

        // The home must close our connection once the handshake timeout elapses.
        let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        assert!(
            closed.is_ok(),
            "home did not close the stalled pre-auth connection within the timeout"
        );

        // And no registration ever fired.
        let reg = tokio::time::timeout(Duration::from_millis(200), registered.recv()).await;
        assert!(reg.is_err(), "a stalled dialer must never register");
        assert!(!home.is_connected("wt-stall"));
    }

    /// Regression: when a registered sandbox's connection dies, the registry must
    /// evict it so `is_connected` stops reporting a dead sandbox reachable (which
    /// would keep the pane path on iroh instead of falling back to ssh).
    #[tokio::test]
    async fn dead_connection_is_evicted_from_registry() {
        let verifier: Arc<dyn TokenVerifier> =
            Arc::new(FnVerifier(|h: &Hello| Some(h.sandbox.clone())));
        let (home, mut registered) = IrohHome::serve(local_endpoint().await, verifier);
        let home_addr = home.addr();

        // Dial + send a valid Hello so the home registers us.
        let dialer = local_endpoint().await;
        let conn = dialer.connect(home_addr, ALPN).await.expect("dial home");
        let (mut send, _recv) = conn.open_bi().await.expect("open bi");
        send.write_all(&encode(&Wire::Hello(Hello {
            token: "tok".into(),
            sandbox: "wt-dead".into(),
        })))
        .await
        .expect("send hello");
        send.finish().expect("finish hello");

        let sandbox = tokio::time::timeout(Duration::from_secs(5), registered.recv())
            .await
            .expect("registration timed out")
            .expect("registered channel closed");
        assert_eq!(sandbox, "wt-dead");
        assert!(home.is_connected("wt-dead"), "should be registered");

        // Gracefully close the connection from the sandbox side. Keep the dialer
        // endpoint alive so the CONNECTION_CLOSE frame actually transmits (dropping
        // it immediately would abort the in-flight close, leaving the home to learn
        // only via the much-longer idle timeout).
        conn.close(0u32.into(), b"agent gone");

        // The watcher must evict the dead entry so is_connected flips to false.
        let evicted = wait_until(|| !home.is_connected("wt-dead"), Duration::from_secs(5)).await;
        assert!(
            evicted,
            "dead connection was not evicted; is_connected still reports it reachable"
        );
    }
}
