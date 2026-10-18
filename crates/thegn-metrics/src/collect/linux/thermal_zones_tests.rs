use super::*;
use std::collections::HashMap;

#[derive(Default)]
struct FakeFs {
    dirs: Vec<PathBuf>,
    files: HashMap<PathBuf, Vec<u8>>,
    opens: Vec<PathBuf>,
    read_paths: Vec<PathBuf>,
}
impl ThermalFs for FakeFs {
    type Handle = PathBuf;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        Ok(self
            .dirs
            .iter()
            .filter(|entry| entry.parent() == Some(path))
            .cloned()
            .collect())
    }
    fn canonical(&mut self, path: &Path) -> io::Result<PathBuf> {
        self.files
            .get(path)
            .map(|_| path.to_owned())
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_paths.push(path.to_owned());
        let bytes = self.files.get(path).ok_or(io::ErrorKind::NotFound)?;
        let len = bytes.len().min(buffer.len());
        buffer[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
    }
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        self.opens.push(path.to_owned());
        self.files
            .contains_key(path)
            .then(|| path.to_owned())
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_small(handle, buffer)
    }
}

fn put(fs: &mut FakeFs, path: PathBuf, value: &str) {
    fs.files.insert(path, value.as_bytes().to_vec());
}

fn fixture() -> (FakeFs, PathBuf) {
    let root = PathBuf::from("/fake-sys");
    let base = root.join("class/thermal");
    let mut fs = FakeFs::default();
    for (name, zone_type, temperature, trip_kind, trip_temp, hyst) in [
        (
            "thermal_zone0",
            "acpitz",
            "48000\n",
            "passive",
            "70000\n",
            "2000\n",
        ),
        (
            "thermal_zone1",
            "x86_pkg_temp",
            "62000\n",
            "critical",
            "95000\n",
            "3000\n",
        ),
    ] {
        let zone = base.join(name);
        fs.dirs.push(zone.clone());
        put(&mut fs, zone.join("type"), &format!("{zone_type}\n"));
        put(&mut fs, zone.join("policy"), "step_wise\n");
        put(&mut fs, zone.join("mode"), "enabled\n");
        put(&mut fs, zone.join("temp"), temperature);
        put(
            &mut fs,
            zone.join("trip_point_0_type"),
            &format!("{trip_kind}\n"),
        );
        put(&mut fs, zone.join("trip_point_0_temp"), trip_temp);
        put(&mut fs, zone.join("trip_point_0_hyst"), hyst);
    }
    for (index, kind, state) in [
        (0, "Processor", "1\n"),
        (1, "Processor", "0\n"),
        (2, "Fan", "2\n"),
    ] {
        let device = base.join(format!("cooling_device{index}"));
        fs.dirs.push(device.clone());
        put(&mut fs, device.join("type"), &format!("{kind}\n"));
        put(&mut fs, device.join("max_state"), "4\n");
        put(&mut fs, device.join("cur_state"), state);
    }
    (fs, root)
}

#[test]
fn discovery_skips_acpitz_temp_and_sample_parses_trips_and_cooling_aggregate() {
    let (fs, root) = fixture();
    let mut collector = ThermalZoneCollector::with_fs(fs, &root);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    assert!(
        !collector
            .fs
            .opens
            .contains(&root.join("class/thermal/thermal_zone0/temp"))
    );
    assert!(
        collector
            .fs
            .opens
            .contains(&root.join("class/thermal/thermal_zone1/temp"))
    );
    collector.fs.read_paths.clear();
    assert_eq!(collector.cadence(), Cadence::Every(5));

    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 5,
                now_ms: 5_000,
            },
            &mut frame,
        )
        .unwrap();
    let part = &frame.parts.thermal_zone;
    assert!(
        !collector
            .fs
            .read_paths
            .contains(&root.join("class/thermal/thermal_zone0/temp"))
    );
    assert!(
        collector
            .fs
            .read_paths
            .contains(&root.join("class/thermal/thermal_zone1/temp"))
    );
    assert_eq!(part.zones.len(), 2);
    assert_eq!(part.zones[0].temp_c, None);
    assert_eq!(part.zones[0].policy.as_deref(), Some("step_wise"));
    assert_eq!(part.zones[0].mode.as_deref(), Some("enabled"));
    assert_eq!(part.zones[0].trips[0].temp_c, Some(70.0));
    assert_eq!(part.zones[0].trips[0].hyst_c, Some(2.0));
    assert_eq!(
        part.zones[0].trips[0].kind,
        crate::collect::model::thermal_zone::TripKind::Passive
    );
    assert_eq!(part.zones[1].temp_c, Some(62.0));
    assert_eq!(part.zones[1].trips[0].temp_c, Some(95.0));
    assert_eq!(part.zones[1].trips[0].hyst_c, Some(3.0));
    assert_eq!(
        part.zones[1].trips[0].kind,
        crate::collect::model::thermal_zone::TripKind::Critical
    );
    assert_eq!(
        part.cooling,
        [
            CoolingAggregate {
                kind: "Fan".to_owned(),
                engaged: 1,
                total: 1
            },
            CoolingAggregate {
                kind: "Processor".to_owned(),
                engaged: 1,
                total: 2
            },
        ]
    );
}

#[test]
fn changed_zone_count_triggers_rediscovery() {
    let (fs, root) = fixture();
    let mut collector = ThermalZoneCollector::with_fs(fs, &root);
    collector.discover(&Env::default());
    collector
        .fs
        .dirs
        .retain(|path| path.file_name().is_none_or(|name| name != "thermal_zone0"));
    let mut frame = MonitorFrame::default();
    for tick in (5..=60).step_by(5) {
        collector
            .sample(
                &TickCtx {
                    tick,
                    now_ms: u64::from(tick) * 1_000,
                },
                &mut frame,
            )
            .unwrap();
    }
    assert_eq!(frame.parts.thermal_zone.zones.len(), 1);
    assert_eq!(frame.parts.thermal_zone.zones[0].zone_type, "x86_pkg_temp");
}

#[test]
fn failed_second_temperature_read_clears_the_previous_sample() {
    let (fs, root) = fixture();
    let mut collector = ThermalZoneCollector::with_fs(fs, &root);
    collector.discover(&Env::default());
    let mut frame = MonitorFrame::default();
    let ctx = TickCtx {
        tick: 5,
        now_ms: 5_000,
    };
    collector.sample(&ctx, &mut frame).unwrap();
    assert_eq!(frame.parts.thermal_zone.zones.len(), 2);
    assert_eq!(frame.parts.thermal_zone.zones[1].temp_c, Some(62.0));

    collector
        .fs
        .files
        .remove(&root.join("class/thermal/thermal_zone1/temp"));
    collector.sample(&ctx, &mut frame).unwrap();
    assert_eq!(frame.parts.thermal_zone.zones.len(), 2);
    assert_eq!(frame.parts.thermal_zone.zones[1].temp_c, None);
}

#[test]
fn device_less_zones_of_the_same_type_get_enumeration_ordinals() {
    let (mut fs, root) = fixture();
    let zone = root.join("class/thermal/thermal_zone2");
    fs.dirs.push(zone.clone());
    put(&mut fs, zone.join("type"), "x86_pkg_temp\n");
    put(&mut fs, zone.join("temp"), "63000\n");

    let mut collector = ThermalZoneCollector::with_fs(fs, &root);
    collector.discover(&Env::default());
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 5,
                now_ms: 5_000,
            },
            &mut frame,
        )
        .unwrap();
    let zones = &frame.parts.thermal_zone.zones;
    assert_eq!(zones.len(), 3);
    assert_eq!(zones[1].key, "x86_pkg_temp:/unknown#0");
    assert_eq!(zones[2].key, "x86_pkg_temp:/unknown#1");
    assert_ne!(zones[1].key, zones[2].key);
}

#[test]
fn sample_after_close_rediscovers_zones() {
    let (fs, root) = fixture();
    let mut collector = ThermalZoneCollector::with_fs(fs, &root);
    collector.discover(&Env::default());
    collector.close();
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 5,
                now_ms: 5_000,
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(frame.parts.thermal_zone.zones.len(), 2);
}

#[test]
fn device_key_path_removes_thermal_zone_index() {
    assert_eq!(
        index_free_device_path(Path::new("devices/platform/thermal_zone4")),
        "/devices/platform/thermal_zone"
    );
}
