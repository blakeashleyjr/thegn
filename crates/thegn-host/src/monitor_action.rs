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
    stages: Vec<crate::monitor_pipeline::StageMeta>,
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
        let roster = crate::monitor_pipeline::DispatchRoster { rows, stages };
        if tx
            .send(crate::hydrate::RefreshKind::Dispatches(Box::new(roster)))
            .is_ok()
        {
            let _ = waker.wake();
        }
    });
}

/// Where an `Enter` on a board row should land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineLanding {
    /// A sidebar row already targets it — the existing door, unchanged, so the
    /// board can never drift from sidebar navigation.
    Row(crate::sidebar::RowTarget),
    /// Registered in the DB but not resident in this session (a dispatch made
    /// by another process onto a worktree this session never opened). Open it
    /// as a group: its tab is a `CenterTree::Leaf(0)` missing leaf, so the lazy
    /// materialize path spawns the pane — the same route a freshly created
    /// worktree takes.
    Open { tab_name: String, path: String },
    /// Nothing known — deleted under the board, or never registered.
    None,
}

/// Resolve a Pipeline-row jump into somewhere to land.
///
/// The sidebar's own rows are tried FIRST, as the routing table — the
/// `handlers::attention::next_target` precedent — so the board lands a worktree
/// exactly where Enter on its sidebar row would, including the cross-workspace
/// case.
///
/// That alone was not enough. `gather_groups` only synthesises sidebar rows
/// from the DB for a **dormant** workspace (`sidebar.rs`'s `if !live` guard), so
/// a worktree of the CURRENT workspace with no resident group has no row and no
/// target — which is precisely the agent-supervision case, a dispatch made by
/// another process onto a worktree this session never opened. Those resolve to
/// [`PipelineLanding::Open`] out of `model.sidebar_db_worktrees`, the registered
/// list, and the loop materialises the group.
///
/// Pure over the model, so both arms are unit-testable. [`PipelineLanding::None`]
/// only when neither source knows the path; the caller says so rather than
/// doing nothing silently.
///
/// Pane-level focus (jumping to the *session* running the stage, not just its
/// worktree) is phase 2: [`PipelineJump::session`] is carried for it and
/// deliberately unused here.
pub fn pipeline_landing(jump: &PipelineJump, model: &crate::chrome::FrameModel) -> PipelineLanding {
    let row = model
        .sidebar_rows
        .iter()
        .find(|r| {
            r.kind == crate::sidebar::RowKind::Worktree
                && r.worktree_path.as_deref() == Some(jump.worktree.as_str())
                && r.tab_target.is_some()
        })
        .and_then(|r| r.tab_target.clone());
    if let Some(target) = row {
        return PipelineLanding::Row(target);
    }
    match model
        .sidebar_db_worktrees
        .iter()
        .find(|w| w.path == jump.worktree)
    {
        Some(w) => PipelineLanding::Open {
            tab_name: w.tab_name.clone(),
            path: w.path.clone(),
        },
        None => PipelineLanding::None,
    }
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

    /// A registered-but-not-resident worktree, as `sidebar_db_worktrees` carries
    /// it.
    fn db_wt(path: &str, tab_name: &str) -> crate::sidebar::DbWorktree {
        crate::sidebar::DbWorktree {
            slug: "app".into(),
            branch: "b".into(),
            repo_path: "/repo".into(),
            tab_name: tab_name.into(),
            path: path.into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        }
    }

    #[test]
    fn a_resident_worktree_still_resolves_to_its_sidebar_row() {
        let mut model = FrameModel::default();
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: Some(RowTarget::Tab(2, 1)),
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        assert_eq!(
            pipeline_landing(&jump("/wt/a"), &model),
            PipelineLanding::Row(RowTarget::Tab(2, 1))
        );
    }

    #[test]
    fn a_registered_but_unopened_worktree_resolves_to_open() {
        // The agent-supervision case: another process dispatched onto a
        // worktree of the CURRENT workspace that this session never opened, so
        // `gather_groups` synthesised no row for it.
        let mut model = FrameModel::default();
        // A targetless row for the right path must not answer either, so put a
        // decoy alongside.
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: None,
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        model.sidebar_db_worktrees.push(db_wt("/wt/a", "app/a"));
        assert_eq!(
            pipeline_landing(&jump("/wt/a"), &model),
            PipelineLanding::Open {
                tab_name: "app/a".into(),
                path: "/wt/a".into(),
            }
        );
    }

    #[test]
    fn an_unknown_worktree_resolves_to_none() {
        let mut model = FrameModel::default();
        // Right target, wrong kind — a workspace row must not answer for a
        // worktree jump.
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/b".into()),
            tab_target: Some(RowTarget::Tab(0, 0)),
            ..SidebarRow::base(RowKind::Workspace, 0, "b", "app")
        });
        model.sidebar_db_worktrees.push(db_wt("/wt/c", "app/c"));
        assert_eq!(
            pipeline_landing(&jump("/wt/b"), &model),
            PipelineLanding::None
        );
        assert_eq!(
            pipeline_landing(&jump("/wt/zz"), &model),
            PipelineLanding::None
        );
    }

    #[test]
    fn a_sidebar_row_wins_over_the_db_row() {
        // Both sources know the path: the existing door keeps precedence, so a
        // resident group is switched to rather than added a second time.
        let mut model = FrameModel::default();
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: Some(RowTarget::Tab(2, 1)),
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        model.sidebar_db_worktrees.push(db_wt("/wt/a", "app/a"));
        assert_eq!(
            pipeline_landing(&jump("/wt/a"), &model),
            PipelineLanding::Row(RowTarget::Tab(2, 1))
        );
    }
}
