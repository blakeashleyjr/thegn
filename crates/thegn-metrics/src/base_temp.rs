//! Linux base-tier temperatures from held hwmon input files.
//!
//! Compared with sysinfo, enumeration order is deterministic (numeric hwmon
//! order and channel order; sysinfo uses `read_dir` and `HashMap` iteration),
//! implausible readings are dropped (including NVMe's 65261.85 C sentinel and
//! disconnected probes' -128 C), and failed sensors are omitted instead of
//! retaining sysinfo's stale last value indefinitely after `refresh(false)`.

use std::path::{Path, PathBuf};

use crate::collect::{
    linux::hwmon::{HwmonFs, RealFs},
    model::hwmon::SensorKind,
    policy::hwmon_group::{parse_milli, plausible_input, wake_gate},
};

const TEXT_ATTRIBUTE_CAP: usize = 4096;

#[derive(Debug)]
struct Slot<H> {
    label: String,
    path: PathBuf,
    handle: Option<H>,
    chip: Option<usize>,
    pending: bool,
}

struct ChipGate<H> {
    runtime: Option<H>,
    state: Option<H>,
    status: Vec<u8>,
    scsi_state: Option<Vec<u8>>,
    gated: bool,
}

pub(crate) struct BaseTemp<F: HwmonFs = RealFs> {
    fs: F,
    hwmon: PathBuf,
    hwmon_count: Option<usize>,
    slots: Vec<Slot<F::Handle>>,
    gates: Vec<ChipGate<F::Handle>>,
    thermal: Vec<Slot<F::Handle>>,
    rediscover: bool,
    buffer: Vec<u8>,
}

impl BaseTemp<RealFs> {
    pub(crate) fn new() -> Self {
        Self::with_fs(RealFs::default(), "/sys")
    }
}

impl<F: HwmonFs> BaseTemp<F> {
    fn with_fs(fs: F, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            fs,
            hwmon: root.join("class/hwmon"),
            hwmon_count: None,
            slots: Vec::new(),
            gates: Vec::new(),
            thermal: Vec::new(),
            rediscover: false,
            buffer: vec![0; 64],
        }
    }

    /// Check for directory changes and reread held inputs and power gates.
    pub(crate) fn fill(&mut self) -> Vec<(String, f32)> {
        let dirs = self.hwmon_dirs();
        if self.hwmon_count != Some(dirs.len()) || self.rediscover {
            self.hwmon_count = Some(dirs.len());
            self.rediscover = false;
            self.discover(&dirs);
        }
        self.refresh_gates();
        let mut values: Vec<(String, f32)> = Vec::with_capacity(self.slots.len());
        let mut lost = false;
        for slot in &mut self.slots {
            if slot.chip.is_some_and(|chip| self.gates[chip].gated) {
                continue;
            }
            if slot.handle.is_none() && slot.pending {
                // A sensor that cannot be opened stays absent until the next rediscovery.
                slot.handle = Result::ok(self.fs.open(&slot.path));
                slot.pending = false;
            }
            let Some(handle) = slot.handle.as_ref() else {
                continue;
            };
            match self.fs.pread(handle, &mut self.buffer) {
                Ok(length) => {
                    if let Some(milli) = parse_milli(&self.buffer[..length])
                        && let Some(value) = plausible_input(SensorKind::Temp, milli)
                    {
                        merge_temperature(&mut values, &slot.label, value);
                    }
                }
                Err(error) if matches!(error.raw_os_error(), Some(2 | 6 | 19)) => {
                    slot.handle = None;
                    lost = true;
                }
                Err(_) => {}
            }
        }
        if lost {
            self.slots
                .retain(|slot| slot.handle.is_some() || slot.pending);
            self.rediscover = true;
        }
        if self.slots.is_empty() {
            self.read_thermal(&mut values);
        }
        values
    }

    fn refresh_gates(&mut self) {
        for (chip, gate) in self.gates.iter_mut().enumerate() {
            let mut buf = [0; 32];
            if let Some(handle) = &gate.runtime
                && let Ok(length) = self.fs.pread(handle, &mut buf)
            {
                gate.status = buf[..length].to_vec();
            }
            if let Some(handle) = &gate.state
                && let Ok(length) = self.fs.pread(handle, &mut buf)
            {
                gate.scsi_state = Some(buf[..length].to_vec());
            }
            let gated = wake_gate(&gate.status, gate.scsi_state.as_deref());
            if gate.gated != gated {
                gate.gated = gated;
                for slot in self.slots.iter_mut().filter(|slot| slot.chip == Some(chip)) {
                    if gated {
                        slot.handle = None;
                        slot.pending = true;
                    }
                }
            }
        }
    }

    fn read_thermal(&mut self, values: &mut Vec<(String, f32)>) {
        for slot in &mut self.thermal {
            if let Some(handle) = &slot.handle
                && let Ok(length) = self.fs.pread(handle, &mut self.buffer)
                && let Some(milli) = parse_milli(&self.buffer[..length])
                && let Some(value) = plausible_input(SensorKind::Temp, milli)
            {
                values.push((slot.label.clone(), value));
            }
        }
    }

    fn hwmon_dirs(&mut self) -> Vec<PathBuf> {
        let Ok(mut dirs) = self.fs.list(&self.hwmon) else {
            return Vec::new();
        };
        dirs.retain(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("hwmon"))
        });
        dirs.sort_by(|left, right| {
            let number = |path: &Path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("hwmon"))
                    .and_then(|digits| digits.parse::<u32>().ok())
            };
            number(left)
                .cmp(&number(right))
                .then_with(|| left.cmp(right))
        });
        dirs
    }

    fn discover(&mut self, dirs: &[PathBuf]) {
        self.slots.clear();
        self.gates.clear();
        self.thermal.clear();
        for dir in dirs {
            let name = read_text(&mut self.fs, &dir.join("name")).unwrap_or_default();
            let runtime_path = dir.join("device/power/runtime_status");
            let state_path = (name == "drivetemp").then(|| dir.join("device/state"));
            let runtime = read_text(&mut self.fs, &runtime_path).unwrap_or_default();
            let scsi_state = state_path
                .as_ref()
                .map(|path| read_text(&mut self.fs, path).unwrap_or_default());
            let gated = wake_gate(runtime.as_bytes(), scsi_state.as_deref().map(str::as_bytes));
            let chip_index = self.gates.len();
            self.gates.push(ChipGate {
                runtime: self.fs.open(&runtime_path).ok(),
                state: state_path.as_ref().and_then(|path| self.fs.open(path).ok()),
                status: runtime.into_bytes(),
                scsi_state: scsi_state.map(String::into_bytes),
                gated,
            });
            let device_model = read_text(&mut self.fs, &dir.join("device/model"));
            let Ok(entries) = self.fs.list(dir) else {
                continue;
            };
            let mut inputs: Vec<(u32, PathBuf)> = entries
                .into_iter()
                .filter_map(|path| {
                    let filename = path.file_name()?.to_str()?;
                    let channel = filename
                        .strip_prefix("temp")
                        .and_then(|suffix| suffix.strip_suffix("_input"))
                        .and_then(|channel| channel.parse::<u32>().ok())?;
                    Some((channel, path))
                })
                .collect();
            inputs.sort_by_key(|(channel, _)| *channel);
            for (channel, path) in inputs {
                let label = read_text(&mut self.fs, &dir.join(format!("temp{channel}_label")))
                    .filter(|label| !label.is_empty());
                let label = format_label(&name, label.as_deref(), device_model.as_deref(), channel);
                if gated {
                    self.slots.push(Slot {
                        label,
                        path,
                        handle: None,
                        chip: Some(chip_index),
                        pending: true,
                    });
                } else if let Ok(handle) = self.fs.open(&path) {
                    self.slots.push(Slot {
                        label,
                        path,
                        handle: Some(handle),
                        chip: Some(chip_index),
                        pending: false,
                    });
                }
            }
        }
        if self.slots.is_empty() {
            let thermal = self.hwmon.parent().unwrap_or(&self.hwmon).join("thermal");
            if let Ok(zones) = self.fs.list(&thermal) {
                let mut zones = zones;
                zones.retain(|zone| {
                    zone.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("thermal_"))
                });
                zones.sort();
                for zone in zones {
                    if read_text(&mut self.fs, &zone.join("type")).is_none() {
                        continue;
                    }
                    let path = zone.join("temp");
                    if let Ok(handle) = self.fs.open(&path) {
                        self.thermal.push(Slot {
                            label: String::new(),
                            path,
                            handle: Some(handle),
                            chip: None,
                            pending: false,
                        });
                    }
                }
            }
        }
    }
}

fn read_text<F: HwmonFs>(fs: &mut F, path: &Path) -> Option<String> {
    let mut buffer = [0; TEXT_ATTRIBUTE_CAP];
    let length = fs.read_small(path, &mut buffer).ok()?;
    if length >= buffer.len() {
        return None;
    }
    Some(
        String::from_utf8(buffer[..length].to_vec())
            .ok()?
            .trim_end()
            .to_owned(),
    )
}

/// Preserve sysinfo's component behavior: keep the first label position and
/// update it with each later valid reading in deterministic hwmon order.
fn merge_temperature(values: &mut Vec<(String, f32)>, label: &str, value: f32) {
    if let Some((_, existing)) = values.iter_mut().find(|(found, _)| found == label) {
        *existing = value;
    } else {
        values.push((label.to_owned(), value));
    }
}

fn format_label(name: &str, label: Option<&str>, model: Option<&str>, channel: u32) -> String {
    match (label, model) {
        (Some(label), Some(model)) => format!("{name} {label} {model}"),
        (Some(label), None) => format!("{name} {label}"),
        (None, Some(model)) => format!("{name} {model}"),
        (None, None) => format!("{name} temp{channel}"),
    }
}

#[cfg(test)]
mod tests {
    use super::BaseTemp;
    use crate::collect::linux::hwmon::HwmonFs;
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};

    #[derive(Default)]
    struct FakeFs {
        files: BTreeMap<PathBuf, Vec<u8>>,
        dirs: Vec<PathBuf>,
        opens: Vec<PathBuf>,
        preads: Vec<PathBuf>,
        eio: Vec<PathBuf>,
    }

    impl FakeFs {
        fn put(&mut self, path: impl Into<PathBuf>, value: &[u8]) {
            let path = path.into();
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if !self.dirs.iter().any(|existing| existing == dir) {
                    self.dirs.push(dir.to_path_buf());
                }
                parent = dir.parent();
            }
            self.files.insert(path, value.to_vec());
        }
    }

    impl HwmonFs for FakeFs {
        type Handle = PathBuf;

        fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            let mut paths: Vec<_> = self
                .dirs
                .iter()
                .filter(|path| path.parent() == Some(dir))
                .cloned()
                .chain(
                    self.files
                        .keys()
                        .filter(|path| path.parent() == Some(dir))
                        .cloned(),
                )
                .collect();
            paths.sort();
            Ok(paths)
        }

        fn canonical(&mut self, path: &Path) -> io::Result<PathBuf> {
            Ok(path.to_path_buf())
        }

        fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
            let bytes = self
                .files
                .get(path)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let length = bytes.len().min(buffer.len());
            buffer[..length].copy_from_slice(&bytes[..length]);
            Ok(length)
        }

        fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
            let exists = self.files.contains_key(path);
            if !exists {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            self.opens.push(path.to_path_buf());
            Ok(path.to_path_buf())
        }

        fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
            self.preads.push(handle.to_path_buf());
            if self.eio.iter().any(|path| path == handle) {
                return Err(io::Error::from_raw_os_error(5));
            }
            let bytes = self
                .files
                .get(handle)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let length = bytes.len().min(buffer.len());
            buffer[..length].copy_from_slice(&bytes[..length]);
            Ok(length)
        }
    }

    fn fixture() -> BaseTemp<FakeFs> {
        let root = Path::new("/fake");
        let mut fs = FakeFs::default();
        for (chip, name) in [
            ("hwmon0", "k10temp"),
            ("hwmon1", "amdgpu"),
            ("hwmon2", "nvme"),
            ("hwmon3", "spd5118"),
            ("hwmon4", "drivetemp"),
        ] {
            fs.put(
                root.join("class/hwmon").join(chip).join("name"),
                format!("{name}\n").as_bytes(),
            );
        }
        fs.put("/fake/class/hwmon/hwmon0/temp1_input", b"61000\n");
        fs.put("/fake/class/hwmon/hwmon0/temp1_label", b"Tctl\n");
        fs.put("/fake/class/hwmon/hwmon1/temp1_input", b"47000\n");
        fs.put("/fake/class/hwmon/hwmon1/temp1_label", b"edge\n");
        fs.put("/fake/class/hwmon/hwmon1/device/model", b"Radeon\n");
        fs.put("/fake/class/hwmon/hwmon2/temp1_input", b"42000\n");
        fs.put("/fake/class/hwmon/hwmon2/temp1_label", b"Composite\n");
        fs.put("/fake/class/hwmon/hwmon2/temp2_input", b"65261850\n");
        fs.put("/fake/class/hwmon/hwmon2/temp2_label", b"Sensor 2\n");
        fs.put("/fake/class/hwmon/hwmon3/temp1_input", b"31000\n");
        fs.put("/fake/class/hwmon/hwmon4/temp1_input", b"52000\n");
        fs.put(
            "/fake/class/hwmon/hwmon4/device/power/runtime_status",
            b"suspended\n",
        );
        fs.put("/fake/class/hwmon/hwmon4/device/state", b"running\n");
        BaseTemp::with_fs(fs, root)
    }

    fn pread_count(fs: &FakeFs, path: &Path) -> usize {
        fs.preads
            .iter()
            .filter(|read_path| read_path.as_path() == path)
            .count()
    }

    #[test]
    fn sysinfo_labels_values_sentinel_and_cpu_pick_match() {
        let mut reader = fixture();
        let temps = reader.fill();
        assert_eq!(temps.len(), 4);
        assert_eq!(temps[0], ("k10temp Tctl".to_owned(), 61.0));
        assert_eq!(temps[1], ("amdgpu edge Radeon".to_owned(), 47.0));
        assert_eq!(temps[2], ("nvme Composite".to_owned(), 42.0));
        assert_eq!(temps[3], ("spd5118 temp1".to_owned(), 31.0));
        assert!(!temps.iter().any(|(label, _)| label == "nvme Sensor 2"));
        assert_eq!(crate::sample::cpu_temp(&temps), Some(61.0));
    }

    #[test]
    fn suspended_drive_is_not_opened_and_steady_state_reuses_each_slot() {
        let mut reader = fixture();
        let first = reader.fill();
        assert_eq!(first.len(), 4);
        assert!(
            reader
                .fs
                .opens
                .iter()
                .all(|path| !path.ends_with("hwmon4/temp1_input"))
        );
        assert_eq!(
            reader
                .fs
                .preads
                .iter()
                .filter(|path| path.to_string_lossy().ends_with("_input"))
                .count(),
            5
        );
        assert_eq!(
            reader
                .fs
                .preads
                .iter()
                .filter(|path| path.ends_with("runtime_status"))
                .count(),
            1
        );
        let second = reader.fill();
        assert_eq!(second, first);
        assert_eq!(
            reader
                .fs
                .preads
                .iter()
                .filter(|path| path.to_string_lossy().ends_with("_input"))
                .count(),
            10
        );
        assert_eq!(
            reader
                .fs
                .preads
                .iter()
                .filter(|path| path.ends_with("runtime_status"))
                .count(),
            2
        );
        assert_eq!(
            reader
                .fs
                .opens
                .iter()
                .filter(|path| path.to_string_lossy().ends_with("_input"))
                .count(),
            5
        );
    }

    #[test]
    fn changed_hwmon_directory_count_rebuilds_the_inventory() {
        let mut reader = fixture();
        assert_eq!(reader.fill().len(), 4);
        reader.fs.put("/fake/class/hwmon/hwmon5/name", b"acpitz\n");
        reader
            .fs
            .put("/fake/class/hwmon/hwmon5/temp1_input", b"39000\n");
        assert!(
            reader
                .fill()
                .iter()
                .any(|(label, value)| label == "acpitz temp1" && *value == 39.0)
        );
    }

    #[test]
    fn duplicate_labels_keep_first_position_and_last_plausible_value() {
        let mut reader = fixture();
        reader.fill();
        reader.fs.put("/fake/class/hwmon/hwmon5/name", b"spd5118\n");
        reader
            .fs
            .put("/fake/class/hwmon/hwmon5/temp1_input", b"33000\n");
        let temps = reader.fill();
        let matches: Vec<_> = temps
            .iter()
            .filter(|(label, _)| label == "spd5118 temp1")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1, 33.0);
        assert_eq!(temps[3].0, "spd5118 temp1");
    }

    #[test]
    fn long_labels_are_complete_and_over_cap_labels_use_generated_fallback() {
        let mut reader = fixture();
        let long_label = "x".repeat(300);
        reader
            .fs
            .put("/fake/class/hwmon/hwmon5/name", b"longchip\n");
        reader
            .fs
            .put("/fake/class/hwmon/hwmon5/temp1_input", b"33000\n");
        reader.fs.put(
            "/fake/class/hwmon/hwmon5/temp1_label",
            format!("{long_label}\n").as_bytes(),
        );
        reader
            .fs
            .put("/fake/class/hwmon/hwmon6/name", b"fallback\n");
        reader
            .fs
            .put("/fake/class/hwmon/hwmon6/temp1_input", b"34000\n");
        reader.fs.put(
            "/fake/class/hwmon/hwmon6/temp1_label",
            format!("{}\n", "y".repeat(5000)).as_bytes(),
        );

        let temps = reader.fill();
        assert!(
            temps
                .iter()
                .any(|(label, value)| label == "fallback temp1" && *value == 34.0)
        );
        assert!(temps.iter().any(|(label, value)| {
            label == &format!("longchip {long_label}") && *value == 33.0
        }));
    }

    #[test]
    fn duplicate_label_rule_table_keeps_first_position_and_last_valid_value() {
        let cases = [
            (
                vec![("a", 10.0), ("b", 20.0), ("a", 30.0)],
                vec![("a", 30.0), ("b", 20.0)],
            ),
            (
                vec![("a", 10.0), ("a", 20.0), ("c", 30.0)],
                vec![("a", 20.0), ("c", 30.0)],
            ),
        ];
        for (readings, expected) in cases {
            let mut values = Vec::new();
            for (label, value) in readings {
                merge_temperature(&mut values, label, value);
            }
            assert_eq!(
                values,
                expected
                    .into_iter()
                    .map(|(label, value)| (label.to_owned(), value))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn runtime_gate_skips_and_resumes_slot_without_directory_change() {
        let mut reader = fixture();
        reader.fill();
        let drive = Path::new("/fake/class/hwmon/hwmon4/temp1_input");
        let gate = Path::new("/fake/class/hwmon/hwmon4/device/power/runtime_status");
        let initial_reads = pread_count(&reader.fs, drive);
        reader.fs.put(
            "/fake/class/hwmon/hwmon4/device/power/runtime_status",
            b"active\n",
        );
        assert!(
            reader
                .fill()
                .iter()
                .any(|(label, _)| label == "drivetemp temp1")
        );
        let active_reads = pread_count(&reader.fs, drive);
        reader.fs.put(
            "/fake/class/hwmon/hwmon4/device/power/runtime_status",
            b"suspended\n",
        );
        assert!(
            !reader
                .fill()
                .iter()
                .any(|(label, _)| label == "drivetemp temp1")
        );
        assert_eq!(pread_count(&reader.fs, drive), active_reads);
        reader.fs.put(
            "/fake/class/hwmon/hwmon4/device/power/runtime_status",
            b"active\n",
        );
        assert!(
            reader
                .fill()
                .iter()
                .any(|(label, _)| label == "drivetemp temp1")
        );
        assert!(
            reader
                .fs
                .opens
                .iter()
                .filter(|path| path.as_path() == drive)
                .count()
                >= 2
        );
        assert!(active_reads > initial_reads);
        assert_eq!(pread_count(&reader.fs, gate), 4);
    }

    #[test]
    fn persistent_eio_keeps_slot_open_and_other_values_arrive() {
        let mut reader = fixture();
        let failing = PathBuf::from("/fake/class/hwmon/hwmon0/temp1_input");
        reader.fs.eio.push(failing.clone());
        let first = reader.fill();
        let opened = reader
            .fs
            .opens
            .iter()
            .filter(|path| *path == &failing)
            .count();
        assert!(!first.iter().any(|(label, _)| label == "k10temp Tctl"));
        assert!(first.iter().any(|(label, _)| label == "amdgpu edge Radeon"));
        for _ in 0..2 {
            assert!(
                reader
                    .fill()
                    .iter()
                    .any(|(label, _)| label == "amdgpu edge Radeon")
            );
        }
        assert_eq!(
            reader
                .fs
                .opens
                .iter()
                .filter(|path| *path == &failing)
                .count(),
            opened
        );
        assert_eq!(
            reader
                .fs
                .preads
                .iter()
                .filter(|path| path.to_string_lossy().ends_with("_input"))
                .count(),
            15
        );
    }

    #[test]
    fn thermal_zone_is_used_when_hwmon_has_no_inputs() {
        let mut fs = FakeFs::default();
        fs.put("/fake/class/thermal/thermal_zone0/type", b"x86_pkg_temp\n");
        fs.put("/fake/class/thermal/thermal_zone0/temp", b"44000\n");
        let mut reader = BaseTemp::with_fs(fs, "/fake");
        assert_eq!(reader.fill(), vec![(String::new(), 44.0)]);
    }

    #[test]
    fn drivetemp_active_runtime_still_obeys_offline_scsi_state() {
        let mut reader = fixture();
        reader.fs.put(
            "/fake/class/hwmon/hwmon4/device/power/runtime_status",
            b"active\n",
        );
        reader
            .fs
            .put("/fake/class/hwmon/hwmon4/device/state", b"offline\n");
        assert!(
            !reader
                .fill()
                .iter()
                .any(|(label, _)| label == "drivetemp temp1")
        );
        assert!(
            !reader
                .fs
                .opens
                .iter()
                .any(|path| path.ends_with("hwmon4/temp1_input"))
        );
    }
}
