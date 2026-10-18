use super::{NetWifiCollector, WifiTransport};
use crate::collect::{
    Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    linux::nl80211::{IFNAME_LEN, InlineBytes, Interface, SSID_LEN, Station},
    model::net_wifi::WifiRow,
    registry::Registry,
    schedule::{Cadence, Mode},
};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicI32, AtomicUsize, Ordering},
};

#[derive(Clone)]
struct FakeTransport {
    open_error: Option<io::ErrorKind>,
    open_errno: Option<i32>,
    interfaces: Vec<Interface>,
    station: Option<Station>,
    station_errno: Arc<AtomicI32>,
    open_count: Arc<AtomicUsize>,
    resolve_count: Arc<AtomicUsize>,
    interface_count: Arc<AtomicUsize>,
    station_count: Arc<AtomicUsize>,
    enoent_once: bool,
    station_enoent_once: bool,
    closed: bool,
}

impl WifiTransport for FakeTransport {
    fn open(&mut self) -> io::Result<()> {
        self.open_count.fetch_add(1, Ordering::SeqCst);
        match (self.open_error, self.open_errno) {
            (_, Some(errno)) => Err(io::Error::from_raw_os_error(errno)),
            (Some(kind), None) => Err(io::Error::new(kind, "fake open failure")),
            (None, None) => Ok(()),
        }
    }

    fn close(&mut self) {
        self.closed = true;
    }

    fn resolve_family(&mut self) -> io::Result<u16> {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        if self.closed {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(37)
    }

    fn interfaces(
        &mut self,
        family: u16,
        out: &mut Vec<Interface>,
        truncated: &mut bool,
    ) -> io::Result<()> {
        assert_eq!(family, 37);
        self.interface_count.fetch_add(1, Ordering::SeqCst);
        if self.enoent_once {
            self.enoent_once = false;
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        out.clear();
        *truncated = false;
        out.extend(self.interfaces.iter().copied());
        Ok(())
    }

    fn station(&mut self, family: u16, ifindex: u32) -> io::Result<Option<Station>> {
        assert_eq!(family, 37);
        assert_eq!(ifindex, 7);
        self.station_count.fetch_add(1, Ordering::SeqCst);
        if self.station_enoent_once {
            self.station_enoent_once = false;
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        let errno = self.station_errno.swap(0, Ordering::SeqCst);
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }
        Ok(self.station.clone())
    }
}

fn bytes<const N: usize>(value: &[u8]) -> InlineBytes<N> {
    let mut output = InlineBytes::default();
    output.bytes[..value.len()].copy_from_slice(value);
    output.len = u8::try_from(value.len()).unwrap();
    output
}

fn managed_interface() -> Interface {
    Interface {
        ifindex: Some(7),
        name: Some(bytes::<IFNAME_LEN>(b"wlan0")),
        iftype: Some(2),
        ssid: Some(bytes::<SSID_LEN>(b"visible network")),
        frequency_mhz: Some(5180),
        channel_width: Some(3),
        ..Default::default()
    }
}

fn connected_station() -> Station {
    Station {
        bssid: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
        signal_dbm: Some(-48),
        signal_avg_dbm: Some(-51),
        tx_bitrate: Some(crate::collect::linux::nl80211::RateInfo {
            bitrate_100kbps: Some(866),
            ..Default::default()
        }),
        rx_bitrate: Some(crate::collect::linux::nl80211::RateInfo {
            bitrate_100kbps: Some(540),
            ..Default::default()
        }),
        tx_retries: Some(4),
        tx_failed: Some(1),
        connected_time_secs: Some(900),
        ..Default::default()
    }
}

fn fixture(
    interfaces: Vec<Interface>,
    station: Option<Station>,
    open_error: Option<io::ErrorKind>,
    open_errno: Option<i32>,
    enoent_once: bool,
    station_enoent_once: bool,
) -> (
    NetWifiCollector,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicI32>,
) {
    let open_count = Arc::new(AtomicUsize::new(0));
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let interface_count = Arc::new(AtomicUsize::new(0));
    let station_count = Arc::new(AtomicUsize::new(0));
    let station_errno = Arc::new(AtomicI32::new(0));
    let transport = FakeTransport {
        open_error,
        open_errno,
        interfaces,
        station,
        station_errno: station_errno.clone(),
        open_count: open_count.clone(),
        resolve_count: resolve_count.clone(),
        interface_count,
        station_count,
        enoent_once,
        station_enoent_once,
        closed: false,
    };
    (
        NetWifiCollector::with_transport(Box::new(transport)),
        open_count,
        resolve_count,
        station_errno,
    )
}

#[test]
fn canned_interface_and_station_replies_produce_visible_ssid_and_masked_bssid() {
    let (mut collector, _, _, _) = fixture(
        vec![managed_interface()],
        Some(connected_station()),
        None,
        None,
        false,
        false,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(collector.id(), CollectorId::NetWifi);
    assert_eq!(collector.cadence(), Cadence::Every(5));
    assert_eq!(
        frame.parts.net_wifi.ifaces,
        vec![WifiRow {
            ifindex: 7,
            name: bytes::<IFNAME_LEN>(b"wlan0"),
            ssid: Some(bytes::<SSID_LEN>(b"visible network")),
            bssid_masked: Some("xx:xx:xx:xx:xx:xx"),
            freq_mhz: Some(5180),
            width_mhz: Some(80),
            signal_dbm: Some(-48),
            signal_avg_dbm: Some(-51),
            tx_bitrate_100kbps: Some(866),
            rx_bitrate_100kbps: Some(540),
            tx_retries: Some(4),
            tx_failed: Some(1),
            connected_secs: Some(900),
        }]
    );
    assert!(frame.sampled.contains(CollectorId::NetWifi));
}

#[test]
fn no_managed_wireless_interface_is_absent_no_hardware() {
    let mut other = managed_interface();
    other.iftype = Some(1);
    let (mut collector, _, _, _) = fixture(vec![other], None, None, None, false, false);
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
}

#[test]
fn permission_denial_is_absent_and_does_not_reopen_each_tick() {
    let (collector, opens, _, _) = fixture(
        Vec::new(),
        None,
        Some(io::ErrorKind::PermissionDenied),
        None,
        false,
        false,
    );
    let mut registry = Registry::new(vec![Box::new(collector)]);
    registry.discover(&Env::default());
    assert_eq!(
        registry.health(CollectorId::NetWifi),
        Health::Absent(Absent::NoPermission)
    );
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetWifi);
    let mut frame = MonitorFrame::default();
    for tick in 0..10 {
        registry.tick(
            Mode::Open,
            demand,
            &TickCtx {
                tick,
                ..Default::default()
            },
            &mut frame,
        );
    }
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}

#[test]
fn unsupported_generic_netlink_family_is_absent_no_hardware() {
    let (mut collector, _, _, _) = fixture(
        Vec::new(),
        None,
        None,
        Some(libc::EAFNOSUPPORT),
        false,
        false,
    );
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
}

#[test]
fn family_is_resolved_again_after_kernel_reports_enoent() {
    let (mut collector, _, resolves, _) = fixture(
        vec![managed_interface()],
        Some(connected_station()),
        None,
        None,
        true,
        false,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    assert_eq!(resolves.load(Ordering::SeqCst), 2);
}

#[test]
fn station_enoent_reresolves_family_and_sample_succeeds_through_registry() {
    let (collector, _, resolves, _) = fixture(
        vec![managed_interface()],
        Some(connected_station()),
        None,
        None,
        false,
        true,
    );
    let mut registry = Registry::new(vec![Box::new(collector)]);
    registry.discover(&Env::default());
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetWifi);
    let mut frame = MonitorFrame::default();
    let tick = wifi_due_tick(&registry, 0);
    registry.tick(
        Mode::Open,
        demand,
        &TickCtx {
            tick,
            ..Default::default()
        },
        &mut frame,
    );
    assert_eq!(resolves.load(Ordering::SeqCst), 2);
    assert_eq!(registry.health(CollectorId::NetWifi), Health::Ok);
    assert_eq!(frame.parts.net_wifi.ifaces.len(), 1);
}

#[test]
fn sample_permission_error_becomes_absent_after_registry_rediscovery() {
    let (collector, opens, _, station_errno) = fixture(
        vec![managed_interface()],
        Some(connected_station()),
        None,
        None,
        false,
        false,
    );
    station_errno.store(libc::EPERM, Ordering::SeqCst);
    let mut registry = Registry::new(vec![Box::new(collector)]);
    registry.discover(&Env::default());
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetWifi);
    let mut frame = MonitorFrame::default();
    let tick = wifi_due_tick(&registry, 0);
    let next_tick = wifi_due_tick(&registry, 1);
    registry.tick(
        Mode::Open,
        demand,
        &TickCtx {
            tick,
            ..Default::default()
        },
        &mut frame,
    );
    assert_eq!(
        registry.health(CollectorId::NetWifi),
        Health::Absent(Absent::NoPermission)
    );
    assert!(frame.parts.net_wifi.ifaces.is_empty());
    registry.tick(
        Mode::Open,
        demand,
        &TickCtx {
            tick: next_tick,
            ..Default::default()
        },
        &mut frame,
    );
    assert_eq!(opens.load(Ordering::SeqCst), 2);
}

fn wifi_due_tick(registry: &Registry, occurrence: usize) -> u64 {
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetWifi);
    (0_u64..30)
        .filter(|tick| {
            registry
                .due(Mode::Open, demand, *tick)
                .contains(CollectorId::NetWifi)
        })
        .nth(occurrence)
        .unwrap()
}

#[test]
fn collector_retains_interface_vector_capacity_for_one_thousand_samples() {
    let (mut collector, _, _, _) = fixture(
        vec![managed_interface()],
        Some(connected_station()),
        None,
        None,
        false,
        false,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    let interface_capacity = collector.interfaces.capacity();
    let row_capacity = frame.parts.net_wifi.ifaces.capacity();
    for _ in 1..1_000 {
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
    }
    assert_eq!(collector.interfaces.capacity(), interface_capacity);
    assert_eq!(interface_capacity, 16);
    assert_eq!(frame.parts.net_wifi.ifaces.capacity(), row_capacity);
}

#[test]
fn interface_rows_are_bounded_and_report_truncation() {
    let (mut collector, _, _, _) = fixture(
        vec![managed_interface(); 17],
        Some(connected_station()),
        None,
        None,
        false,
        false,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.net_wifi.ifaces.len(), 16);
    assert!(frame.parts.net_wifi.truncated);
}
