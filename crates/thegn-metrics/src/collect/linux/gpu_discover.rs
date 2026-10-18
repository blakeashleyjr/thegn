//! Linux GPU identity discovery and runtime-power status sampling.
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::collect::model::gpu::{Accelerator, GpuDevice, IpVersion, RuntimeStatus};
use crate::collect::policy::gpu_family::{family_for, kind_for};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    schedule::Cadence,
};

const DEVICE_CAP: usize = 8;

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
    runtime: Option<H>,
}

pub struct GpuDevicesCollector<F: GpuFs = RealFs> {
    fs: F,
    drm: PathBuf,
    accel: PathBuf,
    devices: Vec<DeviceReader<F::Handle>>,
    accels: Vec<Accelerator>,
    samples_since_discovery: u8,
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
            samples_since_discovery: 0,
            discovered: false,
            truncated: false,
        }
    }

    fn discover_devices(&mut self) -> Health {
        self.devices.clear();
        self.accels.clear();
        self.truncated = false;
        self.samples_since_discovery = 0;
        let entries = self.fs.list(&self.drm).unwrap_or_else(|_| Vec::new());
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
        for (index, card_path) in cards {
            let device_path = card_path.join("device");
            let uevent = read_text(&mut self.fs, &device_path.join("uevent"));
            let slot = uevent
                .as_deref()
                .and_then(|text| value(text, "PCI_SLOT_NAME"))
                .map(str::to_owned);
            let Some(slot) = slot else { continue };
            if self.devices.iter().any(|device| device.device.slot == slot) {
                continue;
            }
            if self.devices.len() == DEVICE_CAP {
                self.truncated = true;
                continue;
            }
            let driver = uevent
                .as_deref()
                .and_then(|text| value(text, "DRIVER"))
                .map(str::to_owned);
            let pci_id = uevent
                .as_deref()
                .and_then(|text| value(text, "PCI_ID"))
                .map(str::to_owned);
            let ip_version = read_ip_version(&mut self.fs, &device_path);
            let vram_total = read_u64(&mut self.fs, &device_path.join("mem_info_vram_total"));
            let gtt_total = read_u64(&mut self.fs, &device_path.join("mem_info_gtt_total"));
            let boot_vga =
                read_text(&mut self.fs, &device_path.join("boot_vga")).and_then(|text| match text
                    .trim()
                {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                });
            let has_uma = self.fs.list(&device_path.join("uma")).is_ok();
            let family =
                family_for(driver.as_deref(), ip_version, pci_id.as_deref()).map(str::to_owned);
            let kind = kind_for(
                driver.as_deref(),
                has_uma,
                vram_total,
                gtt_total,
                Some(&slot),
            );
            let runtime_path = device_path.join("power/runtime_status");
            let runtime = match self.fs.open(&runtime_path) {
                Ok(handle) => Some(handle),
                Err(_) => None,
            };
            self.devices.push(DeviceReader {
                device: GpuDevice {
                    slot,
                    card: index,
                    driver,
                    pci_id,
                    kind,
                    family,
                    ip_version,
                    runtime_status: RuntimeStatus::Unknown,
                    vram_total,
                    gtt_total,
                    boot_vga,
                },
                runtime,
            });
        }
        self.discover_accels();
        self.discovered = true;
        if self.devices.is_empty() && self.accels.is_empty() {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
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
        self.discover_devices()
    }
    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if !self.discovered {
            return Err(CollectError::Gone);
        }
        if self.samples_since_discovery >= 11 {
            self.discover_devices();
        } else {
            self.samples_since_discovery += 1;
        }
        let mut buffer = [0_u8; 32];
        for (index, reader) in self.devices.iter_mut().enumerate() {
            let status = match reader.runtime.as_ref() {
                Some(handle) => match self.fs.pread(handle, &mut buffer) {
                    Ok(length) => std::str::from_utf8(&buffer[..length.min(buffer.len())])
                        .ok()
                        .map(str::trim),
                    Err(_) => None,
                },
                None => None,
            };
            reader.device.runtime_status = status
                .map(|status| match status {
                    "active" => RuntimeStatus::Active,
                    "suspended" => RuntimeStatus::Suspended,
                    _ => RuntimeStatus::Unknown,
                })
                .unwrap_or(RuntimeStatus::Unknown);
            if index < frame.parts.gpu_devices.devices.len() {
                frame.parts.gpu_devices.devices[index].clone_from(&reader.device);
            } else {
                frame.parts.gpu_devices.devices.push(reader.device.clone());
            }
        }
        frame.parts.gpu_devices.devices.truncate(self.devices.len());
        frame.parts.gpu_devices.accels.clone_from(&self.accels);
        frame.parts.gpu_devices.truncated = self.truncated;
        frame.sampled.insert(CollectorId::GpuDevices);
        Ok(())
    }
    fn close(&mut self) {
        self.devices.clear();
        self.accels.clear();
        self.discovered = false;
        self.samples_since_discovery = 0;
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

#[cfg(test)]
#[path = "gpu_discover_tests.rs"]
mod tests;
