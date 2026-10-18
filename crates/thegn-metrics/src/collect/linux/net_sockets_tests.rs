use super::{NetSocketsCollector, SocketTransport};
use crate::collect::{
    CollectError, Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    linux::sock_diag::{AddressFamily, DumpStats, MAX_TCP_SOCKETS, TcpInfo, TcpSock},
    registry::Registry,
    schedule::{Cadence, Mode},
};
use std::io;
use std::net::{IpAddr, Ipv4Addr};
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
}

impl SocketTransport for FakeTransport {
    fn open(&mut self) -> io::Result<()> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        match self.open_error {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Ok(()),
        }
    }

    fn dump(&mut self, out: &mut Vec<TcpSock>) -> io::Result<DumpStats> {
        out.clear();
        out.extend(self.sockets.iter().take(MAX_TCP_SOCKETS).cloned());
        Ok(DumpStats {
            malformed: self.malformed,
            truncated: self.truncated || self.sockets.len() > MAX_TCP_SOCKETS,
        })
    }

    fn close(&mut self) {}
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
    let opens = Arc::new(AtomicUsize::new(0));
    let transport = FakeTransport {
        sockets,
        open_error,
        opens: opens.clone(),
        malformed: 3,
        truncated: false,
    };
    (
        NetSocketsCollector::with_transport(Box::new(transport)),
        opens,
    )
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
fn socket_cap_sets_truncated_without_growing_collector_buffers() {
    let (mut collector, _) = collector(
        vec![socket(1, [198, 51, 100, 1], 1_000, 0); MAX_TCP_SOCKETS + 1],
        None,
    );
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.net_sockets.sockets, MAX_TCP_SOCKETS as u32);
    assert!(frame.parts.net_sockets.truncated);
    let socket_capacity = collector.sockets.capacity();
    let rtt_capacity = collector.rtts.capacity();
    for _ in 0..1_000 {
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
    }
    assert_eq!(collector.sockets.capacity(), socket_capacity);
    assert_eq!(collector.rtts.capacity(), rtt_capacity);
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
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    let socket_capacity = collector.sockets.capacity();
    let rtt_capacity = collector.rtts.capacity();
    for _ in 0..1_000 {
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
    }
    assert_eq!(collector.sockets.capacity(), socket_capacity);
    assert_eq!(collector.rtts.capacity(), rtt_capacity);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
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
