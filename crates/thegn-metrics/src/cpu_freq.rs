//! CPU frequency sampling. Linux keeps the sysfs files open and reads them on
//! the slow tier; other platforms use sysinfo's frequency refresh.

use std::path::Path;

use sysinfo::CpuRefreshKind;

/// Parse a sysfs CPU frequency, expressed in kHz.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_khz(bytes: &[u8]) -> Option<u64> {
    let value = std::str::from_utf8(bytes.trim_ascii())
        .ok()?
        .parse::<u64>()
        .ok()?;
    (value > 0).then_some(value)
}

/// Return the mean frequency in MHz from kHz readings.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn mean_mhz(khz: &[u64]) -> Option<u64> {
    if khz.is_empty() {
        return None;
    }
    Some(khz.iter().sum::<u64>() / khz.len() as u64 / 1000)
}

/// Configure sysinfo without opening per-core frequency files on Linux.
pub(crate) fn cpu_refresh_kind() -> CpuRefreshKind {
    #[cfg(target_os = "linux")]
    {
        CpuRefreshKind::nothing().with_cpu_usage()
    }
    #[cfg(not(target_os = "linux"))]
    {
        CpuRefreshKind::nothing().with_cpu_usage().with_frequency()
    }
}

/// A slow-tier CPU frequency reader.
pub(crate) struct CpuFreqProbe {
    #[cfg(target_os = "linux")]
    files: Vec<std::fs::File>,
    #[cfg(target_os = "linux")]
    buffer: [u8; 64],
}

impl CpuFreqProbe {
    /// Discover each CPU's preferred frequency file once, in CPU index order.
    pub(crate) fn discover(
        #[cfg(target_os = "linux")] root: &Path,
        #[cfg(not(target_os = "linux"))] _root: &Path,
    ) -> Self {
        #[cfg(target_os = "linux")]
        {
            let mut cpus = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let name = name.to_str()?;
                    let suffix = name.strip_prefix("cpu")?;
                    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                        return None;
                    }
                    let index = suffix.parse::<u32>().ok()?;
                    let path = entry.path().join("cpufreq");
                    let file = std::fs::File::open(path.join("cpuinfo_avg_freq"))
                        .or_else(|_| std::fs::File::open(path.join("scaling_cur_freq")))
                        .ok()?;
                    Some((index, file))
                })
                .collect::<Vec<_>>();
            cpus.sort_by_key(|(index, _)| *index);
            Self {
                files: cpus.into_iter().map(|(_, file)| file).collect(),
                buffer: [0; 64],
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {}
        }
    }

    /// Read the current mean frequency in MHz. Linux preads the held sysfs
    /// descriptors; other platforms refresh and read sysinfo's CPU values.
    pub(crate) fn read_mhz(
        &mut self,
        #[cfg(target_os = "linux")] _sys: &mut sysinfo::System,
        #[cfg(not(target_os = "linux"))] sys: &mut sysinfo::System,
    ) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileExt;

            let mut readings = Vec::with_capacity(self.files.len());
            for file in &self.files {
                self.buffer.fill(0);
                let count = match file.read_at(&mut self.buffer, 0) {
                    Ok(count) => count,
                    Err(_) => continue,
                };
                if let Some(value) = parse_khz(&self.buffer[..count]) {
                    readings.push(value);
                }
            }
            mean_mhz(&readings)
        }
        #[cfg(not(target_os = "linux"))]
        {
            sys.refresh_cpu_frequency();
            let frequencies = sys
                .cpus()
                .iter()
                .map(|cpu| cpu.frequency())
                .filter(|frequency| *frequency > 0)
                .collect::<Vec<_>>();
            if frequencies.is_empty() {
                None
            } else {
                Some(frequencies.iter().sum::<u64>() / frequencies.len() as u64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mean_mhz, parse_khz};

    #[test]
    fn parse_khz_handles_valid_and_invalid_sysfs_values() {
        assert_eq!(parse_khz(b"1237661\n"), Some(1_237_661));
        assert_eq!(parse_khz(b" \t1237661\r\n"), Some(1_237_661));
        assert_eq!(parse_khz(b""), None);
        assert_eq!(parse_khz(b"abc"), None);
        assert_eq!(parse_khz(b"0"), None);
    }

    #[test]
    fn mean_mhz_returns_none_for_no_readings() {
        assert_eq!(mean_mhz(&[]), None);
        assert_eq!(mean_mhz(&[1_000_000, 2_000_000]), Some(1500));
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use crate::cpu_freq::CpuFreqProbe;
        use std::fs;
        use std::path::Path;
        use tempfile::tempdir;

        fn cpu(root: &Path, index: u32, avg: Option<&str>, scaling: Option<&str>) {
            let cpufreq = root.join(format!("cpu{index}")).join("cpufreq");
            fs::create_dir_all(&cpufreq).unwrap();
            if let Some(value) = avg {
                fs::write(cpufreq.join("cpuinfo_avg_freq"), value).unwrap();
            }
            if let Some(value) = scaling {
                fs::write(cpufreq.join("scaling_cur_freq"), value).unwrap();
            }
        }

        fn read(root: &Path) -> Option<u64> {
            let mut probe = CpuFreqProbe::discover(root);
            let mut sys = sysinfo::System::new();
            probe.read_mhz(&mut sys)
        }

        #[test]
        fn separate_fixture_trees_produce_their_own_exact_means() {
            let first = tempdir().unwrap();
            cpu(first.path(), 0, Some("1000000\n"), None);
            cpu(first.path(), 1, Some("2000000\n"), None);
            cpu(first.path(), 2, Some("3000000\n"), None);
            cpu(first.path(), 3, Some("4000000\n"), None);

            let second = tempdir().unwrap();
            cpu(second.path(), 0, Some("2000000\n"), None);
            cpu(second.path(), 1, Some("4000000\n"), None);
            cpu(second.path(), 2, Some("6000000\n"), None);
            cpu(second.path(), 3, Some("8000000\n"), None);

            assert_eq!(read(first.path()), Some(2500));
            assert_eq!(read(second.path()), Some(5000));
        }

        #[test]
        fn held_files_are_reread_after_in_place_frequency_changes() {
            let root = tempdir().unwrap();
            cpu(root.path(), 0, Some("1000000\n"), None);
            cpu(root.path(), 1, Some("1000000\n"), None);
            let mut probe = CpuFreqProbe::discover(root.path());
            let mut sys = sysinfo::System::new();
            assert_eq!(probe.read_mhz(&mut sys), Some(1000));

            fs::write(
                root.path().join("cpu0/cpufreq/cpuinfo_avg_freq"),
                "3000000\n",
            )
            .unwrap();
            fs::write(
                root.path().join("cpu1/cpufreq/cpuinfo_avg_freq"),
                "3000000\n",
            )
            .unwrap();
            assert_eq!(probe.read_mhz(&mut sys), Some(3000));
        }

        #[test]
        fn prefers_average_frequency_and_falls_back_to_scaling_frequency() {
            let root = tempdir().unwrap();
            cpu(root.path(), 0, Some("2200000\n"), Some("900000\n"));
            cpu(root.path(), 1, None, Some("1800000\n"));
            assert_eq!(read(root.path()), Some(2000));
        }

        #[test]
        fn absent_and_zero_frequency_sources_are_unobserved() {
            let root = tempdir().unwrap();
            fs::create_dir(root.path().join("cpu0")).unwrap();
            cpu(root.path(), 1, Some("0\n"), None);
            assert_eq!(read(root.path()), None);

            let empty = tempdir().unwrap();
            assert_eq!(read(empty.path()), None);
        }
    }
}
