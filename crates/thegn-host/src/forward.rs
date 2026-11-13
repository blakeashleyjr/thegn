//! In-process supervisor for automatic port forwards (`[forward]`).
//!
//! The *outbound-localhost* sibling of [`crate::share`]. A single background
//! detector thread polls the **active** worktree's sandbox for newly-bound
//! listening ports (via `thegn_svc::forward`) and reports them on a tokio
//! mpsc channel, pulsing the `TerminalWaker` exactly like the config/LSP/share
//! producers. The event loop drains the channel and, for each appeared port,
//! binds a free host port and spawns a userspace TCP proxy that bridges
//! `localhost:<host_port>` into the container's network namespace through the
//! one runtime that holds it — so a dev server started *after* the sandbox came
//! up is previewable at `http://localhost:<host_port>` without a create-time `-p`.
//!
//! Detection (a subprocess `ss`) runs on the detector thread; the proxy's
//! per-connection `exec` children run inside tokio tasks. The loop itself only
//! does fast bookkeeping (a bind syscall + `tokio::spawn`), honoring the
//! ~0%-idle / no-blocking-on-the-loop contract.

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::ForwardConfig;
use thegn_core::forward::diff_listening;
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
        /// Explicit panel requests bypass the `[forward] auto` port filter.
        manual: bool,
    },
    /// A previously-seen port is gone — tear its forward down.
    Vanished {
        worktree: String,
        container_port: u16,
    },
    /// The detector will not forward this worktree's ports; `reason` names why.
    Refused { worktree: String, reason: String },
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
        let bind_host = cfg.bind_host().map_err(|refusal| refusal.to_string())?;
        if self.has(worktree, container_port) {
            return Err(format!("already forwarding port {container_port}"));
        }
        // Profile-aware, matching the detector and the actual container name
        // (`thegn-{profile}-{slug}` under a non-default profile).
        let container = thegn_core::sandbox::container_name_with_profile(
            worktree,
            Some(&thegn_core::profile::name()),
        );
        let argv = thegn_svc::forward::exec_bridge_argv(runtime, &container, container_port);
        let (listener, host_port) = bind_host_port(bind_host, container_port, cfg.port_range())
            .map_err(|e| format!("no free host port for {container_port}: {e}"))?;
        let proxy = spawn_proxy(listener, argv).map_err(|e| format!("proxy spawn failed: {e}"))?;
        let url_host = match bind_host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(_)) => format!("[{bind_host}]"),
            _ => bind_host.to_string(),
        };
        let url = format!("http://{url_host}:{host_port}");
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
                            tracing::debug!(target: "thegn::forward", "bridge ended: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(target: "thegn::forward", "forward accept failed: {e}");
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
        // best-effort: teardown: the child may already have exited or been reaped
        _ = to_child => { let _ = cin.shutdown().await; }
        _ = to_sock => {}
    }
    let _ = child.kill().await; // best-effort: teardown: the child may already have exited or been reaped
    Ok(())
}

/// Shared target the loop updates when the active worktree changes; `None` means
/// "nothing to probe" (home tab, terminal, or no active sandbox).
pub type DetectTarget = Arc<Mutex<Option<String>>>;

/// Requests from the event loop to add a forward without waiting for a port
/// listening probe. The detector owns runtime resolution and event delivery.
pub type ManualForwards = Arc<Mutex<Vec<(String, u16)>>>;

/// Parse the container port entered in the forward panel.
pub(crate) fn parse_manual_port(input: &str) -> Result<u16, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("port is required".into());
    }
    let port = input
        .parse::<u32>()
        .map_err(|_| "port must be a number".to_string())?;
    if port == 0 || port > u32::from(u16::MAX) {
        return Err("port must be between 1 and 65535".into());
    }
    Ok(port as u16)
}

/// Queue a manual request and wake the event loop. The detector has no extra
/// polling channel: if it is asleep in its existing idle backoff, it observes
/// this request within one backoff interval.
pub(crate) fn enqueue_manual_forward(
    requests: &ManualForwards,
    worktree: String,
    container_port: u16,
    waker: &TerminalWaker,
) {
    requests.lock().unwrap().push((worktree, container_port));
    let _ = waker.wake();
}

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
    manual_forwards: ManualForwards,
    auto_probe: Arc<std::sync::atomic::AtomicBool>,
) {
    // Failures widen the sleep up to this cap (e.g. when no worktree is
    // sandboxed); a successful probe resets it to `poll`.
    let idle_backoff = poll.saturating_mul(8).max(Duration::from_secs(10));
    thegn_core::thread_name::try_spawn_named("thegn-forward", move || {
        // Background: a fixed-cadence sampler (poll + backoff) — the result lands
        // at the poll cadence regardless of scheduling, like the proc/metrics samplers.
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);

        // The worktree we're currently tracking + its last port snapshot. We
        // track ONE worktree (the loop watches only the active one) and reset
        // the snapshot whenever the target changes — including ↔ `None`. This
        // mirrors the loop, which tears a worktree's forwards down on switch:
        // re-entering an already-running dev server must re-appear as "new" so
        // it gets re-forwarded, which a stale snapshot would suppress.
        let mut tracked: Option<String> = None;
        let mut last: BTreeSet<u16> = BTreeSet::new();
        let mut runtime: Option<Vec<String>> = None;
        let mut reported: Option<String> = None;
        let mut probe_failures: u32 = 0;
        loop {
            let wt = target.lock().unwrap().clone();
            let manual_requests: Vec<_> = manual_forwards.lock().unwrap().drain(..).collect();
            if wt != tracked {
                last.clear();
                runtime = None;
                reported = None;
                probe_failures = 0;
                tracked = wt.clone();
            }
            let mut sleep = idle_backoff;
            if let Some(wt) = wt {
                // Profile-aware: containers are created as
                // `thegn-{profile}-{slug}`, so the plain name never
                // matched under `--profile` and nothing was ever detected.
                let container = thegn_core::sandbox::container_name_with_profile(
                    &wt,
                    Some(&thegn_core::profile::name()),
                );
                // On a probe error (no sandbox / stopped container) keep the
                // last snapshot for a couple of rounds — don't tear forwards
                // down on a transient blip — but after repeated failures
                // reset `last`, so a RESTARTED container's re-appearing
                // ports register as `appeared` and re-forward. (With `last`
                // held forever, a same-port restart never re-established
                // its forwards until a worktree switch away and back.)
                let mut pulsed = false;
                let manual_requests_for_wt = manual_requests
                    .iter()
                    .any(|(requested, _)| requested == &wt);
                let manual_runtime = if manual_requests_for_wt {
                    match runtime.clone() {
                        Some(prefix) => Some(prefix),
                        None => thegn_svc::forward::resolve_container_runtime(&container)
                            .ok()
                            .map(|(prefix, _)| prefix),
                    }
                } else {
                    None
                };
                if let Some(rt) = manual_runtime {
                    runtime = Some(rt.clone());
                    for event in manual_events(&wt, &rt, manual_requests) {
                        if tx.send(event).is_ok() {
                            pulsed = true;
                        }
                    }
                } else if manual_requests_for_wt {
                    if tx
                        .send(ForwardEvent::Refused {
                            worktree: wt.clone(),
                            reason: format!("manual forward: no container runtime for {container}"),
                        })
                        .is_ok()
                    {
                        pulsed = true;
                    }
                }
                if auto_probe.load(std::sync::atomic::Ordering::Relaxed) {
                    let probe = match runtime.as_deref() {
                        Some(prefix) => thegn_svc::forward::probe_ports_in(prefix, &container)
                            .map(|ports| (prefix.to_vec(), ports)),
                        None => thegn_svc::forward::resolve_container_runtime(&container),
                    };
                    match probe {
                        Err(error) => {
                            runtime = None;
                            if let Some(refusal) =
                                error.downcast_ref::<thegn_svc::forward::AmbiguousRuntime>()
                            {
                                let reason = refusal.to_string();
                                if should_report(&mut reported, &reason)
                                    && tx
                                        .send(ForwardEvent::Refused {
                                            worktree: wt.clone(),
                                            reason,
                                        })
                                        .is_ok()
                                {
                                    pulsed = true;
                                }
                            }
                            probe_failures += 1;
                            if probe_failures >= 3 && !last.is_empty() {
                                last.clear();
                            }
                        }
                        Ok((rt, now)) => {
                            runtime = Some(rt.clone());
                            reported = None;
                            probe_failures = 0;
                            sleep = poll;
                            let (appeared, disappeared) = diff_listening(&last, &now);
                            last = now;
                            for p in appeared {
                                if tx
                                    .send(ForwardEvent::Detected {
                                        worktree: wt.clone(),
                                        container_port: p,
                                        runtime: rt.clone(),
                                        manual: false,
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
                        }
                    }
                } else {
                    sleep = poll;
                }
                if pulsed {
                    let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                }
            }
            std::thread::sleep(sleep);
            // Bail if the loop dropped the receiver (shutdown).
            if tx.is_closed() {
                break;
            }
        }
    })
    .inspect_err(|e| {
        tracing::warn!(target: "thegn::forward", error = %e, "forward detector failed to spawn");
    })
    .ok(); // best-effort: spawn failure is now logged above
}

fn should_report(last: &mut Option<String>, reason: &str) -> bool {
    if last.as_deref() == Some(reason) {
        return false;
    }
    *last = Some(reason.to_string());
    true
}

fn manual_events(
    tracked: &str,
    runtime: &[String],
    requests: Vec<(String, u16)>,
) -> Vec<ForwardEvent> {
    requests
        .into_iter()
        .filter(|(worktree, _)| worktree == tracked)
        .map(|(worktree, container_port)| ForwardEvent::Detected {
            worktree,
            container_port,
            runtime: runtime.to_vec(),
            manual: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_is_reported_once_per_reason() {
        let mut reported = None;

        assert!(should_report(&mut reported, "ambiguous podman/docker"));
        assert!(!should_report(&mut reported, "ambiguous podman/docker"));
        assert!(should_report(
            &mut reported,
            "ambiguous docker/rootful podman"
        ));
    }

    #[test]
    fn parse_manual_port_validates_and_trims() {
        assert!(parse_manual_port("").is_err());
        assert!(parse_manual_port("0").is_err());
        assert!(parse_manual_port("70000").is_err());
        assert_eq!(parse_manual_port(" 8080 "), Ok(8080));
    }

    #[test]
    fn manual_events_only_target_the_tracked_worktree() {
        let events = manual_events(
            "/tracked",
            &["podman".into(), "exec".into()],
            vec![("/tracked".into(), 8080), ("/other".into(), 3000)],
        );
        assert_eq!(
            events,
            vec![ForwardEvent::Detected {
                worktree: "/tracked".into(),
                container_port: 8080,
                runtime: vec!["podman".into(), "exec".into()],
                manual: true,
            }]
        );
    }

    #[test]
    fn auto_probe_is_gated_by_the_live_setting() {
        let source = include_str!("forward.rs");
        let detector = source
            .split_once("pub fn spawn_detector(")
            .map(|(_, body)| body)
            .expect("detector source");
        assert!(detector.contains("if auto_probe.load(std::sync::atomic::Ordering::Relaxed)"));
        assert!(detector.contains("} else {\n                    sleep = poll;"));
    }

    #[test]
    fn unresolved_manual_runtime_reports_a_refusal() {
        let source = include_str!("forward.rs");
        assert!(source.contains("let manual_requests_for_wt = manual_requests"));
        assert!(source.contains("} else if manual_requests_for_wt {"));
        assert!(source.contains("manual forward: no container runtime for {container}"));
    }

    #[test]
    fn start_refuses_a_non_loopback_bind_without_opt_in() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sup = ForwardSupervisor::new();
            let cfg = ForwardConfig {
                bind: "0.0.0.0".into(),
                ..ForwardConfig::default()
            };
            let free_port = {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                listener.local_addr().unwrap().port()
            };
            let result = sup.start(&cfg, "/wt/app", free_port, &["true".into()]);
            assert!(matches!(result, Err(ref message) if message.contains("allow_external_bind")));
            assert!(sup.views("/wt/app").is_empty());
            std::net::TcpListener::bind(("127.0.0.1", free_port))
                .expect("refusal must leave the candidate host port unbound");
        });
    }

    #[test]
    fn start_normalizes_bracketed_ipv6_bind_for_socket_and_url() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sup = ForwardSupervisor::new();
            let cfg = ForwardConfig {
                bind: " [::1] ".into(),
                ..ForwardConfig::default()
            };
            let started = sup
                .start(&cfg, "/wt/app", 0, &["true".into()])
                .expect("accepted bracketed IPv6 must bind using its normalized host");
            assert!(started.url.starts_with("http://[::1]:"));
            assert!(sup.has("/wt/app", 0));
        });
    }

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
            // An ephemeral port that is free *right now*. Note what this does
            // NOT do: bind port 0, read the number, drop the listener, and then
            // assert `start()` re-binds that exact number. That was a real
            // TOCTOU — between the drop and the re-bind the port belongs to
            // whoever asks next, and `bind_host_port` silently falls through to
            // the 8000-8999 range on failure, so a theft surfaced as a confusing
            // assertion failure rather than an error. Several sibling tests in
            // this very module bind ephemeral ports concurrently.
            //
            // Instead: hold the listener while probing, release it immediately
            // before the call, and assert the SHAPE (a port was taken, nothing
            // was remapped) rather than the exact number. `remapped` is the
            // property under test; the specific integer never was.
            let free = {
                let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                l.local_addr().unwrap().port()
            };
            // `runtime` is never executed here (no connections), so any prefix works.
            let started = sup
                .start(&cfg, "/wt/app", free, &["true".to_string()])
                .expect("bind a port");
            let bound = started.host_port;
            assert_eq!(started.url, format!("http://127.0.0.1:{bound}"));
            if bound == free {
                assert!(!started.remapped, "the preferred port was honoured");
            } else {
                // Someone took `free` in the window; the fallback range is the
                // documented behaviour, so this is still a pass — but it must be
                // labelled as a remap.
                assert!(started.remapped, "a fallback port must set `remapped`");
            }
            let free = bound;

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
            // 3000/8080 are CONTAINER ports — map keys, and what `stop_all_on`
            // returns. They are not what gets bound: `start` binds a *host* port,
            // preferring the same number and falling back to `cfg.range` when it
            // is taken. So an occupied 3000 on the machine remaps the host side
            // and leaves this assertion true. Don't "fix" these to ephemeral
            // numbers — the fixed keys are the point.
            sup.start(&cfg, "/wt", 3000, &["true".into()]).unwrap();
            sup.start(&cfg, "/wt", 8080, &["true".into()]).unwrap();
            let mut ports = sup.stop_all_on("/wt");
            ports.sort_unstable();
            assert_eq!(ports, vec![3000, 8080]);
            assert!(sup.views("/wt").is_empty());
        });
    }
}
