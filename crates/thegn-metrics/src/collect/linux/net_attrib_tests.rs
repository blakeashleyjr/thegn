use super::{CgroupFs, CookieDelta, MAX_STATS_PER_REFRESH, OwnerTable};
use crate::collect::linux::sock_diag::{AddressFamily, TcpInfo, TcpSock};
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
        source: IpAddr::V4(Ipv4Addr::LOCALHOST),
        destination: IpAddr::V4(Ipv4Addr::LOCALHOST),
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
            out.extend(children.iter().take(64).cloned());
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

#[test]
fn cookie_delta_baselines_first_dump_and_counts_second_dump_delta() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let first = map.update(&[socket(9, Some(42), 100, 50)], 500, 600, &mut owners);
    assert_eq!((first.owners[0].tx_bytes, first.owners[0].rx_bytes), (0, 0));
    let second = map.update(&[socket(9, Some(42), 145, 83)], 500, 600, &mut owners);
    assert_eq!(
        (second.owners[0].tx_bytes, second.owners[0].rx_bytes),
        (45, 33)
    );
    assert_eq!(
        second.owners[0].kind,
        crate::collect::policy::cgroup_classify::OwnerKind::Pane
    );
}

#[test]
fn new_cookie_counts_since_last_dump_and_closed_cookie_is_swept() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let _baseline = map.update(&[socket(1, Some(42), 10, 20)], 0, 0, &mut owners);
    let arrival = map.update(
        &[socket(1, Some(42), 10, 20), socket(2, Some(42), 30, 40)],
        100,
        100,
        &mut owners,
    );
    assert_eq!(
        (arrival.owners[0].tx_bytes, arrival.owners[0].rx_bytes),
        (30, 40)
    );
    let _closed = map.update(&[], 0, 0, &mut owners);
    let reappeared = map.update(&[socket(2, Some(42), 35, 48)], 100, 100, &mut owners);
    assert_eq!(
        (reappeared.owners[0].tx_bytes, reappeared.owners[0].rx_bytes),
        (35, 48)
    );
}

#[test]
fn cookie_table_capacity_is_stable_across_one_thousand_dumps_and_close_frees_it() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let _first = map.update(&[socket(1, Some(42), 0, 0)], 0, 0, &mut owners);
    let capacity = map.capacity();
    assert_eq!(capacity, 4_096);
    for tick in 1_u64..1_000 {
        let _result = map.update(&[socket(1, Some(42), tick, tick)], 0, 0, &mut owners);
        assert_eq!(map.capacity(), capacity);
    }
    map.close();
    assert_eq!(map.capacity(), 0);
}

#[test]
fn physical_remainder_saturates_at_zero() {
    let mut map = CookieDelta::new();
    let mut owners = table();
    let _baseline = map.update(&[socket(1, Some(42), 0, 0)], 0, 0, &mut owners);
    let result = map.update(&[socket(1, Some(42), 20, 10)], 1, 2, &mut owners);
    assert_eq!((result.unattributed_rx, result.unattributed_tx), (0, 0));
}

#[test]
fn owner_walk_performs_at_most_sixty_four_stats_per_refresh() {
    let root = PathBuf::from("/walk");
    let mut fs = FakeFs::default();
    let mut frontier = vec![root.clone()];
    for depth in 0..80 {
        let next = PathBuf::from(format!("/walk/{depth}"));
        fs.children.insert(frontier[0].clone(), vec![next.clone()]);
        frontier[0] = next;
    }
    let mut table = OwnerTable::new(fs, [root]);
    assert_eq!(table.refresh(&[u64::MAX]), MAX_STATS_PER_REFRESH);
    assert_eq!(table.fs.stats.get(), MAX_STATS_PER_REFRESH);
}
