//! In-process supervisor for automatic port forwards (`[forward]`).
//!
//! The *outbound-localhost* sibling of [`crate::share`]. A single background
//! detector thread polls the **active** worktree's sandbox for newly-bound
//! listening ports (via `superzej_svc::forward`) and reports them on a tokio
//! mpsc channel, pulsing the `TerminalWaker` exactly like the config/LSP/share
//! producers. The event loop drains the channel and, for each appeared port,
//! binds a free host port and spawns a userspace TCP proxy that bridges
//! `localhost:<host_port>` into the container's network namespace via
//! `podman exec` — so a dev server started *after* the sandbox came up is
//! previewable at `http://localhost:<host_port>` without a create-time `-p`.
//!
//! Detection (a subprocess `ss`) runs on the detector thread; the proxy's
//! per-connection `exec` children run inside tokio tasks. The loop itself only
//! does fast bookkeeping (a bind syscall + `tokio::spawn`), honoring the
//! ~0%-idle / no-blocking-on-the-loop contract.

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use superzej_core::config::ForwardConfig;
use superzej_core::forward::diff_listening;
use termwiz::terminal::TerminalWaker;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;

/// A detector → loop message about a port inside the active worktree's sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardEvent {
    /// A new listening port appeared; `runtime` is the OCI prefix that reached
    /// the container (so the loop can build the bridge argv without re-probing).
    Detected {
        worktree: String,
        container_port: u16,
        runtime: Vec<String>,
    },
    /// A previously-seen port is gone — tear its forward down.
    Vanished {
        worktree: String,
        container_port: u16,
    },
}

/// A render-facing snapshot of one active forward (mirrored into `FrameModel`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ForwardView {
    pub worktree: String,
    pub container_port: u16,
    pub host_port: u16,
    /// `http://<bind>:<host_port>` — the preview URL the `o` action opens.
    pub url: String,
    /// Whether the host port differs from the container port (a conflict remap).
    pub remapped: bool,
}

/// A running userspace proxy. Aborting the accept task stops new connections;
/// in-flight bridge children finish on their own (EOF) shortly after.
struct ProxyHandle {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ForwardInstance {
    worktree: String,
    container_port: u16,
    host_port: u16,
    url: String,
    remapped: bool,
    _proxy: ProxyHandle,
}

/// Tracks the live auto-forwards. An event-loop local, mirroring
/// [`crate::share::ShareSupervisor`].
#[derive(Default)]
pub struct ForwardSupervisor {
    instances: Vec<ForwardInstance>,
}

/// The outcome of bringing a forward up — fed back to the loop so it can persist,
/// notify, and re-sync the model.
pub struct Started {
    pub host_port: u16,
    pub url: String,
    pub remapped: bool,
}

impl ForwardSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a forward already exists for `(worktree, container_port)`.
    pub fn has(&self, worktree: &str, container_port: u16) -> bool {
        self.instances
            .iter()
            .any(|i| i.worktree == worktree && i.container_port == container_port)
    }

    /// Render-facing snapshot of all forwards for `worktree` (panel + badge).
    pub fn views(&self, worktree: &str) -> Vec<ForwardView> {
        self.instances
            .iter()
            .filter(|i| i.worktree == worktree)
            .map(|i| ForwardView {
                worktree: i.worktree.clone(),
                container_port: i.container_port,
                host_port: i.host_port,
                url: i.url.clone(),
                remapped: i.remapped,
            })
            .collect()
    }

    /// Bring a forward up for `container_port` on `worktree`: bind a free host
    /// port (preferring the container's own number, remapping into `cfg.range`
    /// on conflict) and spawn the bridge proxy. `runtime` is the OCI prefix that
    /// reached the container. Returns the resolved port/URL, or an error string.
    ///
    /// Must be called from within the tokio runtime (it `tokio::spawn`s the
    /// proxy accept loop).
    pub fn start(
        &mut self,
        cfg: &ForwardConfig,
        worktree: &str,
        container_port: u16,
        runtime: &[String],
    ) -> Result<Started, String> {
        if self.has(worktree, container_port) {
            return Err(format!("already forwarding port {container_port}"));
        }
        let container = superzej_core::sandbox::container_name(worktree);
        let argv = superzej_svc::forward::exec_bridge_argv(runtime, &container, container_port);
        let (listener, host_port) = bind_host_port(&cfg.bind, container_port, cfg.port_range())
            .map_err(|e| format!("no free host port for {container_port}: {e}"))?;
        let proxy = spawn_proxy(listener, argv).map_err(|e| format!("proxy spawn failed: {e}"))?;
        let url = format!("http://{}:{host_port}", cfg.bind);
        let remapped = host_port != container_port;
        self.instances.push(ForwardInstance {
            worktree: worktree.to_string(),
            container_port,
            host_port,
            url: url.clone(),
            remapped,
            _proxy: proxy,
        });
        Ok(Started {
            host_port,
            url,
            remapped,
        })
    }

    /// Tear down the forward for `(worktree, container_port)`. Returns `true` if
    /// one was removed (so the loop can flip the chrome `dirty` flag).
    pub fn stop(&mut self, worktree: &str, container_port: u16) -> bool {
        let before = self.instances.len();
        self.instances
            .retain(|i| !(i.worktree == worktree && i.container_port == container_port));
        before != self.instances.len()
    }

    /// Tear down every forward on `worktree` (e.g. when its sandbox stops).
    /// Returns the host ports that were forwarded.
    pub fn stop_all_on(&mut self, worktree: &str) -> Vec<u16> {
        let mut ports = Vec::new();
        self.instances.retain(|i| {
            if i.worktree == worktree {
                ports.push(i.container_port);
                false
            } else {
                true
            }
        });
        ports
    }

    /// Drop all forwards (teardown on quit).
    pub fn shutdown_all(&mut self) {
        self.instances.clear();
    }
}

/// Bind the first free host port: prefer `desired` (so a preview keeps the dev
/// server's own number), else the lowest free port in `range`. Returns the bound
/// std listener + the chosen port. This bind-walk is the conflict handler — the
/// OS is the source of truth for "in use" (no TOCTOU window).
fn bind_host_port(
    bind_addr: &str,
    desired: u16,
    range: (u16, u16),
) -> std::io::Result<(std::net::TcpListener, u16)> {
    if desired != 0
        && let Ok(l) = std::net::TcpListener::bind((bind_addr, desired))
    {
        return Ok((l, desired));
    }
    let (lo, hi) = range;
    for p in lo..=hi {
        if let Ok(l) = std::net::TcpListener::bind((bind_addr, p)) {
            return Ok((l, p));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "no free host port in range",
    ))
}

/// Turn a bound std listener into a tokio accept loop: for each connection, spawn
/// the `exec`-bridge child and pipe bytes between the TCP client and its stdio.
fn spawn_proxy(listener: std::net::TcpListener, argv: Vec<String>) -> std::io::Result<ProxyHandle> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    let argv = argv.clone();
                    tokio::spawn(async move {
                        if let Err(e) = bridge_conn(sock, &argv).await {
                            tracing::debug!(target: "szhost::forward", "bridge ended: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(target: "szhost::forward", "forward accept failed: {e}");
                    break;
                }
            }
        }
    });
    Ok(ProxyHandle { task })
}

/// Bridge one accepted TCP connection to a fresh `podman exec` child whose stdio
/// is wired to the dev server's loopback inside the container netns.
async fn bridge_conn(mut sock: tokio::net::TcpStream, argv: &[String]) -> std::io::Result<()> {
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut cin = child.stdin.take().expect("piped stdin");
    let mut cout = child.stdout.take().expect("piped stdout");
    let (mut rd, mut wr) = sock.split();
    let to_child = tokio::io::copy(&mut rd, &mut cin);
    let to_sock = tokio::io::copy(&mut cout, &mut wr);
    // Whichever half closes first ends the connection; shut the child stdin so a
    // half-close (client EOF) propagates, then reap.
    tokio::select! {
        _ = to_child => { let _ = cin.shutdown().await; }
        _ = to_sock => {}
    }
    let _ = child.kill().await;
    Ok(())
}

/// Shared target the loop updates when the active worktree changes; `None` means
/// "nothing to probe" (home tab, terminal, or `[forward] auto` off).
pub type DetectTarget = Arc<Mutex<Option<String>>>;

/// Spawn the single off-loop detector thread. It polls the current target
/// worktree's sandbox every `poll` seconds for listening ports, diffs against
/// the last snapshot, and emits [`ForwardEvent`]s. On a probe failure (no
/// sandbox / stopped container) it backs off to avoid burning CPU when nothing
/// is sandboxed — honoring the ~0%-idle contract while still polling external
/// state that genuinely requires it.
pub fn spawn_detector(
    target: DetectTarget,
    poll: Duration,
    tx: UnboundedSender<ForwardEvent>,
    waker: TerminalWaker,
) {
    // Failures widen the sleep up to this cap (e.g. when no worktree is
    // sandboxed); a successful probe resets it to `poll`.
    let idle_backoff = poll.saturating_mul(8).max(Duration::from_secs(10));
    std::thread::Builder::new()
        .name("szforward".into())
        .spawn(move || {
            // The worktree we're currently tracking + its last port snapshot. We
            // track ONE worktree (the loop watches only the active one) and reset
            // the snapshot whenever the target changes — including ↔ `None`. This
            // mirrors the loop, which tears a worktree's forwards down on switch:
            // re-entering an already-running dev server must re-appear as "new" so
            // it gets re-forwarded, which a stale snapshot would suppress.
            let mut tracked: Option<String> = None;
            let mut last: BTreeSet<u16> = BTreeSet::new();
            loop {
                let wt = target.lock().unwrap().clone();
                if wt != tracked {
                    last.clear();
                    tracked = wt.clone();
                }
                let mut sleep = idle_backoff;
                if let Some(wt) = wt {
                    let container = superzej_core::sandbox::container_name(&wt);
                    // On a probe error (no sandbox / stopped container) keep the
                    // last snapshot — don't tear forwards down on a transient blip
                    // — and fall through to the idle backoff. Forwards are cleaned
                    // up explicitly when the worktree closes or the sandbox stops.
                    if let Ok((runtime, now)) =
                        superzej_svc::forward::probe_container_ports(&container)
                    {
                        sleep = poll;
                        let (appeared, disappeared) = diff_listening(&last, &now);
                        last = now;
                        let mut pulsed = false;
                        for p in appeared {
                            if tx
                                .send(ForwardEvent::Detected {
                                    worktree: wt.clone(),
                                    container_port: p,
                                    runtime: runtime.clone(),
                                })
                                .is_ok()
                            {
                                pulsed = true;
                            }
                        }
                        for p in disappeared {
                            if tx
                                .send(ForwardEvent::Vanished {
                                    worktree: wt.clone(),
                                    container_port: p,
                                })
                                .is_ok()
                            {
                                pulsed = true;
                            }
                        }
                        if pulsed {
                            let _ = waker.wake();
                        }
                    }
                }
                std::thread::sleep(sleep);
                // Bail if the loop dropped the receiver (shutdown).
                if tx.is_closed() {
                    break;
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_views_then_stop() {
        // Bind the proxy inside a tiny runtime (start() tokio::spawns).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sup = ForwardSupervisor::new();
            let cfg = ForwardConfig::default();
            // Pick a port that's free *right now* so the preferred-port path is
            // deterministic (don't assume a fixed number is free on the runner).
            let free = {
                let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                l.local_addr().unwrap().port()
            };
            // `runtime` is never executed here (no connections), so any prefix works.
            let started = sup
                .start(&cfg, "/wt/app", free, &["true".to_string()])
                .expect("bind the free port");
            assert_eq!(started.host_port, free);
            assert_eq!(started.url, format!("http://127.0.0.1:{free}"));
            assert!(!started.remapped);

            let v = sup.views("/wt/app");
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].container_port, free);
            assert_eq!(v[0].url, format!("http://127.0.0.1:{free}"));

            // Duplicate is rejected.
            assert!(sup.start(&cfg, "/wt/app", free, &["true".into()]).is_err());

            assert!(sup.stop("/wt/app", free));
            assert!(sup.views("/wt/app").is_empty());
            assert!(!sup.stop("/wt/app", free));
        });
    }

    #[test]
    fn remap_on_conflict_sets_flag() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Occupy a port, then forward "to" it so start() must remap.
            let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = occupied.local_addr().unwrap().port();
            let mut sup = ForwardSupervisor::new();
            // Range that includes a free neighbour for the remap.
            let cfg = ForwardConfig {
                range: format!("{}-{}", port.saturating_add(1), port.saturating_add(50)),
                ..ForwardConfig::default()
            };
            let started = sup
                .start(&cfg, "/wt", port, &["true".into()])
                .expect("remap into range");
            assert_ne!(started.host_port, port, "must avoid the occupied port");
            assert!(started.remapped);
            sup.stop("/wt", port);
        });
    }

    #[test]
    fn stop_all_on_returns_ports() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sup = ForwardSupervisor::new();
            let cfg = ForwardConfig::default();
            sup.start(&cfg, "/wt", 3000, &["true".into()]).unwrap();
            sup.start(&cfg, "/wt", 8080, &["true".into()]).unwrap();
            let mut ports = sup.stop_all_on("/wt");
            ports.sort_unstable();
            assert_eq!(ports, vec![3000, 8080]);
            assert!(sup.views("/wt").is_empty());
        });
    }
}
