//! AMD/DRM GPU sysfs reader. Discovery is fixed at construction; device reads
//! are lazy for runtime-suspended cards and slow-tier fields are cached.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::gpu::GpuReading;

pub(crate) trait SysRead: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Option<String>;
    fn is_dir(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
    fn list(&self, path: &Path) -> Vec<String>;
}

pub(crate) struct RealFs;

impl SysRead for RealFs {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn list(&self, path: &Path) -> Vec<String> {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect()
    }
}

#[derive(Clone, Copy, Default)]
struct SlowReading {
    mem_mib: Option<(u64, u64)>,
    temp_c: Option<f32>,
    power_w: Option<f32>,
}

struct Card {
    device: PathBuf,
    runtime_status: PathBuf,
    busy: PathBuf,
    vram_total: Option<u64>,
    gtt_total: Option<u64>,
    apu: bool,
    vram_used: PathBuf,
    gtt_used: PathBuf,
    temp: Option<PathBuf>,
    power: Option<PathBuf>,
    resolved: bool,
    slow: Option<SlowReading>,
}

pub(crate) struct GpuSysfs {
    io: Arc<dyn SysRead>,
    cards: Mutex<Vec<Card>>,
}

impl GpuSysfs {
    pub(crate) fn new(root: &Path) -> Self {
        Self::with_reader(root, Arc::new(RealFs))
    }

    pub(crate) fn with_reader(root: &Path, io: Arc<dyn SysRead>) -> Self {
        let mut cards = Vec::new();
        let mut slots = HashSet::new();
        let mut names = io.list(root);
        names.sort_by_key(|name| {
            name.strip_prefix("card")
                .and_then(|index| index.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        });
        for name in names {
            let Some(index) = name.strip_prefix("card") else {
                continue;
            };
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let device = root.join(&name).join("device");
            let busy = device.join("gpu_busy_percent");
            if !io.is_file(&busy) {
                continue;
            }
            let slot = io.read_to_string(&device.join("uevent")).and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                    .map(str::to_owned)
            });
            if let Some(slot) = &slot
                && !slots.insert(slot.clone())
            {
                continue;
            }
            let runtime_status = device.join("power/runtime_status");
            let suspended = io
                .read_to_string(&runtime_status)
                .is_some_and(|status| status.trim() == "suspended");
            let mut card = Card {
                device: device.clone(),
                runtime_status,
                busy,
                vram_total: None,
                gtt_total: None,
                apu: false,
                vram_used: device.join("mem_info_vram_used"),
                gtt_used: device.join("mem_info_gtt_used"),
                temp: None,
                power: None,
                resolved: false,
                slow: None,
            };
            if !suspended {
                resolve(&*io, &mut card);
            }
            cards.push(card);
        }
        Self {
            io,
            cards: Mutex::new(cards),
        }
    }

    pub(crate) fn sample(&self, slow: bool) -> GpuReading {
        let mut cards = self
            .cards
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut selected: Option<(bool, usize, u8, GpuReading)> = None;
        for (index, card) in cards.iter_mut().enumerate() {
            let status = self.io.read_to_string(&card.runtime_status);
            if status.is_some_and(|value| value.trim() == "suspended") {
                let reading = GpuReading {
                    util_pct: Some(0),
                    ..GpuReading::default()
                };
                if selected.is_none() {
                    selected = Some((false, index, 0, reading));
                }
                continue;
            }
            if slow && !card.resolved {
                resolve(&*self.io, card);
            }
            let util = self
                .io
                .read_to_string(&card.busy)
                .and_then(|value| value.trim().parse::<u8>().ok())
                .map(|value| value.min(100));
            if slow {
                card.slow = Some(read_slow(&*self.io, card));
            }
            let slow_reading = card.slow.unwrap_or_default();
            let mem_mib = slow_reading.mem_mib;
            let reading = GpuReading {
                util_pct: util,
                mem_mib,
                temp_c: slow_reading.temp_c,
                power_w: slow_reading.power_w,
                mem_shared: card.apu && mem_mib.is_some(),
            };
            if let Some(util) = util
                && selected
                    .as_ref()
                    .is_none_or(|(awake, best_index, best, _)| {
                        !*awake || util > *best || (util == *best && index < *best_index)
                    })
            {
                selected = Some((true, index, util, reading));
            }
        }
        selected.map_or_else(GpuReading::default, |(_, _, _, reading)| reading)
    }

    pub(crate) fn has_cards(&self) -> bool {
        !self
            .cards
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
    }
}

fn resolve(io: &dyn SysRead, card: &mut Card) {
    card.vram_total = bytes_mib(io, &card.device.join("mem_info_vram_total"));
    card.gtt_total = bytes_mib(io, &card.device.join("mem_info_gtt_total"));
    card.apu = io.is_dir(&card.device.join("uma"))
        || (card.vram_total.is_some_and(|value| value <= 2048)
            && card.gtt_total.is_some_and(|value| value >= 8192));
    let hwmon = card.device.join("hwmon");
    let mut monitors = io.list(&hwmon);
    monitors.sort();
    if let Some(monitor) = monitors.into_iter().find(|name| name.starts_with("hwmon")) {
        let directory = hwmon.join(monitor);
        let mut temps = io
            .list(&directory)
            .into_iter()
            .filter(|name| name.starts_with("temp") && name.ends_with("_input"))
            .collect::<Vec<_>>();
        temps.sort();
        card.temp = temps
            .iter()
            .find(|input| {
                let label = input.replace("_input", "_label");
                io.read_to_string(&directory.join(label))
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("edge"))
            })
            .map(|input| directory.join(input))
            .or_else(|| {
                let path = directory.join("temp1_input");
                io.is_file(&path).then_some(path)
            });
        let mut powers = io
            .list(&directory)
            .into_iter()
            .filter(|name| {
                let Some(channel) = name.strip_prefix("power") else {
                    return false;
                };
                let Some((index, attr)) = channel.split_once('_') else {
                    return false;
                };
                !index.is_empty()
                    && index.bytes().all(|byte| byte.is_ascii_digit())
                    && matches!(attr, "average" | "input")
            })
            .collect::<Vec<_>>();
        powers.sort_by_key(|name| {
            let channel = name.strip_prefix("power").unwrap_or(name.as_str());
            let (index, attr) = channel.split_once('_').unwrap_or(("1", "input"));
            (index.parse::<u32>().unwrap_or(u32::MAX), attr != "average")
        });
        card.power = powers
            .iter()
            .find(|name| {
                let label = name
                    .strip_suffix("_average")
                    .or_else(|| name.strip_suffix("_input"))
                    .map(|channel| format!("{channel}_label"));
                label.is_some_and(|label| {
                    io.read_to_string(&directory.join(label))
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("ppt"))
                })
            })
            .or_else(|| powers.iter().find(|name| name.as_str() == "power1_average"))
            .or_else(|| powers.iter().find(|name| name.as_str() == "power1_input"))
            .map(|name| directory.join(name));
    }
    card.resolved = true;
}

fn bytes_mib(io: &dyn SysRead, path: &Path) -> Option<u64> {
    io.read_to_string(path)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
}

fn read_slow(io: &dyn SysRead, card: &Card) -> SlowReading {
    let vram_used = bytes_mib(io, &card.vram_used);
    let mem_mib = if card.apu {
        match (
            vram_used,
            bytes_mib(io, &card.gtt_used),
            card.vram_total,
            card.gtt_total,
        ) {
            (Some(vram), Some(gtt), Some(vram_total), Some(gtt_total)) => vram
                .checked_add(gtt)
                .zip(vram_total.checked_add(gtt_total))
                .filter(|(_, total)| *total > 0),
            _ => None,
        }
    } else {
        match (vram_used, card.vram_total) {
            (Some(used), Some(total)) if total > 0 => Some((used, total)),
            _ => None,
        }
    };
    let temp_c = card.temp.as_ref().and_then(|path| {
        io.read_to_string(path)
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| value / 1000.0)
    });
    let power_w = card.power.as_ref().and_then(|path| {
        io.read_to_string(path)
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| value / 1_000_000.0)
    });
    SlowReading {
        mem_mib,
        temp_c,
        power_w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingFs {
        reads: AtomicUsize,
    }

    impl SysRead for CountingFs {
        fn read_to_string(&self, path: &Path) -> Option<String> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            std::fs::read_to_string(path).ok()
        }
        fn is_dir(&self, path: &Path) -> bool {
            path.is_dir()
        }
        fn is_file(&self, path: &Path) -> bool {
            path.is_file()
        }
        fn list(&self, path: &Path) -> Vec<String> {
            std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        }
    }

    fn put(root: &Path, relative: &str, value: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dev = "card0/device";
        put(root, &format!("{dev}/gpu_busy_percent"), "70\n");
        put(
            root,
            &format!("{dev}/uevent"),
            "PCI_SLOT_NAME=0000:03:00.0\n",
        );
        put(root, &format!("{dev}/power/runtime_status"), "active\n");
        put(root, &format!("{dev}/mem_info_vram_used"), "104857600\n");
        put(root, &format!("{dev}/mem_info_vram_total"), "536870912\n");
        put(root, &format!("{dev}/mem_info_gtt_used"), "1073741824\n");
        put(root, &format!("{dev}/mem_info_gtt_total"), "50256154624\n");
        std::fs::create_dir_all(root.join(dev).join("uma")).unwrap();
        put(root, &format!("{dev}/hwmon/hwmon0/temp1_input"), "91000\n");
        put(
            root,
            &format!("{dev}/hwmon/hwmon0/temp1_label"),
            "junction\n",
        );
        put(root, &format!("{dev}/hwmon/hwmon0/temp2_input"), "80000\n");
        put(root, &format!("{dev}/hwmon/hwmon0/temp2_label"), "edge\n");
        put(
            root,
            &format!("{dev}/hwmon/hwmon0/power1_input"),
            "99999999\n",
        );
        put(
            root,
            &format!("{dev}/hwmon/hwmon0/power2_input"),
            "17458000\n",
        );
        put(root, &format!("{dev}/hwmon/hwmon0/power2_label"), "PPT\n");
        dir
    }

    fn add_card(root: &Path, name: &str, slot: &str, util: u8) {
        let device = format!("{name}/device");
        put(
            root,
            &format!("{device}/gpu_busy_percent"),
            &util.to_string(),
        );
        put(
            root,
            &format!("{device}/uevent"),
            &format!("PCI_SLOT_NAME={slot}\n"),
        );
        put(root, &format!("{device}/power/runtime_status"), "active\n");
        put(root, &format!("{device}/mem_info_vram_used"), "134217728\n");
        put(
            root,
            &format!("{device}/mem_info_vram_total"),
            "8589934592\n",
        );
        put(
            root,
            &format!("{device}/mem_info_gtt_total"),
            "17179869184\n",
        );
        put(
            root,
            &format!("{device}/hwmon/hwmon0/temp1_input"),
            "60000\n",
        );
        put(
            root,
            &format!("{device}/hwmon/hwmon0/power1_average"),
            "22000000\n",
        );
    }

    #[test]
    fn only_card_entries_are_discovered_and_apu_uses_shared_memory() {
        let dir = fixture();
        put(dir.path(), "card0-DP-1/device/gpu_busy_percent", "99");
        put(dir.path(), "renderD128/device/gpu_busy_percent", "98");
        let gpu = GpuSysfs::new(dir.path());
        let reading = gpu.sample(true);
        assert_eq!(reading.util_pct, Some(70));
        assert!(reading.mem_shared);
        assert_eq!(reading.mem_mib, Some((1124, 48440)));
        assert_eq!(reading.temp_c, Some(80.0));
        assert_eq!(reading.power_w, Some(17.458));
    }

    #[test]
    fn duplicate_slots_are_removed_and_busiest_card_supplies_details() {
        let duplicate = fixture();
        add_card(duplicate.path(), "card1", "0000:03:00.0", 99);
        let reading = GpuSysfs::new(duplicate.path()).sample(true);
        assert_eq!(reading.util_pct, Some(70));

        let multiple = fixture();
        put(multiple.path(), "card0/device/gpu_busy_percent", "10");
        add_card(multiple.path(), "card1", "0000:04:00.0", 70);
        let reading = GpuSysfs::new(multiple.path()).sample(true);
        assert_eq!(reading.util_pct, Some(70));
        assert_eq!(reading.mem_mib, Some((128, 8192)));
        assert!(!reading.mem_shared);
    }

    #[test]
    fn large_gtt_classifies_apu_without_uma_directory() {
        let dir = fixture();
        std::fs::remove_dir(dir.path().join("card0/device/uma")).unwrap();
        let reading = GpuSysfs::new(dir.path()).sample(true);
        assert!(reading.mem_shared);
    }

    #[test]
    fn suspended_card_has_no_device_reads_and_non_slow_uses_cache() {
        let dir = fixture();
        let io = Arc::new(CountingFs {
            reads: AtomicUsize::new(0),
        });
        put(
            dir.path(),
            "card0/device/power/runtime_status",
            "suspended\n",
        );
        let gpu = GpuSysfs::with_reader(dir.path(), io.clone());
        io.reads.store(0, Ordering::Relaxed);
        assert_eq!(gpu.sample(false).util_pct, Some(0));
        assert_eq!(io.reads.load(Ordering::Relaxed), 1);
        put(dir.path(), "card0/device/power/runtime_status", "active\n");
        let first = gpu.sample(true);
        let reads_after_slow = io.reads.load(Ordering::Relaxed);
        put(dir.path(), "card0/device/mem_info_vram_used", "209715200\n");
        put(dir.path(), "card0/device/mem_info_gtt_used", "2147483648\n");
        let cached = gpu.sample(false);
        assert_eq!(io.reads.load(Ordering::Relaxed) - reads_after_slow, 2);
        assert_eq!(cached.mem_mib, first.mem_mib);
    }
}
