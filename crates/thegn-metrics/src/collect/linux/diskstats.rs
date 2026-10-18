//! Held-fd Linux `/proc/diskstats` collector with a bounded device topology.
use super::{
    disk_topology::{self, Topology},
    procfile::ProcFile,
};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    delta::Counter,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    instances::{Family, InstanceId, Interned, Interner},
    model::diskstats::{DiskDevice, DiskTotals},
    parse::diskstats,
    schedule::Cadence,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Default)]
struct Rates {
    seen: bool,
    sectors_read: Counter,
    sectors_written: Counter,
    reads: Counter,
    writes: Counter,
    io_ticks: Counter,
}
#[derive(Debug, Default)]
struct AggregateRates {
    sectors_read: Counter,
    sectors_written: Counter,
    reads: Counter,
    writes: Counter,
}
#[derive(Debug)]
pub struct DiskstatsCollector {
    path: PathBuf,
    sys_block: PathBuf,
    file: Option<ProcFile>,
    topology: Topology,
    topology_indices: HashMap<String, usize>,
    interner: Interner,
    over_cap: HashSet<String>,
    ids: HashMap<String, InstanceId>,
    rates: HashMap<String, Rates>,
    aggregate_rates: AggregateRates,
    last_scan: u64,
    topology_rev: u32,
    last_unknown_scan: u64,
    #[cfg(test)]
    intern_attempts: usize,
}
impl DiskstatsCollector {
    pub fn at(diskstats: impl AsRef<Path>, sys_block: impl AsRef<Path>) -> Self {
        Self {
            path: diskstats.as_ref().to_owned(),
            sys_block: sys_block.as_ref().to_owned(),
            file: None,
            topology: Topology::default(),
            topology_indices: HashMap::new(),
            interner: Interner::new(Family::BlockDev),
            over_cap: HashSet::new(),
            ids: HashMap::new(),
            rates: HashMap::new(),
            aggregate_rates: AggregateRates::default(),
            last_scan: 0,
            topology_rev: 0,
            last_unknown_scan: 0,
            #[cfg(test)]
            intern_attempts: 0,
        }
    }
    fn scan(&mut self, now_ms: u64) {
        let next = disk_topology::scan(&self.sys_block, self.topology_rev.wrapping_add(1));
        if next.devs != self.topology.devs {
            self.topology_rev = self.topology_rev.wrapping_add(1);
            self.topology = Topology {
                rev: self.topology_rev,
                ..next
            };
            self.topology_indices = self
                .topology
                .devs
                .iter()
                .enumerate()
                .map(|(index, dev)| (dev.name.to_string(), index))
                .collect();
            self.over_cap.clear();
            self.aggregate_rates = AggregateRates::default();
            let active: HashSet<_> = self
                .topology
                .devs
                .iter()
                .map(|d| d.name.to_string())
                .collect();
            let vanished: Vec<_> = self
                .ids
                .keys()
                .filter(|name| !active.contains(*name))
                .cloned()
                .collect();
            for name in vanished {
                if let Some(id) = self.ids.remove(&name) {
                    self.interner.retire(id);
                }
                self.rates.remove(&name);
            }
        }
        for dev in &self.topology.devs {
            if !self.ids.contains_key(dev.name.as_ref())
                && !self.over_cap.contains(dev.name.as_ref())
            {
                #[cfg(test)]
                {
                    self.intern_attempts += 1;
                }
                match self.interner.intern(&dev.name) {
                    Interned::Id(id) => {
                        self.ids.insert(dev.name.to_string(), id);
                        self.rates.insert(dev.name.to_string(), Rates::default());
                    }
                    Interned::Other => {
                        self.over_cap.insert(dev.name.to_string());
                    }
                }
            }
        }
        self.last_scan = now_ms;
    }
}
impl Collector for DiskstatsCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Diskstats
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        match ProcFile::open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                self.topology = Topology::default();
                self.scan(0);
                Health::Warming
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Health::Absent(Absent::NotOnThisOs)
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Health::Absent(Absent::NoPermission)
            }
            Err(_) => Health::Warming,
        }
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let read = {
            let file = self.file.as_mut().ok_or(CollectError::Gone)?;
            match file.read_all() {
                Ok(bytes) => Ok(bytes),
                Err(error) => Err((error, file.needs_reopen())),
            }
        };
        let bytes = match read {
            Ok(bytes) => bytes,
            Err((_error, true)) => {
                self.file = None;
                return Err(CollectError::Gone);
            }
            Err((error, false)) => return Err(error.into()),
        };
        let mut rows = diskstats::lines(bytes);
        let first = rows.next();
        let empty = first.is_none();
        let unknown = first.into_iter().chain(rows).any(|row| {
            let Ok(name) = std::str::from_utf8(row.name) else {
                return false;
            };
            !self.topology_indices.contains_key(name)
        });
        if empty {
            return Err(CollectError::Parse("/proc/diskstats"));
        }
        let should_rescan = ctx.now_ms.saturating_sub(self.last_scan) >= 60_000
            || (unknown
                && (self.last_unknown_scan == 0
                    || ctx.now_ms.saturating_sub(self.last_unknown_scan) >= 5_000));
        let part = &mut frame.parts.diskstats;
        part.devices.clear();
        part.topology_rev = self.topology_rev;
        part.physical = 0;
        part.truncated = false;
        let mut read_total = 0_u64;
        let mut write_total = 0_u64;
        let mut reads_total = 0_u64;
        let mut writes_total = 0_u64;
        let mut sectors_read_total = 0_u64;
        let mut sectors_written_total = 0_u64;
        for rates in self.rates.values_mut() {
            rates.seen = false;
        }
        for row in diskstats::lines(bytes) {
            let Ok(name) = std::str::from_utf8(row.name) else {
                continue;
            };
            let Some(dev_index) = self.topology_indices.get(name).copied() else {
                continue;
            };
            let dev = &self.topology.devs[dev_index];
            if self.over_cap.contains(name) {
                part.truncated = true;
                continue;
            }
            let id = if let Some(id) = self.ids.get(name).copied() {
                id
            } else {
                #[cfg(test)]
                {
                    self.intern_attempts += 1;
                }
                match self.interner.intern(name) {
                    Interned::Id(id) => {
                        self.ids.insert(name.to_owned(), id);
                        self.rates.insert(name.to_owned(), Rates::default());
                        id
                    }
                    Interned::Other => {
                        self.over_cap.insert(name.to_owned());
                        part.truncated = true;
                        continue;
                    }
                }
            };
            let Some(rates) = self.rates.get_mut(name) else {
                continue;
            };
            rates.seen = true;
            let r_bps = rates
                .sectors_read
                .rate(row.sectors_read, ctx.now_ms)
                .map(|v| v * 512.0);
            let w_bps = rates
                .sectors_written
                .rate(row.sectors_written, ctx.now_ms)
                .map(|v| v * 512.0);
            let r_iops = rates.reads.rate(row.reads, ctx.now_ms);
            let w_iops = rates.writes.rate(row.writes, ctx.now_ms);
            let util = rates
                .io_ticks
                .rate(row.io_ticks, ctx.now_ms)
                .map(|v| (v / 10.0).min(100.0));
            part.devices.push(DiskDevice {
                id,
                name: Arc::clone(&dev.name),
                label: dev.label.clone(),
                kind: dev.kind,
                physical: dev.physical,
                parent: dev
                    .parent
                    .and_then(|idx| self.topology.devs.get(idx))
                    .and_then(|p| self.ids.get(p.name.as_ref()).copied()),
                read_bps: r_bps,
                write_bps: w_bps,
                read_iops: r_iops,
                write_iops: w_iops,
                util_pct: util,
                in_flight: row.in_flight,
            });
            if dev.physical {
                part.physical = part.physical.saturating_add(1);
                read_total = read_total.saturating_add(row.sectors_read);
                write_total = write_total.saturating_add(row.sectors_written);
                reads_total = reads_total.saturating_add(row.reads);
                writes_total = writes_total.saturating_add(row.writes);
                sectors_read_total = sectors_read_total.saturating_add(row.sectors_read);
                sectors_written_total = sectors_written_total.saturating_add(row.sectors_written);
            }
        }
        loop {
            let vanished = self.rates.iter().find_map(|(name, rates)| {
                if rates.seen {
                    None
                } else {
                    self.ids.get(name).map(|id| (name.clone(), *id))
                }
            });
            let Some((name, id)) = vanished else { break };
            self.ids.remove(&name);
            self.rates.remove(&name);
            self.interner.retire(id);
            self.over_cap.clear();
        }
        part.aggregate = DiskTotals {
            read_bps: self
                .aggregate_rates
                .sectors_read
                .rate(read_total, ctx.now_ms)
                .map(|v| v * 512.0),
            write_bps: self
                .aggregate_rates
                .sectors_written
                .rate(write_total, ctx.now_ms)
                .map(|v| v * 512.0),
            read_iops: self.aggregate_rates.reads.rate(reads_total, ctx.now_ms),
            write_iops: self.aggregate_rates.writes.rate(writes_total, ctx.now_ms),
            sectors_read_total,
            sectors_written_total,
        };
        if should_rescan {
            let previous_rev = self.topology_rev;
            self.scan(ctx.now_ms);
            if unknown {
                self.last_unknown_scan = ctx.now_ms.max(1);
            }
            if self.topology_rev != previous_rev {
                part.topology_rev = self.topology_rev;
                part.devices
                    .retain(|device| self.ids.contains_key(device.name.as_ref()));
                part.aggregate = DiskTotals::default();
            }
        }
        part.truncated |= self.interner.truncated();
        frame.sampled.insert(CollectorId::Diskstats);
        Ok(())
    }
    fn close(&mut self) {
        self.file = None;
        self.rates.clear();
        self.ids.clear();
        self.topology = Topology::default();
        self.topology_indices.clear();
        self.over_cap.clear();
        self.interner = Interner::new(Family::BlockDev);
        self.aggregate_rates = AggregateRates::default();
        self.last_scan = 0;
        self.last_unknown_scan = 0;
        #[cfg(test)]
        {
            self.intern_attempts = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DiskstatsCollector;
    use crate::collect::{
        Collector, TickCtx,
        env::Env,
        frame::MonitorFrame,
        health::{Absent, Health},
    };
    use std::fs;
    #[test]
    fn missing_file_is_absent_and_bad_rows_are_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut missing = DiskstatsCollector::at(dir.path().join("missing"), dir.path());
        assert_eq!(
            missing.discover(&Env::default()),
            Health::Absent(Absent::NotOnThisOs)
        );
        let path = dir.path().join("diskstats");
        fs::write(&path, b"garbage\n").unwrap();
        let mut collector = DiskstatsCollector::at(&path, dir.path());
        collector.discover(&Env::default());
        assert!(matches!(
            collector.sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000
                },
                &mut MonitorFrame::default()
            ),
            Err(crate::collect::CollectError::Parse(_))
        ));
    }

    #[test]
    fn rates_count_physical_devices_once_and_reuse_the_row_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("sys/block");
        fs::create_dir_all(sys.join("sda/device")).unwrap();
        fs::write(sys.join("sda/dev"), "8:0\n").unwrap();
        fs::create_dir_all(sys.join("nvme0n1/device")).unwrap();
        fs::write(sys.join("nvme0n1/dev"), "259:0\n").unwrap();
        fs::create_dir_all(sys.join("nvme0n1/nvme0n1p1")).unwrap();
        fs::write(sys.join("nvme0n1/nvme0n1p1/partition"), "1\n").unwrap();
        fs::write(sys.join("nvme0n1/nvme0n1p1/dev"), "259:1\n").unwrap();
        fs::create_dir_all(sys.join("loop0")).unwrap();
        fs::write(sys.join("loop0/dev"), "7:0\n").unwrap();
        for (name, major, minor) in [("dm-0", 254, 0), ("zram0", 253, 0)] {
            fs::create_dir_all(sys.join(name)).unwrap();
            fs::write(sys.join(name).join("dev"), format!("{major}:{minor}\n")).unwrap();
        }
        let proc = dir.path().join("diskstats");
        let row = |name: &str, major: u32, minor: u32, sectors: u64, io_ms: u64| {
            format!(
                "{major} {minor} {name} 1 0 {sectors} 1 1 0 {} 1 0 {io_ms} 10 0 0 0 0 0 0\n",
                sectors.saturating_mul(2)
            )
        };
        fs::write(
            &proc,
            format!(
                "{}{}{}{}{}{}",
                row("sda", 8, 0, 10, 10),
                row("loop0", 7, 0, 100, 10),
                row("nvme0n1", 259, 0, 10, 10),
                row("nvme0n1p1", 259, 1, 30_000, 10),
                row("dm-0", 254, 0, 40_000, 10),
                row("zram0", 253, 0, 50_000, 10)
            ),
        )
        .unwrap();
        let mut collector = DiskstatsCollector::at(&proc, &sys);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        let capacity = frame.parts.diskstats.devices.capacity();
        let pointer = frame.parts.diskstats.devices.as_ptr();
        fs::write(
            &proc,
            format!(
                "{}{}{}{}{}{}",
                row("sda", 8, 0, 12, 510),
                row("loop0", 7, 0, 900, 2_010),
                row("nvme0n1", 259, 0, 12, 2_010),
                row("nvme0n1p1", 259, 1, 30_000, 2_010),
                row("dm-0", 254, 0, 40_000, 2_010),
                row("zram0", 253, 0, 50_000, 2_010)
            ),
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(frame.parts.diskstats.devices.capacity(), capacity);
        assert_eq!(frame.parts.diskstats.devices.as_ptr(), pointer);
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sda")
                .unwrap()
                .read_bps,
            Some(1024.0)
        );
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "nvme0n1")
                .unwrap()
                .read_bps,
            Some(1024.0)
        );
        assert_eq!(frame.parts.diskstats.aggregate.read_bps, Some(2048.0));
        assert_eq!(frame.parts.diskstats.physical, 2);
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sda")
                .unwrap()
                .util_pct,
            Some(50.0)
        );
        assert_eq!(frame.parts.diskstats.aggregate.sectors_read_total, 24);
        fs::write(
            &proc,
            format!(
                "{}{}{}{}{}{}",
                row("sda", 8, 0, 5, 2_010),
                row("loop0", 7, 0, 900, 2_010),
                row("nvme0n1", 259, 0, 12, 2_010),
                row("nvme0n1p1", 259, 1, 30_000, 2_010),
                row("dm-0", 254, 0, 40_000, 2_010),
                row("zram0", 253, 0, 50_000, 2_010)
            ),
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 3,
                    now_ms: 3_000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sda")
                .unwrap()
                .read_bps,
            None
        );
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sda")
                .unwrap()
                .util_pct,
            Some(100.0)
        );
        fs::write(
            &proc,
            format!(
                "{}{}{}{}{}{}",
                row("sda", 8, 0, 7, 2_010),
                row("loop0", 7, 0, 900, 2_010),
                row("nvme0n1", 259, 0, 12, 2_010),
                row("nvme0n1p1", 259, 1, 30_000, 2_010),
                row("dm-0", 254, 0, 40_000, 2_010),
                row("zram0", 253, 0, 50_000, 2_010)
            ),
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 4,
                    now_ms: 4_000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(frame.parts.diskstats.devices[0].read_bps, Some(1024.0));
    }

    #[test]
    fn unknown_device_rescans_and_vanished_device_reappears_primed() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("sys/block");
        fs::create_dir_all(sys.join("sda/device")).unwrap();
        fs::write(sys.join("sda/dev"), "8:0\n").unwrap();
        let proc = dir.path().join("diskstats");
        let row = |name: &str, minor: u32, sectors: u64| {
            format!("8 {minor} {name} 1 0 {sectors} 1 1 0 {sectors} 1 0 10 10 0 0 0 0 0 0\n")
        };
        fs::write(&proc, row("sda", 0, 10)).unwrap();
        let mut collector = DiskstatsCollector::at(&proc, &sys);
        collector.discover(&Env::default());
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        let first_rev = frame.parts.diskstats.topology_rev;

        fs::create_dir_all(sys.join("sdb/device")).unwrap();
        fs::write(sys.join("sdb/dev"), "8:16\n").unwrap();
        fs::write(
            &proc,
            format!("{}{}", row("sda", 0, 11), row("sdb", 16, 20)),
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        assert!(frame.parts.diskstats.topology_rev > first_rev);

        collector
            .sample(
                &TickCtx {
                    tick: 3,
                    now_ms: 3000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sdb")
                .unwrap()
                .read_bps,
            None
        );
        fs::write(&proc, row("sda", 0, 13)).unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 4,
                    now_ms: 4000,
                },
                &mut frame,
            )
            .unwrap();
        fs::write(
            &proc,
            format!("{}{}", row("sda", 0, 14), row("sdb", 16, 25)),
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 5,
                    now_ms: 5000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sdb")
                .unwrap()
                .read_bps,
            None
        );
        collector
            .sample(
                &TickCtx {
                    tick: 6,
                    now_ms: 6000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .find(|dev| dev.name.as_ref() == "sdb")
                .unwrap()
                .read_bps,
            Some(0.0)
        );
    }

    #[test]
    fn periodic_rescan_runs_after_sixty_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("sys/block");
        fs::create_dir_all(sys.join("sda/device")).unwrap();
        fs::write(sys.join("sda/dev"), "8:0\n").unwrap();
        let proc = dir.path().join("diskstats");
        fs::write(&proc, "8 0 sda 1 0 1 1 1 0 1 1 0 10 10 0 0 0 0 0 0\n").unwrap();
        let mut collector = DiskstatsCollector::at(&proc, &sys);
        collector.discover(&Env::default());
        let first_rev = collector.topology_rev;
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1_000,
                },
                &mut frame,
            )
            .unwrap();
        fs::write(sys.join("sda/dev"), "8:1\n").unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 60_001,
                },
                &mut frame,
            )
            .unwrap();
        assert!(frame.parts.diskstats.topology_rev > first_rev);
        let sampled_rev = frame.parts.diskstats.topology_rev;
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        assert!(collector.topology_rev > sampled_rev);
    }

    #[test]
    fn over_cap_names_are_cached_until_an_id_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("sys/block");
        let proc = dir.path().join("diskstats");
        let mut rows = String::new();
        for index in 0..70_u32 {
            let name = format!("sd{index:02}");
            let device = sys.join(&name);
            fs::create_dir_all(device.join("device")).unwrap();
            fs::write(device.join("dev"), format!("8:{index}\n")).unwrap();
            rows.push_str(&format!(
                "8 {index} {name} 1 0 10 1 1 0 20 1 0 10 10 0 0 0 0 0 0\n"
            ));
        }
        fs::write(&proc, &rows).unwrap();
        let mut collector = DiskstatsCollector::at(&proc, &sys);
        collector.discover(&Env::default());
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1_000,
                },
                &mut frame,
            )
            .unwrap();
        assert!(frame.parts.diskstats.truncated);
        let intern_attempts = collector.intern_attempts;

        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2_000,
                },
                &mut frame,
            )
            .unwrap();

        assert_eq!(collector.intern_attempts, intern_attempts);
        assert!(frame.parts.diskstats.truncated);

        let without_first_device = rows
            .lines()
            .filter(|row| !row.contains(" sd00 "))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&proc, without_first_device).unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 3,
                    now_ms: 3_000,
                },
                &mut frame,
            )
            .unwrap();
        assert!(!collector.ids.contains_key("sd00"));

        fs::write(&proc, &rows).unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 4,
                    now_ms: 4_000,
                },
                &mut frame,
            )
            .unwrap();

        assert!(collector.intern_attempts > intern_attempts);
        assert!(frame.parts.diskstats.truncated);
    }

    #[test]
    fn genuinely_new_diskstats_name_still_triggers_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("sys/block");
        fs::create_dir_all(sys.join("sda/device")).unwrap();
        fs::write(sys.join("sda/dev"), "8:0\n").unwrap();
        let proc = dir.path().join("diskstats");
        fs::write(&proc, "8 0 sda 1 0 1 1 1 0 1 1 0 10 10 0 0 0 0 0 0\n").unwrap();
        let mut collector = DiskstatsCollector::at(&proc, &sys);
        collector.discover(&Env::default());
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1_000,
                },
                &mut frame,
            )
            .unwrap();
        let original_rev = collector.topology_rev;

        fs::create_dir_all(sys.join("sdb/device")).unwrap();
        fs::write(sys.join("sdb/dev"), "8:16\n").unwrap();
        fs::write(
            &proc,
            "8 0 sda 1 0 1 1 1 0 1 1 0 10 10 0 0 0 0 0 0\n8 16 sdb 1 0 1 1 1 0 1 1 0 10 10 0 0 0 0 0 0\n",
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2_000,
                },
                &mut frame,
            )
            .unwrap();

        assert!(collector.topology_rev > original_rev);
    }
}
