use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::task::{Context, Poll};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::Instant;

enum ExitNotice {
    Signal(Signal),
    #[cfg(target_os = "linux")]
    Pid(tokio::io::unix::AsyncFd<OwnedFd>),
}
impl ExitNotice {
    fn poll(&mut self, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self {
            Self::Signal(signal) => signal
                .poll_recv(context)
                .map(|notice| notice.ok_or_else(|| io::Error::other("SIGCHLD stream closed"))),
            #[cfg(target_os = "linux")]
            Self::Pid(fd) => fd.poll_read_ready(context).map(|result| {
                result.map(|mut ready| {
                    // pidfds are pollable, not readable byte streams. WNOWAIT below
                    // remains the authority for child status; never read this fd.
                    ready.clear_ready();
                })
            }),
        }
    }
}

pub(crate) struct Prepared(Signal);

impl Prepared {
    pub fn new() -> io::Result<Self> {
        // Subscribe before spawn/first waitid so an early exit cannot be lost.
        let signal = signal(SignalKind::child())?;
        // Retain a non-auto-reaping SIGCHLD disposition even when Linux uses a
        // pidfd for readiness. Opening a pidfd for a zombie is safe only while
        // the std Child remains exclusively unreaped by this owner.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            return Err(io::Error::other(
                "SIGCHLD disposition would discard resident child identity",
            ));
        }
        Ok(Self(signal))
    }

    pub fn spawn(self, mut command: Command) -> io::Result<Process> {
        command.process_group(0);
        let mut child = command.spawn()?;
        let mut errors = Vec::new();
        #[allow(unused_mut)]
        let mut notice = ExitNotice::Signal(self.0);
        #[allow(unused_mut)]
        let mut identity_lost = false;
        #[cfg(target_os = "linux")]
        {
            // Linux >=5.3: flags0 returns a CLOEXEC descriptor. Readiness is
            // event-driven and avoids SIGCHLD self-socket wake restrictions.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
            if fd >= 0 {
                let owned = unsafe { OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) };
                match tokio::io::unix::AsyncFd::new(owned) {
                    Ok(fd) => notice = ExitNotice::Pid(fd),
                    Err(error) => errors.push(format!("pidfd readiness setup: {error}")),
                }
            } else {
                let error = io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
                    tracing::debug!(target: "thegn::plugin", %error, "pidfd unavailable on this kernel; retaining SIGCHLD readiness");
                } else {
                    identity_lost = error.raw_os_error() == Some(libc::ESRCH);
                    errors.push(format!("pidfd creation: {error}"));
                }
            }
        }
        let stdin = child.stdin.take().and_then(|pipe| {
            tokio::process::ChildStdin::from_std(pipe)
                .map(Input)
                .map_err(|e| errors.push(format!("stdin setup: {e}")))
                .ok()
        });
        let stdout = child.stdout.take().and_then(|pipe| {
            tokio::process::ChildStdout::from_std(pipe)
                .map(Output::Stdout)
                .map_err(|e| errors.push(format!("stdout setup: {e}")))
                .ok()
        });
        let stderr = child.stderr.take().and_then(|pipe| {
            tokio::process::ChildStderr::from_std(pipe)
                .map(Output::Stderr)
                .map_err(|e| errors.push(format!("stderr setup: {e}")))
                .ok()
        });
        // Conversion failures retain the child and route through owned cleanup.
        Ok(Process {
            leader: Leader {
                child: Some(child),
                notice,
                identity_lost,
                status: None,
                #[cfg(test)]
                signal_override: None,
            },
            stdin,
            stdout,
            stderr,
            errors,
        })
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
    pub async fn close_stdin(&mut self, _deadline: Instant) -> bool {
        self.stdin = None;
        true
    }
    pub async fn settle_pipes(&mut self, _deadline: Instant) -> bool {
        // Tokio's Unix child pipes use nonblocking descriptors, not blocking
        // pool operations. Dropping them cancels readiness and releases custody.
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        true
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.leader.reaped_status().is_some() {
            return;
        }
        // Last-resort process-exit path only: never block on child wait and
        // never recover identity from a bare PID after a failed/reaped wait.
        match self.leader.terminate() {
            Ok(()) => {
                tracing::warn!(target: "thegn::plugin", "final owner drop requested termination; reaping remains unconfirmed")
            }
            Err(error) => {
                tracing::error!(target: "thegn::plugin", %error, "final owner drop could not terminate still-owned leader")
            }
        }
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        if matches!(self.leader.exited_without_reaping(), Ok(true)) {
            if let Err(error) = self.leader.reap() {
                tracing::error!(target: "thegn::plugin", %error, "final nonblocking reap failed");
            }
        }
    }
}

pub(crate) struct Input(tokio::process::ChildStdin);
impl Input {
    pub async fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes).await
    }
}

pub(crate) enum Output {
    Stdout(tokio::process::ChildStdout),
    Stderr(tokio::process::ChildStderr),
}
impl Output {
    pub async fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(pipe) => pipe.read(bytes).await,
            Self::Stderr(pipe) => pipe.read(bytes).await,
        }
    }
}

pub(crate) struct Leader {
    child: Option<Child>,
    notice: ExitNotice,
    identity_lost: bool,
    status: Option<ExitStatus>,
    #[cfg(test)]
    signal_override: Option<Box<dyn FnMut(libc::pid_t) -> io::Result<()> + Send>>,
}

impl Leader {
    pub fn reaped_status(&self) -> Option<ExitStatus> {
        self.status
    }

    fn exited_without_reaping(&mut self) -> io::Result<bool> {
        let Some(child) = self.child.as_ref() else {
            return Ok(true);
        };
        // SAFETY: zeroed siginfo is a valid output buffer; WNOHANG avoids a
        // blocking wait and WNOWAIT retains identity until group signaling ends.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.identity_lost = true;
            }
            return Err(error);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    pub async fn wait_ready(&mut self) -> io::Result<()> {
        std::future::poll_fn(|context| {
            loop {
                // Register interest BEFORE observing status, matching Tokio's
                // non-losing signal reaper order without consuming child identity.
                let notice = self.notice.poll(context);
                if self.exited_without_reaping()? {
                    return Poll::Ready(Ok(()));
                }
                match notice {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                }
            }
        })
        .await
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        if self.identity_lost {
            return Err(io::Error::other(
                "leader identity was lost; signaling forbidden",
            ));
        }
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| io::Error::other("leader already reaped"))?;
        let group = -(child.id() as libc::pid_t);
        #[cfg(test)]
        if let Some(signal) = self.signal_override.as_mut() {
            return signal(group);
        }
        // SAFETY: this owner has never reaped the child, pinning its identity.
        // The process group is created before exec. No post-reap PID fallback.
        if unsafe { libc::kill(group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    pub fn reap(&mut self) -> io::Result<ExitStatus> {
        if !self.exited_without_reaping()? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "leader still running",
            ));
        }
        self.consume_ready()
    }

    fn consume_ready(&mut self) -> io::Result<ExitStatus> {
        let status = match self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("leader already reaped"))?
            .wait()
        {
            Ok(status) => status,
            Err(error) => {
                self.identity_lost = true;
                return Err(error);
            }
        };
        self.child = None;
        self.status = Some(status);
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[test]
    fn resident_unix_consuming_wait_identity_loss_forbids_later_signals() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let mut command = Command::new("sh");
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Prepared::new().unwrap().spawn(command).unwrap();
        drop(entered);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), process.leader.wait_ready())
                .await
                .unwrap()
                .unwrap();
        });
        let pid = process.leader.child.as_ref().unwrap().id() as libc::pid_t;
        // Consume only our completed fixture child between non-reaping
        // observation and the consuming wait, modeling an external reaper race.
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(
            process.leader.consume_ready().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        // A regression must never actually signal the now-reaped PID in a test.
        process.leader.signal_override = Some(Box::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }));
        assert!(process.leader.terminate().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(runtime.block_on(process.settle_pipes(Instant::now())));
    }
    #[test]
    fn resident_unix_final_process_drop_terminates_owned_fixture_without_blocking() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let process = Prepared::new().unwrap().spawn(command).unwrap();
        let pid = process.leader.child.as_ref().unwrap().id() as libc::pid_t;
        drop(entered);
        let start = std::time::Instant::now();
        drop(process);
        assert!(start.elapsed() < Duration::from_millis(200));
        // Only reap this fixture child. If Drop already reaped it, ECHILD is
        // success; this test never signals a numeric PID after owner release.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let mut status = 0;
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == pid
                || (result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
            {
                break;
            }
            assert_eq!(result, 0);
            assert!(
                std::time::Instant::now() < deadline,
                "final owner release left fixture alive"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn resident_linux_exit_notice_has_no_idle_wakes_and_retains_identity() {
        use std::future::Future;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::task::{Context, Wake, Waker};
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let enter = runtime.enter();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "read -r fixture; exit 7"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut process = Prepared::new().unwrap().spawn(cmd).unwrap();
        drop(enter);
        assert!(matches!(process.leader.notice, ExitNotice::Pid(_)));
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        runtime.block_on(async {
            let mut wait = Box::pin(process.leader.wait_ready());
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            assert_eq!(
                count.0.load(Ordering::Relaxed),
                0,
                "idle exit waiter woke without fixture exit"
            );
            process
                .stdin
                .as_mut()
                .unwrap()
                .write(b"release\n")
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut wait)
                .await
                .unwrap()
                .unwrap();
        });
        assert!(process.leader.exited_without_reaping().unwrap());
        assert!(
            process.leader.reaped_status().is_none(),
            "notification consumed child identity"
        );
        assert_eq!(process.leader.reap().unwrap().code(), Some(7));
        runtime.block_on(process.settle_pipes(tokio::time::Instant::now()));
    }
}
