//! On-demand TCP byte attribution by socket cookie and cgroup v2 id.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::sock_diag::{MAX_TCP_SOCKETS, TcpSock};
use crate::collect::{
    model::net_attrib::{NetAttribution, OwnerRates},
    policy::cgroup_classify,
};

const COOKIE_CAPACITY: usize = 4_096;
const OWNER_CAPACITY: usize = 64;
const MAX_STATS_PER_REFRESH: usize = 64;

#[derive(Debug, Clone, Copy, Default)]
struct CookieEntry {
    cookie: u64,
    tx: u64,
    rx: u64,
    generation: u64,
    occupied: bool,
}

/// Open-addressed per-cookie counters. Allocate with `new`, release with `close`.
#[derive(Default)]
pub struct CookieDelta {
    entries: Option<Box<[CookieEntry]>>,
    generation: u64,
    has_dump: bool,
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
        self.has_dump = false;
    }

    pub fn update<F: CgroupFs>(
        &mut self,
        sockets: &[TcpSock],
        physical_rx: u64,
        physical_tx: u64,
        owners: &mut OwnerTable<F>,
    ) -> NetAttribution {
        let entries = self.entries.get_or_insert_with(|| {
            vec![CookieEntry::default(); COOKIE_CAPACITY].into_boxed_slice()
        });
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        let mut totals = Vec::<(u64, u64, u64)>::with_capacity(OWNER_CAPACITY);
        let mut truncated = sockets.len() > MAX_TCP_SOCKETS;

        for socket in sockets.iter().take(MAX_TCP_SOCKETS) {
            let (Some(cookie), Some(info)) = (socket.cookie, socket.info.as_ref()) else {
                continue;
            };
            let (Some(tx), Some(rx)) = (info.bytes_acked, info.bytes_received) else {
                continue;
            };
            let index = hash_cookie(cookie) % entries.len();
            let Some(slot) = find_slot(entries, index, cookie, generation) else {
                truncated = true;
                continue;
            };
            let entry = &mut entries[slot];
            let (rx_delta, tx_delta) = if entry.occupied && entry.cookie == cookie {
                (rx.saturating_sub(entry.rx), tx.saturating_sub(entry.tx))
            } else if self.has_dump {
                (rx, tx)
            } else {
                (0, 0)
            };
            entry.cookie = cookie;
            entry.rx = rx;
            entry.tx = tx;
            entry.generation = generation;
            entry.occupied = true;

            if let Some(cgroup_id) = socket.cgroup_id {
                match totals.iter_mut().find(|owner| owner.0 == cgroup_id) {
                    Some(owner) => {
                        owner.1 = owner.1.saturating_add(rx_delta);
                        owner.2 = owner.2.saturating_add(tx_delta);
                    }
                    None if totals.len() < OWNER_CAPACITY => {
                        totals.push((cgroup_id, rx_delta, tx_delta));
                    }
                    None => truncated = true,
                }
            }
        }

        for entry in entries.iter_mut() {
            if entry.occupied && entry.generation != generation {
                *entry = CookieEntry::default();
            }
        }
        self.has_dump = true;

        let ids: Vec<u64> = totals.iter().map(|owner| owner.0).collect();
        owners.refresh(&ids);
        let mut rows = Vec::with_capacity(totals.len());
        for (cgroup_id, rx_bytes, tx_bytes) in totals {
            let path = owners.path(cgroup_id).map_or_else(Vec::new, <[u8]>::to_vec);
            let classified = cgroup_classify::classify(&path);
            rows.push(OwnerRates {
                cgroup_id,
                path,
                kind: classified.kind,
                name: classified.name.to_vec(),
                rx_bytes,
                tx_bytes,
            });
        }
        NetAttribution::from_attributed(rows, physical_rx, physical_tx, truncated)
    }
}

fn hash_cookie(cookie: u64) -> usize {
    let mixed = cookie ^ cookie.rotate_left(23) ^ cookie.rotate_right(17);
    mixed as usize
}

fn find_slot(entries: &[CookieEntry], start: usize, cookie: u64, generation: u64) -> Option<usize> {
    let mut reusable = None;
    for offset in 0..entries.len() {
        let index = (start + offset) % entries.len();
        let entry = &entries[index];
        if entry.occupied && entry.cookie == cookie {
            return Some(index);
        }
        if (!entry.occupied || entry.generation != generation) && reusable.is_none() {
            reusable = Some(index);
        }
        if !entry.occupied {
            return reusable;
        }
    }
    reusable
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
            if item.file_type()?.is_dir() && out.len() < MAX_STATS_PER_REFRESH {
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

/// Cached cgroup-id to path resolution, with at most 64 inode stats per refresh.
pub struct OwnerTable<F = RealCgroupFs> {
    fs: F,
    roots: Vec<PathBuf>,
    paths: Vec<(u64, PathBuf)>,
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
        Self {
            fs,
            roots: roots.into_iter().collect(),
            paths: Vec::with_capacity(OWNER_CAPACITY),
        }
    }

    pub fn path(&self, id: u64) -> Option<&[u8]> {
        use std::os::unix::ffi::OsStrExt;
        self.paths
            .iter()
            .find(|(cached_id, _)| *cached_id == id)
            .map(|(_, path)| path.as_os_str().as_bytes())
    }

    pub fn refresh(&mut self, wanted: &[u64]) -> usize {
        let missing: Vec<u64> = wanted
            .iter()
            .copied()
            .filter(|id| !self.paths.iter().any(|(cached, _)| cached == id))
            .collect();
        if missing.is_empty() {
            return 0;
        }
        let mut queue = Vec::with_capacity(MAX_STATS_PER_REFRESH);
        queue.extend(self.roots.iter().take(MAX_STATS_PER_REFRESH).cloned());
        let mut stats = 0;
        let mut cursor = 0;
        while cursor < queue.len() && stats < MAX_STATS_PER_REFRESH {
            let path = queue[cursor].clone();
            cursor += 1;
            stats += 1;
            if let Ok(inode) = self.fs.inode(&path)
                && missing.contains(&inode)
                && self.paths.len() < OWNER_CAPACITY
            {
                self.paths.push((inode, path.clone()));
            }
            if queue.len() < MAX_STATS_PER_REFRESH {
                let mut children = Vec::with_capacity(MAX_STATS_PER_REFRESH - queue.len());
                if self.fs.children(&path, &mut children).is_ok() {
                    queue.extend(
                        children
                            .into_iter()
                            .take(MAX_STATS_PER_REFRESH - queue.len()),
                    );
                }
            }
        }
        stats
    }
}

impl<F> Default for OwnerTable<F>
where
    F: Default,
{
    fn default() -> Self {
        Self::new(F::default(), std::iter::empty())
    }
}

#[cfg(test)]
#[path = "net_attrib_tests.rs"]
mod tests;
