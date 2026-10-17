//! Owned, reusable Git filter-process children (THE-700).

pub mod protocol;
pub mod registry;

use std::io;
use std::process::{Child, Command as ProcessCommand, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use crate::git::filter_process::protocol::{
    Capabilities, Command, FilterClient, Outcome, ProtocolError,
};

const EXIT_GRACE: Duration = Duration::from_millis(500);

enum JobMessage {
    Request {
        command: Command,
        pathname: String,
        content: Arc<[u8]>,
        max_output: usize,
        reply: SyncSender<Result<Outcome, ProtocolError>>,
    },
}

#[derive(Debug)]
pub enum Reap {
    Exited(ExitStatus),
    Killed(ExitStatus),
    Failed(io::Error),
}

#[derive(Debug)]
pub enum FilterProcessError {
    Spawn(io::Error),
    Handshake { error: ProtocolError, reap: Reap },
    TimedOut { reap: Reap },
    Protocol(ProtocolError),
    Dead,
}

impl std::fmt::Display for FilterProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "failed to spawn filter process: {error}"),
            Self::Handshake { error, .. } => write!(formatter, "filter handshake failed: {error}"),
            Self::TimedOut { .. } => formatter.write_str("filter process request timed out"),
            Self::Protocol(error) => write!(formatter, "filter process failed: {error}"),
            Self::Dead => formatter.write_str("filter process is dead"),
        }
    }
}

impl std::error::Error for FilterProcessError {}

pub struct FilterProcess {
    child: Child,
    jobs: Option<SyncSender<JobMessage>>,
    deadline: Duration,
    dead: bool,
    reap: Option<Reap>,
}

impl FilterProcess {
    pub fn spawn(
        mut command: ProcessCommand,
        want: Capabilities,
        deadline: Duration,
    ) -> Result<Self, FilterProcessError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(FilterProcessError::Spawn)?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let reap = terminate_child(&mut child);
                return Err(FilterProcessError::Handshake {
                    error: ProtocolError::Protocol("child stdin was not piped".into()),
                    reap,
                });
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let reap = terminate_child(&mut child);
                return Err(FilterProcessError::Handshake {
                    error: ProtocolError::Protocol("child stdout was not piped".into()),
                    reap,
                });
            }
        };
        let (job_tx, job_rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_result = thread::Builder::new()
            .name("thegn-filter-io".into())
            .spawn(move || serve_filter(stdin, stdout, job_rx, ready_tx, want));
        if let Err(error) = thread_result {
            let reap = terminate_child(&mut child);
            return Err(FilterProcessError::Handshake {
                error: ProtocolError::Io(error),
                reap,
            });
        }
        match ready_rx.recv_timeout(deadline) {
            Ok(Ok(())) => Ok(Self {
                child,
                jobs: Some(job_tx),
                deadline,
                dead: false,
                reap: None,
            }),
            Ok(Err(error)) => {
                let reap = terminate_child(&mut child);
                Err(FilterProcessError::Handshake { error, reap })
            }
            Err(RecvTimeoutError::Timeout) => {
                let reap = terminate_child(&mut child);
                Err(FilterProcessError::TimedOut { reap })
            }
            Err(RecvTimeoutError::Disconnected) => {
                let reap = terminate_child(&mut child);
                Err(FilterProcessError::Handshake {
                    error: ProtocolError::Protocol(
                        "filter I/O thread exited during handshake".into(),
                    ),
                    reap,
                })
            }
        }
    }

    pub fn request(
        &mut self,
        command: Command,
        pathname: &str,
        content: impl Into<Arc<[u8]>>,
        max_output: usize,
    ) -> Result<Outcome, FilterProcessError> {
        if self.dead {
            return Err(FilterProcessError::Dead);
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let job = JobMessage::Request {
            command,
            pathname: pathname.to_owned(),
            content: content.into(),
            max_output,
            reply: reply_tx,
        };
        let send_result = match self.jobs.as_ref() {
            Some(jobs) => jobs.send(job),
            None => return Err(FilterProcessError::Dead),
        };
        if let Err(error) = send_result {
            self.dead = true;
            let _message = error.0;
            let _reap = self.reap_or_terminate();
            return Err(FilterProcessError::Dead);
        }
        let response = match reply_rx.recv_timeout(self.deadline) {
            Ok(response) => response,
            Err(RecvTimeoutError::Timeout) => {
                self.dead = true;
                drop(self.jobs.take());
                let reap = terminate_child(&mut self.child);
                self.reap = Some(clone_reap(&reap));
                return Err(FilterProcessError::TimedOut { reap });
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.dead = true;
                let _reap = self.reap_or_terminate();
                return Err(FilterProcessError::Dead);
            }
        };
        match response {
            Ok(outcome) => Ok(outcome),
            Err(error @ ProtocolError::Unsupported(_))
            | Err(error @ ProtocolError::InvalidPath) => Err(FilterProcessError::Protocol(error)),
            Err(error) => {
                self.dead = true;
                let _reap = self.reap_or_terminate();
                Err(FilterProcessError::Protocol(error))
            }
        }
    }

    pub fn shutdown(mut self) -> Reap {
        self.reap_or_terminate()
    }

    fn reap_or_terminate(&mut self) -> Reap {
        if let Some(reap) = self.reap.take() {
            self.reap = Some(clone_reap(&reap));
            return reap;
        }
        drop(self.jobs.take());
        let reap = reap_child(&mut self.child);
        self.reap = Some(clone_reap(&reap));
        reap
    }
}

impl Drop for FilterProcess {
    fn drop(&mut self) {
        if self.reap.is_none() {
            let reap = self.reap_or_terminate();
            self.reap = Some(clone_reap(&reap));
        }
    }
}

fn clone_reap(reap: &Reap) -> Reap {
    match reap {
        Reap::Exited(status) => Reap::Exited(*status),
        Reap::Killed(status) => Reap::Killed(*status),
        Reap::Failed(error) => Reap::Failed(io::Error::new(error.kind(), error.to_string())),
    }
}

fn terminate_child(child: &mut Child) -> Reap {
    match child.kill() {
        Ok(()) => match child.wait() {
            Ok(status) => Reap::Killed(status),
            Err(error) => Reap::Failed(error),
        },
        Err(kill_error) => {
            tracing::debug!(
                target: "thegn::git",
                %kill_error,
                "filter child kill failed; waiting for exit"
            );
            match child.try_wait() {
                Ok(Some(status)) => Reap::Exited(status),
                Ok(None) => match child.wait() {
                    Ok(status) => Reap::Exited(status),
                    Err(wait_error) => Reap::Failed(wait_error),
                },
                Err(wait_error) => Reap::Failed(wait_error),
            }
        }
    }
}

fn reap_child(child: &mut Child) -> Reap {
    let start = Instant::now();
    let mut interval = thegn_proc::POLL_INTERVAL;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Reap::Exited(status),
            Ok(None) => {}
            Err(error) => return Reap::Failed(error),
        }
        let elapsed = start.elapsed();
        if elapsed >= EXIT_GRACE {
            return match child.kill() {
                Ok(()) => match child.wait() {
                    Ok(status) => Reap::Killed(status),
                    Err(error) => Reap::Failed(error),
                },
                Err(kill_error) => {
                    tracing::debug!(
                        target: "thegn::git",
                        %kill_error,
                        "filter child kill failed; waiting for exit"
                    );
                    match child.try_wait() {
                        Ok(Some(status)) => Reap::Exited(status),
                        Ok(None) => match child.wait() {
                            Ok(status) => Reap::Exited(status),
                            Err(wait_error) => Reap::Failed(wait_error),
                        },
                        Err(wait_error) => Reap::Failed(wait_error),
                    }
                }
            };
        }
        let remaining = EXIT_GRACE.saturating_sub(elapsed);
        thread::sleep(thegn_proc::next_backoff(interval, remaining));
        interval = thegn_proc::next_backoff(interval, remaining);
    }
}

fn serve_filter(
    mut stdin: impl io::Write,
    mut stdout: impl io::Read,
    jobs: Receiver<JobMessage>,
    ready: SyncSender<Result<(), ProtocolError>>,
    want: Capabilities,
) {
    let mut client = match FilterClient::handshake(&mut stdout, &mut stdin, want) {
        Ok(client) => client,
        Err(error) => {
            if let Err(send_error) = ready.send(Err(error)) {
                tracing::debug!(
                    target: "thegn::git",
                    %send_error,
                    "filter handshake result receiver closed"
                );
            }
            return;
        }
    };
    if let Err(send_error) = ready.send(Ok(())) {
        tracing::debug!(target: "thegn::git", %send_error, "filter ready receiver closed");
        return;
    }
    while let Ok(JobMessage::Request {
        command,
        pathname,
        content,
        max_output,
        reply,
    }) = jobs.recv()
    {
        let result = client.request(
            &mut stdout,
            &mut stdin,
            command,
            &pathname,
            &content[..],
            max_output,
        );
        let failed = result.is_err()
            && !matches!(
                &result,
                Err(ProtocolError::Unsupported(_) | ProtocolError::InvalidPath)
            );
        if let Err(send_error) = reply.send(result) {
            tracing::debug!(target: "thegn::git", %send_error, "filter request receiver closed");
        }
        if failed {
            break;
        }
    }
}

#[cfg(test)]
mod tests;
