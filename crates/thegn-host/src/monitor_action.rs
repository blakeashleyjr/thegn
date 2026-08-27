//! Dispatch for Containers-tab row actions (the monitor overlay can't reach the
//! session/panes or spawn a subprocess itself, so it hands the loop a
//! [`ContainerRequest`] and this module carries it out).
//!
//! Lifecycle ops (stop/restart/remove) run on a background thread with a bounded
//! wait — never on the event loop — and report their outcome through a process
//! queue the loop drains into the monitor footer (`drain_results`). Shell-in and
//! logs open a pane via the existing `open_command_pane` path.
//!
//! Ownership is re-checked at the seam: the argv is built only from an
//! [`OwnedContainer`] witness, so a
//! request naming a container thegn does not own produces no command at all.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::actions::open_command_pane;
use crate::compositor::Rect;
use crate::monitor::{ContainerReqKind, ContainerRequest, PipelineJump};
use crate::panes::Panes;
use crate::session::Session;
use termwiz::terminal::TerminalWaker;
use thegn_core::config::Config;
use thegn_core::sandbox::{Backend, oci_runtime_prefix};
use thegn_core::sandbox_manage::{ControlOp, OwnedContainer, mgmt_control_argv};

/// Bounded wait for a lifecycle subprocess. A wedged runtime must not pin the
/// worker thread forever.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
/// Log tail depth for the logs pane.
const LOG_TAIL: u32 = 500;

/// The outcome of a completed lifecycle op, queued for the loop to surface.
#[derive(Debug, Clone)]
pub struct ActionOutcome {
    pub ok: bool,
    pub message: String,
}

fn results() -> &'static Mutex<Vec<ActionOutcome>> {
    static Q: OnceLock<Mutex<Vec<ActionOutcome>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drain the completed-action outcomes for the loop to show (empty = nothing).
pub fn drain_results() -> Vec<ActionOutcome> {
    results()
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// Map a `ContainerInfo.backend` label back to a runtime backend.
fn backend_of(label: &str) -> Option<Backend> {
    match label {
        "docker" => Some(Backend::Docker),
        "podman" => Some(Backend::Podman),
        "podman-rootful" => Some(Backend::PodmanRootful),
        "smolmachines" | "smol" => Some(Backend::Smol),
        "apple" => Some(Backend::Apple),
        _ => None,
    }
}

/// Carry out a row action. Returns an immediate notice for the monitor footer
/// (the confirmation that the request was accepted); lifecycle results arrive
/// later via [`drain_results`].
/// The result of a dispatch: a footer notice, and whether a pane was opened (so
/// the loop can relayout + focus the center).
pub struct Dispatched {
    pub notice: String,
    pub opened_pane: bool,
}

pub fn dispatch(
    req: ContainerRequest,
    cfg: &Config,
    session: &mut Session,
    panes: &mut Panes,
    focused: u32,
    center: Rect,
    waker: &TerminalWaker,
) -> Dispatched {
    let notice = |s: String| Dispatched {
        notice: s,
        opened_pane: false,
    };
    // Structural ownership: a foreign name yields no witness, so nothing runs.
    let Some(owned) = OwnedContainer::claim(&req.name) else {
        return notice(format!("{} is not a thegn-owned container", req.name));
    };
    let Some(backend) = backend_of(&req.backend) else {
        return notice(format!("no management verbs for backend {}", req.backend));
    };
    // OCI backends only (docker/podman) carry management verbs; the prefix is
    // their invocation (`podman`, `sudo -n podman`, `docker`).
    let Some(prefix) = oci_runtime_prefix(backend) else {
        return notice(format!("no management verbs for backend {}", req.backend));
    };

    match req.kind {
        ContainerReqKind::Stop | ContainerReqKind::Restart | ContainerReqKind::Remove => {
            let op = match req.kind {
                ContainerReqKind::Stop => ControlOp::Stop,
                ContainerReqKind::Restart => ControlOp::Restart,
                _ => ControlOp::Remove,
            };
            let Some(sub) = mgmt_control_argv(backend, op, &owned) else {
                return notice(format!("{} does not support {}", req.backend, op.label()));
            };
            let mut argv = prefix;
            argv.extend(sub);
            let label = format!("{} {}", op.label(), owned.name());
            spawn_control(argv, label.clone(), waker.clone());
            notice(format!("{label}…"))
        }
        ContainerReqKind::Logs => {
            // A follow-tail into a pane (the log viewer path). `-f` on top of the
            // bounded tail so the pane keeps streaming.
            let mut argv = prefix;
            argv.extend([
                "logs".into(),
                "-f".into(),
                "--tail".into(),
                LOG_TAIL.to_string(),
                owned.name().to_string(),
            ]);
            let cmd = argv.join(" ");
            open_command_pane(session, panes, focused, &cmd, active_cwd(cfg), center);
            Dispatched {
                notice: format!("logs: {}", owned.name()),
                opened_pane: true,
            }
        }
        ContainerReqKind::Shell => {
            let mut argv = prefix;
            argv.extend([
                "exec".into(),
                "-it".into(),
                owned.name().to_string(),
                "/bin/sh".into(),
            ]);
            let cmd = argv.join(" ");
            open_command_pane(session, panes, focused, &cmd, active_cwd(cfg), center);
            Dispatched {
                notice: format!("shell: {}", owned.name()),
                opened_pane: true,
            }
        }
    }
}

/// A pane opened for shell-in/logs starts at the worktrees dir — a neutral,
/// always-present cwd (the command targets a container, not a path).
fn active_cwd(_cfg: &Config) -> Option<&std::path::Path> {
    None
}

/// Run a lifecycle argv on a background thread with a bounded wait, and queue
/// the outcome for the loop.
fn spawn_control(argv: Vec<String>, label: String, waker: TerminalWaker) {
    std::thread::spawn(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        let outcome = run_bounded(&argv, CONTROL_TIMEOUT)
            .map(|ok| {
                if ok {
                    ActionOutcome {
                        ok: true,
                        message: format!("{label} ✓"),
                    }
                } else {
                    ActionOutcome {
                        ok: false,
                        message: format!("{label} failed"),
                    }
                }
            })
            .unwrap_or_else(|| ActionOutcome {
                ok: false,
                message: format!("{label} timed out"),
            });
        if !outcome.ok {
            tracing::warn!(target: "thegn::sandbox", "container action: {}", outcome.message);
        }
        if let Ok(mut g) = results().lock() {
            g.push(outcome);
        }
        // Wake the loop to drain the queue and (in ~5s) re-list the containers.
        let _ = waker.wake();
    });
}

/// Spawn `argv`, wait up to `timeout`, kill on deadline. `Some(success)` on a
/// clean exit; `None` when it had to be killed.
fn run_bounded(argv: &[String], timeout: Duration) -> Option<bool> {
    let (bin, rest) = argv.split_first()?;
    let mut child = std::process::Command::new(bin)
        .args(rest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                // Reap off-thread so a wedged runtime can't block this worker.
                std::thread::spawn(move || {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "off-loop reap of a killed child on its own thread; never the event loop"
                    )]
                    let _ = child.wait();
                });
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

/// Sample the agent-dispatch roster off the loop and deliver it as
/// [`crate::hydrate::RefreshKind::Dispatches`].
///
/// Off-thread because `Db` is not `Send` and a table read is I/O — neither
/// belongs on the event loop. **Adds no wake source**: it is a one-shot task
/// that pulses the existing `TerminalWaker` once and exits, so the 0%-idle
/// contract is untouched whether the board is open or shut. Background QoS —
/// a board refresh is housekeeping, not the interactive path.
pub fn spawn_dispatch_sample(
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &TerminalWaker,
    stage_order: Vec<String>,
) {
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        use thegn_core::store::NotificationStore;
        // best-effort: the roster is a cache-side ledger and the board is a
        // view of it — an unavailable DB means "no update", never a crash.
        let rows = thegn_core::db::Db::open()
            .ok()
            .and_then(|db| db.list_dispatches().ok())
            .unwrap_or_default();
        let roster = crate::monitor_pipeline::DispatchRoster { rows, stage_order };
        if tx
            .send(crate::hydrate::RefreshKind::Dispatches(Box::new(roster)))
            .is_ok()
        {
            let _ = waker.wake();
        }
    });
}

/// Resolve a Pipeline-row jump into a sidebar row target.
///
/// Reuses the sidebar's own rows as the routing table — the
/// `handlers::attention::next_target` precedent — so the board lands a worktree
/// exactly where Enter on its sidebar row would, including the cross-workspace
/// case. `None` when the worktree has no row to land on (deleted under the
/// board, or belonging to a workspace this instance has never opened); the
/// caller says so rather than doing nothing silently.
///
/// Pane-level focus (jumping to the *session* running the stage, not just its
/// worktree) is phase 2: [`PipelineJump::session`] is carried for it and
/// deliberately unused here.
pub fn pipeline_target(
    jump: &PipelineJump,
    model: &crate::chrome::FrameModel,
) -> Option<crate::sidebar::RowTarget> {
    model
        .sidebar_rows
        .iter()
        .find(|r| {
            r.kind == crate::sidebar::RowKind::Worktree
                && r.worktree_path.as_deref() == Some(jump.worktree.as_str())
                && r.tab_target.is_some()
        })
        .and_then(|r| r.tab_target.clone())
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::sidebar::{RowKind, RowTarget, SidebarRow};

    fn jump(path: &str) -> PipelineJump {
        PipelineJump {
            worktree: path.into(),
            session: Some("s-1".into()),
        }
    }

    #[test]
    fn resolves_a_worktree_row_to_its_tab_target() {
        let mut model = FrameModel::default();
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: Some(RowTarget::Tab(2, 1)),
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        assert_eq!(
            pipeline_target(&jump("/wt/a"), &model),
            Some(RowTarget::Tab(2, 1))
        );
    }

    #[test]
    fn an_unknown_or_targetless_worktree_resolves_to_nothing() {
        let mut model = FrameModel::default();
        // Right path, but no target to land on (a collapsed-parent placeholder).
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: None,
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        // Right target, wrong kind — a workspace row must not answer for a
        // worktree jump.
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/b".into()),
            tab_target: Some(RowTarget::Tab(0, 0)),
            ..SidebarRow::base(RowKind::Workspace, 0, "b", "app")
        });
        assert_eq!(pipeline_target(&jump("/wt/a"), &model), None);
        assert_eq!(pipeline_target(&jump("/wt/b"), &model), None);
        assert_eq!(pipeline_target(&jump("/wt/zz"), &model), None);
    }
}
