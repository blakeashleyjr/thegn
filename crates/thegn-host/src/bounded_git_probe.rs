//! Shared bounded capture for fixed local Git probes, not configured gate commands.
//! One global two-slot budget covers cleanup and gate probes, including late
//! readers, writers and unreaped children. OS spawn/filesystem calls are not
//! interruptible; this is not a general command sandbox or cancellation lease.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

static GIT_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct ProbeError(String);
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProbeError {}
fn probe_error(message: impl Into<String>) -> ProbeError {
    ProbeError(message.into())
}

pub(crate) fn capture(
    command: std::process::Command,
    input: Option<String>,
    label: &str,
) -> Result<Vec<u8>, ProbeError> {
    capture_with(
        command,
        input,
        label,
        std::time::Duration::from_secs(15),
        &GIT_IN_FLIGHT,
        spawn_worker,
    )
}

struct GitBudget(&'static AtomicUsize);
impl Drop for GitBudget {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn git_budget(counter: &'static AtomicUsize) -> Result<Arc<GitBudget>, ProbeError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 2).then_some(n + 1)
        })
        .map(|_| Arc::new(GitBudget(counter)))
        .map_err(|_| probe_error("cleanup Git capacity held by unfinished child/pipe; retry later"))
}

type ReapJob = (std::process::Child, Arc<GitBudget>);
fn reaper() -> Result<&'static mpsc::SyncSender<ReapJob>, ProbeError> {
    static REAPER: OnceLock<Result<mpsc::SyncSender<ReapJob>, String>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel::<ReapJob>(2);
            std::thread::Builder::new()
                .name("thegn-cleanup-reaper".into())
                .spawn(move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                    while let Ok((mut child, budget)) = rx.recv() {
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "blocking reap runs only on this dedicated background thread"
                        )]
                        let status = child.wait();
                        if status.is_err() {
                            // Unknown ownership/reap state must not free capacity for
                            // an unbounded succession of unreaped children.
                            std::mem::forget((child, budget));
                        }
                    }
                })
                .map(|_| tx)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| probe_error(format!("cleanup reaper unavailable: {e}")))
}

fn reap_later(child: std::process::Child, budget: Arc<GitBudget>) {
    let sender = reaper().expect("reaper initialized before child spawn");
    if let Err(job) = sender.try_send((child, budget)) {
        // A dead/full reaper must never release capacity for endless unreaped
        // children. Permanently consume this bounded slot and refuse later work.
        let (mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) = job;
        std::mem::forget(job);
    }
}

type WorkerSpawner = fn(&str, Box<dyn FnOnce() + Send>) -> std::io::Result<()>;
fn spawn_worker(name: &str, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            task();
        })
        .map(drop)
}

fn abort_setup(
    mut child: std::process::Child,
    group: &crate::platform::GroupHandle,
    budget: Arc<GitBudget>,
) {
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => group.kill(),
        Err(_) => {} // unknown wait identity: reaper only, never signal a numeric PID
    }
    reap_later(child, budget);
}

fn capture_with(
    mut command: std::process::Command,
    input: Option<String>,
    label: &str,
    timeout: std::time::Duration,
    counter: &'static AtomicUsize,
    worker: WorkerSpawner,
) -> Result<Vec<u8>, ProbeError> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    const LIMIT: u64 = 2 * 1024 * 1024;
    reaper()?;
    let budget = git_budget(counter)?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, group) = crate::platform::spawn_grouped(&mut command)
        .map_err(|e| probe_error(format!("Git could not start: {e}")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read = |name: &str, stream: Box<dyn Read + Send>| -> std::io::Result<_> {
        let (tx, rx) = std::sync::mpsc::channel();
        let budget = Arc::clone(&budget);
        worker(
            name,
            Box::new(move || {
                let mut bytes = Vec::new();
                let value = stream
                    .take(LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                drop(budget);
                if tx.send(value).is_err() { /* timed-out receiver has relinquished the result */ }
            }),
        )?;
        Ok(rx)
    };
    let out = match read("thegn-cleanup-stdout", Box::new(stdout)) {
        Ok(rx) => rx,
        Err(error) => {
            abort_setup(child, &group, Arc::clone(&budget));
            return Err(probe_error(format!("stdout worker unavailable: {error}")));
        }
    };
    let err = match read("thegn-cleanup-stderr", Box::new(stderr)) {
        Ok(rx) => rx,
        Err(error) => {
            abort_setup(child, &group, Arc::clone(&budget));
            return Err(probe_error(format!("stderr worker unavailable: {error}")));
        }
    };
    let input_result = if let Some(input) = input {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("requested stdin pipe");
        let (tx, rx) = mpsc::channel();
        let writer_budget = Arc::clone(&budget);
        if let Err(error) = worker(
            "thegn-cleanup-stdin",
            Box::new(move || {
                let value = stdin.write_all(input.as_bytes());
                drop(stdin);
                drop(writer_budget);
                if tx.send(value).is_err() { /* timed-out receiver has relinquished the result */ }
            }),
        ) {
            abort_setup(child, &group, Arc::clone(&budget));
            return Err(probe_error(format!("stdin worker unavailable: {error}")));
        }
        Some(rx)
    } else {
        None
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                group.kill(); // owned child has not been reaped; kill helper descendants too
                reap_later(child, Arc::clone(&budget));
                return Err(probe_error("Git operation exceeded cleanup deadline"));
            }
            Err(error) => {
                // No numeric PID/PGID signal when wait ownership is uncertain.
                reap_later(child, Arc::clone(&budget));
                return Err(probe_error(format!("Git wait failed: {error}")));
            }
        }
    };
    // Reader/writer leases retain global capacity if a descendant holds a pipe.
    // No unbounded join/wait on this caller; late helpers cannot cause unlimited
    // per-timeout threads or children because only two budgets may be live.
    if let Some(input_result) = input_result {
        input_result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| probe_error("Git input timed out"))?
            .map_err(|e| probe_error(format!("Git input failed: {e}")))?;
    }
    let stdout = out
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| probe_error("Git stdout reader failed or timed out"))?
        .map_err(|e| probe_error(format!("Git stdout unavailable: {e}")))?;
    let stderr = err
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| probe_error("Git stderr reader failed or timed out"))?
        .map_err(|e| probe_error(format!("Git stderr unavailable: {e}")))?;
    if stdout.len() as u64 > LIMIT || stderr.len() as u64 > LIMIT {
        return Err(probe_error("Git probe output exceeded the safety bound"));
    }
    if !status.success() {
        return Err(probe_error(format!(
            "Git {} refused: {}",
            label,
            String::from_utf8_lossy(&stderr)
                .chars()
                .take(512)
                .collect::<String>()
        )));
    }
    Ok(stdout)
}

#[cfg(test)]
#[path = "bounded_git_probe_tests.rs"]
mod tests;
