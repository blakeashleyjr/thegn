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
