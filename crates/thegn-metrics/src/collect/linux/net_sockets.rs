//! Visible-only TCP state and passive RTT collector over sock_diag.
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{netlink::NetlinkSocket, sock_diag};
use crate::collect::linux::net_attrib::CgroupFs;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::{MonitorFrame, Part},
    health::{Absent, Health},
    linux::{
        net_attrib::{CookieDelta, OwnerTable, RealCgroupFs},
        procfile::ProcFile,
    },
    model::netdev::IfaceName,
    parse::netdev::parse,
    policy::net_scope::{self, Scope},
    schedule::Cadence,
};
use crate::net_kind;

const SOCKET_DUMP_TIMEOUT: Duration = Duration::from_millis(250);
const RECENT_DATA_MS: u32 = 10_000;
const TCP_ESTABLISHED: u8 = 1;
const TCP_LISTEN: u8 = 10;
const IFACE_CAP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IfaceBytes {
    name: IfaceName,
    scope: Scope,
    rx: Option<u64>,
    tx: Option<u64>,
}

trait PhysicalCounters: Send {
    fn read(&mut self, out: &mut Vec<IfaceBytes>) -> io::Result<()>;
    fn close(&mut self);
}

#[derive(Debug, Clone, Copy)]
struct CachedScope {
    name: IfaceName,
    scope: Scope,
}

struct ProcNetDev {
    path: PathBuf,
    sysfs_root: PathBuf,
    file: Option<ProcFile>,
    scopes: Vec<CachedScope>,
}

impl ProcNetDev {
    fn at(path: impl AsRef<Path>, sysfs_root: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            sysfs_root: sysfs_root.as_ref().to_path_buf(),
            file: None,
            scopes: Vec::with_capacity(IFACE_CAP),
        }
    }
}

impl PhysicalCounters for ProcNetDev {
    fn read(&mut self, out: &mut Vec<IfaceBytes>) -> io::Result<()> {
        if self.file.is_none() {
            self.file = Some(ProcFile::open(&self.path)?);
        }
        let file = self.file.as_mut().expect("opened above");
        let bytes = match file.read_all() {
            Ok(bytes) => bytes,
            Err(error) => {
                if file.needs_reopen() {
                    self.file = None;
                }
                return Err(error);
            }
        };
        out.clear();
        let count = parse(bytes, |name, counters| {
            let interface = IfaceName::new(name);
            let scope = if let Some(cached) = self.scopes.iter().find(|item| item.name == interface)
            {
                cached.scope
            } else if self.scopes.len() < IFACE_CAP {
                let name = interface.as_str();
                let facts = net_kind::facts_at(&self.sysfs_root, name);
                let kind = net_scope::classify(name, &facts);
                let scope = net_scope::scope(kind, facts.bonding_slave);
                self.scopes.push(CachedScope {
                    name: interface,
                    scope,
                });
                scope
            } else {
                Scope::Virtual
            };
            if out.len() < IFACE_CAP {
                out.push(IfaceBytes {
                    name: interface,
                    scope,
                    rx: counters.rx_bytes,
                    tx: counters.tx_bytes,
                });
            }
        });
        if count.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "netdev: missing header",
            ));
        }
        Ok(())
    }

    fn close(&mut self) {
        self.file = None;
        self.scopes.clear();
    }
}

fn system_physical_counters() -> Box<dyn PhysicalCounters> {
    Box::new(ProcNetDev::at("/proc/net/dev", "/sys/class/net"))
}

trait SocketTransport: Send {
    fn open(&mut self) -> io::Result<()>;
    fn dump(&mut self, out: &mut Vec<sock_diag::TcpSock>) -> io::Result<sock_diag::DumpStats>;
    fn close(&mut self);
}

#[derive(Default)]
struct NetlinkTransport {
    socket: Option<NetlinkSocket>,
}

impl SocketTransport for NetlinkTransport {
    fn open(&mut self) -> io::Result<()> {
        self.socket = Some(NetlinkSocket::sock_diag()?);
        Ok(())
    }

    fn dump(&mut self, out: &mut Vec<sock_diag::TcpSock>) -> io::Result<sock_diag::DumpStats> {
        if self.socket.is_none() {
            self.open()?;
        }
        let socket = self.socket.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "sock_diag socket is closed")
        })?;
        sock_diag::dump_tcp(socket, out, Instant::now() + SOCKET_DUMP_TIMEOUT)
    }

    fn close(&mut self) {
        self.socket = None;
    }
}

pub struct NetSocketsCollector<F: CgroupFs + Send + 'static = RealCgroupFs> {
    transport: Box<dyn SocketTransport>,
    sockets: Vec<sock_diag::TcpSock>,
    rtts: Vec<u32>,
    physical: Box<dyn PhysicalCounters>,
    now_ms: u64,
    delta: CookieDelta,
    owners: Option<OwnerTable<F>>,
    owner_roots: Vec<PathBuf>,
    owner_factory: Box<dyn Fn() -> F + Send>,
    iface_rows: Vec<IfaceBytes>,
    prev_ifaces: Vec<IfaceBytes>,
    prev_physical_known: bool,
    last_dump_ms: Option<u64>,
}

impl NetSocketsCollector<RealCgroupFs> {
    pub fn new() -> Self {
        Self::with_parts(
            Box::<NetlinkTransport>::default(),
            system_physical_counters(),
            vec![
                PathBuf::from("/sys/fs/cgroup/user.slice"),
                PathBuf::from("/sys/fs/cgroup/system.slice"),
            ],
            Box::new(|| RealCgroupFs),
        )
    }
}

impl<F: CgroupFs + Send + 'static> NetSocketsCollector<F> {
    fn with_parts(
        transport: Box<dyn SocketTransport>,
        physical: Box<dyn PhysicalCounters>,
        owner_roots: Vec<PathBuf>,
        owner_factory: Box<dyn Fn() -> F + Send>,
    ) -> Self {
        Self {
            transport,
            sockets: Vec::new(),
            rtts: Vec::new(),
            physical,
            now_ms: 0,
            delta: CookieDelta::new(),
            owners: None,
            owner_roots,
            owner_factory,
            iface_rows: Vec::with_capacity(IFACE_CAP),
            prev_ifaces: Vec::with_capacity(IFACE_CAP),
            prev_physical_known: false,
            last_dump_ms: None,
        }
    }
}

#[cfg(test)]
impl NetSocketsCollector<RealCgroupFs> {
    fn with_transport(transport: Box<dyn SocketTransport>) -> Self {
        Self::with_parts(
            transport,
            Box::new(EmptyPhysical),
            Vec::new(),
            Box::new(|| RealCgroupFs),
        )
    }
}

#[cfg(test)]
struct EmptyPhysical;

#[cfg(test)]
impl PhysicalCounters for EmptyPhysical {
    fn read(&mut self, out: &mut Vec<IfaceBytes>) -> io::Result<()> {
        out.clear();
        Ok(())
    }

    fn close(&mut self) {}
}

impl Default for NetSocketsCollector<RealCgroupFs> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: CgroupFs + Send + 'static> Collector for NetSocketsCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::NetSockets
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        match self.transport.open() {
            Ok(()) => {
                self.transport.close();
                Health::Warming
            }
            Err(error) => absent_or_warming(&error),
        }
    }

    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        self.now_ms = ctx.now_ms;
        self.sockets.clear();
        self.rtts.clear();
        if self.sockets.capacity() < sock_diag::MAX_TCP_SOCKETS {
            self.sockets.reserve(sock_diag::MAX_TCP_SOCKETS);
        }
        if self.rtts.capacity() < sock_diag::MAX_TCP_SOCKETS {
            self.rtts.reserve(sock_diag::MAX_TCP_SOCKETS);
        }
        let stats = self
            .transport
            .dump(&mut self.sockets)
            .map_err(collect_io_error)?;
        let part = &mut frame.parts.net_sockets;
        part.clear();
        part.malformed = u32::try_from(stats.malformed).unwrap_or(u32::MAX);
        part.truncated = stats.truncated;
        for socket in &self.sockets {
            if let Some(count) = socket
                .state
                .checked_sub(1)
                .and_then(|index| part.states.get_mut(usize::from(index)))
            {
                *count = count.saturating_add(1);
            }
            if socket.state == TCP_LISTEN {
                part.listening = part.listening.saturating_add(1);
            }
            if socket.state == TCP_ESTABLISHED
                && !is_loopback(socket.destination)
                && let Some(info) = socket.info.as_ref()
                && info
                    .last_data_recv_ms
                    .is_some_and(|age| age <= RECENT_DATA_MS)
                && let Some(rtt) = info.rtt_us
            {
                self.rtts.push(rtt);
            }
        }
        part.sockets = u32::try_from(self.sockets.len()).unwrap_or(u32::MAX);
        for rtt in &self.rtts {
            let bucket = match *rtt {
                value if value < 1_000 => 0,
                value if value < 5_000 => 1,
                value if value < 20_000 => 2,
                value if value < 50_000 => 3,
                value if value < 100_000 => 4,
                value if value < 200_000 => 5,
                value if value < 500_000 => 6,
                _ => 7,
            };
            part.rtt_hist[bucket] = part.rtt_hist[bucket].saturating_add(1);
        }
        if !self.rtts.is_empty() {
            let max = self.rtts.iter().copied().max();
            let p50_index = (self.rtts.len() * 50).div_ceil(100) - 1;
            self.rtts.select_nth_unstable(p50_index);
            part.rtt_p50_us = Some(self.rtts[p50_index]);
            let p90_index = (self.rtts.len() * 90).div_ceil(100) - 1;
            self.rtts.select_nth_unstable(p90_index);
            part.rtt_p90_us = Some(self.rtts[p90_index]);
            part.rtt_max_us = max;
        }
        if self.owners.is_none() {
            self.owners = Some(OwnerTable::new(
                (self.owner_factory)(),
                self.owner_roots.clone(),
            ));
        }
        let previous = self.last_dump_ms;
        let interval_ms = previous.map(|last| self.now_ms.saturating_sub(last));
        let read_ok = self.physical.read(&mut self.iface_rows).is_ok();
        let physical_known = self.prev_physical_known && read_ok;
        let (physical_rx, physical_tx) = if physical_known {
            physical_delta(&self.prev_ifaces, &self.iface_rows)
        } else {
            (0, 0)
        };
        self.delta.update_into(
            &self.sockets,
            physical_rx,
            physical_tx,
            self.owners.as_mut().expect("owner table initialized"),
            &mut part.attribution,
        );
        // The Owners partial marker describes socket/owner coverage, not physical rows.
        part.attribution.truncated |= stats.truncated;
        if !read_ok {
            self.iface_rows.clear();
        }
        self.prev_ifaces.clear();
        self.prev_ifaces.extend_from_slice(&self.iface_rows);
        self.prev_physical_known = read_ok;
        part.attribution_interval_ms =
            interval_ms.and_then(|interval| u32::try_from(interval).ok());
        part.physical_known = physical_known;
        self.last_dump_ms = Some(self.now_ms);
        frame.sampled.insert(CollectorId::NetSockets);
        Ok(())
    }

    fn close(&mut self) {
        self.sockets = Vec::new();
        self.rtts = Vec::new();
        self.transport.close();
        self.delta.close();
        self.physical.close();
        self.prev_ifaces.clear();
        self.prev_physical_known = false;
        self.iface_rows.clear();
        self.owners = None;
        self.last_dump_ms = None;
    }
}

fn physical_delta(previous: &[IfaceBytes], current: &[IfaceBytes]) -> (u64, u64) {
    let physical_now = current.iter().any(|iface| iface.scope == Scope::Physical);
    let selected = |iface: &IfaceBytes| {
        if physical_now {
            iface.scope == Scope::Physical
        } else {
            iface.scope != Scope::Loopback
        }
    };
    current
        .iter()
        .filter(|iface| selected(iface))
        .fold((0_u64, 0_u64), |sum, now| {
            let Some(old) = previous
                .iter()
                .find(|old| old.name == now.name && old.scope == now.scope)
            else {
                return sum;
            };
            (
                sum.0.saturating_add(
                    now.rx
                        .zip(old.rx)
                        .map_or(0, |(new, old)| new.saturating_sub(old)),
                ),
                sum.1.saturating_add(
                    now.tx
                        .zip(old.tx)
                        .map_or(0, |(new, old)| new.saturating_sub(old)),
                ),
            )
        })
}

fn is_loopback(address: IpAddr) -> bool {
    address.to_canonical().is_loopback()
}

fn absent_or_warming(error: &io::Error) -> Health {
    if error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(libc::EPERM)
    {
        Health::Absent(Absent::NoPermission)
    } else if error.raw_os_error() == Some(libc::EPROTONOSUPPORT)
        || error.raw_os_error() == Some(libc::EAFNOSUPPORT)
    {
        Health::Absent(Absent::NoHardware)
    } else {
        Health::Warming.after_error(0)
    }
}

fn collect_io_error(error: io::Error) -> CollectError {
    if error.kind() == io::ErrorKind::PermissionDenied
        || matches!(
            error.raw_os_error(),
            Some(libc::EPERM)
                | Some(libc::EPROTONOSUPPORT)
                | Some(libc::EAFNOSUPPORT)
                | Some(libc::EBUSY)
                | Some(libc::ENOBUFS)
        )
        || matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        )
    {
        CollectError::Gone
    } else {
        CollectError::from(error)
    }
}

#[cfg(test)]
#[path = "net_sockets_tests.rs"]
mod tests;
