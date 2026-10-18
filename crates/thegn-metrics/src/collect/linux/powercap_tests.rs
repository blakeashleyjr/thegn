use super::{PowercapCollector, PowercapFs, RealFs};
use crate::collect::{
    CollectError, Collector, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::powercap::PowerDomain,
};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct AccessDenyFs {
    inner: RealFs,
    opens: usize,
}

impl PowercapFs for AccessDenyFs {
    type Handle = File;

    fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(dir)
    }

    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read_small(path, buffer)
    }

    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        self.opens += 1;
        self.inner.open(path)
    }

    fn access_read(&mut self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture permission",
        ))
    }

    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        handle.read_at(buffer, 0)
    }
}

fn zone(root: &Path, dirname: &str, name: &str, max: &str, energy: &str) -> PathBuf {
    let path = root.join(dirname);
    fs::create_dir(&path).unwrap();
    fs::write(path.join("name"), format!("{name}\n")).unwrap();
    fs::write(path.join("max_energy_range_uj"), format!("{max}\n")).unwrap();
    fs::write(path.join("energy_uj"), format!("{energy}\n")).unwrap();
    path
}

fn sample(
    collector: &mut impl Collector,
    frame: &mut MonitorFrame,
    now_ms: u64,
) -> Result<(), CollectError> {
    collector.sample(
        &TickCtx {
            tick: now_ms,
            now_ms,
        },
        frame,
    )
}

#[test]
fn discovers_two_zones_primes_then_reports_wrap_aware_watts() {
    let dir = tempfile::tempdir().unwrap();
    let package = zone(dir.path(), "intel-rapl:0", "package-0", "1000", "990");
    let core = zone(dir.path(), "intel-rapl:0:0", "core", "1000", "100");
    std::os::unix::fs::symlink(&core, package.join("intel-rapl:0:0")).unwrap();
    let mut collector = PowercapCollector::at(dir.path());
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    sample(&mut collector, &mut frame, 1_000).unwrap();
    assert_eq!(frame.parts.powercap.zones.len(), 2);
    assert!(
        frame
            .parts
            .powercap
            .zones
            .iter()
            .all(|zone| zone.watts.is_none())
    );
    assert_eq!(frame.parts.powercap.zones[0].domain, PowerDomain::Package);

    fs::write(package.join("energy_uj"), "10\n").unwrap();
    fs::write(core.join("energy_uj"), "500\n").unwrap();
    sample(&mut collector, &mut frame, 2_000).unwrap();
    assert!((frame.parts.powercap.zones[0].watts.unwrap() - 0.000021).abs() < 0.0000001);
    assert!((frame.parts.powercap.zones[1].watts.unwrap() - 0.0004).abs() < 0.0000001);
    assert_eq!(frame.parts.powercap.zones[0].energy_uj, Some(10));

    fs::write(package.join("energy_uj"), "20\n").unwrap();
    fs::write(core.join("energy_uj"), "900\n").unwrap();
    sample(&mut collector, &mut frame, 62_000).unwrap();
    assert!(
        frame
            .parts
            .powercap
            .zones
            .iter()
            .all(|zone| zone.watts.is_some())
    );
}

#[test]
fn no_zones_is_no_hardware_and_all_unreadable_energy_is_no_permission() {
    let empty = tempfile::tempdir().unwrap();
    let mut collector = PowercapCollector::at(empty.path());
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );

    let denied = tempfile::tempdir().unwrap();
    zone(denied.path(), "intel-rapl:0", "package-0", "1000", "1");
    let mut collector = PowercapCollector::with_fs(
        AccessDenyFs {
            inner: RealFs,
            opens: 0,
        },
        denied.path(),
    );
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoPermission)
    );
    assert_eq!(collector.fs.opens, 0);
}

#[test]
fn a_readable_zone_is_kept_when_another_zone_is_unreadable() {
    struct PartialFs {
        inner: RealFs,
        denied_path: PathBuf,
    }
    impl PowercapFs for PartialFs {
        type Handle = File;

        fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.inner.list(dir)
        }
        fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
            self.inner.read_small(path, buffer)
        }
        fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
            if path == self.denied_path {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fixture permission",
                ))
            } else {
                self.inner.open(path)
            }
        }
        fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
            handle.read_at(buffer, 0)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    zone(dir.path(), "intel-rapl:0", "package-0", "1000", "1");
    let core = zone(dir.path(), "intel-rapl:0:0", "core", "1000", "2");
    let mut collector = PowercapCollector::with_fs(
        PartialFs {
            inner: RealFs,
            denied_path: core.join("energy_uj"),
        },
        dir.path(),
    );
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    sample(&mut collector, &mut frame, 1_000).unwrap();
    assert_eq!(frame.parts.powercap.zones.len(), 1);
    assert_eq!(frame.parts.powercap.zones[0].name.as_ref(), "package-0");
}

#[test]
fn backwards_energy_without_a_parseable_range_is_absent_and_reprimes() {
    let dir = tempfile::tempdir().unwrap();
    let package = zone(dir.path(), "intel-rapl:0", "package-0", "unknown", "100");
    let mut collector = PowercapCollector::at(dir.path());
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    sample(&mut collector, &mut frame, 1_000).unwrap();
    fs::write(package.join("energy_uj"), "50\n").unwrap();
    sample(&mut collector, &mut frame, 2_000).unwrap();
    assert_eq!(frame.parts.powercap.zones[0].watts, None);
    fs::write(package.join("energy_uj"), "70\n").unwrap();
    sample(&mut collector, &mut frame, 3_000).unwrap();
    assert!((frame.parts.powercap.zones[0].watts.unwrap() - 0.00002).abs() < 0.000001);
}

#[test]
fn not_found_is_gone_and_discover_stamps_on_the_next_sample() {
    struct GoneFs {
        inner: RealFs,
        gone: Arc<AtomicBool>,
        lists: Arc<AtomicUsize>,
    }
    impl PowercapFs for GoneFs {
        type Handle = File;

        fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.list(dir)
        }
        fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
            self.inner.read_small(path, buffer)
        }
        fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
            self.inner.open(path)
        }
        fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
            if self.gone.load(Ordering::SeqCst) {
                Err(io::Error::new(io::ErrorKind::NotFound, "removed zone"))
            } else {
                handle.read_at(buffer, 0)
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    zone(dir.path(), "intel-rapl:0", "package-0", "1000", "10");
    let gone = Arc::new(AtomicBool::new(false));
    let lists = Arc::new(AtomicUsize::new(0));
    let mut collector = PowercapCollector::with_fs(
        GoneFs {
            inner: RealFs,
            gone: Arc::clone(&gone),
            lists: Arc::clone(&lists),
        },
        dir.path(),
    );
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    sample(&mut collector, &mut frame, 1_000).unwrap();
    gone.store(true, Ordering::SeqCst);
    assert!(matches!(
        sample(&mut collector, &mut frame, 2_000),
        Err(CollectError::Gone)
    ));

    gone.store(false, Ordering::SeqCst);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    assert_eq!(lists.load(Ordering::SeqCst), 2);
    sample(&mut collector, &mut frame, 62_001).unwrap();
    assert_eq!(lists.load(Ordering::SeqCst), 2);
}
