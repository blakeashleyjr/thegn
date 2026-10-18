//! Rolling Linux DRM fdinfo scanner, isolated behind a small procfs seam.

use std::fs;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use crate::collect::parse::fdinfo::{self, Fdinfo};

pub const READLINKS_PER_TICK: usize = 600;
const PID_CAPACITY: usize = 1024;
const FD_SCRATCH_CAPACITY: usize = 4096;
const FDINFO_CAPACITY: usize = 4096;

pub trait FdFs {
    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()>;
    fn start_time(&mut self, pid: u32) -> io::Result<u64>;
    fn list_fds(&mut self, pid: u32, out: &mut Vec<u32>) -> io::Result<()>;
    fn readlink(&mut self, pid: u32, fd: u32, out: &mut [u8]) -> io::Result<usize>;
    fn read_fdinfo(&mut self, pid: u32, fd: u32, out: &mut Vec<u8>) -> io::Result<usize>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientFd {
    pub pid: u32,
    pub start_time: u64,
    pub fd: u32,
    pub client_id: u64,
    pub pdev: [u8; 32],
    pub pdev_len: u8,
}

#[derive(Default)]
struct Process {
    pid: u32,
    start_time: u64,
    fds: Vec<u32>,
    cursor: usize,
    discovered: bool,
    newly_added: bool,
    start_checked: bool,
}

pub struct DrmFdinfoScanner<F> {
    fs: F,
    processes: Vec<Process>,
    pids: Vec<u32>,
    fd_scratch: Vec<u32>,
    fdinfo: Vec<u8>,
    link: [u8; 256],
    clients: Vec<ClientFd>,
    sweep_cursor: usize,
    pub truncated: bool,
}

impl<F: FdFs> DrmFdinfoScanner<F> {
    pub fn new(fs: F) -> Self {
        Self {
            fs,
            processes: Vec::with_capacity(PID_CAPACITY),
            pids: Vec::with_capacity(PID_CAPACITY),
            fd_scratch: Vec::with_capacity(FD_SCRATCH_CAPACITY),
            fdinfo: Vec::with_capacity(FDINFO_CAPACITY),
            link: [0; 256],
            clients: Vec::with_capacity(128),
            sweep_cursor: 0,
            truncated: false,
        }
    }

    pub fn clients(&self) -> &[ClientFd] {
        &self.clients
    }
    pub fn fs(&self) -> &F {
        &self.fs
    }
    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    /// Enumerate and incrementally scan candidates. `readlinks` is the exact
    /// amount charged against the fixed per-tick budget.
    pub fn tick(&mut self) -> usize {
        self.pids.clear();
        if self.fs.list_pids(&mut self.pids).is_err() {
            return 0;
        }
        self.pids.sort_unstable();
        self.pids.dedup();
        self.truncated = self.pids.len() > PID_CAPACITY;
        self.pids.truncate(PID_CAPACITY);
        let pids = &self.pids;
        self.processes
            .retain(|process| pids.binary_search(&process.pid).is_ok());
        self.clients
            .retain(|client| pids.binary_search(&client.pid).is_ok());

        // Both lists are sorted. Merge-walk once to find new PIDs, then sort
        // the appended tail so later ticks keep the same bounded walk.
        let mut process_index = 0;
        let existing_count = self.processes.len();
        for index in 0..self.pids.len() {
            let pid = self.pids[index];
            while process_index < existing_count && self.processes[process_index].pid < pid {
                process_index += 1;
            }
            if process_index < existing_count && self.processes[process_index].pid == pid {
                continue;
            }
            if self.processes.len() == self.processes.capacity() {
                self.truncated = true;
                break;
            }
            if let Ok(start_time) = self.fs.start_time(pid) {
                self.processes.push(Process {
                    pid,
                    start_time,
                    fds: Vec::with_capacity(64),
                    cursor: 0,
                    discovered: false,
                    newly_added: true,
                    start_checked: true,
                });
            }
        }
        self.processes.sort_unstable_by_key(|process| process.pid);

        // Registered clients need a prompt PID-reuse check. Other known PIDs
        // are checked when the rolling sweep selects them.
        for process in &mut self.processes {
            if !process.newly_added {
                process.start_checked = false;
            }
        }
        for index in 0..self.processes.len() {
            let pid = self.processes[index].pid;
            if !self.processes[index].newly_added
                && self.clients.iter().any(|client| client.pid == pid)
            {
                self.refresh_start_time(index);
            }
        }

        let mut used = 0;
        for index in 0..self.processes.len() {
            if self.processes[index].discovered {
                continue;
            }
            used += self.scan_process(index, READLINKS_PER_TICK - used);
            if used == READLINKS_PER_TICK {
                return used;
            }
        }
        if self.processes.is_empty() {
            return used;
        }
        let per_tick = self.processes.len().div_ceil(30).max(1);
        for _ in 0..per_tick {
            let index = self.sweep_cursor % self.processes.len();
            self.sweep_cursor = (self.sweep_cursor + 1) % self.processes.len();
            if self.processes[index].newly_added {
                continue;
            }
            self.refresh_start_time(index);
            used += self.scan_process(index, READLINKS_PER_TICK - used);
            if used == READLINKS_PER_TICK {
                break;
            }
        }
        for process in &mut self.processes {
            process.newly_added = false;
        }
        used
    }

    fn refresh_start_time(&mut self, index: usize) {
        if self.processes[index].start_checked {
            return;
        }
        self.processes[index].start_checked = true;
        let pid = self.processes[index].pid;
        if let Ok(start_time) = self.fs.start_time(pid)
            && self.processes[index].start_time != start_time
        {
            self.processes[index].start_time = start_time;
            self.processes[index].cursor = 0;
            self.processes[index].discovered = false;
            self.processes[index].fds.clear();
            self.clients.retain(|client| client.pid != pid);
        }
    }

    fn scan_process(&mut self, index: usize, allowance: usize) -> usize {
        if allowance == 0 {
            return 0;
        }
        let pid = self.processes[index].pid;
        let start_time = self.processes[index].start_time;
        if self.processes[index].cursor == 0 {
            self.fd_scratch.clear();
            if self.fs.list_fds(pid, &mut self.fd_scratch).is_err() {
                self.processes[index].discovered = true;
                return 0;
            }
            self.fd_scratch.sort_unstable();
            self.fd_scratch.dedup();
            self.processes[index].fds.clear();
            if self.processes[index].fds.capacity() < self.fd_scratch.len() {
                self.processes[index].fds.reserve(self.fd_scratch.len());
            }
            self.processes[index]
                .fds
                .extend_from_slice(&self.fd_scratch);
            for client in &mut self.clients {
                if client.pid == pid {
                    client.fd = u32::MAX;
                }
            }
        }
        let end = (self.processes[index].cursor + allowance).min(self.processes[index].fds.len());
        let mut readlinks = 0;
        while self.processes[index].cursor < end {
            let fd = self.processes[index].fds[self.processes[index].cursor];
            self.processes[index].cursor += 1;
            readlinks += 1;
            if let Ok(length) = self.fs.readlink(pid, fd, &mut self.link)
                && self.link[..length.min(self.link.len())].starts_with(b"/dev/dri/")
            {
                self.fdinfo.clear();
                if let Ok(length) = self.fs.read_fdinfo(pid, fd, &mut self.fdinfo) {
                    let length = length.min(self.fdinfo.len());
                    let parsed = fdinfo::parse(&self.fdinfo[..length]);
                    self.add_client(pid, start_time, fd, &parsed);
                }
            }
        }
        if self.processes[index].cursor == self.processes[index].fds.len() {
            self.processes[index].cursor = 0;
            self.processes[index].discovered = true;
            self.clients
                .retain(|client| client.pid != pid || client.fd != u32::MAX);
        }
        readlinks
    }

    fn add_client(&mut self, pid: u32, start_time: u64, fd: u32, info: &Fdinfo) {
        let Some(client_id) = info.client_id else {
            return;
        };
        let found = self.clients.iter().position(|client| {
            client.client_id == client_id
                && client.pdev_len == info.pdev_len
                && client.pdev[..usize::from(client.pdev_len)]
                    == info.pdev[..usize::from(info.pdev_len)]
        });
        if let Some(index) = found {
            if self.clients[index].pid == pid {
                self.clients[index].fd = fd;
            }
        } else if self.clients.len() < self.clients.capacity() {
            self.clients.push(ClientFd {
                pid,
                start_time,
                fd,
                client_id,
                pdev: info.pdev,
                pdev_len: info.pdev_len,
            });
        }
    }
}

pub struct RealFdFs {
    proc_root: PathBuf,
    proc_fd: OwnedFd,
}

impl RealFdFs {
    pub fn new() -> Self {
        Self::at("/proc")
    }
    pub fn at(root: impl Into<PathBuf>) -> Self {
        use std::os::fd::FromRawFd;

        let proc_root = root.into();
        let path = std::ffi::CString::new(proc_root.as_os_str().as_encoded_bytes())
            .expect("proc root path contains a NUL byte");
        // SAFETY: the path is a valid NUL-terminated C string; a successful fd
        // is immediately transferred into OwnedFd.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        assert!(
            fd >= 0,
            "failed to open proc root: {}",
            io::Error::last_os_error()
        );
        // SAFETY: `fd` is nonnegative and uniquely owned here.
        let proc_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Self { proc_root, proc_fd }
    }
    fn process_path(&self, pid: u32, name: &str) -> PathBuf {
        self.proc_root.join(pid.to_string()).join(name)
    }
}

impl Default for RealFdFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FdFs for RealFdFs {
    fn list_pids(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        out.clear();
        for entry in fs::read_dir(&self.proc_root)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && let Ok(pid) = name.parse::<u32>()
            {
                out.push(pid);
            }
        }
        Ok(())
    }
    fn start_time(&mut self, pid: u32) -> io::Result<u64> {
        let bytes = fs::read(self.process_path(pid, "stat"))?;
        let close = bytes
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid proc stat"))?;
        let value = bytes[close + 1..]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .nth(19)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing start time"))?;
        parse_decimal(value)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid start time"))
    }
    fn list_fds(&mut self, pid: u32, out: &mut Vec<u32>) -> io::Result<()> {
        out.clear();
        for entry in fs::read_dir(self.process_path(pid, "fd"))? {
            if let Some(name) = entry?.file_name().to_str()
                && let Ok(fd) = name.parse::<u32>()
            {
                out.push(fd);
            }
        }
        Ok(())
    }
    fn readlink(&mut self, pid: u32, fd: u32, out: &mut [u8]) -> io::Result<usize> {
        let mut path = [0_u8; 64];
        let mut cursor = 0;
        cursor += write_decimal(pid, &mut path[cursor..]);
        path[cursor] = b'/';
        cursor += 1;
        path[cursor..cursor + 3].copy_from_slice(b"fd/");
        cursor += 3;
        cursor += write_decimal(fd, &mut path[cursor..]);
        path[cursor] = 0;
        // SAFETY: `path` is NUL terminated and `out` is a valid writable slice.
        let count = unsafe {
            libc::readlinkat(
                self.proc_fd.as_raw_fd(),
                path.as_ptr().cast(),
                out.as_mut_ptr().cast(),
                out.len(),
            )
        };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }
    fn read_fdinfo(&mut self, pid: u32, fd: u32, out: &mut Vec<u8>) -> io::Result<usize> {
        out.clear();
        fs::File::open(self.process_path(pid, &format!("fdinfo/{fd}")))?
            .take(FDINFO_CAPACITY as u64)
            .read_to_end(out)
    }
}

fn write_decimal(mut value: u32, out: &mut [u8]) -> usize {
    let mut digits = [0_u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..count {
        out[index] = digits[count - index - 1];
    }
    count
}

fn parse_decimal(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0_u64, |value, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))
    })
}

#[cfg(test)]
#[path = "drm_fdinfo_tests.rs"]
mod tests;
