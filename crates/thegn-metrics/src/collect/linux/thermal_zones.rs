//! Linux thermal-zone and cooling-device reader.
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::collect::model::hwmon::SensorKind;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::thermal_zone::{CoolingAggregate, ThermalTrip, ThermalZone},
    policy::{
        hwmon_group::plausible_input,
        thermal::{AlarmDetector, Plausibility, classify_trip},
    },
    schedule::Cadence,
};

const FD_CAP: usize = 256;

pub trait ThermalFs: Send {
    type Handle: Send;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn canonical(&mut self, path: &Path) -> io::Result<PathBuf>;
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize>;
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle>;
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize>;
}

pub struct RealFs;
impl ThermalFs for RealFs {
    type Handle = File;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
    fn canonical(&mut self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
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

struct ZoneSlot<H> {
    zone: ThermalZone,
    temp_path: PathBuf,
    temp: Option<H>,
    plausibility: Plausibility,
    alarms: Vec<AlarmDetector>,
}
struct CoolingSlot<H> {
    kind: String,
    aggregate_index: usize,
    max_state: u64,
    state_path: PathBuf,
    state: Option<H>,
}

pub struct ThermalZoneCollector<F: ThermalFs = RealFs> {
    fs: F,
    root: PathBuf,
    zones: Vec<ZoneSlot<F::Handle>>,
    cooling: Vec<CoolingSlot<F::Handle>>,
    cooling_kinds: Vec<String>,
    cooling_counts: Vec<(u16, u16)>,
    held: usize,
    seen_zone_count: Option<usize>,
    samples_since_zone_count: u8,
}

impl ThermalZoneCollector<RealFs> {
    pub fn new() -> Self {
        Self::with_root("/sys")
    }
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::with_fs(RealFs, root)
    }
}
impl Default for ThermalZoneCollector<RealFs> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: ThermalFs> ThermalZoneCollector<F> {
    pub fn with_fs(fs: F, root: impl Into<PathBuf>) -> Self {
        Self {
            fs,
            root: root.into(),
            zones: Vec::new(),
            cooling: Vec::new(),
            cooling_kinds: Vec::new(),
            cooling_counts: Vec::new(),
            held: 0,
            seen_zone_count: None,
            samples_since_zone_count: 0,
        }
    }
    pub fn held_fds(&self) -> usize {
        self.held
    }
    fn base(&self) -> PathBuf {
        self.root.join("class/thermal")
    }

    fn discover_inner(&mut self) -> Health {
        self.zones.clear();
        self.cooling.clear();
        self.cooling_kinds.clear();
        self.held = 0;
        self.samples_since_zone_count = 0;
        let base = self.base();
        let Ok(entries) = self.fs.list(&base) else {
            self.seen_zone_count = Some(0);
            return Health::Absent(Absent::NoHardware);
        };
        let zone_entry_count = entries
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|part| part.to_str())
                    .and_then(|name| indexed_name(name, "thermal_zone"))
                    .is_some()
            })
            .count();
        let canonical_root = self
            .fs
            .canonical(&self.root)
            .unwrap_or_else(|_| self.root.clone());
        let mut zones = Vec::new();
        let mut cooling = Vec::new();
        for path in entries {
            let Some(name) = path.file_name().and_then(|part| part.to_str()) else {
                continue;
            };
            if let Some(index) = indexed_name(name, "thermal_zone") {
                let Some(zone_type) = read_text(&mut self.fs, &path.join("type")) else {
                    continue;
                };
                let device = self
                    .fs
                    .canonical(&path.join("device"))
                    .ok()
                    .and_then(|device| {
                        device
                            .strip_prefix(&canonical_root)
                            .ok()
                            .map(index_free_device_path)
                    })
                    .unwrap_or_else(|| "/unknown".to_owned());
                let policy = read_text(&mut self.fs, &path.join("policy"));
                let mode = read_text(&mut self.fs, &path.join("mode"));
                let trips = read_trips(&mut self.fs, &path);
                // T17: acpitz's zone duplicates the labelled hwmon acpitz input.
                let aliased_type = zone_type.eq_ignore_ascii_case("acpitz");
                let temp_path = path.join("temp");
                let temp = if !aliased_type && self.held < FD_CAP {
                    self.fs.open(&temp_path).ok().inspect(|_| self.held += 1)
                } else {
                    None
                };
                let alarms = trips.iter().map(|_| AlarmDetector::default()).collect();
                zones.push((
                    index,
                    ZoneSlot {
                        zone: ThermalZone {
                            key: String::new(),
                            zone_type,
                            device,
                            policy,
                            mode,
                            temp_c: None,
                            trips,
                            alias: aliased_type,
                            alarm_events: 0,
                        },
                        temp_path,
                        temp,
                        plausibility: Plausibility::default(),
                        alarms,
                    },
                ));
            } else if indexed_name(name, "cooling_device").is_some() {
                let Some(kind) = read_text(&mut self.fs, &path.join("type")) else {
                    continue;
                };
                // max_state is static metadata and validates this cooling entry.
                let Some(max_state) = read_number(&mut self.fs, &path.join("max_state"))
                    .and_then(|value| u64::try_from(value).ok())
                else {
                    continue;
                };
                let state = if self.held < FD_CAP {
                    self.fs
                        .open(&path.join("cur_state"))
                        .ok()
                        .inspect(|_| self.held += 1)
                } else {
                    None
                };
                cooling.push(CoolingSlot {
                    kind,
                    aggregate_index: 0,
                    max_state,
                    state_path: path.join("cur_state"),
                    state,
                });
            }
        }
        zones.sort_by_key(|(index, _)| *index);
        let mut device_less_ordinals = std::collections::BTreeMap::<String, usize>::new();
        for (_, slot) in &mut zones {
            if slot.zone.device == "/unknown" {
                let ordinal = device_less_ordinals
                    .entry(slot.zone.zone_type.clone())
                    .or_default();
                // Kernel enumeration is the only identity available when no device link exists.
                slot.zone.key = format!("{}:{}#{ordinal}", slot.zone.zone_type, slot.zone.device);
                *ordinal += 1;
            } else {
                slot.zone.key = format!("{}:{}", slot.zone.zone_type, slot.zone.device);
            }
        }
        self.seen_zone_count = Some(zone_entry_count);
        self.zones = zones.into_iter().map(|(_, zone)| zone).collect();
        self.cooling_kinds = cooling.iter().map(|slot| slot.kind.clone()).collect();
        self.cooling_kinds.sort();
        self.cooling_kinds.dedup();
        for slot in &mut cooling {
            slot.aggregate_index = self
                .cooling_kinds
                .binary_search(&slot.kind)
                .expect("cooling kind was collected");
        }
        self.cooling_counts.resize(self.cooling_kinds.len(), (0, 0));
        self.cooling = cooling;
        if self.zones.is_empty() && self.cooling.is_empty() {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }

    fn current_zone_count(&mut self) -> Option<usize> {
        let entries = self.fs.list(&self.base()).ok()?;
        Some(
            entries
                .iter()
                .filter(|path| {
                    path.file_name()
                        .and_then(|part| part.to_str())
                        .and_then(|name| indexed_name(name, "thermal_zone"))
                        .is_some()
                })
                .count(),
        )
    }
}

impl<F: ThermalFs> Collector for ThermalZoneCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::ThermalZone
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.discover_inner()
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        // Recount once every 12 samples (~60 s at the collector's 5 s cadence).
        let recount_zones = self.samples_since_zone_count >= 11;
        self.samples_since_zone_count = if recount_zones {
            0
        } else {
            self.samples_since_zone_count + 1
        };
        if self.seen_zone_count.is_none()
            || (recount_zones
                && self
                    .current_zone_count()
                    .is_some_and(|count| Some(count) != self.seen_zone_count))
        {
            self.discover_inner();
        }
        let part = &mut frame.parts.thermal_zone;
        part.reads = 0;
        part.held_fds = self.held.min(u16::MAX as usize) as u16;
        let mut buffer = [0_u8; 64];
        for (zone_index, zone) in self.zones.iter_mut().enumerate() {
            zone.zone.temp_c = None;
            let text = match zone.temp.as_ref() {
                Some(handle) => self.fs.pread(handle, &mut buffer).ok().and_then(|len| {
                    std::str::from_utf8(&buffer[..len.min(buffer.len())])
                        .ok()
                        .map(str::trim)
                }),
                None if !zone.zone.alias => self
                    .fs
                    .read_small(&zone.temp_path, &mut buffer)
                    .ok()
                    .and_then(|len| {
                        std::str::from_utf8(&buffer[..len.min(buffer.len())])
                            .ok()
                            .map(str::trim)
                    }),
                None => None,
            };
            if let Some(text) = text {
                part.reads = part.reads.saturating_add(1);
                if let Ok(raw) = text.parse::<i64>()
                    && let Some(temp_c) = plausible_input(SensorKind::Temp, raw)
                {
                    zone.zone.temp_c = zone
                        .plausibility
                        .accept(temp_c, ctx.now_ms)
                        .then_some(temp_c);
                }
            }
            for (index, trip) in zone.zone.trips.iter_mut().enumerate() {
                if let (Some(temp), Some(threshold)) = (zone.zone.temp_c, trip.temp_c)
                    && let Some(alarm) = zone.alarms.get_mut(index)
                {
                    alarm.update(temp, threshold);
                }
            }
            zone.zone.alarm_events = zone.alarms.iter().map(|alarm| alarm.events()).sum();
            if zone_index < part.zones.len() {
                part.zones[zone_index].clone_from(&zone.zone);
            } else {
                part.zones.push(zone.zone.clone());
            }
        }
        part.zones.truncate(self.zones.len());
        for counts in &mut self.cooling_counts {
            *counts = (0, 0);
        }
        for cooling in &mut self.cooling {
            let len = match cooling.state.as_ref() {
                Some(handle) => self.fs.pread(handle, &mut buffer).ok(),
                None => self.fs.read_small(&cooling.state_path, &mut buffer).ok(),
            };
            let Some(text) = len.and_then(|len| {
                std::str::from_utf8(&buffer[..len.min(buffer.len())])
                    .ok()
                    .map(str::trim)
            }) else {
                continue;
            };
            part.reads = part.reads.saturating_add(1);
            let Ok(state) = text.parse::<u64>() else {
                continue;
            };
            if state > cooling.max_state {
                continue;
            }
            let counts = &mut self.cooling_counts[cooling.aggregate_index];
            counts.1 = counts.1.saturating_add(1);
            if state > 0 {
                counts.0 = counts.0.saturating_add(1);
            }
        }
        let mut output_index = 0;
        for (kind, (engaged, total)) in self.cooling_kinds.iter().zip(&self.cooling_counts) {
            if *total > 0 {
                if output_index < part.cooling.len() {
                    let aggregate = &mut part.cooling[output_index];
                    aggregate.kind.clone_from(kind);
                    aggregate.engaged = *engaged;
                    aggregate.total = *total;
                } else {
                    part.cooling.push(CoolingAggregate {
                        kind: kind.clone(),
                        engaged: *engaged,
                        total: *total,
                    });
                }
                output_index += 1;
            }
        }
        part.cooling.truncate(output_index);
        part.held_fds = self.held.min(u16::MAX as usize) as u16;
        frame.sampled.insert(CollectorId::ThermalZone);
        Ok(())
    }
    fn close(&mut self) {
        self.zones.clear();
        self.cooling.clear();
        self.held = 0;
        self.seen_zone_count = None;
    }
}

fn indexed_name(name: &str, prefix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.parse::<u32>().ok()
}
fn index_free_device_path(path: &Path) -> String {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(|component| {
            if indexed_name(component, "thermal_zone").is_some() {
                "thermal_zone"
            } else {
                component
            }
        })
        .collect::<Vec<_>>();
    format!("/{}", components.join("/"))
}
fn read_text<F: ThermalFs>(fs: &mut F, path: &Path) -> Option<String> {
    let mut buffer = [0_u8; 256];
    let len = fs.read_small(path, &mut buffer).ok()?;
    let text = std::str::from_utf8(&buffer[..len.min(buffer.len())])
        .ok()?
        .trim();
    (!text.is_empty()).then(|| text.to_owned())
}
fn read_number<F: ThermalFs>(fs: &mut F, path: &Path) -> Option<i64> {
    read_text(fs, path)?.parse().ok()
}
fn read_trips<F: ThermalFs>(fs: &mut F, zone_path: &Path) -> Vec<ThermalTrip> {
    let mut trips = Vec::new();
    for index in 0..64_u16 {
        let prefix = format!("trip_point_{index}_");
        let kind = match read_text(fs, &zone_path.join(format!("{prefix}type"))) {
            Some(kind) => kind,
            None => break,
        };
        trips.push(ThermalTrip {
            index,
            kind: classify_trip(&kind),
            temp_c: read_number(fs, &zone_path.join(format!("{prefix}temp")))
                .map(|value| value as f32 / 1000.0),
            hyst_c: read_number(fs, &zone_path.join(format!("{prefix}hyst")))
                .map(|value| value as f32 / 1000.0),
        });
    }
    trips
}

#[cfg(test)]
#[path = "thermal_zones_tests.rs"]
mod tests;
