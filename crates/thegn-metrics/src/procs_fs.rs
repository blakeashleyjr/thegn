//! Filesystem seam for process-table readers.

use std::collections::{BTreeMap, VecDeque};
use std::io;

/// Operations needed by the Linux process-table implementation.
///
/// Implementations report a process that has disappeared as `NotFound`; the
/// table also recognizes Linux `ESRCH` from held stat descriptors.
pub trait ProcFs {
    /// Open stat handle type kept between sampling passes.
    type Stat;

    /// Append the currently visible process IDs.
    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()>;
    /// Open a process's stat file.
    fn open_stat(&mut self, pid: u32) -> io::Result<Self::Stat>;
    /// Read stat bytes from the start of an open stat handle.
    fn read_stat(&mut self, stat: &mut Self::Stat, buf: &mut [u8]) -> io::Result<usize>;
    /// Read a small process file in one operation (for example cmdline/cgroup).
    fn read_small(&mut self, pid: u32, name: &str, buf: &mut [u8]) -> io::Result<usize>;
    /// Return the owner uid associated with an open stat file.
    fn uid_of(&mut self, stat: &Self::Stat) -> io::Result<u32>;
    /// Release an open stat handle.
    fn close_stat(&mut self, stat: Self::Stat);
}

/// Counts of filesystem seam operations, useful for deterministic budget tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcFsOps {
    pub lists: usize,
    pub opens: usize,
    pub reads: usize,
    pub small_reads: usize,
    pub closes: usize,
}

impl ProcFsOps {
    /// Total calls across all operations.
    pub const fn total(self) -> usize {
        self.lists + self.opens + self.reads + self.small_reads + self.closes
    }
}

/// In-memory process record used by [`FakeProcFs`].
#[derive(Debug, Clone, Default)]
pub struct FakeProcess {
    pub stat: Vec<u8>,
    pub uid: u32,
    pub cmdline: Vec<u8>,
    pub cgroup: Vec<u8>,
}

/// A deterministic procfs fake with mutable records and scripted exits.
#[derive(Debug, Default)]
pub struct FakeProcFs {
    processes: BTreeMap<u32, FakeProcess>,
    exits: BTreeMap<u32, VecDeque<usize>>,
    open_errors: BTreeMap<u32, VecDeque<io::ErrorKind>>,
    read_errors: BTreeMap<u32, VecDeque<io::ErrorKind>>,
    gone_reads: BTreeMap<u32, usize>,
    small_read_errors: BTreeMap<(u32, String), VecDeque<io::ErrorKind>>,
    list_order: Option<Vec<u32>>,
    ops: ProcFsOps,
}

impl FakeProcFs {
    /// Create an empty fake filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a PID record. Replacing its stat bytes can script PID reuse.
    pub fn insert(&mut self, pid: u32, process: FakeProcess) {
        self.processes.insert(pid, process);
    }

    /// Remove a PID from subsequent directory listings.
    pub fn remove(&mut self, pid: u32) {
        self.processes.remove(&pid);
    }

    /// Override directory enumeration order for the currently installed PIDs.
    pub fn set_list_order(&mut self, pids: Vec<u32>) {
        self.list_order = Some(pids);
    }

    /// Script a zero-byte stat read on the next read for this PID.
    pub fn exit_on_next_read(&mut self, pid: u32) {
        self.exits.entry(pid).or_default().push_back(1);
    }

    /// Script zero-byte reads for the next `count` reads of this PID.
    pub fn exit_for_reads(&mut self, pid: u32, count: usize) {
        if count > 0 {
            self.exits.entry(pid).or_default().push_back(count);
        }
    }

    /// Script an error for the next stat open of a PID.
    pub fn fail_next_open(&mut self, pid: u32, kind: io::ErrorKind) {
        self.open_errors.entry(pid).or_default().push_back(kind);
    }

    /// Script an error for the next stat read of a PID.
    pub fn fail_next_read(&mut self, pid: u32, kind: io::ErrorKind) {
        self.read_errors.entry(pid).or_default().push_back(kind);
    }

    /// Script a gone error for the next stat read, removing the process before reopen.
    pub fn gone_on_next_read(&mut self, pid: u32) {
        *self.gone_reads.entry(pid).or_default() += 1;
    }

    /// Script an error for the next read of a process's small proc file.
    pub fn fail_next_small_read(&mut self, pid: u32, name: &str, kind: io::ErrorKind) {
        self.small_read_errors
            .entry((pid, name.to_owned()))
            .or_default()
            .push_back(kind);
    }

    /// Operation counters accumulated since construction.
    pub const fn ops(&self) -> ProcFsOps {
        self.ops
    }

    /// Reset operation counters without changing process records or scripts.
    pub fn reset_ops(&mut self) {
        self.ops = ProcFsOps::default();
    }

    /// Number of currently installed fake processes.
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    /// Whether no fake processes are installed.
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

/// Fake open stat-file handle.
#[derive(Debug)]
pub struct FakeStat {
    pid: u32,
}

impl ProcFs for FakeProcFs {
    type Stat = FakeStat;

    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        self.ops.lists += 1;
        if let Some(order) = &self.list_order {
            out.extend(
                order
                    .iter()
                    .copied()
                    .filter(|pid| self.processes.contains_key(pid)),
            );
            out.extend(
                self.processes
                    .keys()
                    .copied()
                    .filter(|pid| !order.contains(pid)),
            );
        } else {
            out.extend(self.processes.keys().copied());
        }
        Ok(())
    }

    fn open_stat(&mut self, pid: u32) -> io::Result<Self::Stat> {
        self.ops.opens += 1;
        if let Some(errors) = self.open_errors.get_mut(&pid)
            && let Some(kind) = errors.pop_front()
        {
            return Err(io::Error::new(kind, "scripted stat open error"));
        }
        if self.processes.contains_key(&pid) {
            Ok(FakeStat { pid })
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "process exited"))
        }
    }

    fn read_stat(&mut self, stat: &mut Self::Stat, buf: &mut [u8]) -> io::Result<usize> {
        self.ops.reads += 1;
        if let Some(remaining) = self.gone_reads.get_mut(&stat.pid)
            && *remaining > 0
        {
            *remaining -= 1;
            self.processes.remove(&stat.pid);
            return Err(io::Error::new(io::ErrorKind::NotFound, "process exited"));
        }
        if let Some(errors) = self.read_errors.get_mut(&stat.pid)
            && let Some(kind) = errors.pop_front()
        {
            return Err(io::Error::new(kind, "scripted stat read error"));
        }
        if let Some(script) = self.exits.get_mut(&stat.pid)
            && let Some(remaining) = script.front_mut()
            && *remaining > 0
        {
            *remaining -= 1;
            if *remaining == 0 {
                script.pop_front();
            }
            self.processes.remove(&stat.pid);
            return Ok(0);
        }
        let process = self
            .processes
            .get(&stat.pid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process exited"))?;
        let count = buf.len().min(process.stat.len());
        buf[..count].copy_from_slice(&process.stat[..count]);
        Ok(count)
    }

    fn read_small(&mut self, pid: u32, name: &str, buf: &mut [u8]) -> io::Result<usize> {
        self.ops.small_reads += 1;
        let key = (pid, name.to_owned());
        if let Some(errors) = self.small_read_errors.get_mut(&key)
            && let Some(kind) = errors.pop_front()
        {
            return Err(io::Error::new(kind, "scripted small proc read error"));
        }
        let process = self
            .processes
            .get(&pid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process exited"))?;
        let bytes = match name {
            "cmdline" => &process.cmdline,
            "cgroup" => &process.cgroup,
            _ => return Err(io::Error::new(io::ErrorKind::NotFound, "unknown proc file")),
        };
        let count = buf.len().min(bytes.len());
        buf[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn uid_of(&mut self, stat: &Self::Stat) -> io::Result<u32> {
        self.processes
            .get(&stat.pid)
            .map(|process| process.uid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process exited"))
    }

    fn close_stat(&mut self, _stat: Self::Stat) {
        self.ops.closes += 1;
    }
}
