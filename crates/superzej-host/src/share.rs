//! In-process supervisor for ingress shares (`[share]`).
//!
//! The inbound sibling of [`crate::proxy_daemon`]: it manages per-worktree
//! `bore` (or future) tunnel-client children as **background subprocesses** (not
//! PTY panes). Each share runs on its own OS thread that drives
//! [`superzej_svc::share`] — spawn the client, wait for its URL, then block on
//! the child — and reports state back over a tokio mpsc channel, pulsing the
//! `TerminalWaker` exactly as the config/LSP/refresh producers do.
//!
//! The supervisor itself is pure bookkeeping (no DB, no rendering); the event
//! loop persists rows and re-syncs the [`crate::chrome::FrameModel`] from
//! [`ShareSupervisor::views`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use superzej_core::config::ShareConfig;
use superzej_core::share::build_share_spec;
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

/// A status update from a share's supervisor thread back to the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareEvent {
    /// The tunnel reported its public URL.
    Up {
        worktree: String,
        port: u16,
        url: String,
    },
    /// The tunnel client exited (clean stop, crash, or `stop()` kill).
    Down { worktree: String, port: u16 },
    /// Bring-up failed (no URL within the timeout, spawn error, …).
    Failed {
        worktree: String,
        port: u16,
        error: String,
    },
}

/// The lifecycle state of one share, as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareState {
    Starting,
    Up(String),
    Failed(String),
}

/// A small render-facing snapshot of one share (mirrored into `FrameModel`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShareView {
    pub worktree: String,
    pub port: u16,
    pub url: Option<String>,
    pub failed: bool,
}

/// Cross-thread handle to one running child: lets [`ShareSupervisor::stop`] /
/// shutdown signal and SIGTERM a child the supervisor thread is `wait`ing on.
struct Shared {
    shutdown: AtomicBool,
    pid: Mutex<Option<u32>>,
}

struct ShareInstance {
    worktree: String,
    port: u16,
    state: ShareState,
    shared: Arc<Shared>,
}

impl ShareInstance {
    fn kill(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.shared.pid.lock().unwrap() {
            // SAFETY: signalling a pid we spawned; harmless if already reaped.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

/// Tracks the live ingress shares. An event-loop local, mirroring
/// [`crate::pins::PinSupervisor`].
#[derive(Default)]
pub struct ShareSupervisor {
    instances: Vec<ShareInstance>,
}

impl ShareSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a share already exists for `(worktree, port)`.
    pub fn has(&self, worktree: &str, port: u16) -> bool {
        self.instances
            .iter()
            .any(|i| i.worktree == worktree && i.port == port)
    }

    /// Render-facing snapshot of all shares for `worktree` (badge + panel).
    pub fn views(&self, worktree: &str) -> Vec<ShareView> {
        self.instances
            .iter()
            .filter(|i| i.worktree == worktree)
            .map(|i| ShareView {
                worktree: i.worktree.clone(),
                port: i.port,
                url: match &i.state {
                    ShareState::Up(u) => Some(u.clone()),
                    _ => None,
                },
                failed: matches!(i.state, ShareState::Failed(_)),
            })
            .collect()
    }

    /// Start a share for `port` on `worktree`. Returns a human error if sharing
    /// is disabled or a share already runs for this `(worktree, port)`.
    pub fn start(
        &mut self,
        cfg: &ShareConfig,
        worktree: &str,
        port: u16,
        tx: &UnboundedSender<ShareEvent>,
        waker: &TerminalWaker,
    ) -> Result<(), String> {
        if self.has(worktree, port) {
            return Err(format!("already sharing port {port}"));
        }
        let Some(spec) = build_share_spec(cfg, port) else {
            return Err("sharing is disabled (set [share] provider = \"bore\")".into());
        };
        let shared = Arc::new(Shared {
            shutdown: AtomicBool::new(false),
            pid: Mutex::new(None),
        });
        let t_shared = shared.clone();
        let t_tx = tx.clone();
        let t_waker = waker.clone();
        let wt = worktree.to_string();
        std::thread::Builder::new()
            .name("szshare".into())
            .spawn(move || supervise(spec, wt, port, t_tx, t_waker, t_shared))
            .map_err(|e| format!("could not spawn share supervisor: {e}"))?;
        self.instances.push(ShareInstance {
            worktree: worktree.to_string(),
            port,
            state: ShareState::Starting,
            shared,
        });
        Ok(())
    }

    /// Apply an event from a supervisor thread. Returns `true` if UI state
    /// changed (so the loop can flip the chrome `dirty` flag).
    pub fn on_event(&mut self, ev: ShareEvent) -> bool {
        match ev {
            ShareEvent::Up {
                worktree,
                port,
                url,
            } => match self.find_mut(&worktree, port) {
                Some(i) => {
                    i.state = ShareState::Up(url);
                    true
                }
                None => false,
            },
            ShareEvent::Failed {
                worktree,
                port,
                error,
            } => match self.find_mut(&worktree, port) {
                Some(i) => {
                    i.state = ShareState::Failed(error);
                    true
                }
                None => false,
            },
            ShareEvent::Down { worktree, port } => {
                let before = self.instances.len();
                self.instances
                    .retain(|i| !(i.worktree == worktree && i.port == port));
                before != self.instances.len()
            }
        }
    }

    /// Stop shares on `worktree`: a specific `port`, or all (`None`). Returns the
    /// number stopped.
    pub fn stop(&mut self, worktree: &str, port: Option<u16>) -> usize {
        let mut n = 0;
        self.instances.retain(|i| {
            let matched = i.worktree == worktree && port.is_none_or(|p| p == i.port);
            if matched {
                i.kill();
                n += 1;
            }
            !matched
        });
        n
    }

    /// SIGTERM every child (teardown on quit).
    pub fn shutdown_all(&mut self) {
        for i in &self.instances {
            i.kill();
        }
        self.instances.clear();
    }

    fn find_mut(&mut self, worktree: &str, port: u16) -> Option<&mut ShareInstance> {
        self.instances
            .iter_mut()
            .find(|i| i.worktree == worktree && i.port == port)
    }
}

/// The per-share supervisor thread: build the plan, bring the tunnel up, report
/// its URL, then block on the child and report when it goes down.
fn supervise(
    spec: superzej_core::share::ShareSpec,
    worktree: String,
    port: u16,
    tx: UnboundedSender<ShareEvent>,
    waker: TerminalWaker,
    shared: Arc<Shared>,
) {
    use superzej_svc::share::{self, ShareProvider};

    let emit = |ev: ShareEvent| {
        let _ = tx.send(ev);
        let _ = waker.wake();
    };

    let plan = match share::for_provider(&spec).plan() {
        Ok(p) => p,
        Err(e) => {
            emit(ShareEvent::Failed {
                worktree,
                port,
                error: e.to_string(),
            });
            return;
        }
    };

    match share::start(&plan, spec.ready_timeout) {
        Ok(running) => {
            *shared.pid.lock().unwrap() = Some(running.child.id());
            // Raced with a stop() before the URL arrived: tear down now.
            if shared.shutdown.load(Ordering::SeqCst) {
                running.stop();
                emit(ShareEvent::Down { worktree, port });
                return;
            }
            emit(ShareEvent::Up {
                worktree: worktree.clone(),
                port,
                url: running.public_url.clone(),
            });
            let share::RunningShare { mut child, .. } = running;
            let _ = child.wait();
            *shared.pid.lock().unwrap() = None;
            emit(ShareEvent::Down { worktree, port });
        }
        Err(e) => emit(ShareEvent::Failed {
            worktree,
            port,
            error: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_event_transitions_and_views() {
        let mut sup = ShareSupervisor::new();
        // Seed an instance without spawning a thread (test the bookkeeping).
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 3000,
            state: ShareState::Starting,
            shared: Arc::new(Shared {
                shutdown: AtomicBool::new(false),
                pid: Mutex::new(None),
            }),
        });
        assert!(sup.views("/wt")[0].url.is_none());
        assert!(sup.on_event(ShareEvent::Up {
            worktree: "/wt".into(),
            port: 3000,
            url: "http://bore.pub:1".into(),
        }));
        let v = sup.views("/wt");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].port, 3000);
        assert_eq!(v[0].url.as_deref(), Some("http://bore.pub:1"));
        assert!(!v[0].failed);

        // An event for an unknown share is a no-op.
        assert!(!sup.on_event(ShareEvent::Up {
            worktree: "/other".into(),
            port: 9,
            url: "x".into(),
        }));

        // Down removes it.
        assert!(sup.on_event(ShareEvent::Down {
            worktree: "/wt".into(),
            port: 3000,
        }));
        assert!(sup.views("/wt").is_empty());
    }

    #[test]
    fn failed_event_marks_view() {
        let mut sup = ShareSupervisor::new();
        sup.instances.push(ShareInstance {
            worktree: "/wt".into(),
            port: 8080,
            state: ShareState::Starting,
            shared: Arc::new(Shared {
                shutdown: AtomicBool::new(false),
                pid: Mutex::new(None),
            }),
        });
        assert!(sup.on_event(ShareEvent::Failed {
            worktree: "/wt".into(),
            port: 8080,
            error: "boom".into(),
        }));
        let v = sup.views("/wt");
        assert!(v[0].failed);
        assert!(v[0].url.is_none());
    }
}
