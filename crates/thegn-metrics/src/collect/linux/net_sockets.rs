//! Visible-only TCP state and passive RTT collector over sock_diag.
use std::io;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::{netlink::NetlinkSocket, sock_diag};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::net_sockets::NetSocketsPart,
    schedule::Cadence,
};

const SOCKET_DUMP_TIMEOUT: Duration = Duration::from_secs(2);
const RECENT_DATA_MS: u32 = 10_000;
const TCP_ESTABLISHED: u8 = 1;
const TCP_LISTEN: u8 = 10;

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
        let socket = self.socket.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "sock_diag socket is closed")
        })?;
        sock_diag::dump_tcp(socket, out, Instant::now() + SOCKET_DUMP_TIMEOUT)
    }

    fn close(&mut self) {
        self.socket = None;
    }
}

pub struct NetSocketsCollector {
    transport: Box<dyn SocketTransport>,
    sockets: Vec<sock_diag::TcpSock>,
    rtts: Vec<u32>,
}

impl NetSocketsCollector {
    pub fn new() -> Self {
        Self::with_transport(Box::<NetlinkTransport>::default())
    }

    fn with_transport(transport: Box<dyn SocketTransport>) -> Self {
        Self {
            transport,
            sockets: Vec::with_capacity(sock_diag::MAX_TCP_SOCKETS),
            rtts: Vec::with_capacity(sock_diag::MAX_TCP_SOCKETS),
        }
    }
}

impl Default for NetSocketsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for NetSocketsCollector {
    fn id(&self) -> CollectorId {
        CollectorId::NetSockets
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        match self.transport.open() {
            Ok(()) => Health::Warming,
            Err(error) => absent_or_warming(&error),
        }
    }

    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let stats = self
            .transport
            .dump(&mut self.sockets)
            .map_err(collect_io_error)?;
        let part = &mut frame.parts.net_sockets;
        *part = NetSocketsPart {
            malformed: u32::try_from(stats.malformed).unwrap_or(u32::MAX),
            truncated: stats.truncated,
            ..NetSocketsPart::default()
        };
        self.rtts.clear();
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
        frame.sampled.insert(CollectorId::NetSockets);
        Ok(())
    }

    fn close(&mut self) {
        self.sockets.clear();
        self.rtts.clear();
        self.transport.close();
    }
}

fn is_loopback(address: IpAddr) -> bool {
    address.is_loopback()
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
            Some(libc::EPERM) | Some(libc::EPROTONOSUPPORT) | Some(libc::EAFNOSUPPORT)
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
