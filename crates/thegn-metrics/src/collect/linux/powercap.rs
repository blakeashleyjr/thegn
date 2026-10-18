//! Linux RAPL reader. Discovery reports NoPermission when every zone's
//! `energy_uj` is unreadable, NoHardware when no RAPL zones exist, and Ok
//! with unreadable zones omitted when at least one zone can be read. It never
//! escalates privileges.
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::powercap::{PowerDomain, PowercapZone},
    schedule::Cadence,
};

const REDISCOVER_AFTER_MS: u64 = 60_000;
const BUFFER_LEN: usize = 64;

pub trait PowercapFs: Send {
    type Handle: Send;
    fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize>;
    fn access_read(&mut self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle>;
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize>;
}

pub struct RealFs;
impl PowercapFs for RealFs {
    type Handle = File;

    fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        File::open(path)?.read(buffer)
    }

    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        File::open(path)
    }

    fn access_read(&mut self, path: &Path) -> io::Result<()> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in sysfs path"))?;
        let result = unsafe { libc::access(path.as_ptr(), libc::R_OK) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        handle.read_at(buffer, 0)
    }
}

struct Held<H> {
    name: Arc<str>,
    domain: PowerDomain,
    max_energy_range_uj: Option<u64>,
    energy: Option<H>,
    previous: Option<(u64, u64)>,
}

pub struct PowercapCollector<F: PowercapFs = RealFs> {
    fs: F,
    root: PathBuf,
    zones: Vec<Held<F::Handle>>,
    last_discovery_ms: Option<u64>,
}

impl PowercapCollector<RealFs> {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::with_fs(RealFs, root)
    }
}

impl<F: PowercapFs> PowercapCollector<F> {
    pub fn with_fs(fs: F, root: impl Into<PathBuf>) -> Self {
        Self {
            fs,
            root: root.into(),
            zones: Vec::new(),
            last_discovery_ms: None,
        }
    }

    fn discover_zones(&mut self) -> Health {
        self.zones.clear();
        let entries = match self.fs.list(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Health::Absent(Absent::NoPermission);
            }
            Err(_) => return Health::Absent(Absent::NoHardware),
        };
        let mut paths = Vec::new();
        for entry in &entries {
            if is_zone_path(entry) {
                paths.push(entry.clone());
                if let Ok(children) = self.fs.list(entry) {
                    paths.extend(children.into_iter().filter(|path| is_zone_path(path)));
                }
            }
        }
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return Health::Absent(Absent::NoHardware);
        }

        let mut buffer = [0_u8; BUFFER_LEN];
        let mut energy_opened = 0;
        let mut permission_denied = 0;
        for path in paths {
            let Some(dir_name) = path.file_name() else {
                continue;
            };
            let name = self
                .read_trimmed(&path.join("name"), &mut buffer)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| dir_name.to_string_lossy().into_owned());
            let max_energy_range_uj = self
                .read_trimmed(&path.join("max_energy_range_uj"), &mut buffer)
                .and_then(parse_u64);
            let energy_path = path.join("energy_uj");
            if let Err(error) = self.fs.access_read(&energy_path) {
                if error.kind() == io::ErrorKind::PermissionDenied {
                    permission_denied += 1;
                }
                continue;
            }
            let energy = match self.fs.open(&energy_path) {
                Ok(handle) => {
                    energy_opened += 1;
                    Some(handle)
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    permission_denied += 1;
                    None
                }
                Err(_) => None,
            };
            if energy.is_some() {
                self.zones.push(Held {
                    name: Arc::from(name.as_str()),
                    domain: domain_for(&name),
                    max_energy_range_uj,
                    energy,
                    previous: None,
                });
            }
        }
        if energy_opened == 0 && permission_denied > 0 {
            Health::Absent(Absent::NoPermission)
        } else if energy_opened == 0 {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }

    fn read_trimmed<'a>(&mut self, path: &Path, buffer: &'a mut [u8]) -> Option<&'a [u8]> {
        let length = self.fs.read_small(path, buffer).ok()?.min(buffer.len());
        Some(trim_ascii(&buffer[..length]))
    }
}

impl<F: PowercapFs> Collector for PowercapCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::Powercap
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        self.last_discovery_ms = Some(0);
        self.discover_zones()
    }

    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if self
            .last_discovery_ms
            .is_none_or(|last| ctx.now_ms.saturating_sub(last) >= REDISCOVER_AFTER_MS)
        {
            let health = self.discover_zones();
            self.last_discovery_ms = Some(ctx.now_ms);
            if health != Health::Ok {
                return Err(CollectError::Gone);
            }
        }
        frame.parts.powercap.zones.clear();
        let mut buffer = [0_u8; BUFFER_LEN];
        let mut gone = false;
        for zone in &mut self.zones {
            let Some(handle) = zone.energy.as_ref() else {
                continue;
            };
            let energy_uj = match self.fs.pread(handle, &mut buffer) {
                Ok(length) => parse_u64(trim_ascii(&buffer[..length.min(buffer.len())])),
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ENODEV) =>
                {
                    gone = true;
                    break;
                }
                Err(_) => None,
            };
            let watts = energy_uj.and_then(|energy| {
                let previous = zone.previous.replace((energy, ctx.now_ms));
                previous.and_then(|(previous_energy, previous_ms)| {
                    let dt_ms = ctx.now_ms.saturating_sub(previous_ms);
                    (dt_ms > 0)
                        .then(|| {
                            let delta =
                                wrapped_delta(previous_energy, energy, zone.max_energy_range_uj);
                            delta as f64 / dt_ms as f64 / 1_000.0
                        })
                        .map(|value| value as f32)
                })
            });
            frame.parts.powercap.zones.push(PowercapZone {
                name: zone.name.clone(),
                domain: zone.domain,
                watts,
                energy_uj,
            });
        }
        if gone {
            self.zones.clear();
            self.last_discovery_ms = None;
            return Err(CollectError::Gone);
        }
        frame.sampled.insert(CollectorId::Powercap);
        Ok(())
    }

    fn close(&mut self) {
        self.zones.clear();
        self.last_discovery_ms = None;
    }
}

fn is_zone_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(suffix) = name.strip_prefix("intel-rapl:") else {
        return false;
    };
    let mut parts = suffix.split(':');
    parts
        .next()
        .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn domain_for(name: &str) -> PowerDomain {
    let name = name.to_ascii_lowercase();
    if name.starts_with("package") {
        PowerDomain::Package
    } else if name == "core" || name.starts_with("core-") {
        PowerDomain::Core
    } else if name == "uncore" || name.starts_with("uncore-") {
        PowerDomain::Uncore
    } else if name == "dram" || name.starts_with("dram-") {
        PowerDomain::Dram
    } else if name == "psys" || name.starts_with("psys-") {
        PowerDomain::Psys
    } else {
        PowerDomain::Other
    }
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn wrapped_delta(previous: u64, current: u64, max_range: Option<u64>) -> u64 {
    if current >= previous {
        current - previous
    } else if let Some(max_range) = max_range {
        let modulus = u128::from(max_range) + 1;
        ((u128::from(current) + modulus - u128::from(previous)) % modulus) as u64
    } else {
        0
    }
}

#[cfg(test)]
#[path = "powercap_tests.rs"]
mod tests;
