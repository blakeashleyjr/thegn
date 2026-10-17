//! Pure data types for Linux-style process records.

/// A process identity that remains distinct when the kernel reuses a PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcId {
    pub pid: u32,
    pub start_ticks: u64,
}

/// The state byte reported by `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    Running,
    Sleeping,
    DiskSleep,
    Zombie,
    Stopped,
    TracingStop,
    Dead,
    DeadLower,
    Wakekill,
    Waking,
    Parked,
    Idle,
    Unknown(u8),
}

impl ProcState {
    /// Convert a kernel state byte without assuming it is recognized.
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            b'R' => Self::Running,
            b'S' => Self::Sleeping,
            b'D' => Self::DiskSleep,
            b'Z' => Self::Zombie,
            b'T' => Self::Stopped,
            b't' => Self::TracingStop,
            b'X' => Self::Dead,
            b'x' => Self::DeadLower,
            b'K' => Self::Wakekill,
            b'W' => Self::Waking,
            b'P' => Self::Parked,
            b'I' => Self::Idle,
            other => Self::Unknown(other),
        }
    }

    /// Convert this state back to its kernel byte. `Dead` uses `X`.
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Running => b'R',
            Self::Sleeping => b'S',
            Self::DiskSleep => b'D',
            Self::Zombie => b'Z',
            Self::Stopped => b'T',
            Self::TracingStop => b't',
            Self::Dead => b'X',
            Self::DeadLower => b'x',
            Self::Wakekill => b'K',
            Self::Waking => b'W',
            Self::Parked => b'P',
            Self::Idle => b'I',
            Self::Unknown(byte) => byte,
        }
    }
}

/// Linux `PF_KTHREAD` flag.
pub const PF_KTHREAD: u64 = 0x0020_0000;

/// Inline process name storage, matching the kernel's 64-byte task name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comm {
    len: u8,
    bytes: [u8; 64],
}

impl Comm {
    /// Store the first 64 bytes of a process name.
    pub fn from_bytes(input: &[u8]) -> Self {
        let len = input.len().min(64);
        let mut bytes = [0; 64];
        bytes[..len].copy_from_slice(&input[..len]);
        Self {
            len: len as u8,
            bytes,
        }
    }

    /// The stored name bytes, which need not be UTF-8.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Render the stored bytes lossily as UTF-8.
    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.as_bytes())
    }
}

impl Default for Comm {
    fn default() -> Self {
        Self {
            len: 0,
            bytes: [0; 64],
        }
    }
}

/// Fields from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stat {
    pub pid: u32,
    pub comm: Comm,
    pub state: ProcState,
    pub ppid: u32,
    pub pgrp: u32,
    pub session: u32,
    pub tty_nr: i32,
    pub tpgid: i32,
    pub flags: u64,
    pub minflt: u64,
    pub majflt: u64,
    pub utime: u64,
    pub stime: u64,
    pub cutime: i64,
    pub cstime: i64,
    pub priority: i64,
    pub nice: i64,
    pub num_threads: u64,
    pub starttime: u64,
    pub vsize: u64,
    pub rss_pages: i64,
    pub processor: Option<u32>,
    pub rt_priority: Option<u32>,
    pub policy: Option<u32>,
    pub delayacct_blkio_ticks: Option<u64>,
}

impl Stat {
    /// Stable process identity for this observation.
    pub const fn id(&self) -> ProcId {
        ProcId {
            pid: self.pid,
            start_ticks: self.starttime,
        }
    }

    /// Whether the kernel marks this task as a kernel thread.
    pub const fn is_kthread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }

    /// Whether this process has exited but remains unreaped.
    pub const fn is_zombie(&self) -> bool {
        matches!(self.state, ProcState::Zombie)
    }
}

impl Default for ProcState {
    fn default() -> Self {
        Self::Unknown(0)
    }
}

/// Selected fields from `/proc/<pid>/status`; memory values are KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    pub tgid: Option<u32>,
    pub pid: Option<u32>,
    pub ppid: Option<u32>,
    pub uid: Option<[u32; 4]>,
    pub gid: Option<[u32; 4]>,
    pub vm_rss_kib: Option<u64>,
    pub rss_anon_kib: Option<u64>,
    pub rss_file_kib: Option<u64>,
    pub rss_shmem_kib: Option<u64>,
    pub vm_swap_kib: Option<u64>,
    pub threads: Option<u32>,
    pub voluntary_ctxt: Option<u64>,
    pub nonvoluntary_ctxt: Option<u64>,
}

/// Byte counters from `/proc/<pid>/io`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Io {
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

/// Timing counters from `/proc/<pid>/schedstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchedStat {
    pub run_ns: u64,
    pub wait_ns: u64,
    pub timeslices: u64,
}

/// Borrowed membership line from `/proc/<pid>/cgroup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupLine<'a> {
    pub hier_id: u32,
    pub controllers: &'a [u8],
    pub path: &'a [u8],
}
