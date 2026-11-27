#[cfg(target_os = "linux")]
use std::collections::{BTreeMap, VecDeque};
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::task::{Context, Poll};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::Instant;

const MAX_TRACKED_DESCENDANTS: usize = 4096;
#[cfg(target_os = "linux")]
const MAX_DESCENDANT_DEPTH: usize = 64;

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static PROC_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROC_DIR_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static GLOBAL_PROC_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Test-only `/proc` root for [`linux_descendants_subtree_only`], so a run
    /// can simulate a failed root walk or a helper orphaned between walks.
    static SUBTREE_PROC_ROOT: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_os = "linux")]
fn proc_read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
    #[cfg(test)]
    PROC_DIR_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    #[cfg(test)]
    PROC_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    std::fs::read_dir(path)
}

#[cfg(target_os = "linux")]
fn proc_read_to_string(path: &Path) -> io::Result<String> {
    #[cfg(test)]
    PROC_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    std::fs::read_to_string(path)
}

#[cfg(target_os = "linux")]
fn proc_stat_ppid_start(stat: &str) -> Option<(libc::pid_t, u64)> {
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let ppid = fields.get(1)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((ppid, start_time))
}

#[cfg(target_os = "linux")]
/// Snapshot descendants while `root` is still unreaped: that pins the leader
/// identity and makes the root of this walk authoritative.
pub(crate) fn linux_descendants(root: libc::pid_t) -> Vec<(libc::pid_t, u64)> {
    linux_descendants_in(Path::new("/proc"), root)
}

#[cfg(target_os = "linux")]
fn linux_descendants_in(proc_root: &Path, root: libc::pid_t) -> Vec<(libc::pid_t, u64)> {
    match linux_descendants_subtree(proc_root, root) {
        Ok(descendants) => descendants,
        Err(()) => linux_descendants_full(proc_root, root),
    }
}

#[cfg(target_os = "linux")]
fn linux_descendants_full(proc_root: &Path, root: libc::pid_t) -> Vec<(libc::pid_t, u64)> {
    #[cfg(test)]
    GLOBAL_PROC_SCANS.with(|scans| scans.set(scans.get().saturating_add(1)));
    let Ok(entries) = proc_read_dir(proc_root) else {
        return Vec::new();
    };
    let mut children = BTreeMap::<libc::pid_t, Vec<(libc::pid_t, u64)>>::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(stat) = proc_read_to_string(&entry.path().join("stat")) else {
            continue;
        };
        if let Some((ppid, start_time)) = proc_stat_ppid_start(&stat) {
            children.entry(ppid).or_default().push((pid, start_time));
        }
    }

    let mut descendants = Vec::new();
    let mut pending = VecDeque::from([root]);
    let mut visited = std::collections::BTreeSet::from([root]);
    let mut capped = false;
    while let Some(parent) = pending.pop_front() {
        let Some(rows) = children.get(&parent) else {
            continue;
        };
        for &(pid, start_time) in rows {
            if !visited.insert(pid) {
                continue;
            }
            if descendants.len() == MAX_TRACKED_DESCENDANTS {
                capped = true;
                break;
            }
            descendants.push((pid, start_time));
            pending.push_back(pid);
        }
        if capped {
            break;
        }
    }
    if capped {
        tracing::warn!(
            target: "thegn::plugin",
            limit = MAX_TRACKED_DESCENDANTS,
            "plugin descendant snapshot reached its process limit"
        );
    }
    descendants
}

#[cfg(target_os = "linux")]
fn linux_descendants_subtree(
    proc_root: &Path,
    root: libc::pid_t,
) -> Result<Vec<(libc::pid_t, u64)>, ()> {
    let (descendants, capped) = linux_descendants_subtree_walk(proc_root, root)?;
    if capped {
        tracing::warn!(
            target: "thegn::plugin",
            limit = MAX_TRACKED_DESCENDANTS,
            "plugin descendant snapshot reached its process limit"
        );
    }
    Ok(descendants)
}

/// Walk `/proc/<pid>/task/*/children` from `root`. The flag reports whether
/// the walk stopped at [`MAX_TRACKED_DESCENDANTS`]; callers decide whether
/// (and how often) that deserves a warning.
#[cfg(target_os = "linux")]
fn linux_descendants_subtree_walk(
    proc_root: &Path,
    root: libc::pid_t,
) -> Result<(Vec<(libc::pid_t, u64)>, bool), ()> {
    let mut descendants = Vec::new();
    let mut pending = VecDeque::from([(root, 0_usize)]);
    let mut visited = std::collections::BTreeSet::from([root]);
    let mut capped = false;
    while let Some((parent, depth)) = pending.pop_front() {
        let task_root = proc_root.join(parent.to_string()).join("task");
        let tasks = match proc_read_dir(&task_root) {
            Ok(tasks) => tasks,
            Err(_) if parent != root => continue,
            Err(_) => return Err(()),
        };
        for task in tasks {
            let task = match task {
                Ok(task) => task,
                Err(_) if parent != root => continue,
                Err(_) => return Err(()),
            };
            let Some(tid) = task
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            let children_path = task_root.join(tid.to_string()).join("children");
            let children = match proc_read_to_string(&children_path) {
                Ok(children) => children,
                Err(_) if parent != root => continue,
                Err(_) => return Err(()),
            };
            for child in children.split_whitespace() {
                let Ok(child) = child.parse::<libc::pid_t>() else {
                    continue;
                };
                if !visited.insert(child) {
                    continue;
                }
                if descendants.len() == MAX_TRACKED_DESCENDANTS {
                    capped = true;
                    break;
                }
                let Ok(stat) = proc_read_to_string(&proc_root.join(child.to_string()).join("stat"))
                else {
                    continue;
                };
                let Some((_, start_time)) = proc_stat_ppid_start(&stat) else {
                    continue;
                };
                descendants.push((child, start_time));
                if depth < MAX_DESCENDANT_DEPTH {
                    pending.push_back((child, depth + 1));
                }
            }
            if capped {
                break;
            }
        }
        if capped {
            break;
        }
    }
    Ok((descendants, capped))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn linux_descendants(_root: libc::pid_t) -> Vec<(libc::pid_t, u64)> {
    Vec::new()
}

/// Cheap periodic descendant refresh for a live one-shot plugin run: the
/// `/proc/<pid>/task/*/children` subtree walk from `root` ONLY.
///
/// Unlike [`linux_descendants`], a failed root walk returns an empty list and
/// NEVER falls back to the full `/proc` scan: this runs every few tens of
/// milliseconds while a plugin is alive, and a full `/proc` walk per poll is
/// exactly the cost THE-1996 removed. The walk's own cap is silent here;
/// [`merge_tracked`] warns once when the tracked set fills.
#[cfg(target_os = "linux")]
pub(crate) fn linux_descendants_subtree_only(root: libc::pid_t) -> Vec<(libc::pid_t, u64)> {
    #[cfg(test)]
    {
        if let Some(proc_root) = SUBTREE_PROC_ROOT.with(|cell| cell.borrow().clone()) {
            return linux_descendants_subtree_only_in(&proc_root, root);
        }
    }
    linux_descendants_subtree_only_in(Path::new("/proc"), root)
}

#[cfg(target_os = "linux")]
fn linux_descendants_subtree_only_in(
    proc_root: &Path,
    root: libc::pid_t,
) -> Vec<(libc::pid_t, u64)> {
    linux_descendants_subtree_walk(proc_root, root)
        .map(|(descendants, _capped)| descendants)
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn linux_descendants_subtree_only(_root: libc::pid_t) -> Vec<(libc::pid_t, u64)> {
    Vec::new()
}

/// Run `f` with [`linux_descendants_subtree_only`] reading `proc_root`
/// instead of `/proc` on this thread (the supervising thread of a plugin run).
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn with_subtree_proc_root_for_test<R>(proc_root: &Path, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<std::path::PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            SUBTREE_PROC_ROOT.with(|cell| *cell.borrow_mut() = previous);
        }
    }
    let previous =
        SUBTREE_PROC_ROOT.with(|cell| cell.borrow_mut().replace(proc_root.to_path_buf()));
    let _restore = Restore(previous);
    f()
}

#[cfg(all(test, not(target_os = "linux")))]
pub(crate) fn with_subtree_proc_root_for_test<R>(
    _proc_root: &std::path::Path,
    f: impl FnOnce() -> R,
) -> R {
    f()
}

/// Full `/proc` scans performed on this thread (Linux tests only).
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn global_proc_scans_for_test() -> usize {
    GLOBAL_PROC_SCANS.with(std::cell::Cell::get)
}

/// Merge a fresh descendant snapshot into the run's tracked set, deduplicated
/// by `(pid, start_time)` and capped at [`MAX_TRACKED_DESCENDANTS`]. The set
/// only grows during a run, so the cap warning fires once: on the merge that
/// fills it.
pub(crate) fn merge_tracked(into: &mut Vec<(libc::pid_t, u64)>, fresh: Vec<(libc::pid_t, u64)>) {
    if fresh.is_empty() {
        return;
    }
    let was_full = into.len() >= MAX_TRACKED_DESCENDANTS;
    let mut seen = into
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    for entry in fresh {
        if into.len() >= MAX_TRACKED_DESCENDANTS {
            break;
        }
        if seen.insert(entry) {
            into.push(entry);
        }
    }
    if !was_full && into.len() >= MAX_TRACKED_DESCENDANTS {
        tracing::warn!(
            target: "thegn::plugin",
            limit = MAX_TRACKED_DESCENDANTS,
            "plugin tracked-descendant set reached its process limit"
        );
    }
}

#[cfg(target_os = "linux")]
/// SIGKILL every tracked descendant whose current start time still matches
/// the snapshot (the pid-reuse guard), returning how many were signalled.
pub(crate) fn kill_tracked(pids: &[(libc::pid_t, u64)]) -> usize {
    let mut signalled = 0;
    for &(pid, start_time) in pids {
        let stat_path = format!("/proc/{pid}/stat");
        let Ok(stat) = proc_read_to_string(Path::new(&stat_path)) else {
            continue;
        };
        if !proc_stat_ppid_start(&stat)
            .is_some_and(|(_, current_start)| current_start == start_time)
        {
            continue;
        }
        // SAFETY: plain signal syscall on a pid whose start time was just
        // re-verified against the tracked snapshot.
        if unsafe { libc::kill(pid, libc::SIGKILL) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                tracing::debug!(target: "thegn::plugin", pid, %error, "tracked plugin descendant kill failed");
            }
        } else {
            signalled += 1;
        }
    }
    signalled
}

/// Live (state not Z/X) members of process group `pgid` among `tracked`,
/// start-time checked. O(tracked), never a /proc-wide scan.
#[cfg(target_os = "linux")]
pub(crate) fn tracked_live_in_group(tracked: &[(libc::pid_t, u64)], pgid: libc::pid_t) -> bool {
    #[cfg(test)]
    {
        if let Some(proc_root) = SUBTREE_PROC_ROOT.with(|cell| cell.borrow().clone()) {
            return tracked_live_in_group_in(&proc_root, tracked, pgid);
        }
    }
    tracked_live_in_group_in(Path::new("/proc"), tracked, pgid)
}

#[cfg(target_os = "linux")]
fn tracked_live_in_group_in(
    proc_root: &Path,
    tracked: &[(libc::pid_t, u64)],
    pgid: libc::pid_t,
) -> bool {
    tracked.iter().any(|&(pid, start_time)| {
        let Ok(stat) = proc_read_to_string(&proc_root.join(pid.to_string()).join("stat")) else {
            return false;
        };
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            return false;
        };
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        let live = fields
            .first()
            .is_some_and(|state| !matches!(*state, "Z" | "X"));
        let in_group = fields.get(2).and_then(|f| f.parse::<libc::pid_t>().ok()) == Some(pgid);
        let same_start = fields.get(19).and_then(|f| f.parse::<u64>().ok()) == Some(start_time);
        live && in_group && same_start
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn tracked_live_in_group(_tracked: &[(libc::pid_t, u64)], _pgid: libc::pid_t) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn kill_tracked(_pids: &[(libc::pid_t, u64)]) -> usize {
    0
}

pub(crate) fn platform_is_executable_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

pub(crate) fn platform_executable_trust(metadata: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    if metadata.mode() & 0o002 != 0 {
        return Err("is world-writable".to_string());
    }
    let uid = metadata.uid();
    // SAFETY: geteuid has no preconditions and returns the calling process uid.
    let current_uid = unsafe { libc::geteuid() };
    if uid != current_uid && uid != 0 {
        return Err(format!("is owned by uid {uid}, not by you or root"));
    }
    Ok(())
}

pub(crate) fn platform_executable_identity_metadata(
    metadata: &std::fs::Metadata,
) -> (Option<u32>, Option<u32>) {
    use std::os::unix::fs::MetadataExt;

    (Some(metadata.uid()), Some(metadata.mode() & 0o7777))
}

pub(crate) fn platform_voice_model_identity_metadata(
    metadata: &std::fs::Metadata,
) -> (Option<u64>, Option<u64>, Option<i64>, Option<i64>) {
    use std::os::unix::fs::MetadataExt;

    (
        Some(metadata.dev()),
        Some(metadata.ino()),
        Some(metadata.ctime()),
        Some(metadata.ctime_nsec()),
    )
}

#[cfg(test)]
pub(crate) fn platform_set_mode_for_test(path: &std::path::Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
pub(crate) fn platform_non_utf8_path_for_test(
    parent: &std::path::Path,
) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    Some(parent.join(std::ffi::OsStr::from_bytes(b"src-\xff.rs")))
}

pub(crate) fn platform_os_string_from_bytes(bytes: Vec<u8>) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;

    Some(std::ffi::OsString::from_vec(bytes))
}

pub(crate) fn platform_is_transient_accept_resource_error(raw_os_error: Option<i32>) -> bool {
    matches!(
        raw_os_error,
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM)
    )
}

#[cfg(test)]
pub(crate) fn platform_accept_resource_error_for_test() -> io::Error {
    io::Error::from_raw_os_error(libc::EMFILE)
}

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
                // SAFETY: `owned` is the pidfd just opened above; it moves into the
                // AsyncFd and nothing else closes or replaces it.
                match unsafe { tokio::io::unix::AsyncFd::register(owned) }.map_err(io::Error::from)
                {
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
                #[cfg(test)]
                wait_calls: 0,
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
        if matches!(self.leader.exited_without_reaping(), Ok(true))
            && let Err(error) = self.leader.reap()
        {
            tracing::error!(target: "thegn::plugin", %error, "final nonblocking reap failed");
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
    #[cfg(test)]
    wait_calls: usize,
}

impl Leader {
    pub fn reaped_status(&self) -> Option<ExitStatus> {
        self.status
    }

    fn exited_without_reaping(&mut self) -> io::Result<bool> {
        if self.identity_lost {
            return Err(io::Error::other(
                "leader identity was lost; observation forbidden",
            ));
        }
        let Some(child) = self.child.as_ref() else {
            return Ok(true);
        };
        // SAFETY: zeroed siginfo is a valid output buffer; WNOHANG avoids a
        // blocking wait and WNOWAIT retains identity until group signaling ends.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        #[cfg(test)]
        {
            self.wait_calls += 1;
        }
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
        let leader_pid = child.id() as libc::pid_t;
        let tracked = {
            #[cfg(test)]
            if self.signal_override.is_some() {
                Vec::new()
            } else {
                linux_descendants(leader_pid)
            }
            #[cfg(not(test))]
            {
                linux_descendants(leader_pid)
            }
        };
        let group = -(child.id() as libc::pid_t);
        let mut group_result = self.signal_group(group);
        // macOS can fail kill(-pgid) with EPERM for a zombie-only group. Verify
        // every member before treating that result as an empty group.
        let zombie_group = cfg!(target_os = "macos")
            && group_result
                .as_ref()
                .is_err_and(|error| error.raw_os_error() == Some(libc::EPERM))
            && !thegn_proc::group_has_live_members(leader_pid as u32);
        if zombie_group {
            group_result = Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        // Group success does not prove the leader still belonged to that
        // group. Ensure this exclusively owned, unreaped child terminates too;
        // never discover or signal its potentially changed process group.
        let leader_result = self
            .child
            .as_mut()
            .expect("unreaped child ownership")
            .kill()
            // A leader already observed exited needs no signal; how a platform
            // reports signalling its zombie does not matter.
            .or_else(|error| if zombie_group { Ok(()) } else { Err(error) });
        kill_tracked(&tracked);
        // macOS answers EPERM (not ESRCH) when signalling a group or pid whose only
        // member is an exited-but-unreaped zombie. The leader is unreaped here, so
        // a still-observable exit means there is nothing left to signal.
        let saw_eperm = [&group_result, &leader_result]
            .iter()
            .any(|r| matches!(r, Err(e) if e.raw_os_error() == Some(libc::EPERM)));
        let leader_gone = cfg!(target_os = "macos")
            && saw_eperm
            && matches!(self.exited_without_reaping(), Ok(true));
        let eperm_is_gone = |error: &io::Error| {
            cfg!(target_os = "macos") && leader_gone && error.raw_os_error() == Some(libc::EPERM)
        };
        let group_result = match group_result {
            Err(error) if eperm_is_gone(&error) => Ok(()),
            other => other,
        };
        let leader_result = match leader_result {
            Err(error) if eperm_is_gone(&error) => Ok(()),
            other => other,
        };
        match (group_result, leader_result) {
            (Ok(()), leader) => leader,
            (Err(group), leader) if group.raw_os_error() == Some(libc::ESRCH) => leader,
            (Err(group), Ok(())) => Err(group),
            (Err(group), Err(leader)) => Err(io::Error::other(format!(
                "group termination: {group}; leader termination: {leader}"
            ))),
        }
    }

    fn signal_group(&mut self, group: libc::pid_t) -> io::Result<()> {
        #[cfg(test)]
        if let Some(signal) = self.signal_override.as_mut() {
            return signal(group);
        }
        // SAFETY: terminate checked still-owned/unreaped identity. The initial
        // group was created before exec; no post-reap numeric fallback exists.
        if unsafe { libc::kill(group, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
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
        if self.identity_lost {
            return Err(io::Error::other(
                "leader identity was lost; consuming wait forbidden",
            ));
        }
        #[cfg(test)]
        {
            self.wait_calls += 1;
        }
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
    #[cfg(target_os = "linux")]
    use std::fs;
    use std::process::Stdio;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    fn fake_proc_stat(pid: libc::pid_t, ppid: libc::pid_t, start_time: u64) -> String {
        let mut fields = vec!["0".to_owned(); 20];
        fields[0] = "S".to_owned();
        fields[1] = ppid.to_string();
        fields[19] = start_time.to_string();
        format!("{pid} (fixture with spaces ) {}", fields.join(" "))
    }

    #[cfg(target_os = "linux")]
    fn fake_proc_process(root: &std::path::Path, pid: libc::pid_t, stat: &str, children: &str) {
        let task = root
            .join(pid.to_string())
            .join("task")
            .join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(root.join(pid.to_string()).join("stat"), stat).unwrap();
        fs::write(task.join("children"), children).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_subtree_fast_path_avoids_global_scan() {
        let root = tempfile::tempdir().unwrap();
        let leader = 501;
        fake_proc_process(
            root.path(),
            leader,
            &fake_proc_stat(leader, 1, 10),
            "502 503",
        );
        fake_proc_process(root.path(), 502, &fake_proc_stat(502, leader, 11), "");
        fake_proc_process(root.path(), 503, &fake_proc_stat(503, leader, 12), "");
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));
        PROC_READS.with(|reads| reads.set(0));

        let descendants = linux_descendants_in(root.path(), leader);
        assert_eq!(descendants, vec![(502, 11), (503, 12)]);
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 0));
        PROC_READS.with(|reads| assert!(reads.get() > 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_missing_children_falls_back_to_full_scan() {
        let root = tempfile::tempdir().unwrap();
        let leader = 601;
        let task = root
            .path()
            .join(leader.to_string())
            .join("task")
            .join(leader.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(
            root.path().join(leader.to_string()).join("stat"),
            fake_proc_stat(leader, 1, 20),
        )
        .unwrap();
        fake_proc_process(root.path(), 602, &fake_proc_stat(602, leader, 21), "");
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));

        assert_eq!(linux_descendants_in(root.path(), leader), vec![(602, 21)]);
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_subtree_only_never_falls_back_to_full_scan() {
        let root = tempfile::tempdir().unwrap();
        // Root walk fails (no `children` file): the periodic refresh must give
        // up empty-handed instead of scanning all of /proc.
        let leader = 611;
        let task = root
            .path()
            .join(leader.to_string())
            .join("task")
            .join(leader.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(
            root.path().join(leader.to_string()).join("stat"),
            fake_proc_stat(leader, 1, 20),
        )
        .unwrap();
        fake_proc_process(root.path(), 612, &fake_proc_stat(612, leader, 21), "");
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));

        assert!(linux_descendants_subtree_only_in(root.path(), leader).is_empty());
        // A missing root entirely is the same failure.
        assert!(linux_descendants_subtree_only_in(root.path(), 99_999).is_empty());
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_subtree_only_walks_the_subtree() {
        let root = tempfile::tempdir().unwrap();
        let leader = 621;
        fake_proc_process(root.path(), leader, &fake_proc_stat(leader, 1, 10), "622");
        fake_proc_process(root.path(), 622, &fake_proc_stat(622, leader, 11), "623");
        fake_proc_process(root.path(), 623, &fake_proc_stat(623, 622, 12), "");
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));

        assert_eq!(
            linux_descendants_subtree_only_in(root.path(), leader),
            vec![(622, 11), (623, 12)]
        );
        assert_eq!(
            with_subtree_proc_root_for_test(root.path(), || linux_descendants_subtree_only(leader)),
            vec![(622, 11), (623, 12)]
        );
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tracked_live_in_group_checks_state_group_and_start_without_dir_reads() {
        let root = tempfile::tempdir().unwrap();
        let stat = |pid: libc::pid_t, state: &str, pgrp: libc::pid_t, start: u64| {
            let mut fields = vec!["0".to_owned(); 20];
            fields[0] = state.to_owned();
            fields[1] = "1".to_owned();
            fields[2] = pgrp.to_string();
            fields[19] = start.to_string();
            format!("{pid} (fixture ) {}", fields.join(" "))
        };
        fake_proc_process(root.path(), 801, &stat(801, "S", 800, 10), "");
        fake_proc_process(root.path(), 802, &stat(802, "Z", 800, 11), "");
        fake_proc_process(root.path(), 803, &stat(803, "S", 999, 12), "");
        fake_proc_process(root.path(), 804, &stat(804, "S", 800, 99), "");
        PROC_DIR_READS.with(|reads| reads.set(0));
        assert!(tracked_live_in_group_in(root.path(), &[(801, 10)], 800));
        assert!(
            !tracked_live_in_group_in(root.path(), &[(802, 11)], 800),
            "zombie"
        );
        assert!(
            !tracked_live_in_group_in(root.path(), &[(803, 12)], 800),
            "other group"
        );
        assert!(
            !tracked_live_in_group_in(root.path(), &[(804, 10)], 800),
            "start mismatch"
        );
        assert!(
            !tracked_live_in_group_in(root.path(), &[(805, 1)], 800),
            "missing"
        );
        assert!(tracked_live_in_group_in(
            root.path(),
            &[(802, 11), (801, 10)],
            800
        ));
        assert!(!tracked_live_in_group_in(root.path(), &[], 800));
        PROC_DIR_READS.with(|reads| assert_eq!(reads.get(), 0, "no /proc directory listing"));
    }

    #[test]
    fn merge_tracked_dedupes_by_pid_and_start_time() {
        let mut tracked = vec![(10, 100)];
        merge_tracked(
            &mut tracked,
            vec![(10, 100), (10, 101), (11, 110), (11, 110), (10, 100)],
        );
        // Same pid with a different start time is a different process.
        assert_eq!(tracked, vec![(10, 100), (10, 101), (11, 110)]);
        merge_tracked(&mut tracked, Vec::new());
        assert_eq!(tracked, vec![(10, 100), (10, 101), (11, 110)]);
    }

    #[test]
    fn merge_tracked_caps_at_the_descendant_limit() {
        let mut tracked = (0..(MAX_TRACKED_DESCENDANTS as libc::pid_t - 1))
            .map(|pid| (pid, 1))
            .collect::<Vec<_>>();
        let first_new = MAX_TRACKED_DESCENDANTS as libc::pid_t;
        merge_tracked(
            &mut tracked,
            vec![(first_new, 1), (first_new + 1, 1), (first_new + 2, 1)],
        );
        assert_eq!(tracked.len(), MAX_TRACKED_DESCENDANTS);
        assert_eq!(tracked.last(), Some(&(first_new, 1)));
        merge_tracked(&mut tracked, vec![(first_new + 3, 1)]);
        assert_eq!(tracked.len(), MAX_TRACKED_DESCENDANTS);
        assert!(!tracked.contains(&(first_new + 3, 1)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_ignores_exited_descendant_task_race() {
        let root = tempfile::tempdir().unwrap();
        let leader = 701;
        fake_proc_process(root.path(), leader, &fake_proc_stat(leader, 1, 30), "702");
        fs::create_dir_all(root.path().join("702")).unwrap();
        fs::write(
            root.path().join("702").join("stat"),
            fake_proc_stat(702, leader, 31),
        )
        .unwrap();
        GLOBAL_PROC_SCANS.with(|scans| scans.set(0));

        assert_eq!(linux_descendants_in(root.path(), leader), vec![(702, 31)]);
        GLOBAL_PROC_SCANS.with(|scans| assert_eq!(scans.get(), 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_subtree_respects_node_cap() {
        let root = tempfile::tempdir().unwrap();
        let leader = 650;
        let ids = (7000..(7000 + MAX_TRACKED_DESCENDANTS as i32 + 1)).collect::<Vec<_>>();
        let children = ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        fake_proc_process(
            root.path(),
            leader,
            &fake_proc_stat(leader, 1, 30),
            &children,
        );
        for (offset, pid) in ids.iter().enumerate() {
            fs::create_dir_all(root.path().join(pid.to_string())).unwrap();
            fs::write(
                root.path().join(pid.to_string()).join("stat"),
                fake_proc_stat(*pid, leader, 31 + offset as u64),
            )
            .unwrap();
        }

        assert_eq!(
            linux_descendants_subtree(root.path(), leader)
                .unwrap()
                .len(),
            MAX_TRACKED_DESCENDANTS
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_stat_parser_rejects_truncated_input() {
        assert_eq!(proc_stat_ppid_start("9 (name with ) spaces) S 1 9 0"), None);
        assert_eq!(proc_stat_ppid_start("garbage"), None);
    }

    #[test]
    fn a_child_handed_to_the_reaper_is_eventually_reaped() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        crate::plugin::proc::set_process_group(&mut command);
        let child = command.spawn().unwrap();
        let pid = child.id() as i32;
        crate::plugin::proc::kill_group(child.id());
        crate::plugin::proc::hand_off_unreaped_child(child);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // A zombie still answers kill(pid, 0); ESRCH proves the detached
            // waiter reaped the child rather than merely dropping its handle.
            let result = unsafe { libc::kill(pid, 0) };
            if result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "detached reaper left child {pid} unreaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
        let waits = process.leader.wait_calls;
        assert!(process.leader.exited_without_reaping().is_err());
        assert!(process.leader.consume_ready().is_err());
        assert!(process.leader.reap().is_err());
        assert_eq!(
            process.leader.wait_calls, waits,
            "lost identity reached another numeric wait syscall"
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
    #[test]
    fn resident_unix_group_result_always_terminates_exact_owned_leader() {
        for group_exists in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let entered = runtime.enter();
            let mut command = Command::new("sh");
            // Shell builtin only: no descendant or unrelated group is created.
            command
                .args(["-c", "read -r fixture"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut process = Prepared::new().unwrap().spawn(command).unwrap();
            drop(entered);
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            process.leader.signal_override = Some(Box::new(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                if group_exists {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(libc::ESRCH))
                }
            }));
            process.leader.terminate().unwrap();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(1), process.leader.wait_ready())
                    .await
                    .unwrap()
                    .unwrap();
            });
            assert!(process.leader.reaped_status().is_none());
            assert!(!process.leader.reap().unwrap().success());
            assert!(process.leader.terminate().is_err());
            assert_eq!(
                calls.load(Ordering::Relaxed),
                1,
                "reaped identity reached signaling again"
            );
            assert!(runtime.block_on(process.settle_pipes(Instant::now())));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resident_macos_eperm_for_zombie_group_is_gone_without_reaping_leader() {
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
        process.leader.signal_override =
            Some(Box::new(|_| Err(io::Error::from_raw_os_error(libc::EPERM))));

        process.leader.terminate().unwrap();

        assert!(process.leader.reaped_status().is_none());
        assert!(process.leader.reap().unwrap().success());
        runtime.block_on(process.settle_pipes(Instant::now()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_stat_parses_ppid_and_start_time() {
        let mut fields = vec!["0".to_string(); 20];
        fields[0] = "S".to_string();
        fields[1] = "123".to_string();
        fields[19] = "98765".to_string();
        let stat = format!("42 (a) b (c) {}", fields.join(" "));

        assert_eq!(proc_stat_ppid_start(&stat), Some((123, 98765)));
        assert_eq!(proc_stat_ppid_start("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descendants_walks_a_tree_and_skips_the_root() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let child_pid = child.id() as libc::pid_t;
        let descendants = linux_descendants(std::process::id() as libc::pid_t);
        let child_result = child.kill();
        let child_status = child.wait().unwrap();

        assert!(
            child_result.is_ok(),
            "fixture child should still be killable"
        );
        assert!(!child_status.success());
        assert!(
            descendants.iter().any(|(pid, _)| *pid == child_pid),
            "snapshot did not include direct child {child_pid}"
        );
        assert!(
            descendants
                .iter()
                .all(|(pid, _)| *pid != std::process::id() as libc::pid_t),
            "snapshot included its root"
        );
    }

    #[cfg(target_os = "linux")]
    fn setsid_available() -> bool {
        Command::new("setsid")
            .arg("true")
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resident_linux_terminate_kills_a_setsid_grandchild() {
        if !setsid_available() {
            return;
        }
        let pid_dir = tempfile::tempdir().unwrap();
        let pid_file = pid_dir.path().join("grandchild.pid");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                r#"setsid sh -c 'echo $$ > "$1"; exec sleep 300' _ "$1" & wait"#,
                "_",
                pid_file.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Prepared::new().unwrap().spawn(command).unwrap();
        let leader_pid = process.leader.child.as_ref().unwrap().id() as libc::pid_t;
        drop(entered);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            if std::time::Instant::now() >= deadline {
                process.leader.terminate().unwrap();
                runtime.block_on(async {
                    process.leader.wait_ready().await.unwrap();
                });
                process.leader.reap().unwrap();
                panic!("setsid grandchild did not publish its pid");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let stat_path = format!("/proc/{grandchild_pid}/stat");
        let start_time = loop {
            if let Ok(stat) = std::fs::read_to_string(&stat_path)
                && let Some((_, start_time)) = proc_stat_ppid_start(&stat)
            {
                break start_time;
            }
            if std::time::Instant::now() >= deadline {
                process.leader.terminate().unwrap();
                runtime.block_on(async {
                    process.leader.wait_ready().await.unwrap();
                });
                process.leader.reap().unwrap();
                panic!("setsid grandchild {grandchild_pid} vanished before its stat was read");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let grandchild_group = unsafe { libc::getpgid(grandchild_pid) };
        let leader_group = unsafe { libc::getpgid(leader_pid) };
        process.leader.terminate().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut grandchild_stopped = false;
        while std::time::Instant::now() < deadline {
            match std::fs::read_to_string(&stat_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    grandchild_stopped = true;
                    break;
                }
                Ok(stat)
                    if stat
                        .rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().next())
                        == Some("Z") =>
                {
                    grandchild_stopped = true;
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        if !grandchild_stopped {
            kill_tracked(&[(grandchild_pid, start_time)]);
        }
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), process.leader.wait_ready())
                .await
                .unwrap()
                .unwrap();
        });
        process.leader.reap().unwrap();

        assert_ne!(
            grandchild_group, leader_group,
            "fixture did not escape its group"
        );
        assert!(
            grandchild_stopped,
            "setsid grandchild {grandchild_pid} survived resident termination"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resident_linux_terminate_spares_an_unrelated_process() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        let mut command = Command::new("sh");
        command
            .args(["-c", "read -r fixture"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut plugin = Prepared::new().unwrap().spawn(command).unwrap();
        drop(entered);

        let mut unrelated = Command::new("sleep");
        unrelated.arg("30").process_group(0);
        let mut unrelated = unrelated.spawn().unwrap();
        plugin.leader.terminate().unwrap();
        let unrelated_alive = unsafe { libc::kill(unrelated.id() as libc::pid_t, 0) } == 0;

        unrelated.kill().unwrap();
        let unrelated_status = unrelated.wait().unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), plugin.leader.wait_ready())
                .await
                .unwrap()
                .unwrap();
        });
        plugin.leader.reap().unwrap();

        assert!(
            unrelated_alive,
            "unrelated process was signaled by plugin teardown"
        );
        assert!(!unrelated_status.success());
    }
}
