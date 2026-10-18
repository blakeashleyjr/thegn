//! Linux GPU identity discovery and runtime-power status sampling.
//!
//! Empty discovery returns `Absent`, and the registry does not sample absent
//! collectors. A later hotplug therefore requires the collector to be
//! rediscovered by its owner; periodic rediscovery only runs after hardware was
//! found.
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::collect::model::gpu::{Accelerator, GpuDevice, GpuKind, IpVersion, RuntimeStatus};
use crate::collect::policy::gpu_family::{family_for, kind_for};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    schedule::Cadence,
};

const DEVICE_CAP: usize = 8;
const REDISCOVER_AFTER_MS: u64 = 60_000;

pub trait GpuFs: Send {
    type Handle: Send;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize>;
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle>;
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize>;
}

pub struct RealFs;
impl GpuFs for RealFs {
    type Handle = File;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        File::open(path)?.read(buffer)
    }
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        File::open(path)
    }
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        handle.read_at(buffer, 0)
    }
}

struct DeviceReader<H> {
    device: GpuDevice,
    device_path: PathBuf,
    runtime: Option<H>,
    initialized: bool,
}

pub struct GpuDevicesCollector<F: GpuFs = RealFs> {
    fs: F,
    drm: PathBuf,
    accel: PathBuf,
    devices: Vec<DeviceReader<F::Handle>>,
    accels: Vec<Accelerator>,
    last_discovery_ms: Option<u64>,
    generation: u64,
    published_generation: Option<u64>,
    discovered: bool,
    truncated: bool,
}

impl GpuDevicesCollector<RealFs> {
    pub fn new() -> Self {
        Self::at("/sys/class/drm", "/sys/class/accel")
    }
    pub fn at(drm: impl Into<PathBuf>, accel: impl Into<PathBuf>) -> Self {
        Self::with_fs(RealFs, drm, accel)
    }
}

impl<F: GpuFs> GpuDevicesCollector<F> {
    pub fn with_fs(fs: F, drm: impl Into<PathBuf>, accel: impl Into<PathBuf>) -> Self {
        Self {
            fs,
            drm: drm.into(),
            accel: accel.into(),
            devices: Vec::new(),
            accels: Vec::new(),
            last_discovery_ms: None,
            generation: 0,
            published_generation: None,
            discovered: false,
            truncated: false,
        }
    }

    fn discover_devices(&mut self) {
        let entries = self.fs.list(&self.drm).unwrap_or_default();
        let mut cards = entries
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let digits = name.strip_prefix("card")?;
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                Some((digits.parse::<u32>().ok()?, path))
            })
            .collect::<Vec<_>>();
        cards.sort_by_key(|(index, _)| *index);
        let mut changed = false;
        self.devices.retain(|reader| {
            let present = cards.iter().any(|(index, _)| *index == reader.device.card);
            changed |= !present;
            present
        });
        self.truncated = false;
        for (index, card_path) in cards {
            if self
                .devices
                .iter()
                .any(|reader| reader.device.card == index)
            {
                continue;
            }
            if self.devices.len() == DEVICE_CAP {
                self.truncated = true;
                continue;
            }
            let device_path = card_path.join("device");
            let runtime_path = device_path.join("power/runtime_status");
            let runtime = self.fs.open(&runtime_path).ok();
            let runtime_status = read_runtime(&mut self.fs, runtime.as_ref());
            self.devices.push(DeviceReader {
                device: GpuDevice {
                    slot: String::new(),
                    card: index,
                    driver: None,
                    pci_id: None,
                    kind: GpuKind::Unknown,
                    family: None,
                    ip_version: None,
                    runtime_status,
                    vram_total: None,
                    gtt_total: None,
                    boot_vga: None,
                },
                device_path,
                runtime,
                initialized: false,
            });
            changed = true;
            if runtime_status == RuntimeStatus::Active {
                initialize_device(
                    &mut self.fs,
                    self.devices.last_mut().expect("just pushed reader"),
                );
            }
            let last = self.devices.len() - 1;
            if self.devices[last].initialized
                && self.devices[..last]
                    .iter()
                    .any(|known| known.device.slot == self.devices[last].device.slot)
            {
                self.devices.pop();
            } else {
                changed = true;
            }
        }
        self.devices.sort_by_key(|reader| reader.device.card);
        // Listing the accel class is cheap and keeps rediscovery limited to
        // class membership. Accelerator metadata remains from initial discovery.
        let _accel_entries = self.fs.list(&self.accel).unwrap_or_default();
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        self.discovered = true;
    }

    fn discover_accels(&mut self) {
        let Ok(entries) = self.fs.list(&self.accel) else {
            return;
        };
        let mut entries = entries
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let digits = name.strip_prefix("accel")?;
                if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                Some((digits.parse::<u32>().ok()?, path))
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|(index, _)| *index);
        for (_, path) in entries {
            let uevent = read_text(&mut self.fs, &path.join("device/uevent"));
            let Some(slot) = uevent
                .as_deref()
                .and_then(|text| value(text, "PCI_SLOT_NAME"))
            else {
                continue;
            };
            let driver = uevent
                .as_deref()
                .and_then(|text| value(text, "DRIVER"))
                .map(str::to_owned);
            self.accels.push(Accelerator {
                slot: slot.to_owned(),
                driver,
                name: read_text(&mut self.fs, &path.join("name")).unwrap_or_else(|| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("accel")
                        .to_owned()
                }),
            });
        }
    }
}

impl Default for GpuDevicesCollector<RealFs> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: GpuFs> Collector for GpuDevicesCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::GpuDevices
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.accels.clear();
        self.discover_devices();
        self.discover_accels();
        if self.devices.is_empty() && self.accels.is_empty() {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if !self.discovered {
            return Err(CollectError::Gone);
        }
        if self
            .last_discovery_ms
            .is_some_and(|last| ctx.now_ms.saturating_sub(last) >= REDISCOVER_AFTER_MS)
        {
            self.discover_devices();
            self.last_discovery_ms = Some(ctx.now_ms);
        } else if self.last_discovery_ms.is_none() {
            self.last_discovery_ms = Some(ctx.now_ms);
        }
        for reader in &mut self.devices {
            reader.device.runtime_status = read_runtime(&mut self.fs, reader.runtime.as_ref());
            if reader.device.runtime_status == RuntimeStatus::Active && !reader.initialized {
                initialize_device(&mut self.fs, reader);
                self.generation = self.generation.wrapping_add(1);
            }
        }
        if self.published_generation != Some(self.generation) {
            frame.parts.gpu_devices.devices.clear();
            frame
                .parts
                .gpu_devices
                .devices
                .extend(self.devices.iter().map(|reader| reader.device.clone()));
            frame.parts.gpu_devices.accels.clone_from(&self.accels);
            self.published_generation = Some(self.generation);
        } else {
            for (published, reader) in frame
                .parts
                .gpu_devices
                .devices
                .iter_mut()
                .zip(&self.devices)
            {
                published.runtime_status = reader.device.runtime_status;
            }
        }
        frame.parts.gpu_devices.truncated = self.truncated;
        frame.sampled.insert(CollectorId::GpuDevices);
        Ok(())
    }
    fn close(&mut self) {
        self.devices.clear();
        self.accels.clear();
        self.discovered = false;
        self.last_discovery_ms = None;
        self.published_generation = None;
    }
}

fn read_text<F: GpuFs>(fs: &mut F, path: &Path) -> Option<String> {
    let mut buffer = [0_u8; 512];
    let length = fs.read_small(path, &mut buffer).ok()?;
    Some(
        std::str::from_utf8(&buffer[..length.min(buffer.len())])
            .ok()?
            .trim()
            .to_owned(),
    )
}

fn read_u64<F: GpuFs>(fs: &mut F, path: &Path) -> Option<u64> {
    read_text(fs, path)?.parse().ok()
}

fn value<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == key).then_some(value.trim())
    })
}

fn read_ip_version<F: GpuFs>(fs: &mut F, device_path: &Path) -> Option<IpVersion> {
    let base = device_path.join("ip_discovery/die/0/GC/0");
    Some(IpVersion {
        major: read_u64(fs, &base.join("major"))?.try_into().ok()?,
        minor: read_u64(fs, &base.join("minor"))?.try_into().ok()?,
        revision: read_u64(fs, &base.join("revision"))?.try_into().ok()?,
    })
}

fn read_runtime<F: GpuFs>(fs: &mut F, handle: Option<&F::Handle>) -> RuntimeStatus {
    let Some(handle) = handle else {
        return RuntimeStatus::Unknown;
    };
    let mut buffer = [0_u8; 32];
    match fs.pread(handle, &mut buffer) {
        Ok(length) => match std::str::from_utf8(&buffer[..length.min(buffer.len())])
            .ok()
            .map(str::trim)
        {
            Some("active") => RuntimeStatus::Active,
            Some("suspended") => RuntimeStatus::Suspended,
            _ => RuntimeStatus::Unknown,
        },
        Err(_) => RuntimeStatus::Unknown,
    }
}

fn initialize_device<F: GpuFs>(fs: &mut F, reader: &mut DeviceReader<F::Handle>) {
    let uevent = read_text(fs, &reader.device_path.join("uevent"));
    let Some(slot) = uevent
        .as_deref()
        .and_then(|text| value(text, "PCI_SLOT_NAME"))
        .map(str::to_owned)
    else {
        return;
    };
    let driver = uevent
        .as_deref()
        .and_then(|text| value(text, "DRIVER"))
        .map(str::to_owned);
    let pci_id = uevent
        .as_deref()
        .and_then(|text| value(text, "PCI_ID"))
        .map(str::to_owned);
    let ip_version = read_ip_version(fs, &reader.device_path);
    let vram_total = read_u64(fs, &reader.device_path.join("mem_info_vram_total"));
    let gtt_total = read_u64(fs, &reader.device_path.join("mem_info_gtt_total"));
    let boot_vga =
        read_text(fs, &reader.device_path.join("boot_vga")).and_then(|text| match text.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        });
    let has_uma = fs.list(&reader.device_path.join("uma")).is_ok();
    reader.device.slot = slot;
    reader.device.driver = driver;
    reader.device.pci_id = pci_id;
    reader.device.ip_version = ip_version;
    reader.device.vram_total = vram_total;
    reader.device.gtt_total = gtt_total;
    reader.device.boot_vga = boot_vga;
    reader.device.family = family_for(
        reader.device.driver.as_deref(),
        ip_version,
        reader.device.pci_id.as_deref(),
    )
    .map(str::to_owned);
    reader.device.kind = kind_for(
        reader.device.driver.as_deref(),
        has_uma,
        vram_total,
        gtt_total,
        Some(&reader.device.slot),
    );
    reader.initialized = true;
}

#[cfg(test)]
#[path = "gpu_discover_tests.rs"]
mod tests;
