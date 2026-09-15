//! Fixture-only retained process groups. Observe without reaping, finish group
//! signals while identity is still owned, then consume exactly one ready wait.
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;
const COMMAND_DEADLINE: Duration = Duration::from_secs(30);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);

enum Slot {
    Vacant,
    Reserved,
    Held {
        _child: Option<Child>,
        _root: Arc<tempfile::TempDir>,
    },
}
fn slots() -> std::sync::MutexGuard<'static, [Slot; 16]> {
    static SLOTS: OnceLock<Mutex<[Slot; 16]>> = OnceLock::new();
    SLOTS
        .get_or_init(|| Mutex::new(std::array::from_fn(|_| Slot::Vacant)))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct Owned {
    child: Option<Child>,
    root: Arc<tempfile::TempDir>,
    slot: usize,
    identity_lost: bool,
    cleanup_failed: bool,
    settlement_attempted: bool,
    receipt: PathBuf,
    started: bool,
}
impl Owned {
    fn spawn(command: &mut Command, root: Arc<tempfile::TempDir>, sequence: usize) -> Self {
        // Do not change process-wide disposition. Refuse auto-reaping, which
        // cannot provide the unreaped-identity contract used below.
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: valid output storage, null new action means read-only query.
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) },
            0
        );
        let action = unsafe { action.assume_init() };
        assert_ne!(action.sa_sigaction, libc::SIG_IGN);
        assert_eq!(action.sa_flags & libc::SA_NOCLDWAIT, 0);
        let slot = {
            let mut slots = slots();
            let slot = slots
                .iter()
                .position(|s| matches!(s, Slot::Vacant))
                .expect("private child custody exhausted");
            slots[slot] = Slot::Reserved;
            slot
        };
        let receipt = root
            .path()
            .join(format!("receipts/{sequence}.cleanup.json"));
        let mut owned = Self {
            child: None,
            root,
            slot,
            identity_lost: false,
            cleanup_failed: false,
            settlement_attempted: false,
            receipt,
            started: false,
        };
        command.process_group(0);
        owned.child = Some(command.spawn().expect("spawn private command"));
        owned.started = true;
        owned
    }

    fn exited(&mut self) -> io::Result<bool> {
        if self.identity_lost {
            return Err(io::Error::other("private child identity already lost"));
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: exclusively owned child, valid output; WNOWAIT preserves its
        // identity until group cleanup finishes. No other fixture waits on it.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.as_ref().unwrap().id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            self.identity_lost = true;
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.assume_init().si_pid() } != 0)
    }

    fn settle(&mut self) -> Option<ExitStatus> {
        if self.child.is_none() || self.identity_lost || self.settlement_attempted {
            return None;
        }
        self.settlement_attempted = true;
        // Re-observe before any signal, including cleanup invoked during unwind.
        if self.exited().is_err() {
            return None;
        }
        let child = self.child.as_mut().unwrap();
        let pid = i32::try_from(child.id()).expect("private PID fits pid_t");
        // SAFETY: this exact unreaped child created group pid; never signal a
        // discovered/replacement group. Fixtures do not escape their group.
        let signal = unsafe { libc::kill(-pid, libc::SIGKILL) };
        let group_ok =
            signal == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        let leader_ok = child.kill().is_ok();
        if !group_ok || !leader_ok {
            self.cleanup_failed = true;
        }
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        loop {
            match self.exited() {
                Ok(true) => {
                    // Only a ready, still-owned leader reaches the consuming
                    // wait. An error permanently forbids further PID operations.
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "private Linux integration child is already observed exited with WNOWAIT; consume only after owned group signals"
                    )]
                    let waited = self.child.as_mut().unwrap().wait();
                    match waited {
                        Ok(status) => {
                            self.child.take();
                            return Some(status);
                        }
                        Err(_) => {
                            self.identity_lost = true;
                            return None;
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        let retain = self.child.is_some() || self.cleanup_failed || std::thread::panicking();
        if self.child.is_some() && self.settle().is_none() {
            self.cleanup_failed = true;
        }
        let receipt = serde_json::json!({ "started": self.started, "leader_reaped": self.started && self.child.is_none(), "identity_lost": self.identity_lost, "cleanup_error": self.cleanup_failed, "retained": retain, "tree_containment": "fixed fixture group only; no escaped-tree guarantee" });
        fs::write(&self.receipt, receipt.to_string()).unwrap_or(());
        let mut slots = slots();
        slots[self.slot] = if retain {
            Slot::Held {
                _child: self.child.take(),
                _root: Arc::clone(&self.root),
            }
        } else {
            Slot::Vacant
        };
    }
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u128,
}
fn text(path: &Path) -> String {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() as u64 <= OUTPUT_LIMIT);
    String::from_utf8(bytes).unwrap()
}

pub fn run(mut command: Command, root: Arc<tempfile::TempDir>, sequence: usize) -> Output {
    let stdout = root.path().join(format!("receipts/{sequence}.stdout"));
    let stderr = root.path().join(format!("receipts/{sequence}.stderr"));
    command
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let started = Instant::now();
    let deadline = started + COMMAND_DEADLINE;
    let mut child = Owned::spawn(&mut command, root, sequence);
    drop(command);
    loop {
        for path in [&stdout, &stderr] {
            assert!(
                fs::metadata(path).unwrap().len() <= OUTPUT_LIMIT,
                "private output exceeded monitored limit"
            );
        }
        if child.exited().expect("observe private command") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "private command deadline (30 seconds)"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = child
        .settle()
        .expect("private child/group settlement failed; custody retained");
    assert!(
        !child.cleanup_failed,
        "private group cleanup returned an error"
    );
    Output {
        status,
        stdout: text(&stdout),
        stderr: text(&stderr),
        elapsed_ms: started.elapsed().as_millis(),
    }
}
