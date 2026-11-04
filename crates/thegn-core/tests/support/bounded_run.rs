//! Bounded child-process runner for the sandbox integration tests (THE-307): a total deadline, an output
//! byte cap per stream, and a kill-then-wait so no payload outlives its test.
#![allow(dead_code, reason = "each including test binary uses a subset")]

use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Generous: sandbox payloads include container starts. A hang is the failure this guards.
pub const DEADLINE: Duration = Duration::from_secs(120);
/// Per stream (stdout, stderr).
pub const OUTPUT_CAP: u64 = 1024 * 1024;
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum Outcome {
    Exited(Output),
    TimedOut,
    OverCap,
}

#[derive(Debug)]
enum RunError {
    BeforeSpawn(std::io::Error),
    AfterSpawn(std::io::Error),
}

impl RunError {
    fn into_io_error(self) -> std::io::Error {
        match self {
            Self::BeforeSpawn(error) | Self::AfterSpawn(error) => error,
        }
    }
}

fn kill_and_wait(child: &mut std::process::Child) -> std::io::Result<()> {
    match child.kill() {
        Ok(()) => {
            child.wait()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            child.wait()?;
        }
        Err(error) => {
            child.wait()?;
            return Err(error);
        }
    }
    Ok(())
}

/// Run `command` with null stdin and both streams captured into anonymous temp files.
pub fn run(command: &mut Command, deadline: Duration, cap: u64) -> std::io::Result<Outcome> {
    run_inner(command, deadline, cap).map_err(RunError::into_io_error)
}

fn run_inner(command: &mut Command, deadline: Duration, cap: u64) -> Result<Outcome, RunError> {
    let mut out_file = tempfile::tempfile().map_err(RunError::BeforeSpawn)?;
    let mut err_file = tempfile::tempfile().map_err(RunError::BeforeSpawn)?;
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            out_file.try_clone().map_err(RunError::BeforeSpawn)?,
        ))
        .stderr(Stdio::from(
            err_file.try_clone().map_err(RunError::BeforeSpawn)?,
        ))
        .spawn()
        .map_err(RunError::BeforeSpawn)?;
    let status = loop {
        let child_status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                kill_and_wait(&mut child).map_err(RunError::AfterSpawn)?;
                return Err(RunError::AfterSpawn(error));
            }
        };
        if let Some(status) = child_status {
            break status;
        }
        let over = match (out_file.metadata(), err_file.metadata()) {
            (Ok(stdout), Ok(stderr)) => stdout.len() > cap || stderr.len() > cap,
            (Err(error), _) | (_, Err(error)) => {
                kill_and_wait(&mut child).map_err(RunError::AfterSpawn)?;
                return Err(RunError::AfterSpawn(error));
            }
        };
        if over || started.elapsed() >= deadline {
            // InvalidInput means the child already exited; other kill errors propagate.
            kill_and_wait(&mut child).map_err(RunError::AfterSpawn)?;
            return Ok(if over {
                Outcome::OverCap
            } else {
                Outcome::TimedOut
            });
        }
        std::thread::sleep(POLL);
    };
    if out_file.metadata().map_err(RunError::AfterSpawn)?.len() > cap
        || err_file.metadata().map_err(RunError::AfterSpawn)?.len() > cap
    {
        return Ok(Outcome::OverCap);
    }
    let mut stdout = Vec::new();
    out_file
        .seek(SeekFrom::Start(0))
        .map_err(RunError::AfterSpawn)?;
    (&out_file)
        .take(cap)
        .read_to_end(&mut stdout)
        .map_err(RunError::AfterSpawn)?;
    let mut stderr = Vec::new();
    err_file
        .seek(SeekFrom::Start(0))
        .map_err(RunError::AfterSpawn)?;
    (&err_file)
        .take(cap)
        .read_to_end(&mut stderr)
        .map_err(RunError::AfterSpawn)?;
    Ok(Outcome::Exited(Output {
        status,
        stdout,
        stderr,
    }))
}

/// `Command::output` with the limits above; a deadline or cap breach is a test FAILURE (panic), never a skip.
pub fn output(command: &mut Command) -> std::io::Result<Output> {
    output_result(run_inner(command, DEADLINE, OUTPUT_CAP), command)
}

fn output_result(result: Result<Outcome, RunError>, command: &Command) -> std::io::Result<Output> {
    match result {
        Err(RunError::BeforeSpawn(error)) => Err(error),
        Err(RunError::AfterSpawn(error)) => {
            panic!("sandbox payload supervision failed: {error}: {command:?}")
        }
        Ok(Outcome::Exited(output)) => Ok(output),
        Ok(Outcome::TimedOut) => {
            panic!("sandbox payload still running after {DEADLINE:?}: {command:?}")
        }
        Ok(Outcome::OverCap) => {
            panic!("sandbox payload wrote more than {OUTPUT_CAP} bytes: {command:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, RunError, output_result};
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::Command;

    #[test]
    fn output_panics_with_post_spawn_supervision_errors() {
        let command = Command::new("payload");
        let result = Err(RunError::AfterSpawn(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "kill denied",
        )));
        let panic = catch_unwind(AssertUnwindSafe(|| output_result(result, &command)));
        match panic {
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied());
                assert!(
                    message.is_some_and(|message| message.contains("kill denied")),
                    "panic must retain the post-spawn error text: {message:?}"
                );
            }
            Ok(_) => panic!("post-spawn errors must fail the test, not skip it"),
        }
    }

    #[test]
    fn output_returns_pre_spawn_errors_to_the_caller() {
        let command = Command::new("payload");
        let result = Err(RunError::BeforeSpawn(io::Error::new(
            io::ErrorKind::NotFound,
            "missing executable",
        )));
        match output_result(result, &command) {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
            Ok(output) => panic!("expected pre-spawn error, got {output:?}"),
        }
    }

    #[test]
    fn output_preserves_normal_exit_outcome_mapping() {
        let command = Command::new("payload");
        let outcome = Outcome::Exited(std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        assert!(output_result(Ok(outcome), &command).is_ok());
    }
}
