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
    let mut out_file = tempfile::tempfile()?;
    let mut err_file = tempfile::tempfile()?;
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file.try_clone()?))
        .stderr(Stdio::from(err_file.try_clone()?))
        .spawn()?;
    let status = loop {
        let child_status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                kill_and_wait(&mut child)?;
                return Err(error);
            }
        };
        if let Some(status) = child_status {
            break status;
        }
        let over = match (out_file.metadata(), err_file.metadata()) {
            (Ok(stdout), Ok(stderr)) => stdout.len() > cap || stderr.len() > cap,
            (Err(error), _) | (_, Err(error)) => {
                kill_and_wait(&mut child)?;
                return Err(error);
            }
        };
        if over || started.elapsed() >= deadline {
            // best-effort: the child may have exited between try_wait and kill
            kill_and_wait(&mut child)?;
            return Ok(if over {
                Outcome::OverCap
            } else {
                Outcome::TimedOut
            });
        }
        std::thread::sleep(POLL);
    };
    if out_file.metadata()?.len() > cap || err_file.metadata()?.len() > cap {
        return Ok(Outcome::OverCap);
    }
    let mut stdout = Vec::new();
    out_file.seek(SeekFrom::Start(0))?;
    (&out_file).take(cap).read_to_end(&mut stdout)?;
    let mut stderr = Vec::new();
    err_file.seek(SeekFrom::Start(0))?;
    (&err_file).take(cap).read_to_end(&mut stderr)?;
    Ok(Outcome::Exited(Output {
        status,
        stdout,
        stderr,
    }))
}

/// `Command::output` with the limits above; a deadline or cap breach is a test FAILURE (panic), never a skip.
pub fn output(command: &mut Command) -> std::io::Result<Output> {
    match run(command, DEADLINE, OUTPUT_CAP)? {
        Outcome::Exited(output) => Ok(output),
        Outcome::TimedOut => {
            panic!("sandbox payload still running after {DEADLINE:?}: {command:?}")
        }
        Outcome::OverCap => {
            panic!("sandbox payload wrote more than {OUTPUT_CAP} bytes: {command:?}")
        }
    }
}
