use crate::{
    Capture, Containment, End, GroupState, Leader, Limits, POLL_INTERVAL, Run, Stream, Which,
    aggregate_fit,
};
use std::{
    io::{self, Read},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Instant,
};

enum Message {
    Bytes(Which, Vec<u8>),
    Eof(Which),
}

fn reader<R: Read + Send + 'static>(mut pipe: R, which: Which, tx: SyncSender<Message>) {
    let mut buf = [0_u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => {
                if tx.send(Message::Eof(which)).is_err() {
                    break;
                }
                break;
            }
            Ok(count) => {
                if tx
                    .send(Message::Bytes(which, buf[..count].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn start_reader<R: Read + Send + 'static>(pipe: Option<R>, which: Which, tx: &SyncSender<Message>) {
    if let Some(pipe) = pipe {
        let sender = tx.clone();
        let result = thread::Builder::new()
            .name("thegn-proc-read".to_owned())
            .spawn(move || reader(pipe, which, sender));
        if let Err(_spawn_error) = result {
            // The pipe closes when its owner is dropped; no reader was created.
        }
    }
}

fn append(capture: &mut Capture, stream: Stream, bytes: &[u8]) -> bool {
    capture.total = capture.total.saturating_add(bytes.len() as u64);
    match stream {
        Stream::Null => false,
        Stream::Head { cap } => {
            let remaining = cap.saturating_sub(capture.bytes.len());
            capture
                .bytes
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            if bytes.len() > remaining {
                capture.truncated = true;
            }
            false
        }
        Stream::HeadOrFail { cap } => {
            let remaining = cap.saturating_sub(capture.bytes.len());
            capture
                .bytes
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            if bytes.len() > remaining {
                capture.truncated = true;
                true
            } else {
                false
            }
        }
        Stream::Tail { cap } => {
            capture.bytes.extend_from_slice(bytes);
            if capture.bytes.len() > cap {
                let excess = capture.bytes.len() - cap;
                capture.bytes.drain(..excess);
                capture.truncated = true;
            }
            false
        }
    }
}

fn drain(rx: &Receiver<Message>, run: &mut Run, limits: &Limits) -> Option<Which> {
    loop {
        match rx.try_recv() {
            Ok(Message::Bytes(which, bytes)) => {
                let (capture, stream) = match which {
                    Which::Stdout => (&mut run.stdout, limits.stdout),
                    Which::Stderr => (&mut run.stderr, limits.stderr),
                };
                if append(capture, stream, &bytes) {
                    return Some(which);
                }
            }
            Ok(Message::Eof(which)) => match which {
                Which::Stdout => run.stdout.eof = true,
                Which::Stderr => run.stderr.eof = true,
            },
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
    }
}

fn wait(mut child: Child) -> Leader {
    match child.wait() {
        Ok(status) => Leader::Reaped(status),
        Err(error) => Leader::ReapFailed(error),
    }
}

fn detach(child: Child) -> Leader {
    let owner = Arc::new(Mutex::new(Some(child)));
    let worker_owner = Arc::clone(&owner);
    let spawned = thread::Builder::new()
        .name("thegn-proc-reap".to_owned())
        .spawn(move || {
            if let Ok(mut slot) = worker_owner.lock()
                && let Some(mut child) = slot.take()
            {
                match child.wait() {
                    Ok(_status) => {}
                    Err(_error) => {}
                }
            }
        });
    match spawned {
        Ok(handle) => {
            drop(handle);
            Leader::Detached
        }
        Err(_spawn_error) => {
            let child = match owner.lock() {
                Ok(mut slot) => slot.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            match child {
                Some(child) => wait(child),
                None => Leader::Detached,
            }
        }
    }
}

pub(super) fn run(cmd: &mut Command, limits: &Limits, cancel: &AtomicBool) -> Run {
    cmd.stdin(Stdio::null());
    cmd.stdout(match limits.stdout {
        Stream::Null => Stdio::null(),
        _ => Stdio::piped(),
    });
    cmd.stderr(match limits.stderr {
        Stream::Null => Stdio::null(),
        _ => Stdio::piped(),
    });
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Run {
                end: End::Spawn(error),
                group: GroupState::Unknown,
                leader: Leader::NotSpawned,
                leader_exited_first: false,
                stdout: Capture::default(),
                stderr: Capture::default(),
                containment: Containment::DirectChildOnly,
            };
        }
    };
    let (tx, rx) = mpsc::sync_channel(16);
    let mut child = Some(child);
    start_reader(
        child.as_mut().and_then(|child| child.stdout.take()),
        Which::Stdout,
        &tx,
    );
    start_reader(
        child.as_mut().and_then(|child| child.stderr.take()),
        Which::Stderr,
        &tx,
    );
    let start = Instant::now();
    let mut run = Run {
        end: End::LeaderExited,
        group: GroupState::Drained,
        leader: Leader::NotSpawned,
        leader_exited_first: false,
        stdout: Capture::default(),
        stderr: Capture::default(),
        containment: Containment::DirectChildOnly,
    };
    let mut status = None;
    let mut leader_exit_at: Option<Instant> = None;
    loop {
        if let Some(which) = drain(&rx, &mut run, limits) {
            run.end = End::OutputLimit(which);
            break;
        }
        if status.is_some() {
            run.leader_exited_first = true;
            run.end = End::LeaderExited;
            if cancel.load(Ordering::Acquire) {
                run.end = End::Cancelled;
                break;
            }
            if start.elapsed() >= limits.timeout
                || limits.quiesce_after_leader.is_some_and(|quiesce| {
                    leader_exit_at.is_some_and(|exited| exited.elapsed() >= quiesce)
                })
            {
                break;
            }
            if (run.stdout.eof || matches!(limits.stdout, Stream::Null))
                && (run.stderr.eof || matches!(limits.stderr, Stream::Null))
            {
                break;
            }
        } else if cancel.load(Ordering::Acquire) {
            run.end = End::Cancelled;
            break;
        } else if start.elapsed() >= limits.timeout {
            run.end = End::TimedOut;
            break;
        } else {
            match child
                .as_mut()
                .expect("child retained until reap")
                .try_wait()
            {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    leader_exit_at = Some(Instant::now());
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    run.end = End::Supervise(error);
                    run.group = GroupState::Unknown;
                    break;
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }

    if matches!(
        &run.end,
        End::TimedOut | End::Cancelled | End::OutputLimit(_) | End::Supervise(_)
    ) {
        let kill_result = child.as_mut().expect("child retained until reap").kill();
        if let Err(error) = kill_result
            && error.kind() != io::ErrorKind::InvalidInput
        {
            run.leader = Leader::ReapFailed(error);
        }
        let settle = Instant::now();
        loop {
            match child
                .as_mut()
                .expect("child retained until reap")
                .try_wait()
            {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    run.group = GroupState::Terminated;
                    break;
                }
                Ok(None) if settle.elapsed() < limits.kill_settle => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    run.group = GroupState::Survivors;
                    break;
                }
                Err(error) => {
                    run.leader = Leader::ReapFailed(error);
                    run.group = GroupState::Survivors;
                    break;
                }
            }
        }
    }
    if let Some(exit) = status {
        // try_wait already reaped this direct child.
        run.leader = Leader::Reaped(exit);
    } else if let Some(child) = child.take() {
        run.leader = if matches!(run.group, GroupState::Survivors) {
            detach(child)
        } else {
            wait(child)
        };
    }
    if !matches!(run.group, GroupState::Unknown | GroupState::Survivors) {
        run.group = if matches!(&run.end, End::LeaderExited) {
            GroupState::Drained
        } else {
            GroupState::Terminated
        };
    }
    let grace = Instant::now();
    while !((run.stdout.eof || matches!(limits.stdout, Stream::Null))
        && (run.stderr.eof || matches!(limits.stderr, Stream::Null)))
        && grace.elapsed() < limits.reader_grace
    {
        if let Some(which) = drain(&rx, &mut run, limits) {
            run.end = End::OutputLimit(which);
        }
        thread::sleep(POLL_INTERVAL);
    }
    if let Some(which) = drain(&rx, &mut run, limits) {
        run.end = End::OutputLimit(which);
    }
    aggregate_fit(
        &mut run.stdout,
        &mut run.stderr,
        limits.stdout,
        limits.stderr,
        limits.aggregate_tail_cap,
    );
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn head_append_keeps_first_bytes_and_drains_without_limiting() {
        let mut capture = Capture::default();
        let first = vec![1; 1024 * 1024];
        let next = vec![2; 2 * 1024 * 1024];
        assert!(!append(
            &mut capture,
            Stream::Head { cap: first.len() },
            &first
        ));
        assert!(!append(
            &mut capture,
            Stream::Head { cap: first.len() },
            &next
        ));
        assert_eq!(capture.bytes, first);
        assert!(capture.truncated);
        assert_eq!(capture.total, 3 * 1024 * 1024);
    }

    #[test]
    fn clean_exit_with_eof_does_not_wait_for_quiesce() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "echo done"]);
        let limits = Limits {
            stdout: Stream::Tail { cap: 64 },
            stderr: Stream::Tail { cap: 64 },
            quiesce_after_leader: Some(Duration::from_secs(10)),
            ..Limits::silent(Duration::from_secs(20))
        };
        let started = Instant::now();
        let result = run(&mut cmd, &limits, &AtomicBool::new(false));
        assert!(result.success());
        assert!(result.stdout.eof);
        assert!(result.stderr.eof);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
