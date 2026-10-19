use super::{
    IfaceBytes, NetSocketsCollector, PhysicalCounters, ProcNetDev, SocketTransport, physical_delta,
};
use crate::collect::{
    CollectError, Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    linux::{
        net_attrib::CgroupFs,
        sock_diag::{AddressFamily, DumpStats, MAX_TCP_SOCKETS, TcpInfo, TcpSock},
    },
    model::netdev::IfaceName,
    policy::net_scope::Scope,
    registry::Registry,
    schedule::{Cadence, Mode},
};
use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct FakeTransport {
    sockets: Vec<TcpSock>,
    open_error: Option<i32>,
    opens: Arc<AtomicUsize>,
    malformed: usize,
    truncated: bool,
    opened: bool,
    dump_error: Option<io::ErrorKind>,
    batches: Vec<Vec<TcpSock>>,
}

impl SocketTransport for FakeTransport {
    fn open(&mut self) -> io::Result<()> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        match self.open_error.take() {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => {
                self.opened = true;
                Ok(())
            }
        }
    }

    fn dump(&mut self, out: &mut Vec<TcpSock>) -> io::Result<DumpStats> {
        if !self.opened {
            self.open()?;
        }
        if let Some(kind) = self.dump_error.take() {
            return Err(io::Error::new(kind, "scripted dump error"));
        }
        out.clear();
        if self.batches.is_empty() {
            out.extend(self.sockets.iter().take(MAX_TCP_SOCKETS).cloned());
        } else {
            out.extend(self.batches.remove(0).into_iter().take(MAX_TCP_SOCKETS));
        }
        Ok(DumpStats {
            malformed: self.malformed,
            truncated: self.truncated,
        })
    }

    fn close(&mut self) {
        self.opened = false;
    }
}

fn socket(state: u8, remote: [u8; 4], rtt_us: u32, last_data_recv_ms: u32) -> TcpSock {
    TcpSock {
        family: AddressFamily::Ipv4,
        state,
        source: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
        destination: IpAddr::V4(Ipv4Addr::from(remote)),
        source_port: 10000,
        destination_port: 443,
        inode: 1,
        cookie: None,
        cgroup_id: None,
        uid: 1000,
        rqueue: 0,
        wqueue: 0,
        info: Some(TcpInfo {
            rtt_us: Some(rtt_us),
            last_data_recv_ms: Some(last_data_recv_ms),
            ..Default::default()
        }),
    }
}

fn collector(
    sockets: Vec<TcpSock>,
    open_error: Option<i32>,
) -> (NetSocketsCollector, Arc<AtomicUsize>) {
    collector_with_dump_error(sockets, open_error, None)
}

fn collector_with_dump_error(
    sockets: Vec<TcpSock>,
    open_error: Option<i32>,
    dump_error: Option<io::ErrorKind>,
) -> (NetSocketsCollector, Arc<AtomicUsize>) {
    let opens = Arc::new(AtomicUsize::new(0));
    let transport = FakeTransport {
        sockets,
        open_error,
        opens: opens.clone(),
        malformed: 3,
        truncated: false,
        opened: false,
        dump_error,
        batches: Vec::new(),
    };
    (
        NetSocketsCollector::with_transport(Box::new(transport)),
        opens,
    )
}

#[derive(Default)]
struct FakeCgroupFs;

impl CgroupFs for FakeCgroupFs {
    fn children(&self, _path: &Path, _out: &mut Vec<PathBuf>) -> io::Result<()> {
        Ok(())
    }

    fn inode(&self, _path: &Path) -> io::Result<u64> {
        Err(io::ErrorKind::NotFound.into())
    }
}

struct FakePhysical {
    batches: VecDeque<Vec<IfaceBytes>>,
}

impl PhysicalCounters for FakePhysical {
    fn read(&mut self, out: &mut Vec<IfaceBytes>) -> io::Result<()> {
        out.clear();
        if let Some(rows) = self.batches.pop_front() {
            out.extend(rows);
        }
        Ok(())
    }

    fn truncated(&self) -> bool {
        false
    }

    fn close(&mut self) {}
}

fn tcp_with_counters(cookie: u64, cgroup_id: u64, rx: u64, tx: u64) -> TcpSock {
    let mut socket = socket(1, [198, 51, 100, 7], 1_000, 0);
    socket.cookie = Some(cookie);
    socket.cgroup_id = Some(cgroup_id);
    socket.info = Some(TcpInfo {
        bytes_received: Some(rx),
        bytes_acked: Some(tx),
        ..Default::default()
    });
    socket
}

fn iface(name: &[u8], scope: Scope, rx: u64, tx: u64) -> IfaceBytes {
    IfaceBytes {
        name: IfaceName::new(name),
        scope,
        rx: Some(rx),
        tx: Some(tx),
    }
}

#[test]
fn canned_dump_reports_states_listener_recent_non_loopback_rtts_and_histogram() {
    let (mut collector, _) = collector(
        vec![
            socket(1, [198, 51, 100, 1], 800, 0),
            socket(1, [198, 51, 100, 2], 4_000, 1_000),
            socket(1, [198, 51, 100, 3], 30_000, 10_001),
            socket(1, [127, 0, 0, 1], 70_000, 0),
            socket(10, [0, 0, 0, 0], 900_000, 0),
            socket(8, [198, 51, 100, 4], 900, 0),
        ],
        None,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();

    assert_eq!(collector.id(), CollectorId::NetSockets);
    assert_eq!(collector.cadence(), Cadence::Every(5));
    let part = &frame.parts.net_sockets;
    assert_eq!(part.states[0], 4);
    assert_eq!(part.states[7], 1);
    assert_eq!(part.states[9], 1);
    assert_eq!(part.listening, 1);
    assert_eq!(
        (part.rtt_p50_us, part.rtt_p90_us, part.rtt_max_us),
        (Some(800), Some(4_000), Some(4_000))
    );
    assert_eq!(part.rtt_hist, [1, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(part.sockets, 6);
    assert_eq!(part.malformed, 3);
    assert!(frame.sampled.contains(CollectorId::NetSockets));
}

#[test]
fn collector_passes_through_transport_truncation_and_reuses_sample_buffers() {
    let (mut collector, _) = collector(
        vec![socket(1, [198, 51, 100, 1], 1_000, 0); MAX_TCP_SOCKETS + 1],
        None,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.net_sockets.sockets, MAX_TCP_SOCKETS as u32);
    assert!(!frame.parts.net_sockets.truncated);
    let socket_capacity = collector.sockets.capacity();
    let rtt_capacity = collector.rtts.capacity();
    assert!(socket_capacity >= MAX_TCP_SOCKETS);
    for _ in 0..1_000 {
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
    }
    assert_eq!(collector.sockets.capacity(), socket_capacity);
    assert_eq!(collector.rtts.capacity(), rtt_capacity);
}

#[test]
fn socket_attribution_and_physical_remainder_share_the_dump_interval() {
    let opens = Arc::new(AtomicUsize::new(0));
    let first = vec![
        tcp_with_counters(11, 101, 1_000, 500),
        tcp_with_counters(12, 202, 2_000, 900),
    ];
    let second = vec![
        tcp_with_counters(11, 101, 1_125, 560),
        tcp_with_counters(12, 202, 2_240, 1_000),
    ];
    let third = vec![
        tcp_with_counters(11, 101, 100_000, 100_000),
        tcp_with_counters(12, 202, 100_000, 100_000),
    ];
    let transport = FakeTransport {
        sockets: Vec::new(),
        open_error: None,
        opens,
        malformed: 0,
        truncated: false,
        opened: false,
        dump_error: None,
        batches: vec![first, second, third],
    };
    let physical = FakePhysical {
        batches: VecDeque::from([
            vec![iface(b"eth0", Scope::Physical, 10_000, 20_000)],
            vec![iface(b"eth0", Scope::Physical, 10_500, 20_300)],
            vec![iface(b"eth0", Scope::Physical, 10_501, 20_301)],
        ]),
    };
    let mut collector = NetSocketsCollector::<FakeCgroupFs>::with_parts(
        Box::new(transport),
        Box::new(physical),
        vec![PathBuf::from("/fake")],
        Box::new(|| FakeCgroupFs),
    );
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                now_ms: 5_000,
                ..Default::default()
            },
            &mut frame,
        )
        .unwrap();
    let baseline = &frame.parts.net_sockets;
    assert!(baseline.attribution_interval_ms.is_none());
    assert!(!baseline.physical_known);
    assert!(
        baseline
            .attribution
            .owners
            .iter()
            .all(|owner| owner.rx_bytes == 0 && owner.tx_bytes == 0)
    );

    collector
        .sample(
            &TickCtx {
                now_ms: 10_000,
                ..Default::default()
            },
            &mut frame,
        )
        .unwrap();
    let part = &frame.parts.net_sockets;
    assert_eq!(part.attribution_interval_ms, Some(5_000));
    assert!(part.physical_known);
    assert_eq!(part.attribution.owners.len(), 2);
    assert_eq!(
        (
            part.attribution.owners[0].rx_bytes,
            part.attribution.owners[0].tx_bytes
        ),
        (125, 60)
    );
    assert_eq!(
        (
            part.attribution.owners[1].rx_bytes,
            part.attribution.owners[1].tx_bytes
        ),
        (240, 100)
    );
    assert_eq!(
        (
            part.attribution.unattributed_rx,
            part.attribution.unattributed_tx
        ),
        (135, 140)
    );
    let owner_capacity = part.attribution.owners.capacity();

    collector
        .sample(
            &TickCtx {
                now_ms: 15_000,
                ..Default::default()
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(
        (
            frame.parts.net_sockets.attribution.unattributed_rx,
            frame.parts.net_sockets.attribution.unattributed_tx
        ),
        (0, 0)
    );
    assert_eq!(
        frame.parts.net_sockets.attribution.owners.capacity(),
        owner_capacity,
        "owner capacity {}",
        frame.parts.net_sockets.attribution.owners.capacity()
    );

    collector.close();
    collector
        .sample(
            &TickCtx {
                now_ms: 20_000,
                ..Default::default()
            },
            &mut frame,
        )
        .unwrap();
    assert!(frame.parts.net_sockets.attribution_interval_ms.is_none());
    assert!(!frame.parts.net_sockets.physical_known);
}

#[test]
fn physical_deltas_ignore_loopback_new_links_and_counter_resets() {
    let previous = [
        iface(b"eth0", Scope::Physical, 100, 200),
        iface(b"lo", Scope::Loopback, 500, 600),
        iface(b"eth1", Scope::Physical, 1_000, 1_000),
    ];
    let current = [
        iface(b"eth0", Scope::Physical, 150, 280),
        iface(b"lo", Scope::Loopback, 900, 900),
        iface(b"eth1", Scope::Physical, 20, 30),
        iface(b"eth2", Scope::Physical, 5_000, 6_000),
    ];
    assert_eq!(physical_delta(&previous, &current), (50, 80));
    let fallback_previous = [iface(b"veth1", Scope::Container, 100, 100)];
    let fallback_current = [
        iface(b"veth1", Scope::Container, 140, 150),
        iface(b"lo", Scope::Loopback, 900, 900),
    ];
    assert_eq!(
        physical_delta(&fallback_previous, &fallback_current),
        (40, 50)
    );
}

#[test]
fn proc_netdev_reader_uses_the_shared_parser_and_excludes_loopback_scope() {
    let dir = tempfile::tempdir().unwrap();
    let proc_path = dir.path().join("net-dev-fixture");
    let sysfs = dir.path().join("sys-class-net");
    std::fs::create_dir(&sysfs).unwrap();
    std::fs::write(
        &proc_path,
        b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nlo: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\neth0: 300 1 0 0 0 0 0 0 400 1 0 0 0 0 0 0\n",
    )
    .unwrap();
    let mut reader = ProcNetDev::at(proc_path, sysfs);
    let mut rows = Vec::with_capacity(2);
    reader.read(&mut rows).unwrap();
    assert_eq!(rows.len(), 2);
    let loopback = rows
        .iter()
        .find(|row| row.name.as_str() == "lo")
        .expect("lo row");
    assert_eq!(loopback.scope, Scope::Loopback);
    assert_eq!((loopback.rx, loopback.tx), (Some(100), Some(200)));
}

#[test]
fn permission_denial_is_absent_and_does_not_retry_on_each_tick() {
    let (collector, opens) = collector(Vec::new(), Some(libc::EPERM));
    let mut registry = Registry::new(vec![Box::new(collector)]);
    registry.discover(&Env::default());
    assert_eq!(
        registry.health(CollectorId::NetSockets),
        Health::Absent(Absent::NoPermission)
    );
    assert!(CollectorId::NetSockets.visible_only());
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetSockets);
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
fn transient_discovery_open_failure_recovers_on_later_sample() {
    let (collector, opens) = collector(
        vec![socket(1, [198, 51, 100, 1], 1_000, 0)],
        Some(libc::EMFILE),
    );
    let mut registry = Registry::new(vec![Box::new(collector)]);
    registry.discover(&Env::default());
    assert!(matches!(
        registry.health(CollectorId::NetSockets),
        Health::Failing { .. }
    ));
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetSockets);
    let mut frame = MonitorFrame::default();
    let retry_tick = (1..=5)
        .find(|tick| {
            registry
                .due(Mode::Open, demand, *tick)
                .contains(CollectorId::NetSockets)
        })
        .unwrap();
    registry.tick(
        Mode::Open,
        demand,
        &TickCtx {
            tick: retry_tick,
            ..Default::default()
        },
        &mut frame,
    );
    assert_eq!(frame.parts.net_sockets.sockets, 1);
    assert_eq!(opens.load(Ordering::SeqCst), 2);
}

#[test]
fn mapped_ipv4_loopback_is_excluded_from_rtt_statistics() {
    let mut row = socket(1, [127, 0, 0, 1], 42_000, 0);
    row.family = AddressFamily::Ipv6;
    row.destination = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
    let (mut collector, _) = collector(vec![row], None);
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.net_sockets.rtt_p50_us, None);
}

#[test]
fn timed_out_dump_is_gone_and_registry_reopens_before_next_sample() {
    let (collector, opens) = collector_with_dump_error(
        vec![socket(1, [198, 51, 100, 1], 1_000, 0)],
        None,
        Some(io::ErrorKind::TimedOut),
    );
    let mut registry = Registry::new(vec![Box::new(collector)]);
    registry.discover(&Env::default());
    let mut demand = CollectorSet::empty();
    demand.insert(CollectorId::NetSockets);
    let mut frame = MonitorFrame::default();
    let first_sample_tick = (0..5)
        .find(|tick| {
            registry
                .due(Mode::Open, demand, *tick)
                .contains(CollectorId::NetSockets)
        })
        .unwrap();
    registry.tick(
        Mode::Open,
        demand,
        &TickCtx {
            tick: first_sample_tick,
            ..Default::default()
        },
        &mut frame,
    );
    assert_eq!(opens.load(Ordering::SeqCst), 3);
    let next_sample_tick = first_sample_tick + 5;
    registry.tick(
        Mode::Open,
        demand,
        &TickCtx {
            tick: next_sample_tick,
            ..Default::default()
        },
        &mut frame,
    );
    assert_eq!(frame.parts.net_sockets.sockets, 1);
    assert_eq!(opens.load(Ordering::SeqCst), 4);
}

#[test]
fn permission_or_unsupported_protocol_maps_to_absence() {
    let (mut collector, _) = collector(Vec::new(), Some(libc::EPROTONOSUPPORT));
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
}

#[test]
fn open_collector_reuses_socket_and_percentile_buffers_over_one_thousand_samples() {
    let (mut collector, opens) = collector(vec![socket(1, [198, 51, 100, 1], 12_000, 100)], None);
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    assert_eq!(collector.sockets.capacity(), 0);
    assert_eq!(collector.rtts.capacity(), 0);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    let socket_capacity = collector.sockets.capacity();
    let rtt_capacity = collector.rtts.capacity();
    for _ in 0..1_000 {
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
    }
    assert_eq!(collector.sockets.capacity(), socket_capacity);
    assert_eq!(collector.rtts.capacity(), rtt_capacity);
    assert_eq!(opens.load(Ordering::SeqCst), 2);
}

#[test]
fn live_sock_diag_dump_is_bounded_and_skips_when_unavailable() {
    let mut collector = NetSocketsCollector::new();
    if matches!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware | Absent::NoPermission)
    ) {
        return;
    }
    let mut frame = MonitorFrame::default();
    match collector.sample(&TickCtx::default(), &mut frame) {
        Ok(()) => {
            assert!(frame.parts.net_sockets.sockets <= MAX_TCP_SOCKETS as u32);
            assert!(frame.parts.net_sockets.rtt_hist.iter().sum::<u32>() <= MAX_TCP_SOCKETS as u32);
        }
        Err(CollectError::Gone) => {}
        Err(error) => panic!("live sock_diag dump failed: {error:?}"),
    }
    collector.close();
}
