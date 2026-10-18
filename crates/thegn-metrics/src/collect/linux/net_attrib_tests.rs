use super::{
    CgroupFs, CookieDelta, MAX_FRONTIER_PATHS, MAX_STATS_PER_REFRESH, OwnerTable, hash_cookie,
};
use crate::collect::{
    linux::sock_diag::{AddressFamily, TcpInfo, TcpSock},
    model::net_attrib::NetAttribution,
};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

fn socket(cookie: u64, cgroup_id: Option<u64>, tx: u64, rx: u64) -> TcpSock {
    TcpSock {
        family: AddressFamily::Ipv4,
        state: 1,
        source: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
        destination: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 2)),
        source_port: 1000,
        destination_port: 443,
        inode: 7,
        uid: 1000,
        rqueue: 0,
        wqueue: 0,
        cookie: Some(cookie),
        cgroup_id,
        info: Some(TcpInfo {
            bytes_acked: Some(tx),
            bytes_received: Some(rx),
            ..Default::default()
        }),
    }
}

#[derive(Default)]
struct FakeFs {
    inodes: HashMap<PathBuf, u64>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
    stats: Cell<usize>,
}

impl CgroupFs for FakeFs {
    fn children(&self, path: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        if let Some(children) = self.children.get(path) {
            out.extend(children.iter().cloned());
        }
        Ok(())
    }

    fn inode(&self, path: &Path) -> io::Result<u64> {
        self.stats.set(self.stats.get() + 1);
        self.inodes
            .get(path)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))
    }
}

fn table() -> OwnerTable<FakeFs> {
    let root = PathBuf::from("/cgroup/user.slice");
    let pane = root.join("thegn.slice/run-p1-i2.scope");
    let mut fs = FakeFs::default();
    fs.inodes.insert(root.clone(), 1);
    fs.inodes.insert(pane.clone(), 42);
    fs.children.insert(root.clone(), vec![pane]);
    OwnerTable::new(fs, [root])
}

fn update(
    map: &mut CookieDelta,
    sockets: &[TcpSock],
    physical_rx: u64,
    physical_tx: u64,
    owners: &mut OwnerTable<FakeFs>,
    out: &mut NetAttribution,
) {
    map.update_into(sockets, physical_rx, physical_tx, owners, out);
}

fn owner_delta(result: &NetAttribution, cgroup_id: u64) -> (u64, u64) {
    let owner = result
        .owners
        .iter()
        .find(|owner| owner.cgroup_id == cgroup_id)
        .expect("socket owner remains present");
    (owner.tx_bytes, owner.rx_bytes)
}

fn colliding_cookie(first: u64) -> u64 {
    let home = hash_cookie(first) % 4_096;
    (first + 1..)
        .find(|cookie| hash_cookie(*cookie) % 4_096 == home)
        .expect("cookie hash has a collision")
}

#[test]
fn cookie_delta_baselines_first_dump_and_counts_second_dump_delta() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[socket(9, Some(42), 100, 50)],
        500,
        600,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (0, 0)
    );
    update(
        &mut map,
        &[socket(9, Some(42), 145, 83)],
        500,
        600,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (45, 33)
    );
    assert_eq!(
        result.owners[0].kind,
        crate::collect::policy::cgroup_classify::OwnerKind::Pane
    );
}

#[test]
fn new_cookie_is_baselined_and_closed_cookie_is_swept() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[socket(1, Some(42), 10, 20)],
        0,
        0,
        &mut owners,
        &mut result,
    );
    update(
        &mut map,
        &[socket(1, Some(42), 10, 20), socket(2, Some(42), 30, 40)],
        100,
        100,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (0, 0)
    );
    update(&mut map, &[], 0, 0, &mut owners, &mut result);
    update(
        &mut map,
        &[socket(2, Some(42), 35, 48)],
        100,
        100,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (0, 0)
    );
    update(
        &mut map,
        &[socket(2, Some(42), 38, 54)],
        100,
        100,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (3, 6)
    );
}

#[test]
fn swept_collision_keeps_surviving_cookie_probe_reachable() {
    let a = 10;
    let b = colliding_cookie(a);
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[socket(a, Some(42), 10, 20), socket(b, Some(42), 100, 200)],
        0,
        0,
        &mut owners,
        &mut result,
    );
    update(
        &mut map,
        &[socket(b, Some(42), 107, 211)],
        100,
        100,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (7, 11)
    );
}

#[test]
fn colliding_new_cookie_does_not_replace_unvisited_live_cookie() {
    let a = 21;
    let b = colliding_cookie(a);
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[socket(a, Some(42), 100, 200)],
        0,
        0,
        &mut owners,
        &mut result,
    );
    update(
        &mut map,
        &[
            socket(b, Some(42), 5_000, 6_000),
            socket(a, Some(42), 107, 211),
        ],
        100,
        100,
        &mut owners,
        &mut result,
    );
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (7, 11)
    );
}

#[test]
fn tombstone_rebuild_keeps_a_forced_collision_chain_reachable() {
    let a = 10;
    let b = colliding_cookie(a);
    let c = colliding_cookie(b);
    let home = hash_cookie(a) % 4_096;
    assert_eq!(hash_cookie(b) % 4_096, home);
    assert_eq!(hash_cookie(c) % 4_096, home);

    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[
            socket(a, Some(41), 10, 20),
            socket(b, Some(42), 100, 200),
            socket(c, Some(43), 200, 300),
        ],
        0,
        0,
        &mut owners,
        &mut result,
    );

    // Removing a leaves b and c behind the home-slot tombstone.
    update(
        &mut map,
        &[socket(b, Some(42), 107, 211), socket(c, Some(43), 207, 311)],
        0,
        0,
        &mut owners,
        &mut result,
    );
    assert_eq!(owner_delta(&result, 42), (7, 11));
    assert_eq!(owner_delta(&result, 43), (7, 11));
    let chain_entries = map.entries.as_ref().expect("cookie table stays allocated");
    let b_slot = chain_entries
        .iter()
        .position(|entry| entry.occupied && entry.cookie == b)
        .expect("b remains behind the tombstone");
    let c_slot = chain_entries
        .iter()
        .position(|entry| entry.occupied && entry.cookie == c)
        .expect("c remains behind the tombstone");
    assert!(chain_entries[home].tombstone);
    assert_eq!(
        (b_slot + chain_entries.len() - home) % chain_entries.len(),
        1
    );
    assert_eq!(
        (c_slot + chain_entries.len() - home) % chain_entries.len(),
        2
    );

    for batch in 0_u64..4 {
        let mut sockets = Vec::with_capacity(502);
        sockets.push(socket(
            b,
            Some(42),
            107 + (batch + 1) * 7,
            211 + (batch + 1) * 11,
        ));
        sockets.push(socket(
            c,
            Some(43),
            207 + (batch + 1) * 7,
            311 + (batch + 1) * 11,
        ));
        sockets.extend(
            (0_u64..500).map(|index| socket(3_000_000 + batch * 500 + index, None, batch, batch)),
        );
        update(&mut map, &sockets, 0, 0, &mut owners, &mut result);

        assert_eq!(owner_delta(&result, 42), (7, 11));
        assert_eq!(owner_delta(&result, 43), (7, 11));
        if batch == 3 {
            assert_eq!(
                map.tombstone_count(),
                0,
                "threshold-crossing dump rebuilds the table"
            );
        }
    }

    let sockets = [
        socket(a, Some(41), 9_000, 9_000),
        socket(b, Some(42), 142, 266),
        socket(c, Some(43), 242, 366),
    ];
    update(&mut map, &sockets, 0, 0, &mut owners, &mut result);
    assert_eq!(owner_delta(&result, 41), (0, 0));
    assert_eq!(owner_delta(&result, 42), (7, 11));
    assert_eq!(owner_delta(&result, 43), (7, 11));

    let entries = map.entries.as_ref().expect("cookie table stays allocated");
    let a_slot = entries
        .iter()
        .position(|entry| entry.occupied && entry.cookie == a)
        .expect("reappearing a gets its own live slot");
    let b_slot = entries
        .iter()
        .position(|entry| entry.occupied && entry.cookie == b)
        .expect("b remains live");
    let c_slot = entries
        .iter()
        .position(|entry| entry.occupied && entry.cookie == c)
        .expect("c remains live");
    assert_ne!(a_slot, b_slot);
    assert_ne!(a_slot, c_slot);
    assert_ne!(b_slot, c_slot);
}

#[test]
fn cookie_and_result_capacities_are_stable_across_one_thousand_dumps() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[socket(1, Some(42), 0, 0)],
        0,
        0,
        &mut owners,
        &mut result,
    );
    let cookie_capacity = map.capacity();
    let totals_capacity = map.totals.capacity();
    let ids_capacity = map.ids.capacity();
    let owners_capacity = result.owners.capacity();
    assert_eq!(cookie_capacity, 4_096);
    for tick in 1_u64..1_000 {
        update(
            &mut map,
            &[socket(1, Some(42), tick, tick)],
            0,
            0,
            &mut owners,
            &mut result,
        );
        assert_eq!(map.capacity(), cookie_capacity);
        assert_eq!(map.totals.capacity(), totals_capacity);
        assert_eq!(map.ids.capacity(), ids_capacity);
        assert_eq!(result.owners.capacity(), owners_capacity);
    }
    map.close();
    assert_eq!(map.capacity(), 0);
}

#[test]
fn socket_churn_rebuilds_tombstones_without_changing_live_deltas() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    let capacity = 4_096;

    for tick in 0_u64..200 {
        let mut sockets = Vec::with_capacity(520);
        for index in 0_u64..20 {
            sockets.push(socket(index + 1, Some(1_000 + index), tick * 3, tick * 5));
        }
        for index in 0_u64..500 {
            sockets.push(socket(1_000_000 + tick * 500 + index, None, tick, tick));
        }
        update(&mut map, &sockets, 0, 0, &mut owners, &mut result);

        assert_eq!(map.capacity(), capacity);
        assert!(
            map.tombstone_count() <= capacity / 4,
            "dump {tick}: {} tombstones exceed the quarter-capacity limit",
            map.tombstone_count()
        );
        assert!(
            map.probes_last_update() > 0,
            "dump {tick}: probe counter must record socket lookups"
        );
        assert!(
            map.probes_last_update() < 64 * sockets.len(),
            // After rebuilding, at most 520 live entries + 1,024 tombstones + 520
            // inserts occupy 4,096 slots (under 51%); sequential cookies hash to
            // adjacent slots. Without rebuild, the table saturates in about 8
            // dumps (4,096 / 500), and each subsequent miss scans all 4,096 slots.
            "dump {tick}: {} probes for {} sockets exceeds the derived 64 probes/socket bound",
            map.probes_last_update(),
            sockets.len()
        );

        for index in 0_u64..20 {
            let owner = result
                .owners
                .iter()
                .find(|owner| owner.cgroup_id == 1_000 + index)
                .expect("long-lived socket owner remains present");
            let expected: (u64, u64) = if tick == 0 { (0, 0) } else { (3, 5) };
            assert_eq!((owner.tx_bytes, owner.rx_bytes), expected);
        }
    }
    map.close();
    assert_eq!(map.capacity(), 0);
    assert!(map.scratch.is_none());
}

#[test]
fn socket_churn_probe_count_stays_bounded_after_table_warmup() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();

    for tick in 0_u64..200 {
        let sockets: Vec<_> = (0_u64..500)
            .map(|index| socket(2_000_000 + tick * 500 + index, None, tick, tick))
            .collect();
        update(&mut map, &sockets, 0, 0, &mut owners, &mut result);
    }

    let last_probe_count = map.probes_last_update();
    assert!(
        last_probe_count > 0,
        "probe counter must record socket lookups"
    );
    assert!(
        last_probe_count < 64 * 500,
        "final churn dump used {last_probe_count} probes for 500 sockets; the derived 64/socket bound is based on under 51% occupancy (520 live + 1,024 tombstones + 520 inserts in 4,096 slots) and adjacent sequential-cookie hashes, unlike a saturated table that scans 4,096 slots per miss; expected fewer than {}",
        64 * 500
    );
}

#[test]
fn physical_remainder_saturates_at_zero() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    update(
        &mut map,
        &[socket(1, Some(42), 0, 0)],
        0,
        0,
        &mut owners,
        &mut result,
    );
    update(
        &mut map,
        &[socket(1, Some(42), 20, 10)],
        1,
        2,
        &mut owners,
        &mut result,
    );
    assert_eq!((result.unattributed_rx, result.unattributed_tx), (0, 0));
}

#[test]
fn loopback_traffic_does_not_reduce_physical_remainder() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let mut result = NetAttribution::default();
    let mut loopback = socket(1, Some(42), 0, 0);
    loopback.source = IpAddr::V4(Ipv4Addr::LOCALHOST);
    loopback.destination = IpAddr::V4(Ipv4Addr::LOCALHOST);
    update(
        &mut map,
        &[loopback.clone()],
        0,
        0,
        &mut owners,
        &mut result,
    );
    loopback.info.as_mut().unwrap().bytes_acked = Some(20);
    loopback.info.as_mut().unwrap().bytes_received = Some(10);
    update(&mut map, &[loopback], 100, 100, &mut owners, &mut result);
    assert_eq!(
        (result.owners[0].tx_bytes, result.owners[0].rx_bytes),
        (20, 10)
    );
    assert_eq!((result.unattributed_rx, result.unattributed_tx), (100, 100));
}

#[test]
fn owner_walk_resumes_deeper_than_sixty_four_nodes_with_bounded_stats() {
    let root = PathBuf::from("/walk");
    let mut fs = FakeFs::default();
    let mut path = root.clone();
    fs.inodes.insert(path.clone(), 1);
    for depth in 0..80 {
        let next = path.join(depth.to_string());
        fs.inodes.insert(next.clone(), (depth + 2) as u64);
        fs.children.insert(path.clone(), vec![next.clone()]);
        path = next;
    }
    let target = 81;
    let mut table = OwnerTable::new(fs, [root]);
    assert_eq!(table.refresh(&[target]), MAX_STATS_PER_REFRESH);
    assert_eq!(table.fs.stats.get(), MAX_STATS_PER_REFRESH);
    assert_eq!(table.path(target), None);
    let stats_before = table.fs.stats.get();
    assert!(table.refresh(&[target]) <= MAX_STATS_PER_REFRESH);
    assert!(table.fs.stats.get() - stats_before <= MAX_STATS_PER_REFRESH);
    assert!(table.path(target).is_some());
}

#[test]
fn owner_cache_evicts_ids_outside_wanted_set() {
    let root = PathBuf::from("/owners");
    let mut fs = FakeFs::default();
    fs.inodes.insert(root.clone(), 1);
    let children: Vec<PathBuf> = (2..=80)
        .map(|id| {
            let child = root.join(id.to_string());
            fs.inodes.insert(child.clone(), id);
            child
        })
        .collect();
    fs.children.insert(root.clone(), children);
    let mut table = OwnerTable::new(fs, [root]);
    for id in 2..=80 {
        table.refresh(&[id]);
    }
    assert!(table.path(80).is_some());
    assert!(table.paths.len() <= 1);
}

#[test]
fn owner_frontier_is_preallocated_and_bounded() {
    let table = table();
    assert!(table.frontier.capacity() >= MAX_FRONTIER_PATHS);
}
