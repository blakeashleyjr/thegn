//! Linux base-tier temperatures from held hwmon input files.

use std::path::{Path, PathBuf};

use crate::collect::{
    linux::hwmon::{HwmonFs, RealFs},
    model::hwmon::SensorKind,
    policy::hwmon_group::{parse_milli, plausible_input, wake_gate},
};

#[derive(Debug)]
struct Slot<H> {
    label: String,
    handle: H,
}

pub(crate) struct BaseTemp<F: HwmonFs = RealFs> {
    fs: F,
    hwmon: PathBuf,
    hwmon_count: Option<usize>,
    slots: Vec<Slot<F::Handle>>,
    buffer: Vec<u8>,
    #[cfg(test)]
    reads: usize,
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
            buffer: vec![0; 64],
            #[cfg(test)]
            reads: 0,
        }
    }

    /// Discover once, then check the hwmon directory count on each slow tick.
    /// Input descriptors stay held across steady state; unavailable sensors are
    /// omitted and failed reads are dropped so a later slow tick can rediscover.
    pub(crate) fn fill(&mut self) -> Vec<(String, f32)> {
        let dirs = self.hwmon_dirs();
        if self.hwmon_count != Some(dirs.len()) {
            self.hwmon_count = Some(dirs.len());
            self.discover(&dirs);
        }
        let mut values = Vec::with_capacity(self.slots.len());
        let mut failed = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            #[cfg(test)]
            {
                self.reads += 1;
            }
            match self.fs.pread(&slot.handle, &mut self.buffer) {
                Ok(length) => {
                    if let Some(milli) = parse_milli(&self.buffer[..length])
                        && let Some(value) = plausible_input(SensorKind::Temp, milli)
                    {
                        values.push((slot.label.clone(), value));
                    }
                }
                Err(_) => failed.push(index),
            }
        }
        if !failed.is_empty() {
            for index in failed.into_iter().rev() {
                self.slots.remove(index);
            }
            self.hwmon_count = None;
        }
        values
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
        let mut retry_open = false;
        for dir in dirs {
            let name = read_text(&mut self.fs, &dir.join("name")).unwrap_or_default();
            let runtime = read_text(&mut self.fs, &dir.join("device/power/runtime_status"))
                .unwrap_or_default();
            let scsi_state = if name == "drivetemp" {
                Some(read_text(&mut self.fs, &dir.join("device/state")).unwrap_or_default())
            } else {
                None
            };
            if wake_gate(runtime.as_bytes(), scsi_state.as_deref().map(str::as_bytes)) {
                continue;
            }
            let device_model = read_text(&mut self.fs, &dir.join("device/model"));
            let Ok(entries) = self.fs.list(dir) else {
                continue;
            };
            let mut inputs: Vec<(u32, PathBuf)> = entries
                .into_iter()
                .filter_map(|path| {
                    let filename = path.file_name()?.to_str()?;
                    let Some(channel) = filename
                        .strip_prefix("temp")
                        .and_then(|suffix| suffix.strip_suffix("_input"))
                        .and_then(|channel| channel.parse::<u32>().ok())
                    else {
                        return None;
                    };
                    Some((channel, path))
                })
                .collect();
            inputs.sort_by_key(|(channel, _)| *channel);
            for (channel, path) in inputs {
                let label = read_text(&mut self.fs, &dir.join(format!("temp{channel}_label")))
                    .filter(|label| !label.is_empty());
                let label = format_label(&name, label.as_deref(), device_model.as_deref(), channel);
                match self.fs.open(&path) {
                    Ok(handle) => self.slots.push(Slot { label, handle }),
                    Err(_) => retry_open = true,
                }
            }
        }
        if retry_open {
            self.hwmon_count = None;
        }
    }
}

fn read_text<F: HwmonFs>(fs: &mut F, path: &Path) -> Option<String> {
    let mut buffer = [0; 256];
    let length = fs.read_small(path, &mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer[..length]).trim().to_owned())
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
        reads: usize,
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
            self.reads += 1;
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
        BaseTemp::with_fs(fs, root)
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
        let cpu_temp = temps
            .iter()
            .filter(|(label, _)| {
                label.to_ascii_lowercase().contains("cpu")
                    || label.to_ascii_lowercase().contains("k10temp")
            })
            .map(|(_, value)| *value)
            .reduce(f32::max);
        assert_eq!(cpu_temp, Some(61.0));
    }

    #[test]
    fn suspended_drive_is_not_opened_and_steady_state_reuses_each_slot() {
        let mut reader = fixture();
        let first = reader.fill();
        assert_eq!(first.len(), 3);
        assert!(
            reader
                .fs
                .opens
                .iter()
                .all(|path| !path.ends_with("hwmon4/temp1_input"))
        );
        assert_eq!(reader.reads, 4);
        let second = reader.fill();
        assert_eq!(second, first);
        assert_eq!(reader.reads, 8);
        assert_eq!(reader.fs.opens.len(), 4);
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
}
