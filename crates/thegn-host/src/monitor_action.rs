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
use std::time::Duration;

use crate::actions::open_command_pane;
use crate::compositor::Rect;
use crate::monitor::{ContainerReqKind, ContainerRequest};
use crate::panes::Panes;
use crate::session::Session;
use termwiz::terminal::TerminalWaker;
use thegn_core::config::Config;
use thegn_core::sandbox::{Backend, oci_runtime_prefix};
use thegn_core::sandbox_manage::{ControlOp, OwnedContainer, mgmt_control_argv};
use thegn_proc::{End, GroupState, Leader, Limits};

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
    thegn_core::thread_name::spawn_named("thegn-mon-ctl", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        let outcome = run_control(&argv, CONTROL_TIMEOUT - Duration::from_millis(250), &label);
        if !outcome.ok {
            tracing::warn!(target: "thegn::sandbox", "container action: {}", outcome.message);
        }
        if let Ok(mut g) = results().lock() {
            g.push(outcome);
        }
        // Wake the loop to drain the queue and (in ~5s) re-list the containers.
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

fn describe(run: &thegn_proc::Run, label: &str) -> ActionOutcome {
    let message = match &run.end {
        End::Spawn(error) => format!("{label} could not start: {error}"),
        End::TimedOut => format!("{label} timed out"),
        End::LeaderExited if run.group == GroupState::Survivors => {
            format!("{label} did not settle")
        }
        End::LeaderExited if run.success() && run.group != GroupState::Survivors => {
            format!("{label} ✓")
        }
        End::LeaderExited if matches!(&run.leader, Leader::Reaped(_)) => {
            format!("{label} failed")
        }
        _ => format!("{label} did not settle"),
    };
    ActionOutcome {
        ok: run.success() && run.group != GroupState::Survivors,
        message,
    }
}

fn run_control(argv: &[String], timeout: Duration, label: &str) -> ActionOutcome {
    let Some((bin, rest)) = argv.split_first() else {
        return ActionOutcome {
            ok: false,
            message: format!("{label} could not start: empty command"),
        };
    };
    let mut command = std::process::Command::new(bin);
    command.args(rest);
    let limits = Limits {
        kill_settle: Duration::from_millis(250),
        ..Limits::silent(timeout)
    };
    let run = thegn_proc::run(
        &mut command,
        &limits,
        &std::sync::atomic::AtomicBool::new(false),
    );
    describe(&run, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn missing_runtime_reports_could_not_start_not_timeout() {
        let result = run_control(
            &["/no/such/runtime".into()],
            Duration::from_secs(1),
            "stop x",
        );
        assert!(!result.ok);
        assert!(result.message.contains("could not start"));
        assert!(!result.message.contains("timed out"));
    }

    #[test]
    fn control_reports_exit_and_timeout_outcomes() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let failed = run_control(
            &["/bin/sh".into(), "-c".into(), "exit 2".into()],
            Duration::from_secs(1),
            "stop x",
        );
        assert!(!failed.ok);
        assert!(failed.message.contains("failed"));

        let timed_out = run_control(
            &["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            Duration::from_millis(200),
            "stop x",
        );
        assert!(!timed_out.ok);
        assert!(timed_out.message.contains("timed out"));

        let succeeded = run_control(
            &["/bin/sh".into(), "-c".into(), "exit 0".into()],
            Duration::from_secs(1),
            "stop x",
        );
        assert!(succeeded.ok);
    }
}
