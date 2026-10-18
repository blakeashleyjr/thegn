//! Held generic-netlink Wi-Fi collector. Sampling only uses nl80211 GET dumps.
use std::io;
use std::time::{Duration, Instant};

use super::{netlink::NetlinkSocket, nl80211};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::net_wifi::WifiRow,
    schedule::Cadence,
};

const IF_TYPE_MANAGED: u32 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

trait WifiTransport: Send {
    fn open(&mut self) -> io::Result<()>;
    fn close(&mut self);
    fn resolve_family(&mut self) -> io::Result<u16>;
    fn interfaces(
        &mut self,
        family: u16,
        out: &mut Vec<nl80211::Interface>,
        truncated: &mut bool,
    ) -> io::Result<()>;
    fn station(&mut self, family: u16, ifindex: u32) -> io::Result<Option<nl80211::Station>>;
}

#[derive(Default)]
struct NetlinkTransport {
    socket: Option<NetlinkSocket>,
    request: Vec<u8>,
}

impl NetlinkTransport {
    fn socket(&mut self) -> io::Result<&mut NetlinkSocket> {
        self.socket
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "nl80211 socket is closed"))
    }
}

impl WifiTransport for NetlinkTransport {
    fn open(&mut self) -> io::Result<()> {
        self.socket = Some(NetlinkSocket::open(libc::NETLINK_GENERIC)?);
        Ok(())
    }

    fn close(&mut self) {
        self.socket = None;
    }

    fn resolve_family(&mut self) -> io::Result<u16> {
        nl80211::resolve_family(self.socket()?, Instant::now() + REQUEST_TIMEOUT)
    }

    fn interfaces(
        &mut self,
        family: u16,
        out: &mut Vec<nl80211::Interface>,
        truncated: &mut bool,
    ) -> io::Result<()> {
        let (socket, request) = (&mut self.socket, &mut self.request);
        nl80211::dump_interfaces(
            socket.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "nl80211 socket is closed")
            })?,
            family,
            out,
            truncated,
            request,
            Instant::now() + REQUEST_TIMEOUT,
        )
    }

    fn station(&mut self, family: u16, ifindex: u32) -> io::Result<Option<nl80211::Station>> {
        let (socket, request) = (&mut self.socket, &mut self.request);
        nl80211::dump_station(
            socket.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "nl80211 socket is closed")
            })?,
            family,
            ifindex,
            request,
            Instant::now() + REQUEST_TIMEOUT,
        )
    }
}

pub struct NetWifiCollector {
    transport: Box<dyn WifiTransport>,
    family: Option<u16>,
    interfaces: Vec<nl80211::Interface>,
    interfaces_truncated: bool,
}

impl NetWifiCollector {
    pub fn new() -> Self {
        Self::with_transport(Box::<NetlinkTransport>::default())
    }

    fn with_transport(transport: Box<dyn WifiTransport>) -> Self {
        Self {
            transport,
            family: None,
            interfaces: Vec::with_capacity(nl80211::MAX_INTERFACES),
            interfaces_truncated: false,
        }
    }

    fn resolve(&mut self) -> io::Result<u16> {
        let family = self.transport.resolve_family()?;
        self.family = Some(family);
        Ok(family)
    }

    fn refresh_interfaces(&mut self) -> io::Result<()> {
        let family = self.family.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "nl80211 family is unresolved")
        })?;
        self.interfaces_truncated = false;
        match self.transport.interfaces(
            family,
            &mut self.interfaces,
            &mut self.interfaces_truncated,
        ) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                let family = self.resolve()?;
                self.interfaces_truncated = false;
                self.transport.interfaces(
                    family,
                    &mut self.interfaces,
                    &mut self.interfaces_truncated,
                )
            }
            result => result,
        }
    }

    fn station(&mut self, family: u16, ifindex: u32) -> io::Result<Option<nl80211::Station>> {
        match self.transport.station(family, ifindex) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                let family = self.resolve()?;
                self.transport.station(family, ifindex)
            }
            result => result,
        }
    }
}

impl Default for NetWifiCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for NetWifiCollector {
    fn id(&self) -> CollectorId {
        CollectorId::NetWifi
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        if let Err(error) = self.transport.open() {
            return absent_or_warming(&error);
        }
        if let Err(error) = self.resolve() {
            return absent_or_warming(&error);
        }
        match self.refresh_interfaces() {
            Ok(()) if self.interfaces.iter().any(is_managed) => Health::Warming,
            Ok(()) => Health::Absent(Absent::NoHardware),
            Err(error) => absent_or_warming(&error),
        }
    }

    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if let Err(error) = self.refresh_interfaces() {
            return Err(collect_io_error(error));
        }
        let part = &mut frame.parts.net_wifi;
        part.ifaces.clear();
        part.truncated = self.interfaces_truncated;
        for index in 0..self.interfaces.len() {
            let interface = self.interfaces[index];
            if !is_managed(&interface) {
                continue;
            }
            let (Some(ifindex), Some(name)) = (interface.ifindex, interface.name) else {
                continue;
            };
            if part.ifaces.len() == nl80211::MAX_INTERFACES {
                part.truncated = true;
                break;
            }
            let family = self.family.ok_or(CollectError::Gone)?;
            let station = match self.station(family, ifindex) {
                Ok(station) => station,
                Err(error) => return Err(collect_io_error(error)),
            };
            let Some(station) = station else {
                continue;
            };
            part.ifaces
                .push(wifi_row(&interface, ifindex, name, &station));
        }
        if !self.interfaces.iter().any(is_managed) {
            return Err(CollectError::Gone);
        }
        frame.sampled.insert(CollectorId::NetWifi);
        Ok(())
    }

    fn close(&mut self) {
        self.interfaces.clear();
        self.interfaces_truncated = false;
        self.family = None;
        self.transport.close();
    }
}

fn is_managed(interface: &nl80211::Interface) -> bool {
    interface.iftype == Some(IF_TYPE_MANAGED)
}

fn absent_or_warming(error: &io::Error) -> Health {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Health::Absent(Absent::NoPermission)
    } else if matches!(
        error.raw_os_error(),
        Some(libc::EAFNOSUPPORT) | Some(libc::EPROTONOSUPPORT)
    ) || error.kind() == io::ErrorKind::NotFound
    {
        Health::Absent(Absent::NoHardware)
    } else {
        Health::Warming.after_error(0)
    }
}

fn collect_io_error(error: io::Error) -> CollectError {
    if error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::NotFound
        || matches!(
            error.raw_os_error(),
            Some(libc::ENOENT) | Some(libc::EAFNOSUPPORT) | Some(libc::EPROTONOSUPPORT)
        )
    {
        CollectError::Gone
    } else {
        CollectError::from(error)
    }
}

fn wifi_row(
    interface: &nl80211::Interface,
    ifindex: u32,
    name: nl80211::InlineBytes<{ nl80211::IFNAME_LEN }>,
    station: &nl80211::Station,
) -> WifiRow {
    WifiRow {
        ifindex,
        name,
        ssid: interface.ssid,
        bssid_masked: station.bssid.map(|_| nl80211::MASKED_BSSID),
        freq_mhz: interface.frequency_mhz,
        width_mhz: interface.channel_width.and_then(channel_width_mhz),
        signal_dbm: station.signal_dbm,
        signal_avg_dbm: station.signal_avg_dbm,
        tx_bitrate_100kbps: station
            .tx_bitrate
            .as_ref()
            .and_then(|rate| rate.bitrate_100kbps),
        rx_bitrate_100kbps: station
            .rx_bitrate
            .as_ref()
            .and_then(|rate| rate.bitrate_100kbps),
        tx_retries: station.tx_retries,
        tx_failed: station.tx_failed,
        connected_secs: station.connected_time_secs,
    }
}

fn channel_width_mhz(width: u32) -> Option<u32> {
    match width {
        0 | 1 => Some(20),
        2 => Some(40),
        3 => Some(80),
        4 | 5 => Some(160),
        6 => Some(5),
        7 => Some(10),
        8 => Some(1),
        9 => Some(2),
        10 => Some(4),
        11 => Some(8),
        12 => Some(16),
        13 => Some(320),
        _ => None,
    }
}

#[cfg(test)]
#[path = "net_wifi_tests.rs"]
mod tests;
