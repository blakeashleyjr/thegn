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

use anyhow::{Context, Result, anyhow};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use thegn_core::iroh_wire::{ALPN, ExecReq, Hello, Wire, WireDecoder, encode};
use tokio::sync::{mpsc, watch};

use crate::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec};

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
        let conns: Registry = Arc::new(Mutex::new(HashMap::new()));
        let (registered_tx, registered_rx) = mpsc::unbounded_channel();

        tokio::spawn(accept_loop(
            endpoint.clone(),
            verifier,
            conns.clone(),
            registered_tx.clone(),
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
            // The agent's first bi-stream carries the Hello handshake.
            let mut recv = match conn.accept_bi().await {
                Ok((_send, recv)) => recv,
                Err(_) => return,
            };
            let mut dec = WireDecoder::new();
            let hello = match read_frame(&mut recv, &mut dec).await {
                Ok(Some(Wire::Hello(h))) => h,
                _ => {
                    conn.close(1u32.into(), b"no hello");
                    return;
                }
            };
            match verifier.verify(&hello) {
                Some(sandbox) => {
                    tracing::info!("home: sandbox '{sandbox}' registered over iroh");
                    if let Ok(mut m) = conns.lock() {
                        m.insert(sandbox.clone(), conn);
                    }
                    let _ = registered_tx.send(sandbox);
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
