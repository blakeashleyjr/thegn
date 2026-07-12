//! Headless supervision of the `tgproxy` LLM-proxy daemon.
//!
//! When `[llm_proxy].enabled` is set, the host launches `tgproxy` as a background
//! child (no PTY pane) and keeps it alive: a dedicated OS thread `wait()`s on the
//! process and respawns it on unexpected exit, with a shutdown flag to stop the
//! respawn loop. The thread blocks in `wait()`, so the event loop's ~0% idle-CPU
//! invariant is untouched.
//!
//! Teardown: the host normally exits via `std::process::exit`, which kills the
//! whole process group (the child dies with it). [`ProxyHandle`]'s `Drop` is a
//! belt-and-suspenders clean stop for the graceful-return path — it sets the
//! shutdown flag and sends `SIGTERM` to the running child.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Shared {
    shutdown: AtomicBool,
    /// PID of the currently-running child (for `SIGTERM` on shutdown).
    pid: Mutex<Option<u32>>,
}

/// Keeps the supervised daemon alive for the lifetime of the handle.
pub struct ProxyHandle {
    shared: Arc<Shared>,
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.shared.pid.lock().unwrap() {
            crate::platform::terminate_pid(pid);
        }
    }
}

/// Launches the daemon from the resolved config (no-op unless
/// `[llm_proxy].enabled`), diagnosing the common silent-failure where thegn
/// launches ITS OWN proxy and routes agents to it, but no routes are configured
/// — the proxy then 404s every agent request. (When `route_agent` points at an
/// EXTERNAL proxy — `enabled = false` — routes live there, so no warning.)
pub fn launch_from_config(cfg: &thegn_core::config::Config) -> Option<ProxyHandle> {
    if cfg.llm_proxy.enabled
        && cfg.llm_proxy.route_agent
        && cfg.llm_proxy.config_path.trim().is_empty()
    {
        tracing::warn!(
            target: "thegn::startup",
            "[llm_proxy] enabled + route_agent but config_path is empty — the proxy will \
             answer /health but 404 agent requests until you point config_path at a routes file."
        );
    }
    cfg.llm_proxy.launch_spec().and_then(launch)
}

/// Launches and supervises `tgproxy` from a `(program, args, env)` launch spec
/// (as produced by `LlmProxyConfig::launch_spec`). Returns `None` if the
/// supervisor thread can't be spawned.
pub fn launch(spec: (String, Vec<String>, BTreeMap<String, String>)) -> Option<ProxyHandle> {
    let (program, args, env) = spec;
    let bin = resolve_binary(&program);
    let shared = Arc::new(Shared {
        shutdown: AtomicBool::new(false),
        pid: Mutex::new(None),
    });
    let thread_shared = shared.clone();

    let spawned = std::thread::Builder::new()
        .name("tgproxy-supervisor".into())
        .spawn(move || supervise(bin, args, env, thread_shared));

    match spawned {
        Ok(_) => {
            tracing::info!(target: "thegn::startup", "tgproxy daemon launched");
            Some(ProxyHandle { shared })
        }
        Err(e) => {
            tracing::warn!("could not spawn tgproxy supervisor: {e}");
            None
        }
    }
}

/// The supervisor loop: spawn → wait → respawn (with backoff) until shutdown.
// off-loop: runs on its own supervisor std::thread (spawned in launch above).
#[expect(clippy::disallowed_methods)]
fn supervise(bin: PathBuf, args: Vec<String>, env: BTreeMap<String, String>, shared: Arc<Shared>) {
    let backoff = Duration::from_millis(500);
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let mut cmd = Command::new(&bin);
        cmd.args(&args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("tgproxy spawn failed ({}): {e}", bin.display());
                std::thread::sleep(backoff);
                continue;
            }
        };
        *shared.pid.lock().unwrap() = Some(child.id());
        let status = child.wait();
        *shared.pid.lock().unwrap() = None;
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }
        tracing::warn!("tgproxy exited ({status:?}) — respawning");
        std::thread::sleep(backoff);
    }
}

/// Resolves the daemon binary: prefer a sibling of the host binary (tgproxy
/// ships next to thegn in the same bin / Nix-store dir), else fall back to the
/// bare name so `PATH` resolves it.
pub(crate) fn resolve_binary(program: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(program);
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from(program)
}
