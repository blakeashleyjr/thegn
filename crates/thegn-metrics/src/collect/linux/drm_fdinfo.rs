//! Rolling Linux DRM fdinfo scanner, isolated behind a small procfs seam.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use crate::collect::parse::fdinfo::{self, Fdinfo};

pub const READLINKS_PER_TICK: usize = 600;
const PID_CAPACITY: usize = 1024;
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
}

impl<F: FdFs> DrmFdinfoScanner<F> {
    pub fn new(fs: F) -> Self {
        Self {
            fs,
            processes: Vec::with_capacity(PID_CAPACITY),
            pids: Vec::with_capacity(PID_CAPACITY),
            fd_scratch: Vec::with_capacity(FDINFO_CAPACITY),
            fdinfo: Vec::with_capacity(FDINFO_CAPACITY),
            link: [0; 256],
            clients: Vec::with_capacity(128),
            sweep_cursor: 0,
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
        let pids = &self.pids;
        self.processes
            .retain(|process| pids.binary_search(&process.pid).is_ok());
        self.clients
            .retain(|client| pids.binary_search(&client.pid).is_ok());
        for index in 0..self.pids.len() {
            let pid = self.pids[index];
            if self.processes.iter().any(|process| process.pid == pid) {
                continue;
            }
            let Ok(start_time) = self.fs.start_time(pid) else {
                continue;
            };
            if self.processes.len() == self.processes.capacity() {
                break;
            }
            self.processes.push(Process {
                pid,
                start_time,
                fds: Vec::with_capacity(64),
                cursor: 0,
                discovered: false,
            });
        }
        for process in &mut self.processes {
            if let Ok(start_time) = self.fs.start_time(process.pid)
                && process.start_time != start_time
            {
                process.start_time = start_time;
                process.cursor = 0;
                process.discovered = false;
                process.fds.clear();
                self.clients
                    .retain(|client| client.pid != process.pid || client.start_time == start_time);
            }
        }
        let mut used = 0;
        // Fresh PIDs are scanned immediately, with progress retained if their
        // descriptor count exceeds this tick's allowance.
        let mut had_new_process = false;
        for index in 0..self.processes.len() {
            if self.processes[index].discovered {
                continue;
            }
            had_new_process = true;
            used += self.scan_process(index, READLINKS_PER_TICK - used);
            if used == READLINKS_PER_TICK {
                return used;
            }
        }
        if had_new_process {
            return used;
        }
        if self.processes.is_empty() {
            return used;
        }
        let per_tick = self.processes.len().div_ceil(30).max(1);
        for _ in 0..per_tick {
            let index = self.sweep_cursor % self.processes.len();
            self.sweep_cursor = (self.sweep_cursor + 1) % self.processes.len();
            used += self.scan_process(index, READLINKS_PER_TICK - used);
            if used == READLINKS_PER_TICK {
                break;
            }
        }
        used
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
        }
        let end = (self.processes[index].cursor + allowance).min(self.processes[index].fds.len());
        let mut readlinks = 0;
        while self.processes[index].cursor < end {
            let fd = self.processes[index].fds[self.processes[index].cursor];
            self.processes[index].cursor += 1;
            readlinks += 1;
            if let Ok(length) = self.fs.readlink(pid, fd, &mut self.link)
                && self.link[..length].starts_with(b"/dev/dri/")
            {
                self.fdinfo.clear();
                if let Ok(length) = self.fs.read_fdinfo(pid, fd, &mut self.fdinfo) {
                    let parsed = fdinfo::parse(&self.fdinfo[..length]);
                    self.add_client(pid, start_time, fd, &parsed);
                }
            }
        }
        if self.processes[index].cursor == self.processes[index].fds.len() {
            self.processes[index].cursor = 0;
            self.processes[index].discovered = true;
        }
        readlinks
    }

    fn add_client(&mut self, pid: u32, start_time: u64, fd: u32, info: &Fdinfo) {
        let Some(client_id) = info.client_id else {
            return;
        };
        let found = self.clients.iter().position(|client| {
            client.pid == pid
                && client.start_time == start_time
                && client.client_id == client_id
                && client.pdev_len == info.pdev_len
                && client.pdev[..usize::from(client.pdev_len)]
                    == info.pdev[..usize::from(info.pdev_len)]
        });
        if let Some(index) = found {
            self.clients[index].fd = fd;
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
}

impl RealFdFs {
    pub fn new() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
        }
    }
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            proc_root: root.into(),
        }
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
        let target = fs::read_link(self.process_path(pid, &format!("fd/{fd}")))?;
        let bytes = target.as_os_str().as_encoded_bytes();
        let count = bytes.len().min(out.len());
        out[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn read_fdinfo(&mut self, pid: u32, fd: u32, out: &mut Vec<u8>) -> io::Result<usize> {
        out.clear();
        fs::File::open(self.process_path(pid, &format!("fdinfo/{fd}")))?.read_to_end(out)
    }
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
