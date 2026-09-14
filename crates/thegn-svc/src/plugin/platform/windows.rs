//! Standard child pipes use synchronous Windows handles. Keep each blocking
//! operation on its own finite, owned thread; never put it in Tokio's pool.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

type ReadReply = oneshot::Sender<io::Result<Vec<u8>>>;
type WriteReply = oneshot::Sender<io::Result<usize>>;
enum Operation {
    Attach(File),
    Read(ReadReply),
    #[cfg(test)]
    ReadPaused {
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
        reply: ReadReply,
    },
    Write(Vec<u8>, WriteReply),
}

struct Worker {
    tx: Option<mpsc::Sender<Operation>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    attached: bool,
}

fn thread_finished(thread: &JoinHandle<()>) -> io::Result<bool> {
    // Confirm OS thread termination, including TLS destruction, before join.
    // The thread handle remains owned even after its numeric id can be reused.
    match unsafe { WaitForSingleObject(thread.as_raw_handle().cast(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

impl Worker {
    fn new(name: &str) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut file: Option<File> = None;
                while let Ok(operation) = rx.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    match operation {
                        Operation::Attach(pipe) => file = Some(pipe),
                        Operation::Read(reply) => {
                            let result = file
                                .as_mut()
                                .ok_or_else(|| io::Error::other("pipe unavailable"))
                                .and_then(|pipe| {
                                    let mut bytes = vec![0; 8192];
                                    pipe.read(&mut bytes).map(|len| {
                                        bytes.truncate(len);
                                        bytes
                                    })
                                });
                            // A cancelled owner still retains and joins this worker.
                            drop(reply.send(result));
                        }
                        #[cfg(test)]
                        Operation::ReadPaused {
                            entered,
                            resume,
                            reply,
                        } => {
                            entered.send(()).expect("fixture barrier receiver");
                            resume.recv().expect("fixture barrier release");
                            let mut bytes = vec![0; 8192];
                            let result =
                                file.as_mut()
                                    .expect("fixture pipe")
                                    .read(&mut bytes)
                                    .map(|len| {
                                        bytes.truncate(len);
                                        bytes
                                    });
                            drop(reply.send(result));
                        }
                        Operation::Write(bytes, reply) => {
                            let result = file
                                .as_mut()
                                .ok_or_else(|| io::Error::other("pipe unavailable"))
                                .and_then(|pipe| pipe.write(&bytes));
                            drop(reply.send(result));
                        }
                    }
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            stop,
            thread: Some(thread),
            attached: false,
        })
    }

    fn send(&mut self, operation: Operation) -> io::Result<()> {
        if matches!(&operation, Operation::Attach(_)) {
            self.attached = true;
        }
        self.tx
            .as_ref()
            .ok_or_else(|| io::Error::other("pipe closing"))?
            .send(operation)
            .map_err(|_| io::Error::other("pipe worker closed"))
    }

    fn request_close(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx = None; // releases a worker waiting for its next command
    }

    fn cancel_and_join_finished(&mut self) -> io::Result<bool> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(true);
        };
        if thread_finished(thread)? {
            return self
                .thread
                .take()
                .expect("owned thread")
                .join()
                .map(|()| true)
                .map_err(|_| io::Error::other("pipe worker panicked"));
        }
        // SAFETY: JoinHandle owns this exact thread until confirmed completion.
        // It is never returned to a pool or reused for another pipe/session.
        if unsafe { CancelSynchronousIo(thread.as_raw_handle().cast()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NOT_FOUND as i32) {
                return Err(error);
            }
        }
        // ERROR_NOT_FOUND may mean cancellation raced just BEFORE ReadFile.
        // It never means settled; the closing loop retries on this same handle.
        Ok(false)
    }
}

pub(crate) struct Prepared {
    input: Worker,
    output: Worker,
    error: Worker,
}
impl Prepared {
    pub fn new() -> io::Result<Self> {
        // All fallible thread admission occurs BEFORE any child is created.
        let input = Worker::new("thegn-plugin-stdin")?;
        let output = Worker::new("thegn-plugin-stdout")?;
        let error = Worker::new("thegn-plugin-stderr")?;
        Ok(Self {
            input,
            output,
            error,
        })
    }

    pub fn spawn(mut self, command: Command) -> io::Result<Process> {
        let mut child = tokio::process::Command::from(command).spawn()?;
        let mut errors = Vec::new();
        let stdin = child
            .stdin
            .take()
            .map(|pipe| pipe.into_owned_handle().map(File::from));
        let stdout = child
            .stdout
            .take()
            .map(|pipe| pipe.into_owned_handle().map(File::from));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| pipe.into_owned_handle().map(File::from));
        for (worker, pipe, name) in [
            (&mut self.input, stdin, "stdin"),
            (&mut self.output, stdout, "stdout"),
            (&mut self.error, stderr, "stderr"),
        ] {
            let result = pipe
                .unwrap_or_else(|| Err(io::Error::other("pipe missing")))
                .and_then(|file| worker.send(Operation::Attach(file)));
            if let Err(error) = result {
                errors.push(format!("{name} setup: {error}"));
            }
        }
        Ok(Process {
            leader: Leader {
                child,
                status: None,
            },
            stdin: Some(Input {
                worker: self.input,
                pending: None,
            }),
            stdout: Some(Output {
                worker: self.output,
                pending: None,
            }),
            stderr: Some(Output {
                worker: self.error,
                pending: None,
            }),
            errors,
        })
    }
}

// Pre-spawn failures only leave idle workers; disconnect their receivers and
// join before returning. Once attached to a pipe, settlement is explicitly
// owned by Process, which retains timed-out workers in the supervisor registry.
impl Drop for Worker {
    fn drop(&mut self) {
        self.request_close();
        if let Some(thread) = self.thread.take() {
            if !self.attached || matches!(thread_finished(&thread), Ok(true)) {
                if thread.join().is_err() {
                    tracing::warn!(target: "thegn::plugin", "pipe worker panicked during final release");
                }
            } else {
                // App exit cannot retain custody beyond process death. This is
                // an explicit unresolved outcome, never a successful join.
                tracing::warn!(target: "thegn::plugin", "unsettled pipe thread released at final owner drop");
            }
        }
    }
}

pub(crate) struct Process {
    pub leader: Leader,
    pub stdin: Option<Input>,
    pub stdout: Option<Output>,
    pub stderr: Option<Output>,
    pub errors: Vec<String>,
}

impl Process {
    pub async fn close_stdin(&mut self, deadline: Instant) -> bool {
        if let Some(input) = &mut self.stdin {
            input.worker.request_close();
            loop {
                match input.worker.cancel_and_join_finished() {
                    Ok(true) => {
                        self.stdin = None;
                        return true;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.errors.push(format!("stdin cancellation: {error}"));
                        return false;
                    }
                }
                if Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep_until((Instant::now() + Duration::from_millis(5)).min(deadline))
                    .await;
            }
        }
        true
    }

    pub async fn settle_pipes(&mut self, deadline: Instant) -> bool {
        if let Some(input) = &mut self.stdin {
            input.worker.request_close();
        }
        if let Some(output) = &mut self.stdout {
            output.worker.request_close();
        }
        if let Some(error) = &mut self.stderr {
            error.worker.request_close();
        }
        loop {
            let mut all_done = true;
            for worker in self
                .stdin
                .iter_mut()
                .map(|p| &mut p.worker)
                .chain(self.stdout.iter_mut().map(|p| &mut p.worker))
                .chain(self.stderr.iter_mut().map(|p| &mut p.worker))
            {
                match worker.cancel_and_join_finished() {
                    Ok(done) => all_done &= done,
                    Err(error) => {
                        self.errors.push(format!("pipe cancellation: {error}"));
                        all_done = false;
                    }
                }
            }
            if all_done {
                self.stdin = None;
                self.stdout = None;
                self.stderr = None;
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            // Active shutdown only; no resident idle timer or polling.
            tokio::time::sleep_until((Instant::now() + Duration::from_millis(5)).min(deadline))
                .await;
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.leader.reaped_status().is_none() {
            if let Err(error) = self.leader.terminate() {
                tracing::error!(target: "thegn::plugin", %error, "final owner drop could not terminate retained process handle");
            } else {
                tracing::warn!(target: "thegn::plugin", "final owner drop requested termination; reaping remains unconfirmed");
            }
        }
        for worker in self
            .stdin
            .iter_mut()
            .map(|pipe| &mut pipe.worker)
            .chain(self.stdout.iter_mut().map(|pipe| &mut pipe.worker))
            .chain(self.stderr.iter_mut().map(|pipe| &mut pipe.worker))
        {
            worker.request_close();
            if let Err(error) = worker.cancel_and_join_finished() {
                tracing::error!(target: "thegn::plugin", %error, "final exact-thread cancellation failed");
            }
        }
        // No blocking joins or claim that a cancellation request settled I/O.
        // Held threads cannot remain in this process after application exit.
    }
}

pub(crate) struct Input {
    worker: Worker,
    pending: Option<oneshot::Receiver<io::Result<usize>>>,
}
impl Input {
    pub async fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.pending.is_none() {
            let (tx, rx) = oneshot::channel();
            self.worker.send(Operation::Write(
                bytes[..bytes.len().min(8192)].to_vec(),
                tx,
            ))?;
            self.pending = Some(rx);
        }
        let result = self.pending.as_mut().expect("pending write").await;
        self.pending = None;
        result.map_err(|_| io::Error::other("pipe worker closed"))?
    }
}

pub(crate) struct Output {
    worker: Worker,
    pending: Option<oneshot::Receiver<io::Result<Vec<u8>>>>,
}
impl Output {
    pub async fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.len() < 8192 {
            return Err(io::Error::other("read buffer must hold one pipe chunk"));
        }
        if self.pending.is_none() {
            let (tx, rx) = oneshot::channel();
            self.worker.send(Operation::Read(tx))?;
            self.pending = Some(rx);
        }
        let result = self.pending.as_mut().expect("pending read").await;
        self.pending = None;
        let chunk = result.map_err(|_| io::Error::other("pipe worker closed"))??;
        bytes[..chunk.len()].copy_from_slice(&chunk);
        Ok(chunk.len())
    }
}

pub(crate) struct Leader {
    child: tokio::process::Child,
    status: Option<ExitStatus>,
}
impl Leader {
    pub fn reaped_status(&self) -> Option<ExitStatus> {
        self.status
    }
    pub async fn wait_ready(&mut self) -> io::Result<()> {
        if self.status.is_none() {
            self.status = Some(self.child.wait().await?);
        }
        Ok(())
    }
    pub fn terminate(&mut self) -> io::Result<()> {
        // Windows process handles retain identity after exit; no PID lookup.
        if self.status.is_some() {
            Ok(())
        } else {
            self.child.start_kill()
        }
    }
    pub fn reap(&mut self) -> io::Result<ExitStatus> {
        self.status
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "leader still running"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn resident_windows_cancellation_before_syscall_must_be_repeated() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered_runtime = runtime.enter();
        let mut command = Command::new("cmd");
        command
            .args(["/C", "set /p fixture="])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Prepared::new().unwrap().spawn(command).unwrap();
        drop(entered_runtime);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (reply, _result) = oneshot::channel();
        let worker = &mut process.stdout.as_mut().unwrap().worker;
        worker
            .send(Operation::ReadPaused {
                entered: entered_tx,
                resume: resume_rx,
                reply,
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.request_close();
        // Worker already passed its latch but has not entered ReadFile. This
        // request may return ERROR_NOT_FOUND, and must never count as settled.
        assert!(!worker.cancel_and_join_finished().unwrap());
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        runtime.block_on(async {
            loop {
                if worker.cancel_and_join_finished().unwrap() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "second cancellation never settled exact worker"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        // stdin remains owned/open during that proof; child EOF did not rescue
        // the blocked read. Cleanup only this fixture's retained process handle.
        process.leader.terminate().unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), process.leader.wait_ready())
                .await
                .unwrap()
                .unwrap();
            process.leader.reap().unwrap();
            assert!(
                process
                    .settle_pipes(Instant::now() + Duration::from_secs(2))
                    .await
            );
        });
    }
}
