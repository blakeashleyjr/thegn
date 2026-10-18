//! On-demand TCP byte attribution by socket cookie and cgroup v2 id.
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::net::{IpAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::sock_diag::{MAX_TCP_SOCKETS, TcpSock};
use crate::collect::{
    model::net_attrib::{NetAttribution, OwnerRates},
    policy::cgroup_classify,
};

const COOKIE_CAPACITY: usize = 4_096;
const OWNER_CAPACITY: usize = 64;
const MAX_STATS_PER_REFRESH: usize = 64;
const MAX_FRONTIER_PATHS: usize = 4_096;

#[derive(Debug, Clone, Copy, Default)]
struct CookieEntry {
    cookie: u64,
    tx: u64,
    rx: u64,
    generation: u64,
    occupied: bool,
    tombstone: bool,
}

/// Open-addressed per-cookie counters. Allocate with `new`, release with `close`.
#[derive(Default)]
pub struct CookieDelta {
    entries: Option<Box<[CookieEntry]>>,
    generation: u64,
    totals: Vec<(u64, u64, u64)>,
    ids: Vec<u64>,
}

impl CookieDelta {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn capacity(&self) -> usize {
        self.entries.as_ref().map_or(0, |entries| entries.len())
    }

    pub fn close(&mut self) {
        self.entries = None;
        self.generation = 0;
        self.totals.clear();
        self.ids.clear();
    }

    /// Reusable result form for a steady-state, allocation-free attribution pass.
    /// `physical_rx` and `physical_tx` are interface byte deltas over this dump
    /// pair's interval, not cumulative interface counters.
    pub fn update_into<F: CgroupFs>(
        &mut self,
        sockets: &[TcpSock],
        physical_rx: u64,
        physical_tx: u64,
        owners: &mut OwnerTable<F>,
        out: &mut NetAttribution,
    ) {
        let entries = self.entries.get_or_insert_with(|| {
            vec![CookieEntry::default(); COOKIE_CAPACITY].into_boxed_slice()
        });
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        self.totals.clear();
        let mut truncated = sockets.len() > MAX_TCP_SOCKETS;
        let mut attributed_rx = 0_u64;
        let mut attributed_tx = 0_u64;

        for socket in sockets.iter().take(MAX_TCP_SOCKETS) {
            let (Some(cookie), Some(info)) = (socket.cookie, socket.info.as_ref()) else {
                continue;
            };
            let (Some(tx), Some(rx)) = (info.bytes_acked, info.bytes_received) else {
                continue;
            };
            let index = hash_cookie(cookie) % entries.len();
            let Some(slot) = find_slot(entries, index, cookie) else {
                truncated = true;
                continue;
            };
            let entry = &mut entries[slot];
            let (rx_delta, tx_delta) = if entry.occupied && entry.cookie == cookie {
                (rx.saturating_sub(entry.rx), tx.saturating_sub(entry.tx))
            } else {
                (0, 0)
            };
            entry.cookie = cookie;
            entry.rx = rx;
            entry.tx = tx;
            entry.generation = generation;
            entry.occupied = true;
            entry.tombstone = false;

            if let Some(cgroup_id) = socket.cgroup_id {
                let tracked = match self.totals.iter().position(|owner| owner.0 == cgroup_id) {
                    Some(index) => {
                        let owner = &mut self.totals[index];
                        owner.1 = owner.1.saturating_add(rx_delta);
                        owner.2 = owner.2.saturating_add(tx_delta);
                        true
                    }
                    None if self.totals.len() < OWNER_CAPACITY => {
                        self.totals.push((cgroup_id, rx_delta, tx_delta));
                        true
                    }
                    None => {
                        truncated = true;
                        false
                    }
                };
                if tracked && !is_loopback(socket.source) && !is_loopback(socket.destination) {
                    attributed_rx = attributed_rx.saturating_add(rx_delta);
                    attributed_tx = attributed_tx.saturating_add(tx_delta);
                }
            }
        }

        for entry in entries.iter_mut() {
            if entry.occupied && entry.generation != generation {
                entry.occupied = false;
                entry.tombstone = true;
            }
        }

        self.ids.clear();
        self.ids.extend(self.totals.iter().map(|owner| owner.0));
        owners.refresh(&self.ids);
        out.owners.clear();
        for &(cgroup_id, rx_bytes, tx_bytes) in &self.totals {
            let (path, kind, name) = owners.label(cgroup_id).unwrap_or_else(empty_label);
            out.owners.push(OwnerRates {
                cgroup_id,
                path,
                kind,
                name,
                rx_bytes,
                tx_bytes,
            });
        }
        out.unattributed_rx = physical_rx.saturating_sub(attributed_rx);
        out.unattributed_tx = physical_tx.saturating_sub(attributed_tx);
        out.truncated = truncated;
    }
}

fn empty_label() -> (Arc<[u8]>, cgroup_classify::OwnerKind, Arc<[u8]>) {
    (
        Arc::from([]),
        cgroup_classify::OwnerKind::Other,
        Arc::from([]),
    )
}

fn is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => {
            address.is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
}

fn hash_cookie(cookie: u64) -> usize {
    let mixed = cookie ^ cookie.rotate_left(23) ^ cookie.rotate_right(17);
    mixed as usize
}

fn find_slot(entries: &[CookieEntry], start: usize, cookie: u64) -> Option<usize> {
    let mut tombstone = None;
    for offset in 0..entries.len() {
        let index = (start + offset) % entries.len();
        let entry = &entries[index];
        if entry.occupied && entry.cookie == cookie {
            return Some(index);
        }
        if entry.tombstone && tombstone.is_none() {
            tombstone = Some(index);
        }
        if !entry.occupied && !entry.tombstone {
            return Some(tombstone.unwrap_or(index));
        }
    }
    tombstone
}

/// Small filesystem seam used by the bounded cgroup-id walk and its tests.
pub trait CgroupFs {
    fn children(&self, path: &Path, out: &mut Vec<PathBuf>) -> io::Result<()>;
    fn inode(&self, path: &Path) -> io::Result<u64>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealCgroupFs;

impl CgroupFs for RealCgroupFs {
    fn children(&self, path: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for item in fs::read_dir(path)? {
            let item = item?;
            if item.file_type()?.is_dir() && out.len() < MAX_FRONTIER_PATHS {
                out.push(item.path());
            }
        }
        Ok(())
    }

    fn inode(&self, path: &Path) -> io::Result<u64> {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(path)?.ino())
    }
}

struct OwnerLabel {
    id: u64,
    path: PathBuf,
    path_bytes: Arc<[u8]>,
    kind: cgroup_classify::OwnerKind,
    name: Arc<[u8]>,
}

/// Cached cgroup-id to path resolution, with at most 64 inode stats per refresh.
pub struct OwnerTable<F = RealCgroupFs> {
    fs: F,
    roots: Vec<PathBuf>,
    frontier: VecDeque<PathBuf>,
    children: Vec<PathBuf>,
    paths: Vec<OwnerLabel>,
}

impl OwnerTable<RealCgroupFs> {
    pub fn system() -> Self {
        Self::new(
            RealCgroupFs,
            [
                PathBuf::from("/sys/fs/cgroup/user.slice"),
                PathBuf::from("/sys/fs/cgroup/system.slice"),
            ],
        )
    }
}

impl<F: CgroupFs> OwnerTable<F> {
    pub fn new(fs: F, roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let roots: Vec<PathBuf> = roots.into_iter().collect();
        let mut frontier = VecDeque::with_capacity(MAX_FRONTIER_PATHS);
        frontier.extend(roots.iter().take(MAX_FRONTIER_PATHS).cloned());
        Self {
            fs,
            roots,
            frontier,
            children: Vec::with_capacity(MAX_FRONTIER_PATHS),
            paths: Vec::with_capacity(OWNER_CAPACITY),
        }
    }

    pub fn path(&self, id: u64) -> Option<&[u8]> {
        use std::os::unix::ffi::OsStrExt;
        self.paths
            .iter()
            .find(|label| label.id == id)
            .map(|label| label.path.as_os_str().as_bytes())
    }

    fn label(&self, id: u64) -> Option<(Arc<[u8]>, cgroup_classify::OwnerKind, Arc<[u8]>)> {
        self.paths
            .iter()
            .find(|label| label.id == id)
            .map(|label| (label.path_bytes.clone(), label.kind, label.name.clone()))
    }

    pub fn refresh(&mut self, wanted: &[u64]) -> usize {
        self.paths.retain(|label| wanted.contains(&label.id));
        if wanted
            .iter()
            .all(|id| self.paths.iter().any(|label| label.id == *id))
        {
            return 0;
        }
        if self.frontier.is_empty() {
            self.frontier
                .extend(self.roots.iter().take(MAX_FRONTIER_PATHS).cloned());
        }
        let mut stats = 0;
        while stats < MAX_STATS_PER_REFRESH
            && !wanted
                .iter()
                .all(|id| self.paths.iter().any(|label| label.id == *id))
        {
            let Some(path) = self.frontier.pop_front() else {
                break;
            };
            stats += 1;
            if let Ok(inode) = self.fs.inode(&path)
                && wanted.contains(&inode)
                && !self.paths.iter().any(|label| label.id == inode)
                && self.paths.len() < OWNER_CAPACITY
            {
                use std::os::unix::ffi::OsStrExt;
                let path_bytes: Arc<[u8]> = Arc::from(path.as_os_str().as_bytes());
                let classified = cgroup_classify::classify(&path_bytes);
                let kind = classified.kind;
                let name: Arc<str> = Arc::from(classified.name);
                self.paths.push(OwnerLabel {
                    id: inode,
                    path: path.clone(),
                    path_bytes,
                    kind,
                    name,
                });
            }
            self.children.clear();
            if self.frontier.len() < MAX_FRONTIER_PATHS
                && self.fs.children(&path, &mut self.children).is_ok()
            {
                let remaining = MAX_FRONTIER_PATHS - self.frontier.len();
                self.frontier
                    .extend(self.children.drain(..).take(remaining));
            }
        }
        stats
    }
}

impl<F> Default for OwnerTable<F>
where
    F: Default + CgroupFs,
{
    fn default() -> Self {
        Self::new(F::default(), std::iter::empty())
    }
}

#[cfg(test)]
#[path = "net_attrib_tests.rs"]
mod tests;
