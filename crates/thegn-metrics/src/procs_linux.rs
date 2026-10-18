//! Native Linux process table backed by held `/proc/<pid>/stat` descriptors.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::PathBuf;
use std::time::Duration;

use crate::procs_fs::ProcFs;
use crate::procs_model::{ProcId, Stat};

const SMALL_FILE_CAP: usize = 4096;
const STAT_BUFFER_CAP: usize = 4096;
const MAX_KEPT_FDS: usize = 4096;

/// Linux implementation of [`ProcFs`].
#[derive(Debug)]
pub struct RealProcFs {
    root: File,
    root_path: PathBuf,
}

impl RealProcFs {
    /// Open the process filesystem at `/proc`.
    pub fn new() -> io::Result<Self> {
        Self::at("/proc")
    }

    /// Open a procfs-compatible tree, primarily useful for controlled callers.
    pub fn at(path: impl Into<PathBuf>) -> io::Result<Self> {
        let root_path = path.into();
        let root = File::open(&root_path)?;
        Ok(Self { root, root_path })
    }

    fn process_path(&self, pid: u32, name: &str) -> PathBuf {
        self.root_path.join(pid.to_string()).join(name)
    }
}

impl ProcFs for RealProcFs {
    type Stat = File;

    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        // `read_dir` allocates an entry/name per PID; raw getdents64 reuse is deferred.
        for entry in fs::read_dir(&self.root_path)? {
            let entry = entry?;
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            {
                out.push(pid);
            }
        }
        Ok(())
    }

    fn open_stat(&mut self, pid: u32) -> io::Result<Self::Stat> {
        let relative = CString::new(format!("{pid}/stat"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: `root` owns a valid directory fd and `relative` is a live,
        // NUL-terminated path for the duration of the call.
        let fd = unsafe {
            libc::openat(
                self.root.as_raw_fd(),
                relative.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `openat` returned a new fd, and ownership is transferred to
        // exactly one `File` here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn read_stat(&mut self, stat: &mut Self::Stat, buf: &mut [u8]) -> io::Result<usize> {
        stat.read_at(buf, 0)
    }

    fn read_small(&mut self, pid: u32, name: &str, buf: &mut [u8]) -> io::Result<usize> {
        fs::File::open(self.process_path(pid, name))?.read_at(buf, 0)
    }

    fn uid_of(&mut self, stat: &Self::Stat) -> io::Result<u32> {
        Ok(stat.metadata()?.uid())
    }

    fn close_stat(&mut self, stat: Self::Stat) {
        drop(stat);
    }
}

/// CPU counter movement over the elapsed monotonic interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuDelta {
    pub ticks: u64,
    pub elapsed: Duration,
    pub percent: Option<f64>,
}

/// One cached process row. Ghost rows have no live stat descriptor.
#[derive(Debug)]
pub struct ProcEntry<S> {
    pub id: ProcId,
    pub stat: Stat,
    pub uid: u32,
    pub cmdline: Vec<u8>,
    pub cgroup: Option<Vec<u8>>,
    pub cpu_delta: Option<CpuDelta>,
    pub ghost: bool,
    pub last_seen: Duration,
    pub last_visible: Duration,
    handle: Option<S>,
}

impl<S> ProcEntry<S> {
    /// Whether this row currently retains an open stat descriptor.
    pub const fn has_kept_fd(&self) -> bool {
        self.handle.is_some()
    }
}

/// Sorted process cache driven by repeated procfs passes.
#[derive(Debug)]
pub struct ProcTable<F: ProcFs> {
    fs: F,
    entries: Vec<ProcEntry<F::Stat>>,
    listed_pids: Vec<u32>,
    kept_max: usize,
    kept_count: usize,
    ticks_per_second: Option<u64>,
}

impl<F: ProcFs> ProcTable<F> {
    /// Construct with a caller-selected descriptor budget.
    pub fn with_kept_max(fs: F, kept_max: usize) -> Self {
        Self {
            fs,
            entries: Vec::new(),
            listed_pids: Vec::new(),
            kept_max: kept_max.min(MAX_KEPT_FDS),
            kept_count: 0,
            ticks_per_second: clock_ticks_per_second(),
        }
    }

    /// Sorted cached rows, including retained ghosts.
    pub fn entries(&self) -> &[ProcEntry<F::Stat>] {
        &self.entries
    }

    /// Shared access to the backing filesystem, for diagnostics and tests.
    pub fn fs(&self) -> &F {
        &self.fs
    }

    /// Mutable access to filesystem operation counters and controls.
    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    /// Reset rows and release every retained stat descriptor.
    pub fn reset(&mut self) {
        for mut entry in self.entries.drain(..) {
            if let Some(handle) = entry.handle.take() {
                self.fs.close_stat(handle);
            }
        }
        self.kept_count = 0;
    }

    /// Refresh the process table. `visible` lists rows that have recently been shown.
    pub fn pass(&mut self, now: Duration, visible: &[u32]) -> io::Result<()> {
        self.listed_pids.clear();
        self.fs.list_pids(&mut self.listed_pids)?;
        let was_sorted = self.listed_pids.windows(2).all(|pair| pair[0] < pair[1]);
        if !was_sorted {
            self.listed_pids.sort_unstable();
            self.listed_pids.dedup();
        }

        let mut index = 0;
        let mut listed_index = 0;
        while listed_index < self.listed_pids.len() {
            let pid = self.listed_pids[listed_index];
            while index < self.entries.len() && self.entries[index].id.pid < pid {
                self.mark_ghost(index);
                index += 1;
            }
            let visible_now = visible.contains(&pid);
            if index < self.entries.len() && self.entries[index].id.pid == pid {
                self.refresh_existing(index, now, visible_now)?;
                index += 1;
            } else {
                if self.open_new(index, pid, now, visible_now) {
                    index += 1;
                }
            }
            listed_index += 1;
        }
        while index < self.entries.len() {
            self.mark_ghost(index);
            index += 1;
        }
        Ok(())
    }

    fn mark_ghost(&mut self, index: usize) {
        // P-2: define a bounded ghost lifetime and prune old rows.
        let entry = &mut self.entries[index];
        entry.ghost = true;
        entry.cpu_delta = None;
        if let Some(handle) = entry.handle.take() {
            self.fs.close_stat(handle);
            self.kept_count -= 1;
        }
    }

    fn refresh_existing(&mut self, index: usize, now: Duration, visible: bool) -> io::Result<()> {
        let Some(mut handle) = self.entries[index].handle.take() else {
            let pid = self.entries[index].id.pid;
            match self.fs.open_stat(pid) {
                Ok(mut opened) => {
                    let result = self.refresh_from_handle(index, &mut opened, now, visible);
                    if self.entries[index].ghost {
                        self.fs.close_stat(opened);
                        return result;
                    }
                    if self.should_keep(now, visible) {
                        self.entries[index].handle = Some(opened);
                        self.kept_count += 1;
                    } else {
                        self.fs.close_stat(opened);
                    }
                    return result;
                }
                Err(error) if is_process_gone(&error) => {
                    self.mark_ghost(index);
                    return Ok(());
                }
                Err(_) => return Ok(()),
            }
        };
        self.kept_count -= 1;
        let result = self.refresh_from_handle(index, &mut handle, now, visible);
        if self.entries[index].ghost {
            self.fs.close_stat(handle);
            self.reopen_existing(index, now, visible)?;
        } else {
            self.entries[index].handle = Some(handle);
            self.kept_count += 1;
        }
        result
    }

    fn reopen_existing(&mut self, index: usize, now: Duration, visible: bool) -> io::Result<()> {
        let pid = self.entries[index].id.pid;
        let mut handle = match self.fs.open_stat(pid) {
            Ok(handle) => handle,
            Err(_) => return Ok(()),
        };
        let mut buffer = [0; STAT_BUFFER_CAP];
        let count = match self.fs.read_stat(&mut handle, &mut buffer) {
            Ok(count) => count,
            Err(_) => {
                self.fs.close_stat(handle);
                return Ok(());
            }
        };
        let Some(stat) = crate::procs_parse::parse_stat(&buffer[..count]) else {
            self.fs.close_stat(handle);
            return Ok(());
        };
        if stat.pid != pid {
            self.fs.close_stat(handle);
            return Ok(());
        }
        let metadata = initial_metadata(&mut self.fs, pid, &handle);
        match metadata {
            Ok((uid, cmdline, cgroup)) => {
                {
                    let entry = &mut self.entries[index];
                    entry.id = stat.id();
                    entry.stat = stat;
                    entry.uid = uid;
                    entry.cmdline = cmdline;
                    entry.cgroup = cgroup;
                    entry.cpu_delta = None;
                    entry.last_seen = now;
                    entry.ghost = false;
                    if visible {
                        entry.last_visible = now;
                    }
                }
                if self.should_keep(now, visible) {
                    self.entries[index].handle = Some(handle);
                    self.kept_count += 1;
                } else {
                    self.fs.close_stat(handle);
                }
            }
            Err(_) => self.fs.close_stat(handle),
        }
        Ok(())
    }

    fn should_keep(&mut self, now: Duration, visible: bool) -> bool {
        if self.kept_count < self.kept_max {
            return self.kept_max > 0;
        }
        if self.kept_max == 0 {
            return false;
        }
        let incoming_visible_at = if visible { now } else { Duration::ZERO };
        let victim = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.handle.is_some())
            .min_by_key(|(_, entry)| (entry.last_visible, entry.id.pid))
            .map(|(victim, _)| victim);
        if let Some(victim) = victim
            && incoming_visible_at > self.entries[victim].last_visible
            && let Some(old_handle) = self.entries[victim].handle.take()
        {
            self.fs.close_stat(old_handle);
            self.kept_count -= 1;
            return true;
        }
        false
    }

    fn refresh_from_handle(
        &mut self,
        index: usize,
        handle: &mut F::Stat,
        now: Duration,
        visible: bool,
    ) -> io::Result<()> {
        let mut buffer = [0; STAT_BUFFER_CAP];
        let count = match self.fs.read_stat(handle, &mut buffer) {
            Ok(0) => {
                self.mark_ghost(index);
                return Ok(());
            }
            Ok(count) => count,
            Err(error) if is_process_gone(&error) => {
                self.mark_ghost(index);
                return Ok(());
            }
            Err(_) => return Ok(()),
        };
        let Some(stat) = crate::procs_parse::parse_stat(&buffer[..count]) else {
            self.mark_ghost(index);
            return Ok(());
        };
        if stat.pid != self.entries[index].id.pid {
            self.mark_ghost(index);
            return Ok(());
        }
        let identity_changed = stat.starttime != self.entries[index].id.start_ticks;
        let identity_fields = if identity_changed {
            let uid = match self.fs.uid_of(handle) {
                Ok(uid) => uid,
                Err(error) if is_process_gone(&error) => {
                    self.mark_ghost(index);
                    return Ok(());
                }
                Err(_) => return Ok(()),
            };
            let cmdline = match read_small_capped(&mut self.fs, stat.pid, "cmdline") {
                Ok(cmdline) => cmdline,
                Err(error) if is_process_gone(&error) => {
                    self.mark_ghost(index);
                    return Ok(());
                }
                Err(error) if is_permission_denied(&error) => Vec::new(),
                Err(_) => return Ok(()),
            };
            let cgroup = match read_cgroup(&mut self.fs, stat.pid) {
                Ok(cgroup) => cgroup,
                Err(error) if is_process_gone(&error) => {
                    self.mark_ghost(index);
                    return Ok(());
                }
                Err(error) if is_permission_denied(&error) => None,
                Err(_) => return Ok(()),
            };
            Some((uid, cmdline, cgroup))
        } else {
            None
        };
        let old = &mut self.entries[index];
        if let Some((uid, cmdline, cgroup)) = identity_fields {
            old.id = stat.id();
            old.cpu_delta = None;
            old.uid = uid;
            old.cmdline = cmdline;
            old.cgroup = cgroup;
        } else {
            let ticks = stat
                .utime
                .saturating_sub(old.stat.utime)
                .saturating_add(stat.stime.saturating_sub(old.stat.stime));
            old.cpu_delta = Some(CpuDelta {
                ticks,
                elapsed: now.saturating_sub(old.last_seen),
                percent: cpu_percent(
                    ticks,
                    now.saturating_sub(old.last_seen),
                    self.ticks_per_second,
                ),
            });
        }
        old.stat = stat;
        old.last_seen = now;
        old.ghost = false;
        if visible {
            old.last_visible = now;
        }
        Ok(())
    }

    fn open_new(&mut self, index: usize, pid: u32, now: Duration, visible: bool) -> bool {
        let mut handle = match self.fs.open_stat(pid) {
            Ok(handle) => handle,
            Err(_) => return false,
        };
        let mut buffer = [0; STAT_BUFFER_CAP];
        let count = match self.fs.read_stat(&mut handle, &mut buffer) {
            Ok(count) => count,
            Err(error) if is_process_gone(&error) => {
                self.fs.close_stat(handle);
                return false;
            }
            Err(_) => {
                self.fs.close_stat(handle);
                return false;
            }
        };
        let Some(stat) = crate::procs_parse::parse_stat(&buffer[..count]) else {
            self.fs.close_stat(handle);
            return false;
        };
        if stat.pid != pid {
            self.fs.close_stat(handle);
            return false;
        }
        let metadata = initial_metadata(&mut self.fs, pid, &handle);
        let (uid, cmdline, cgroup) = match metadata {
            Ok(metadata) => metadata,
            Err(_) => {
                self.fs.close_stat(handle);
                return false;
            }
        };
        let incoming_visible_at = if visible { now } else { Duration::ZERO };
        let keep = self.should_keep(now, visible);
        let kept_handle = if keep {
            self.kept_count += 1;
            Some(handle)
        } else {
            self.fs.close_stat(handle);
            None
        };
        self.entries.insert(
            index,
            ProcEntry {
                id: stat.id(),
                stat,
                uid,
                cmdline,
                cgroup,
                cpu_delta: None,
                ghost: false,
                last_seen: now,
                last_visible: incoming_visible_at,
                handle: kept_handle,
            },
        );
        true
    }
}

fn read_small_capped<F: ProcFs>(fs: &mut F, pid: u32, name: &str) -> io::Result<Vec<u8>> {
    let mut buf = [0; SMALL_FILE_CAP];
    let count = fs.read_small(pid, name, &mut buf)?;
    Ok(buf[..count.min(buf.len())].to_vec())
}

fn initial_metadata<F: ProcFs>(
    fs: &mut F,
    pid: u32,
    handle: &F::Stat,
) -> io::Result<(u32, Vec<u8>, Option<Vec<u8>>)> {
    let uid = fs.uid_of(handle)?;
    let cmdline = match read_small_capped(fs, pid, "cmdline") {
        Ok(cmdline) => cmdline,
        Err(error) if is_permission_denied(&error) => Vec::new(),
        Err(error) => return Err(error),
    };
    let cgroup = match read_cgroup(fs, pid) {
        Ok(cgroup) => cgroup,
        Err(error) if is_permission_denied(&error) => None,
        Err(error) => return Err(error),
    };
    Ok((uid, cmdline, cgroup))
}

fn read_cgroup<F: ProcFs>(fs: &mut F, pid: u32) -> io::Result<Option<Vec<u8>>> {
    let bytes = read_small_capped(fs, pid, "cgroup")?;
    Ok(crate::procs_parse::unified_cgroup_path(&bytes).map(<[u8]>::to_vec))
}

fn is_process_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
    ) || error.raw_os_error() == Some(libc::ESRCH)
}

fn is_permission_denied(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::PermissionDenied)
        || matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
}

fn clock_ticks_per_second() -> Option<u64> {
    // SAFETY: `sysconf` reads a process-wide constant and has no pointer arguments.
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks_per_second)
        .ok()
        .filter(|value| *value > 0)
}

fn cpu_percent(ticks: u64, elapsed: Duration, ticks_per_second: Option<u64>) -> Option<f64> {
    let ticks_per_second = ticks_per_second?;
    if elapsed.is_zero() {
        return None;
    }
    Some(ticks as f64 * 100.0 / ticks_per_second as f64 / elapsed.as_secs_f64())
}

/// Descriptor budget derived from the soft `RLIMIT_NOFILE` and capped at 4096.
pub fn kept_max_from_rlimit() -> usize {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid writable `rlimit` structure for this call.
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if result != 0 {
        return 0;
    }
    usize::try_from(limit.rlim_cur)
        .unwrap_or(usize::MAX)
        .saturating_div(4)
        .min(MAX_KEPT_FDS)
}

impl ProcTable<RealProcFs> {
    /// Construct a production table with a descriptor budget from the OS limit.
    pub fn new(fs: RealProcFs) -> Self {
        Self::with_kept_max(fs, kept_max_from_rlimit())
    }
}

#[cfg(test)]
#[path = "procs_linux_tests.rs"]
mod tests;
