//! CLI access to the local container estate.
//!
//! Listing delegates discovery to the core's bounded runtime seam. Lifecycle
//! and logs commands build argv only after claiming an [`OwnedContainer`], and
//! execute through a bounded process-group runner so a wedged runtime cannot
//! hang the CLI forever.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::Serialize;
use thegn_core::sandbox::{self, Backend, ContainerInfo, oci_runtime_prefix};
use thegn_core::sandbox_manage::{ControlOp, OwnedContainer, mgmt_control_argv, mgmt_logs_argv};
use thegn_proc::{End, GroupState, Limits, Stream};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const LOG_TIMEOUT: Duration = Duration::from_secs(15);
const LOG_OUTPUT_CAP: usize = 256 * 1024;
const MAX_TAIL: u32 = 10_000;

/// The lifecycle verbs exposed by `sandbox ctl`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Control {
    Stop,
    Start,
    Restart,
}

impl Control {
    fn core(self) -> ControlOp {
        match self {
            Self::Stop => ControlOp::Stop,
            Self::Start => ControlOp::Start,
            Self::Restart => ControlOp::Restart,
        }
    }
}

/// Stable machine-readable row for `sandbox ls --json`. Keep this DTO
/// explicit: [`ContainerInfo`] is an internal model and may gain rendering
/// fields without changing the CLI contract.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct ContainerJson {
    name: String,
    image: String,
    status: String,
    owned: bool,
    backend: String,
    cpu: String,
    mem: String,
    net: String,
    containment: String,
    mounts: String,
}

impl From<&ContainerInfo> for ContainerJson {
    fn from(row: &ContainerInfo) -> Self {
        Self {
            name: row.name.clone(),
            image: row.image.clone(),
            status: row.status.clone(),
            owned: row.ours,
            backend: row.backend.clone(),
            cpu: row.cpu.clone(),
            mem: row.mem.clone(),
            net: row.net.clone(),
            containment: row.containment.clone(),
            mounts: row.mounts.clone(),
        }
    }
}

/// List the running local containers. No runtime is a valid empty result.
pub fn list(json: bool, stats: bool) -> Result<()> {
    let rows = list_with(stats, |with_stats| {
        if with_stats {
            sandbox::running_containers_with_stats()
        } else {
            sandbox::running_containers()
        }
    });
    if json {
        let output: Vec<_> = rows.iter().map(ContainerJson::from).collect();
        super::emit_json(&output)
    } else {
        thegn_core::out!("{}", format_listing(&rows, stats));
        Ok(())
    }
}

/// Control an owned container. The ownership claim happens before the
/// confirmation or any runtime command, so a foreign name is a hard error.
pub fn ctl(op: Control, name: &str, yes: bool) -> Result<()> {
    let plans = control_plan(op, name)?;
    if !yes && !super::confirm(&format!("{} {}?", op.core().label(), name)) {
        thegn_core::outln!("aborted");
        return Ok(());
    }
    execute_plans(
        plans,
        &format!("could not {} {name}", op.core().label()),
        |argv| run_argv(argv, CONTROL_TIMEOUT, 0),
    )?;
    thegn_core::outln!("{} {}", op.core().label(), name);
    Ok(())
}

/// Print a bounded, non-following log tail for an owned container.
pub fn logs(name: &str, tail: u32) -> Result<()> {
    if !(1..=MAX_TAIL).contains(&tail) {
        bail!("--tail must be between 1 and {MAX_TAIL}");
    }
    let plans = logs_plan(name, tail)?;
    let result = execute_plans(plans, &format!("could not read logs for {name}"), |argv| {
        run_argv(argv, LOG_TIMEOUT, LOG_OUTPUT_CAP)
    })?;
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout.write_all(&result.stdout) {
        tracing::debug!(%error, "container logs stdout closed");
    }
    if let Err(error) = stdout.flush() {
        tracing::debug!(%error, "container logs stdout flush failed");
    }
    Ok(())
}

/// The core listing seam is deliberately passed in, just like
/// `running_containers_with`: formatter and JSON tests never need an OCI
/// runtime.
fn list_with(
    with_stats: bool,
    discover: impl FnOnce(bool) -> Vec<ContainerInfo>,
) -> Vec<ContainerInfo> {
    let mut rows = discover(with_stats);
    rows.sort_by(|left, right| {
        (!left.ours, left.name.as_str(), left.backend.as_str()).cmp(&(
            !right.ours,
            right.name.as_str(),
            right.backend.as_str(),
        ))
    });
    rows
}

/// Render the human table as a pure string so its layout is regression-tested
/// without capturing process stdout.
fn format_listing(rows: &[ContainerInfo], with_stats: bool) -> String {
    let mut output = String::new();
    if with_stats {
        output.push_str("NAME\tBACKEND\tSTATUS\tIMAGE\tCPU\tMEM\tNET\n");
    } else {
        output.push_str("NAME\tBACKEND\tSTATUS\tIMAGE\n");
    }
    for row in rows {
        if with_stats {
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                row.name, row.backend, row.status, row.image, row.cpu, row.mem, row.net
            )
            .expect("writing a String cannot fail");
        } else {
            writeln!(
                output,
                "{}\t{}\t{}\t{}",
                row.name, row.backend, row.status, row.image
            )
            .expect("writing a String cannot fail");
        }
    }
    output
}

fn managed_backends() -> [Backend; 3] {
    [Backend::Podman, Backend::PodmanRootful, Backend::Docker]
}

fn control_plan(op: Control, name: &str) -> Result<Vec<(Backend, Vec<String>)>> {
    let Some(owned) = OwnedContainer::claim(name) else {
        bail!("refusing to control foreign container {name}");
    };
    let mut plans = Vec::new();
    for backend in managed_backends() {
        let Some(prefix) = oci_runtime_prefix(backend) else {
            continue;
        };
        let Some(subcommand) = mgmt_control_argv(backend, op.core(), &owned) else {
            continue;
        };
        let mut argv = prefix;
        argv.extend(subcommand);
        plans.push((backend, argv));
    }
    if plans.is_empty() {
        bail!("no local container runtime supports {}", op.core().label());
    }
    Ok(plans)
}

fn logs_plan(name: &str, tail: u32) -> Result<Vec<(Backend, Vec<String>)>> {
    let Some(owned) = OwnedContainer::claim(name) else {
        bail!("refusing to read logs from foreign container {name}");
    };
    let mut plans = Vec::new();
    for backend in managed_backends() {
        let Some(prefix) = oci_runtime_prefix(backend) else {
            continue;
        };
        let Some(subcommand) = mgmt_logs_argv(backend, &owned, tail) else {
            continue;
        };
        let mut argv = prefix;
        argv.extend(subcommand);
        plans.push((backend, argv));
    }
    if plans.is_empty() {
        bail!("no local container runtime supports logs");
    }
    Ok(plans)
}

struct CommandResult {
    success: bool,
    stdout: Vec<u8>,
    message: String,
}

/// Execute the first backend plan that succeeds. The closure is the process
/// seam: tests can provide a hermetic runner, while production uses the
/// bounded `thegn_proc` implementation below.
fn execute_plans(
    plans: Vec<(Backend, Vec<String>)>,
    label: &str,
    mut execute: impl FnMut(&[String]) -> CommandResult,
) -> Result<CommandResult> {
    let mut errors = Vec::new();
    for (backend, argv) in plans {
        let result = execute(&argv);
        if result.success {
            return Ok(result);
        }
        errors.push(format!("{}: {}", backend.label(), result.message));
    }
    bail!("{label}: {}", errors.join("; "))
}

fn run_argv(argv: &[String], timeout: Duration, output_cap: usize) -> CommandResult {
    let Some((program, args)) = argv.split_first() else {
        return CommandResult {
            success: false,
            stdout: Vec::new(),
            message: "empty command".into(),
        };
    };
    let mut command = std::process::Command::new(program);
    command.args(args);
    let stream = if output_cap == 0 {
        Stream::Null
    } else {
        Stream::Tail { cap: output_cap }
    };
    let limits = Limits {
        stdout: stream,
        stderr: if output_cap == 0 {
            Stream::Null
        } else {
            Stream::Tail { cap: output_cap }
        },
        kill_settle: Duration::from_millis(250),
        ..Limits::silent(timeout)
    };
    let run = thegn_proc::run(&mut command, &limits, &AtomicBool::new(false));
    let success = run.success() && run.group == GroupState::Drained;
    let message = match run.end {
        End::Spawn(error) => format!("could not start: {error}"),
        End::TimedOut => "timed out".into(),
        _ if success => String::new(),
        _ => "runtime command failed".into(),
    };
    CommandResult {
        success,
        stdout: run.stdout.bytes,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn row(name: &str, ours: bool) -> ContainerInfo {
        ContainerInfo {
            name: name.into(),
            image: "alpine:latest".into(),
            status: "Up".into(),
            ours,
            backend: "docker".into(),
            cpu: "1%".into(),
            mem: "2MiB".into(),
            net: "3kB / 4kB".into(),
            containment: "worktree+caches".into(),
            mounts: String::new(),
        }
    }

    #[test]
    fn formatter_sorts_owned_rows_first() {
        let rows = list_with(false, |_| {
            vec![row("foreign", false), row("thegn-wt", true)]
        });
        let rendered = format_listing(&rows, false);
        assert!(rendered.find("thegn-wt").unwrap() < rendered.find("foreign").unwrap());
    }

    #[test]
    fn json_formatter_has_stable_fields_and_stats() {
        let json = ContainerJson::from(&row("thegn-wt", true));
        assert_eq!(json.name, "thegn-wt");
        assert_eq!(json.image, "alpine:latest");
        assert_eq!(json.status, "Up");
        assert!(json.owned);
        assert_eq!(json.backend, "docker");
        assert_eq!(json.cpu, "1%");
        assert_eq!(json.mem, "2MiB");
        assert_eq!(json.net, "3kB / 4kB");
        assert_eq!(json.containment, "worktree+caches");
        assert!(json.mounts.is_empty());
    }

    #[test]
    fn clap_parses_container_commands() {
        assert!(
            crate::Cli::try_parse_from(["thegn", "sandbox", "ls", "--json", "--stats"]).is_ok()
        );
        assert!(
            crate::Cli::try_parse_from(["thegn", "sandbox", "ctl", "restart", "thegn-wt", "--yes"])
                .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from(["thegn", "sandbox", "logs", "thegn-wt", "--tail", "25"])
                .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from(["thegn", "sandbox", "logs", "thegn-wt", "--tail", "0"])
                .is_err()
        );
    }

    #[test]
    fn foreign_container_is_rejected_before_control_argv() {
        let error = control_plan(Control::Stop, "postgres")
            .unwrap_err()
            .to_string();
        assert!(error.contains("foreign container"));
    }

    #[test]
    fn logs_argv_is_bounded_and_non_following() {
        let plans = logs_plan("thegn-wt", 25).unwrap();
        assert!(plans.iter().all(|(_, argv)| !argv.contains(&"-f".into())));
        assert!(plans.iter().all(|(_, argv)| argv.contains(&"25".into())));
    }

    #[test]
    fn execution_uses_a_hermetic_runner_seam() {
        let plans = control_plan(Control::Stop, "thegn-wt").unwrap();
        let mut seen = Vec::new();
        let result = execute_plans(plans, "control failed", |argv| {
            seen.push(argv.to_vec());
            CommandResult {
                success: true,
                stdout: Vec::new(),
                message: String::new(),
            }
        })
        .unwrap();
        assert!(result.success);
        assert_eq!(seen.len(), 1);
    }
}
