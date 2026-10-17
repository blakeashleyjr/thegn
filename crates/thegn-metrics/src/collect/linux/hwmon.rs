//! Shared Linux hwmon reader for the §2.3 CPU, GPU, thermal and detail consumers.
//! `gpu_sysfs.rs` and `sample.rs` switch their existing hwmon temperature readers in G-2/T-4.
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::hwmon::{
        ChipGroup, CostClass, HwmonChip, HwmonLayout, HwmonPart, HwmonSensor, HwmonValue, Level,
        Thresholds, ValueStatus,
    },
    policy::hwmon_group::{
        cost_for, group_for, level, parse_attr, parse_milli, plausible_input, plausible_threshold,
        stable_key, wake_gate,
    },
    schedule::Cadence,
};

pub const FD_CAP: usize = 256;

pub trait HwmonFs: Send {
    type Handle: Send;
    fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    fn canonical(&mut self, path: &Path) -> io::Result<PathBuf>;
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize>;
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle>;
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize>;
}

#[derive(Debug, Clone)]
pub struct RealFs {
    pub root: PathBuf,
}
impl RealFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}
impl Default for RealFs {
    fn default() -> Self {
        Self::new("/sys")
    }
}
impl HwmonFs for RealFs {
    type Handle = File;
    fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
    fn canonical(&mut self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        let mut file = File::open(path)?;
        file.read(buffer)
    }
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        File::open(path)
    }
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        handle.read_at(buffer, 0)
    }
}

struct ChipGate {
    path: PathBuf,
    state_path: Option<PathBuf>,
    status: Vec<u8>,
    scsi_state: Option<Vec<u8>>,
    gated: bool,
    can_wake: bool,
}
struct ProbedChip {
    order: PathBuf,
    dir: PathBuf,
    name: String,
    device: String,
    group: ChipGroup,
    gated: bool,
    can_wake: bool,
    state_path: Option<PathBuf>,
    status: Vec<u8>,
    state: Option<Vec<u8>>,
}
struct Slot<H> {
    path: PathBuf,
    sensor: HwmonSensor,
    handle: Option<H>,
    value: HwmonValue,
}

pub struct HwmonCollector<F: HwmonFs = RealFs> {
    fs: F,
    root: PathBuf,
    fd_cap: usize,
    layout: Option<Arc<HwmonLayout>>,
    slots: Vec<Slot<F::Handle>>,
    gates: Vec<ChipGate>,
    health: Health,
    generation: u64,
    held: usize,
    needs_probe: bool,
}
impl HwmonCollector<RealFs> {
    pub fn new() -> Self {
        Self::with_fs(RealFs::default(), "/sys", FD_CAP)
    }
}
impl Default for HwmonCollector<RealFs> {
    fn default() -> Self {
        Self::new()
    }
}
impl<F: HwmonFs> HwmonCollector<F> {
    pub fn with_fs(fs: F, root: impl Into<PathBuf>, fd_cap: usize) -> Self {
        Self {
            fs,
            root: root.into(),
            fd_cap,
            layout: None,
            slots: Vec::new(),
            gates: Vec::new(),
            health: Health::Warming,
            generation: 0,
            held: 0,
            needs_probe: false,
        }
    }
    pub fn held_fds(&self) -> usize {
        self.held
    }

    fn probe(&mut self) -> Health {
        self.drop_slots();
        self.gates.clear();
        let class = self.root.join("class/hwmon");
        let mut dirs = match self.fs.list(&class) {
            Ok(paths) => paths,
            Err(_) => {
                self.layout = None;
                self.health = Health::Absent(Absent::NoHardware);
                return self.health;
            }
        };
        dirs.sort();
        let canonical_root = self
            .fs
            .canonical(&self.root)
            .unwrap_or_else(|_| self.root.clone());
        let mut chips = Vec::<ProbedChip>::new();
        for dir in dirs {
            let name = match read_text(&mut self.fs, &dir.join("name")) {
                Some(value) if !value.is_empty() => value,
                _ => continue,
            };
            let Some(group) = group_for(&name) else {
                continue;
            };
            let device_path = dir.join("device");
            let device_abs = self.fs.canonical(&device_path).ok();
            let device = device_abs
                .as_ref()
                .and_then(|path| path.strip_prefix(&canonical_root).ok())
                .map(|p| format!("/{}", p.display()))
                .unwrap_or_else(|| "virtual".to_owned());
            let runtime_path = dir.join("device/power/runtime_status");
            let state_path = (name == "drivetemp").then(|| dir.join("device/state"));
            let status = match read_text(&mut self.fs, &runtime_path) {
                Some(value) => value,
                None => String::new(),
            };
            let scsi_state = state_path.as_ref().map(|path| {
                let mut bytes = Vec::with_capacity(32);
                if let Some(value) = read_text(&mut self.fs, path) {
                    bytes.extend_from_slice(value.as_bytes());
                }
                bytes
            });
            let is_gated = wake_gate(status.as_bytes(), scsi_state.as_deref());
            let can_wake = matches!(status.as_str(), "active" | "suspended" | "suspending")
                || state_path.is_some();
            let order = device_abs.unwrap_or_else(|| dir.clone());
            chips.push(ProbedChip {
                order,
                dir,
                name,
                device,
                group,
                gated: is_gated,
                can_wake,
                state_path,
                status: bounded_bytes(status.as_bytes()),
                state: scsi_state,
            });
        }
        chips.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
        let mut layout = HwmonLayout {
            generation: self.generation.saturating_add(1),
            ..HwmonLayout::default()
        };
        let mut group_ordinals = [0_u16; 9];
        let mut occupied = Vec::<String>::new();
        for chip in &chips {
            let ordinal = group_ordinals[group_index(chip.group)];
            group_ordinals[group_index(chip.group)] = ordinal.saturating_add(1);
            let gated = chip.gated;
            let chip_index = layout.chips.len() as u16;
            layout.chips.push(HwmonChip {
                name: chip.name.clone(),
                device: chip.device.clone(),
                group: chip.group,
                ordinal,
                gated,
            });
            let runtime_path = chip.dir.join("device/power/runtime_status");
            self.gates.push(ChipGate {
                path: runtime_path,
                state_path: chip
                    .state_path
                    .as_ref()
                    .map(|_| chip.dir.join("device/state")),
                status: chip.status.clone(),
                scsi_state: chip.state.clone(),
                gated,
                can_wake: chip.can_wake,
            });
            if gated {
                continue;
            }
            let Ok(attrs) = self.fs.list(&chip.dir) else {
                continue;
            };
            let mut candidates = attrs
                .iter()
                .filter_map(|path| {
                    let attr = path.file_name()?.to_str()?;
                    let (kind, channel, suffix) = parse_attr(attr)?;
                    let dynamic = match kind {
                        crate::collect::model::hwmon::SensorKind::Pwm => suffix.is_empty(),
                        crate::collect::model::hwmon::SensorKind::Fan => {
                            suffix == "_input" || suffix == "_target"
                        }
                        crate::collect::model::hwmon::SensorKind::Power => {
                            suffix == "_average" || suffix == "_input"
                        }
                        _ => suffix == "_input",
                    };
                    dynamic.then_some((kind, channel, suffix, path.clone()))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(b.2)));
            let mut seen_channels =
                Vec::<(crate::collect::model::hwmon::SensorKind, u16, &'static str)>::new();
            for (kind, channel, suffix, path) in candidates {
                let selector = match kind {
                    crate::collect::model::hwmon::SensorKind::Power => "_average",
                    crate::collect::model::hwmon::SensorKind::Fan => "_input",
                    _ => "",
                };
                if suffix != selector
                    && seen_channels
                        .iter()
                        .any(|(k, c, s)| *k == kind && *c == channel && *s == selector)
                {
                    continue;
                }
                if !selector.is_empty() {
                    seen_channels.push((kind, channel, selector));
                }
                let key = stable_key(&chip.name, Some(&chip.device), kind, channel);
                let unique = dedupe_key(key, &mut occupied);
                let label_path = chip.dir.join(format!("{}{channel}_label", prefix(kind)));
                let label = read_text(&mut self.fs, &label_path);
                let thresholds = read_thresholds(&mut self.fs, &chip.dir, kind, channel);
                let sensor = HwmonSensor {
                    key: unique,
                    chip: chip_index,
                    kind,
                    channel,
                    label,
                    thresholds,
                    cost: cost_for(&chip.name),
                };
                let handle = if sensor.cost != CostClass::VerySlow && self.held < self.fd_cap {
                    match self.fs.open(&path) {
                        Ok(fd) => {
                            self.held += 1;
                            Some(fd)
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };
                layout.sensors.push(sensor.clone());
                self.slots.push(Slot {
                    path,
                    sensor,
                    handle,
                    value: HwmonValue::default(),
                });
            }
        }
        self.generation = layout.generation;
        self.layout = Some(Arc::new(layout));
        self.needs_probe = false;
        self.health = if self.gates.is_empty() {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        };
        self.health
    }

    fn drop_slots(&mut self) {
        self.slots.clear();
        self.held = 0;
    }
}

impl<F: HwmonFs> Collector for HwmonCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::Hwmon
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.probe()
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if self.layout.is_none()
            || self.needs_probe
            || (ctx.tick > 0 && ctx.tick.is_multiple_of(30_u64))
        {
            self.probe();
        }
        let mut reads = 0_u32;
        let mut woke = false;
        for gate in &mut self.gates {
            if gate.gated && gate.can_wake {
                let mut buf = [0_u8; 32];
                let status = self.fs.read_small(&gate.path, &mut buf);
                reads = reads.saturating_add(1);
                if let Ok(length) = status {
                    gate.status.clear();
                    gate.status.extend_from_slice(&buf[..length]);
                }
                if let Some(path) = &gate.state_path {
                    let state = self.fs.read_small(path, &mut buf);
                    reads = reads.saturating_add(1);
                    if let Ok(length) = state
                        && let Some(state_bytes) = &mut gate.scsi_state
                    {
                        state_bytes.clear();
                        state_bytes.extend_from_slice(&buf[..length]);
                    }
                }
                gate.gated = wake_gate(&gate.status, gate.scsi_state.as_deref());
                woke |= !gate.gated;
            }
        }
        if woke {
            self.needs_probe = true;
        }
        if self.needs_probe {
            self.probe();
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if self
                .gates
                .get(usize::from(slot.sensor.chip))
                .is_some_and(|gate| gate.gated)
            {
                slot.value.status = ValueStatus::Gated;
                continue;
            }
            if !(ctx.tick + index as u64).is_multiple_of(u64::from(slot.sensor.cost.period())) {
                slot.value.status = ValueStatus::Carried;
                continue;
            }
            let mut buf = [0_u8; 32];
            let result = match &slot.handle {
                Some(handle) => self.fs.pread(handle, &mut buf),
                None => self.fs.read_small(&slot.path, &mut buf),
            };
            reads = reads.saturating_add(1);
            match result {
                Ok(length) => match parse_milli(&buf[..length])
                    .and_then(|n| plausible_input(slot.sensor.kind, n))
                {
                    Some(value) => {
                        slot.value = HwmonValue {
                            value: Some(value),
                            level: level(Some(value), &slot.sensor.thresholds),
                            status: ValueStatus::Fresh,
                        };
                    }
                    None => {
                        slot.value = HwmonValue {
                            value: None,
                            level: Level::Normal,
                            status: ValueStatus::Implausible,
                        };
                    }
                },
                Err(error) => {
                    slot.value.value = None;
                    slot.value.level = Level::Normal;
                    slot.value.status = ValueStatus::Error;
                    if matches!(error.kind(), io::ErrorKind::NotFound)
                        || matches!(error.raw_os_error(), Some(code) if code == libc::ENODEV || code == libc::ESTALE)
                    {
                        self.needs_probe = true;
                    }
                }
            }
        }
        let part: &mut HwmonPart = &mut frame.parts.hwmon;
        part.layout = self.layout.clone();
        part.values.clear();
        part.values.extend(self.slots.iter().map(|slot| slot.value));
        part.reads = reads;
        part.held_fds = self.held.min(usize::from(u16::MAX)) as u16;
        part.gated_chips = self
            .gates
            .iter()
            .filter(|gate| gate.gated)
            .count()
            .min(usize::from(u16::MAX)) as u16;
        Ok(())
    }
    fn close(&mut self) {
        self.drop_slots();
        self.gates.clear();
        self.layout = None;
        self.needs_probe = false;
    }
}

fn read_text<F: HwmonFs>(fs: &mut F, path: &Path) -> Option<String> {
    let mut buf = [0_u8; 256];
    let length = fs.read_small(path, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..length]).trim().to_owned())
}
fn bounded_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(32);
    buffer.extend_from_slice(bytes);
    buffer
}
fn prefix(kind: crate::collect::model::hwmon::SensorKind) -> &'static str {
    match kind {
        crate::collect::model::hwmon::SensorKind::Temp => "temp",
        crate::collect::model::hwmon::SensorKind::Fan => "fan",
        crate::collect::model::hwmon::SensorKind::Pwm => "pwm",
        crate::collect::model::hwmon::SensorKind::Power => "power",
        crate::collect::model::hwmon::SensorKind::Voltage => "in",
        crate::collect::model::hwmon::SensorKind::Current => "curr",
        crate::collect::model::hwmon::SensorKind::Freq => "freq",
        crate::collect::model::hwmon::SensorKind::Energy => "energy",
    }
}
fn group_index(group: ChipGroup) -> usize {
    match group {
        ChipGroup::Cpu => 0,
        ChipGroup::Gpu => 1,
        ChipGroup::Nvme => 2,
        ChipGroup::Sata => 3,
        ChipGroup::Dimm => 4,
        ChipGroup::Acpi => 5,
        ChipGroup::BoardEc => 6,
        ChipGroup::Wifi => 7,
        ChipGroup::Other => 8,
    }
}
fn dedupe_key(key: String, used: &mut Vec<String>) -> String {
    if !used.contains(&key) {
        used.push(key.clone());
        return key;
    }
    let mut ordinal = 2_u32;
    loop {
        let candidate = format!("{key}~{ordinal}");
        if !used.contains(&candidate) {
            used.push(candidate.clone());
            return candidate;
        }
        ordinal = ordinal.saturating_add(1);
    }
}
fn read_thresholds<F: HwmonFs>(
    fs: &mut F,
    dir: &Path,
    kind: crate::collect::model::hwmon::SensorKind,
    channel: u16,
) -> Thresholds {
    let mut thresholds = Thresholds::default();
    for suffix in ["min", "lcrit", "max", "crit", "emergency", "crit_hyst"] {
        let path = dir.join(format!("{}{channel}_{suffix}", prefix(kind)));
        let value = read_text(fs, &path)
            .and_then(|text| plausible_threshold(kind, parse_milli(text.as_bytes())?));
        match suffix {
            "min" => thresholds.min = value,
            "lcrit" => thresholds.lcrit = value,
            "max" => thresholds.max = value,
            "crit" => thresholds.crit = value,
            "emergency" => thresholds.emergency = value,
            "crit_hyst" => thresholds.crit_hyst = value,
            _ => {}
        }
    }
    thresholds
}

#[cfg(test)]
mod tests {
    use super::{Collector, HwmonCollector, HwmonFs, RealFs};
    use crate::collect::{
        TickCtx,
        env::Env,
        frame::MonitorFrame,
        model::hwmon::{Level, ValueStatus},
    };
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Trace(Arc<Mutex<Vec<PathBuf>>>);
    struct TracedHandle {
        file: fs::File,
        path: PathBuf,
    }
    struct CountingFs {
        inner: RealFs,
        trace: Trace,
        fail_path: Option<PathBuf>,
    }
    impl HwmonFs for CountingFs {
        type Handle = TracedHandle;
        fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
            self.inner.list(path)
        }
        fn canonical(&mut self, path: &Path) -> io::Result<PathBuf> {
            self.inner.canonical(path)
        }
        fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
            self.trace.0.lock().unwrap().push(path.to_owned());
            self.inner.read_small(path, buffer)
        }
        fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
            self.trace.0.lock().unwrap().push(path.to_owned());
            Ok(TracedHandle {
                file: self.inner.open(path)?,
                path: path.to_owned(),
            })
        }
        fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
            self.trace.0.lock().unwrap().push(handle.path.clone());
            if self
                .fail_path
                .as_ref()
                .is_some_and(|path| path == &handle.path)
            {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            self.inner.pread(&handle.file, buffer)
        }
    }

    fn fixture(order: &[usize]) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = temp.path().join("sys");
        let class = root.join("class/hwmon");
        fs::create_dir_all(&class).unwrap();
        let specs = [
            "nvme",
            "acpi_fan",
            "acpitz",
            "k10temp",
            "amdgpu",
            "spd5118",
            "spd5118",
            "cros_ec",
            "mt7925_phy0",
            "BAT1",
            "ACAD",
            "ucsi_source_psy_0",
            "ucsi_source_psy_1",
            "ucsi_source_psy_2",
            "ucsi_source_psy_3",
        ];
        for (slot, spec_index) in order.iter().copied().enumerate() {
            let name = specs[spec_index];
            let hwmon = class.join(format!("hwmon{slot}"));
            fs::create_dir_all(&hwmon).unwrap();
            fs::write(hwmon.join("name"), name).unwrap();
            let device = root.join(format!("devices/chip{spec_index}"));
            fs::create_dir_all(device.join("power")).unwrap();
            fs::write(
                device.join("power/runtime_status"),
                if name == "k10temp" || name == "amdgpu" {
                    "active"
                } else {
                    "unsupported"
                },
            )
            .unwrap();
            symlink(&device, hwmon.join("device")).unwrap();
            match name {
                "nvme" => {
                    fs::write(hwmon.join("temp1_input"), "51000\n").unwrap();
                    fs::write(hwmon.join("temp1_max"), "81850\n").unwrap();
                    fs::write(hwmon.join("temp1_crit"), "84850\n").unwrap();
                    fs::write(hwmon.join("temp2_input"), "65261850\n").unwrap();
                    fs::write(hwmon.join("temp2_max"), "65261850\n").unwrap();
                    fs::write(hwmon.join("temp3_input"), "-273150\n").unwrap();
                    fs::write(hwmon.join("temp3_min"), "-273150\n").unwrap();
                }
                "acpi_fan" => {
                    fs::write(hwmon.join("fan1_input"), "1200\n").unwrap();
                    fs::write(hwmon.join("fan1_target"), "1500\n").unwrap();
                    fs::write(hwmon.join("power1_input"), "1000000\n").unwrap();
                }
                "acpitz" | "k10temp" | "amdgpu" | "mt7925_phy0" => {
                    fs::write(hwmon.join("temp1_input"), "55000\n").unwrap();
                }
                "spd5118" => {
                    fs::write(
                        hwmon.join("temp1_input"),
                        if spec_index == 5 {
                            "57250\n"
                        } else {
                            "62250\n"
                        },
                    )
                    .unwrap();
                    fs::write(hwmon.join("temp1_max"), "55000\n").unwrap();
                    fs::write(hwmon.join("temp1_crit"), "85000\n").unwrap();
                }
                "cros_ec" => {
                    fs::write(hwmon.join("temp1_input"), "52000\n").unwrap();
                    fs::write(hwmon.join("fan1_input"), "1300\n").unwrap();
                    fs::write(hwmon.join("fan1_target"), "1400\n").unwrap();
                }
                _ => {
                    fs::write(hwmon.join("in0_input"), "12000\n").unwrap();
                }
            }
        }
        (temp, root)
    }
    fn drivetemp_fixture(runtime: &str, state: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = temp.path().join("sys");
        let hwmon = root.join("class/hwmon/hwmon0");
        let device = root.join("devices/disk0");
        fs::create_dir_all(&hwmon).unwrap();
        fs::create_dir_all(device.join("power")).unwrap();
        fs::write(hwmon.join("name"), "drivetemp").unwrap();
        fs::write(hwmon.join("temp1_input"), "47000\n").unwrap();
        fs::write(device.join("power/runtime_status"), runtime).unwrap();
        fs::write(device.join("state"), state).unwrap();
        symlink(device, hwmon.join("device")).unwrap();
        (temp, root)
    }
    use std::os::unix::fs::PermissionsExt;

    fn collector_with_cap(root: &Path, trace: Trace, cap: usize) -> HwmonCollector<CountingFs> {
        HwmonCollector::with_fs(
            CountingFs {
                inner: RealFs::new("/"),
                trace,
                fail_path: None,
            },
            root,
            cap,
        )
    }
    fn collector(root: &Path, trace: Trace) -> HwmonCollector<CountingFs> {
        collector_with_cap(root, trace, 256)
    }

    #[test]
    fn hx370_inventory_filters_sentinels_groups_dimm_ordinals_and_fan_target() {
        let order = (0..15).collect::<Vec<_>>();
        let (_temp, root) = fixture(&order);
        let mut collector = collector(&root, Trace::default());
        assert_eq!(
            collector.discover(&Env::default()),
            crate::collect::health::Health::Ok
        );
        let mut frame = MonitorFrame::default();
        for tick in 0_u64..5 {
            collector
                .sample(&TickCtx { tick, now_ms: tick }, &mut frame)
                .unwrap();
        }
        let layout = frame.parts.hwmon.layout.as_ref().unwrap();
        assert_eq!(layout.chips.len(), 9);
        assert_eq!(
            layout
                .chips
                .iter()
                .filter(|chip| chip.group == crate::collect::model::hwmon::ChipGroup::Dimm)
                .count(),
            2
        );
        assert_eq!(
            layout
                .chips
                .iter()
                .filter(|chip| chip.group == crate::collect::model::hwmon::ChipGroup::Dimm)
                .map(|chip| chip.ordinal)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(
            layout
                .sensors
                .iter()
                .all(|sensor| !sensor.key.contains("hwmon"))
        );
        let nvme_temp1 = layout
            .sensors
            .iter()
            .find(|sensor| sensor.key.starts_with("nvme@") && sensor.key.ends_with("#temp1"))
            .unwrap();
        assert_eq!(nvme_temp1.thresholds.max, Some(81.85));
        assert_eq!(nvme_temp1.thresholds.crit, Some(84.85));
        let nvme_temp2 = layout
            .sensors
            .iter()
            .find(|sensor| sensor.key.starts_with("nvme@") && sensor.key.ends_with("#temp2"))
            .unwrap();
        assert_eq!(nvme_temp2.thresholds.max, None);
        let nvme_temp2_index = layout
            .sensors
            .iter()
            .position(|sensor| sensor.key == nvme_temp2.key)
            .unwrap();
        assert_eq!(frame.parts.hwmon.values[nvme_temp2_index].value, None);
        assert_eq!(
            frame.parts.hwmon.values[nvme_temp2_index].status,
            ValueStatus::Implausible
        );
        let nvme_temp3 = layout
            .sensors
            .iter()
            .find(|sensor| sensor.key.starts_with("nvme@") && sensor.key.ends_with("#temp3"))
            .unwrap();
        assert_eq!(nvme_temp3.thresholds.min, None);
        let nvme_temp3_index = layout
            .sensors
            .iter()
            .position(|sensor| sensor.key == nvme_temp3.key)
            .unwrap();
        assert_eq!(frame.parts.hwmon.values[nvme_temp3_index].value, None);
        assert_eq!(
            frame.parts.hwmon.values[nvme_temp3_index].status,
            ValueStatus::Implausible
        );
        assert!(
            layout
                .sensors
                .iter()
                .filter(|sensor| sensor.key.starts_with("spd5118@"))
                .all(|sensor| frame.parts.hwmon.values[layout
                    .sensors
                    .iter()
                    .position(|s| s.key == sensor.key)
                    .unwrap()]
                .level
                    == Level::OverMax)
        );
        assert!(
            layout
                .sensors
                .iter()
                .any(|sensor| sensor.key.contains("#fan1~2"))
        );
    }

    #[test]
    fn renumbered_devices_keep_keys_and_values_and_capacity_is_reused() {
        let first_order = (0..15).collect::<Vec<_>>();
        let second_order = [8, 3, 1, 7, 2, 6, 0, 5, 4, 14, 13, 12, 11, 10, 9];
        let (_a, root_a) = fixture(&first_order);
        let (_b, root_b) = fixture(&second_order);
        let mut a = collector(&root_a, Trace::default());
        let mut b = collector(&root_b, Trace::default());
        a.discover(&Env::default());
        b.discover(&Env::default());
        let mut fa = MonitorFrame::default();
        let mut fb = MonitorFrame::default();
        a.sample(&TickCtx::default(), &mut fa).unwrap();
        b.sample(&TickCtx::default(), &mut fb).unwrap();
        let mut ka = fa
            .parts
            .hwmon
            .layout
            .as_ref()
            .unwrap()
            .sensors
            .iter()
            .map(|s| s.key.clone())
            .collect::<Vec<_>>();
        let mut kb = fb
            .parts
            .hwmon
            .layout
            .as_ref()
            .unwrap()
            .sensors
            .iter()
            .map(|s| s.key.clone())
            .collect::<Vec<_>>();
        ka.sort();
        kb.sort();
        assert_eq!(ka, kb);
        let mut va = fa
            .parts
            .hwmon
            .layout
            .as_ref()
            .unwrap()
            .sensors
            .iter()
            .zip(&fa.parts.hwmon.values)
            .map(|(sensor, value)| (sensor.key.clone(), *value))
            .collect::<Vec<_>>();
        let mut vb = fb
            .parts
            .hwmon
            .layout
            .as_ref()
            .unwrap()
            .sensors
            .iter()
            .zip(&fb.parts.hwmon.values)
            .map(|(sensor, value)| (sensor.key.clone(), *value))
            .collect::<Vec<_>>();
        va.sort_by(|left, right| left.0.cmp(&right.0));
        vb.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(va, vb);
        assert!(!fa.parts.hwmon.values.is_empty());
        let ptr = fa.parts.hwmon.values.as_ptr();
        a.sample(&TickCtx { tick: 1, now_ms: 1 }, &mut fa).unwrap();
        assert_eq!(ptr, fa.parts.hwmon.values.as_ptr());
    }

    #[test]
    fn samples_stay_under_read_budget_and_close_releases_descriptors() {
        let order = (0..15).collect::<Vec<_>>();
        let (_temp, root) = fixture(&order);
        let mut collector = collector(&root, Trace::default());
        collector.discover(&Env::default());
        let mut frame = MonitorFrame::default();
        let mut seen_fresh = Vec::new();
        for tick in 0_u64..30 {
            collector.fs.trace.0.lock().unwrap().clear();
            collector
                .sample(&TickCtx { tick, now_ms: tick }, &mut frame)
                .unwrap();
            assert!(frame.parts.hwmon.reads <= 30);
            assert!(collector.fs.trace.0.lock().unwrap().len() <= 30);
            if seen_fresh.len() != frame.parts.hwmon.values.len() {
                seen_fresh.resize(frame.parts.hwmon.values.len(), false);
            }
            for (seen, value) in seen_fresh.iter_mut().zip(&frame.parts.hwmon.values) {
                *seen |= value.status != ValueStatus::Carried;
            }
        }
        assert!(seen_fresh.iter().all(|seen| *seen));
        collector.close();
        assert_eq!(collector.held_fds(), 0);
    }

    #[test]
    fn fd_cap_keeps_fast_slots_readable_and_enodev_is_slot_local() {
        let order = (0..15).collect::<Vec<_>>();
        let (_temp, root) = fixture(&order);
        let mut collector = collector_with_cap(&root, Trace::default(), 4);
        collector.discover(&Env::default());
        assert_eq!(collector.held_fds(), 4);
        let mut frame = MonitorFrame::default();
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        let layout = frame.parts.hwmon.layout.as_ref().unwrap();
        for (sensor, value) in layout.sensors.iter().zip(&frame.parts.hwmon.values) {
            if sensor.cost == crate::collect::model::hwmon::CostClass::Fast {
                assert!(
                    matches!(value.status, ValueStatus::Fresh | ValueStatus::Implausible),
                    "{}",
                    sensor.key
                );
            }
        }
        collector.fs.fail_path = Some(root.join("class/hwmon/hwmon0/temp1_input"));
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        assert_eq!(frame.parts.hwmon.values[0].status, ValueStatus::Error);
        for value in frame.parts.hwmon.values.iter().skip(1) {
            assert_ne!(value.status, ValueStatus::Error);
        }
        collector.close();
        assert_eq!(collector.held_fds(), 0);
    }

    #[test]
    fn suspended_and_offline_drivetemp_stay_unread_until_wake() {
        for (runtime, initial_state, wake_state) in [
            ("suspended", "running", "running"),
            ("active", "offline", "running"),
        ] {
            let (_temp, root) = drivetemp_fixture(runtime, initial_state);
            let trace = Trace::default();
            let mut collector = collector(&root, trace.clone());
            collector.discover(&Env::default());
            let mut frame = MonitorFrame::default();
            for tick in 0_u64..30 {
                collector
                    .sample(&TickCtx { tick, now_ms: tick }, &mut frame)
                    .unwrap();
            }
            let hwmon = root.join("class/hwmon/hwmon0");
            let gate_runtime = hwmon.join("device/power/runtime_status");
            let gate_state = hwmon.join("device/state");
            assert!(
                trace
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|path| path == &hwmon.join("name")
                        || path == &gate_runtime
                        || path == &gate_state)
            );
            assert!(
                frame
                    .parts
                    .hwmon
                    .layout
                    .as_ref()
                    .unwrap()
                    .sensors
                    .is_empty()
            );
            fs::write(gate_runtime, "active").unwrap();
            fs::write(gate_state, wake_state).unwrap();
            collector
                .sample(
                    &TickCtx {
                        tick: 30,
                        now_ms: 30,
                    },
                    &mut frame,
                )
                .unwrap();
            assert_eq!(frame.parts.hwmon.values.len(), 1);
            assert_eq!(frame.parts.hwmon.values[0].status, ValueStatus::Fresh);
        }
    }
}
