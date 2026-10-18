//! Held-fd Linux power-supply reader and F3b collector.
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::power_supply::{
        PowerSupplyPart, SupplyKind, SupplyReading, SupplyStatus, active_usb_type,
        charge_to_energy_uwh,
    },
    schedule::Cadence,
};

pub const MAX_SUPPLIES: usize = 16;
const REDISCOVER_AFTER_MS: u64 = 60_000;
const BUFFER_LEN: usize = 128;
const ATTRS: [&str; 9] = [
    "status",
    "capacity",
    "energy_now",
    "charge_now",
    "voltage_now",
    "current_now",
    "power_now",
    "online",
    "usb_type",
];

pub trait SupplyFs: Send {
    type Handle: Send;
    fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize>;
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle>;
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize>;
}

pub struct RealFs;
impl SupplyFs for RealFs {
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
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        handle.read_at(buffer, 0)
    }
}

struct Held<H> {
    handles: [Option<H>; ATTRS.len()],
    static_values: [Option<u64>; 6],
    name: Arc<str>,
    kind: SupplyKind,
    device_scope: bool,
}
#[derive(Debug, Default, Clone, Copy)]
#[cfg(test)]
pub struct Counters {
    pub scans: usize,
    pub opens: usize,
    pub reads: usize,
    pub adapter_reads: usize,
}

pub struct SupplyReader<F: SupplyFs = RealFs> {
    fs: F,
    root: PathBuf,
    supplies: Vec<Held<F::Handle>>,
    #[cfg(test)]
    counters: Counters,
    #[cfg(test)]
    fail_next_read: bool,
    #[cfg(test)]
    fail_next_power_read: bool,
}
impl SupplyReader<RealFs> {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::with_fs(RealFs, root)
    }
}
impl<F: SupplyFs> SupplyReader<F> {
    pub fn with_fs(fs: F, root: impl Into<PathBuf>) -> Self {
        Self {
            fs,
            root: root.into(),
            supplies: Vec::new(),
            #[cfg(test)]
            counters: Counters::default(),
            #[cfg(test)]
            fail_next_read: false,
            #[cfg(test)]
            fail_next_power_read: false,
        }
    }
    pub fn held_fds(&self) -> usize {
        self.supplies
            .iter()
            .map(|s| s.handles.iter().filter(|h| h.is_some()).count())
            .sum()
    }
    #[cfg(test)]
    pub fn counters(&self) -> Counters {
        self.counters
    }
    #[cfg(test)]
    pub fn fail_next_read_for_test(&mut self) {
        self.fail_next_read = true;
    }
    #[cfg(test)]
    pub fn fail_next_power_read_for_test(&mut self) {
        self.fail_next_power_read = true;
    }

    pub fn discover(&mut self) -> Health {
        self.supplies.clear();
        #[cfg(test)]
        {
            self.counters.scans += 1;
        }
        let entries = match self.fs.list(&self.root) {
            Ok(entries) => entries,
            Err(_) => {
                return Health::Absent(Absent::NoHardware);
            }
        };
        let mut buffer = [0_u8; BUFFER_LEN];
        let mut typed_entries = Vec::new();
        for dir in entries {
            let Some(kind_bytes) = self.read_small(&dir.join("type"), &mut buffer) else {
                continue;
            };
            let kind = match trim(kind_bytes) {
                b"Battery" => SupplyKind::Battery,
                b"Mains" => SupplyKind::Mains,
                value if value.starts_with(b"USB") => SupplyKind::Usb,
                b"Unknown" => continue,
                _ => SupplyKind::Other,
            };
            typed_entries.push((dir, kind));
        }
        typed_entries.sort_by_key(|(_, kind)| match kind {
            SupplyKind::Battery => 0,
            SupplyKind::Mains => 1,
            SupplyKind::Usb => 2,
            SupplyKind::Other => 3,
        });
        let mut counted_supplies = 0;
        for (dir, kind) in typed_entries {
            let Some(file_name) = dir.file_name() else {
                continue;
            };
            let name: Arc<str> = file_name.to_string_lossy().into_owned().into();
            let scope = self
                .read_small(&dir.join("scope"), &mut buffer)
                .is_some_and(|b| trim(b) == b"Device");
            if !scope && counted_supplies >= MAX_SUPPLIES {
                continue;
            }
            let mut handles: [Option<F::Handle>; ATTRS.len()] = std::array::from_fn(|_| None);
            let attrs = if kind == SupplyKind::Other {
                &[(7, "online")][..]
            } else {
                &[
                    (0, "status"),
                    (1, "capacity"),
                    (2, "energy_now"),
                    (3, "charge_now"),
                    (4, "voltage_now"),
                    (5, "current_now"),
                    (6, "power_now"),
                    (7, "online"),
                    (8, "usb_type"),
                ][..]
            };
            for (index, attr) in attrs {
                if let Ok(handle) = self.fs.open(&dir.join(attr)) {
                    #[cfg(test)]
                    {
                        self.counters.opens += 1;
                    }
                    handles[*index] = Some(handle);
                }
            }
            if kind == SupplyKind::Other && handles[7].is_none() {
                continue;
            }
            let mut static_values = [None; 6];
            for (slot, attr) in static_values.iter_mut().zip([
                "energy_full",
                "energy_full_design",
                "charge_full",
                "charge_full_design",
                "voltage_min_design",
                "cycle_count",
            ]) {
                *slot = self
                    .read_small(&dir.join(attr), &mut buffer)
                    .and_then(parse_u64);
            }
            self.supplies.push(Held {
                handles,
                static_values,
                name,
                kind,
                device_scope: scope,
            });
            if !scope {
                counted_supplies += 1;
            }
        }
        if !self
            .supplies
            .iter()
            .any(|supply| supply.kind == SupplyKind::Battery && !supply.device_scope)
        {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }

    pub fn read_full(&mut self, part: &mut PowerSupplyPart) -> Result<(), CollectError> {
        part.supplies.clear();
        #[cfg(test)]
        if self.fail_next_read {
            self.fail_next_read = false;
            return Err(CollectError::Gone);
        }
        let mut buffer = [0_u8; BUFFER_LEN];
        for supply in &self.supplies {
            let mut values = [None; ATTRS.len()];
            let mut status = None;
            let mut usb_type = None;
            let mut current_now = None;
            for (index, handle) in supply.handles.iter().enumerate() {
                if let Some(handle) = handle {
                    #[cfg(test)]
                    {
                        self.counters.reads += 1;
                    }
                    match self.fs.pread(handle, &mut buffer) {
                        Ok(len) => {
                            let bytes = trim(&buffer[..len.min(buffer.len())]);
                            if index == 0 {
                                status = SupplyStatus::parse(bytes);
                            } else if index == 8 {
                                usb_type = active_usb_type(bytes);
                            } else if index == 5 {
                                current_now = parse_i64(bytes);
                            } else {
                                values[index] = parse_u64(bytes);
                            }
                        }
                        Err(error)
                            if matches!(index, 0 | 1)
                                && supply.kind == SupplyKind::Battery
                                && !supply.device_scope
                                && (error.kind() == io::ErrorKind::NotFound
                                    || error.raw_os_error() == Some(libc::ENODEV)) =>
                        {
                            return Err(CollectError::Gone);
                        }
                        Err(_) => {}
                    }
                }
            }
            let get_u64 = |index: usize| values[index];
            let mut reading = SupplyReading {
                name: supply.name.clone(),
                kind: supply.kind,
                device_scope: supply.device_scope,
                online: get_u64(7).map(|v| v != 0),
                status,
                capacity_pct: get_u64(1).and_then(to_u8),
                energy_now_uwh: get_u64(2),
                energy_full_uwh: supply.static_values[0],
                energy_full_design_uwh: supply.static_values[1],
                voltage_now_uv: get_u64(4),
                voltage_min_design_uv: supply.static_values[4],
                current_now_ua: current_now,
                power_now_uw: get_u64(6),
                cycle_count: supply.static_values[5].and_then(to_u32),
                usb_type_active: usb_type,
            };
            if reading.kind == SupplyKind::Battery && reading.energy_now_uwh.is_none() {
                reading.energy_now_uwh = get_u64(3).and_then(|charge| {
                    charge_to_energy_uwh(charge, reading.voltage_min_design_uv?)
                });
            }
            if reading.kind == SupplyKind::Battery && reading.energy_full_uwh.is_none() {
                reading.energy_full_uwh = supply.static_values[2].and_then(|charge| {
                    charge_to_energy_uwh(charge, reading.voltage_min_design_uv?)
                });
            }
            if reading.kind == SupplyKind::Battery && reading.energy_full_design_uwh.is_none() {
                reading.energy_full_design_uwh = supply.static_values[3].and_then(|charge| {
                    charge_to_energy_uwh(charge, reading.voltage_min_design_uv?)
                });
            }
            part.supplies.push(reading);
        }
        Ok(())
    }

    pub fn read_base(
        &mut self,
        out: &mut Vec<BaseSupply>,
        signal_critical_errors: bool,
    ) -> Result<(), CollectError> {
        out.clear();
        #[cfg(test)]
        if self.fail_next_read {
            self.fail_next_read = false;
            return Err(CollectError::Gone);
        }
        let mut buffer = [0_u8; BUFFER_LEN];
        let mut adapter_online = false;
        let mut status_ac = false;
        let fs = &mut self.fs;
        let supplies = &self.supplies;
        #[cfg(test)]
        let counters = &mut self.counters;
        for supply in supplies {
            if supply.kind != SupplyKind::Battery || supply.device_scope {
                continue;
            }
            let status = read_slot(
                fs,
                &supply.handles[0],
                &mut buffer,
                signal_critical_errors,
                #[cfg(test)]
                counters,
            )?
            .and_then(SupplyStatus::parse);
            let capacity = read_slot(
                fs,
                &supply.handles[1],
                &mut buffer,
                signal_critical_errors,
                #[cfg(test)]
                counters,
            )?
            .and_then(parse_u64)
            .and_then(to_u8);
            status_ac |= matches!(
                status,
                Some(SupplyStatus::Charging | SupplyStatus::Full | SupplyStatus::NotCharging)
            );
            let energy = read_optional(
                fs,
                &supply.handles[2],
                &mut buffer,
                #[cfg(test)]
                counters,
            )
            .and_then(parse_u64);
            let charge = if energy.is_none() {
                read_optional(
                    fs,
                    &supply.handles[3],
                    &mut buffer,
                    #[cfg(test)]
                    counters,
                )
                .and_then(parse_u64)
            } else {
                None
            };
            #[cfg(test)]
            let power = if self.fail_next_power_read {
                self.fail_next_power_read = false;
                None
            } else {
                read_optional(fs, &supply.handles[6], &mut buffer, counters).and_then(parse_u64)
            };
            #[cfg(not(test))]
            let power = read_optional(fs, &supply.handles[6], &mut buffer).and_then(parse_u64);
            let needs_iv = power.is_none() || (energy.is_none() && charge.is_some());
            let current = if needs_iv {
                read_optional(
                    fs,
                    &supply.handles[5],
                    &mut buffer,
                    #[cfg(test)]
                    counters,
                )
                .and_then(parse_i64)
            } else {
                None
            };
            let voltage = if needs_iv {
                read_optional(
                    fs,
                    &supply.handles[4],
                    &mut buffer,
                    #[cfg(test)]
                    counters,
                )
                .and_then(parse_u64)
            } else {
                None
            };
            out.push(BaseSupply {
                capacity,
                on_ac_status: matches!(
                    status,
                    Some(SupplyStatus::Charging | SupplyStatus::Full | SupplyStatus::NotCharging)
                ),
                on_ac: false,
                charging: status == Some(SupplyStatus::Charging),
                energy_now: energy,
                charge_now: charge,
                power_now: power,
                current_now: current,
                voltage_now: voltage,
                energy_full: supply.static_values[0],
                charge_full: supply.static_values[2],
                voltage_min_design: supply.static_values[4],
            });
        }
        if !status_ac && !out.is_empty() {
            for supply in supplies {
                if supply.kind != SupplyKind::Battery && supply.handles[7].is_some() {
                    #[cfg(test)]
                    {
                        counters.adapter_reads += 1;
                    }
                    if read_optional(
                        fs,
                        &supply.handles[7],
                        &mut buffer,
                        #[cfg(test)]
                        counters,
                    )
                    .and_then(parse_u64)
                        == Some(1)
                    {
                        adapter_online = true;
                        break;
                    }
                }
            }
        }
        for row in out {
            row.on_ac = status_ac || adapter_online;
        }
        Ok(())
    }
    pub fn close(&mut self) {
        self.supplies.clear();
    }

    fn read_small<'a>(&mut self, path: &Path, buffer: &'a mut [u8]) -> Option<&'a [u8]> {
        let Ok(len) = self.fs.read_small(path, buffer) else {
            return None;
        };
        Some(trim(&buffer[..len.min(buffer.len())]))
    }
}

#[derive(Debug, Default)]
pub struct BaseSupply {
    pub capacity: Option<u8>,
    pub on_ac_status: bool,
    pub on_ac: bool,
    pub charging: bool,
    pub energy_now: Option<u64>,
    pub charge_now: Option<u64>,
    pub power_now: Option<u64>,
    pub current_now: Option<i64>,
    pub voltage_now: Option<u64>,
    pub energy_full: Option<u64>,
    pub charge_full: Option<u64>,
    pub voltage_min_design: Option<u64>,
}

pub struct PowerSupplyCollector<F: SupplyFs = RealFs> {
    reader: SupplyReader<F>,
    discovered_at_ms: Option<u64>,
    last_now_ms: u64,
}
impl PowerSupplyCollector {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::with_fs(RealFs, root)
    }
}
impl<F: SupplyFs> PowerSupplyCollector<F> {
    pub fn with_fs(fs: F, root: impl Into<PathBuf>) -> Self {
        Self {
            reader: SupplyReader::with_fs(fs, root),
            discovered_at_ms: None,
            last_now_ms: 0,
        }
    }
}
impl<F: SupplyFs> Collector for PowerSupplyCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::PowerSupply
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(2)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        let health = self.reader.discover();
        self.discovered_at_ms = Some(self.last_now_ms);
        health
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        self.last_now_ms = ctx.now_ms;
        let mut rediscovered = false;
        if self
            .discovered_at_ms
            .is_none_or(|then| ctx.now_ms.saturating_sub(then) >= REDISCOVER_AFTER_MS)
        {
            self.discover(&Env::default());
            self.discovered_at_ms = Some(ctx.now_ms);
            rediscovered = true;
        }
        match self.reader.read_full(&mut frame.parts.power_supply) {
            Ok(()) => {
                frame.sampled.insert(CollectorId::PowerSupply);
                Ok(())
            }
            Err(error) => {
                if !rediscovered {
                    self.reader.discover();
                    self.discovered_at_ms = Some(ctx.now_ms);
                }
                match self.reader.read_full(&mut frame.parts.power_supply) {
                    Ok(()) => {
                        frame.sampled.insert(CollectorId::PowerSupply);
                        Ok(())
                    }
                    Err(_) => Err(error),
                }
            }
        }
    }
    fn close(&mut self) {
        self.reader.close();
    }
}

fn read_slot<'a, F: SupplyFs>(
    fs: &mut F,
    handle: &Option<F::Handle>,
    buffer: &'a mut [u8],
    signal_critical_errors: bool,
    #[cfg(test)] counters: &mut Counters,
) -> Result<Option<&'a [u8]>, CollectError> {
    let Some(handle) = handle else {
        return Ok(None);
    };
    #[cfg(test)]
    {
        counters.reads += 1;
    }
    match fs.pread(handle, buffer) {
        Ok(len) => Ok(Some(trim(&buffer[..len.min(buffer.len())]))),
        Err(_) if signal_critical_errors => Err(CollectError::Gone),
        Err(_) => Ok(None),
    }
}
fn read_optional<'a, F: SupplyFs>(
    fs: &mut F,
    handle: &Option<F::Handle>,
    buffer: &'a mut [u8],
    #[cfg(test)] counters: &mut Counters,
) -> Option<&'a [u8]> {
    read_slot(
        fs,
        handle,
        buffer,
        false,
        #[cfg(test)]
        counters,
    )
    .unwrap_or_default()
}

fn trim(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}
fn parse_u64(bytes: &[u8]) -> Option<u64> {
    let Ok(text) = std::str::from_utf8(trim(bytes)) else {
        return None;
    };
    let Ok(value) = text.parse() else { return None };
    Some(value)
}
fn parse_i64(bytes: &[u8]) -> Option<i64> {
    let Ok(text) = std::str::from_utf8(trim(bytes)) else {
        return None;
    };
    let Ok(value) = text.parse() else { return None };
    Some(value)
}
fn to_u8(value: u64) -> Option<u8> {
    let Ok(value) = u8::try_from(value.min(100)) else {
        return None;
    };
    Some(value)
}
fn to_u32(value: u64) -> Option<u32> {
    let Ok(value) = u32::try_from(value) else {
        return None;
    };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Default)]
    struct FakeFs {
        dirs: Vec<PathBuf>,
        files: HashMap<PathBuf, Vec<u8>>,
        failures: HashSet<PathBuf>,
        failure_codes: HashMap<PathBuf, i32>,
        lists: usize,
        opens: usize,
    }
    impl SupplyFs for FakeFs {
        type Handle = PathBuf;
        fn list(&mut self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.lists += 1;
            Ok(self
                .dirs
                .iter()
                .filter(|path| path.parent() == Some(dir))
                .cloned()
                .collect())
        }
        fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
            if self.failures.contains(path) {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            let bytes = self.files.get(path).ok_or(io::ErrorKind::NotFound)?;
            let len = bytes.len().min(buffer.len());
            buffer[..len].copy_from_slice(&bytes[..len]);
            Ok(len)
        }
        fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
            self.opens += 1;
            if self.files.contains_key(path) {
                Ok(path.to_owned())
            } else {
                Err(io::ErrorKind::NotFound.into())
            }
        }
        fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(code) = self.failure_codes.get(handle) {
                return Err(io::Error::from_raw_os_error(*code));
            }
            if self.failures.contains(handle) {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            let bytes = self.files.get(handle).ok_or(io::ErrorKind::NotFound)?;
            let len = bytes.len().min(buffer.len());
            buffer[..len].copy_from_slice(&bytes[..len]);
            Ok(len)
        }
    }

    fn fake_supply(fs: &mut FakeFs, root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        fs.dirs.push(dir.clone());
        for (key, value) in attrs {
            fs.files.insert(dir.join(key), value.as_bytes().to_vec());
        }
    }

    #[test]
    fn real_reader_discovers_and_reads_fixture_supply() {
        let dir = tempfile::tempdir().unwrap();
        let battery = dir.path().join("BAT1");
        std::fs::create_dir(&battery).unwrap();
        for (name, value) in [
            ("type", "Battery\n"),
            ("status", "Discharging\n"),
            ("capacity", "71\n"),
            ("energy_now", "12000000\n"),
            ("energy_full", "20000000\n"),
            ("energy_full_design", "22000000\n"),
        ] {
            std::fs::write(battery.join(name), value).unwrap();
        }
        let mut reader = SupplyReader::at(dir.path());
        assert_eq!(reader.discover(), Health::Ok);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies.len(), 1);
        assert_eq!(part.supplies[0].capacity_pct, Some(71));
        assert_eq!(part.supplies[0].energy_now_uwh, Some(12_000_000));
    }

    #[test]
    fn charge_tree_converts_now_full_and_design_using_minimum_voltage() {
        let dir = tempfile::tempdir().unwrap();
        let battery = dir.path().join("BAT1");
        std::fs::create_dir(&battery).unwrap();
        for (name, value) in [
            ("type", "Battery\n"),
            ("status", "Discharging\n"),
            ("charge_now", "3462000\n"),
            ("charge_full", "3462000\n"),
            ("charge_full_design", "3915000\n"),
            ("voltage_min_design", "15480000\n"),
        ] {
            std::fs::write(battery.join(name), value).unwrap();
        }
        let mut reader = SupplyReader::at(dir.path());
        assert_eq!(reader.discover(), Health::Ok);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies[0].energy_now_uwh, Some(53_591_760));
        assert_eq!(part.supplies[0].energy_full_uwh, Some(53_591_760));
        assert_eq!(part.supplies[0].energy_full_design_uwh, Some(60_604_200));
    }

    #[test]
    fn optional_enodev_degrades_one_field_and_close_releases_handles() {
        let root = PathBuf::from("/fake/power");
        let mut fs = FakeFs::default();
        fake_supply(
            &mut fs,
            &root,
            "BAT1",
            &[
                ("type", "Battery\n"),
                ("status", "Discharging\n"),
                ("capacity", "74\n"),
                ("energy_now", "12000000\n"),
                ("cycle_count", "11\n"),
            ],
        );
        fs.failures.insert(root.join("BAT1/cycle_count"));
        let mut reader = SupplyReader::with_fs(fs, &root);
        assert_eq!(reader.discover(), Health::Ok);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies[0].capacity_pct, Some(74));
        assert_eq!(part.supplies[0].status, Some(SupplyStatus::Discharging));
        assert_eq!(part.supplies[0].cycle_count, None);
        assert!(reader.held_fds() > 0);
        reader.close();
        assert_eq!(reader.held_fds(), 0);
    }

    #[test]
    fn empty_directory_is_absent_and_usb_rows_keep_active_type() {
        let root = PathBuf::from("/fake/power");
        let mut empty = SupplyReader::with_fs(FakeFs::default(), &root);
        assert_eq!(empty.discover(), Health::Absent(Absent::NoHardware));
        let mut fs = FakeFs::default();
        fake_supply(
            &mut fs,
            &root,
            "AC",
            &[("type", "Mains\n"), ("online", "1\n")],
        );
        fake_supply(
            &mut fs,
            &root,
            "USB0",
            &[
                ("type", "USB\n"),
                ("online", "1\n"),
                ("usb_type", "C [PD] PD_PPS\n"),
            ],
        );
        let mut reader = SupplyReader::with_fs(fs, &root);
        assert_eq!(reader.discover(), Health::Ok);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies.len(), 2);
        assert_eq!(part.supplies[0].online, Some(true));
        assert_eq!(part.supplies[1].online, Some(true));
        assert_eq!(
            part.supplies[1].usb_type_active,
            Some(crate::collect::model::power_supply::UsbType::Pd)
        );
    }

    #[test]
    fn collector_reports_no_hardware_then_publishes_ac_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut collector = PowerSupplyCollector::at(dir.path());
        assert_eq!(
            collector.discover(&Env::default()),
            Health::Absent(Absent::NoHardware)
        );
        let mains = dir.path().join("ACAD");
        std::fs::create_dir(&mains).unwrap();
        std::fs::write(mains.join("type"), "Mains\n").unwrap();
        std::fs::write(mains.join("online"), "1\n").unwrap();
        assert_eq!(
            collector.discover(&Env::default()),
            Health::Absent(Absent::NoHardware)
        );
        let mut frame = MonitorFrame::default();
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        assert_eq!(frame.parts.power_supply.supplies.len(), 1);
        assert_eq!(frame.parts.power_supply.supplies[0].kind, SupplyKind::Mains);
        assert_eq!(frame.parts.power_supply.supplies[0].online, Some(true));
    }

    #[test]
    fn collector_steady_sample_does_not_list_or_open() {
        let root = PathBuf::from("/fake/power");
        let mut fs = FakeFs::default();
        fake_supply(
            &mut fs,
            &root,
            "BAT0",
            &[
                ("type", "Battery\n"),
                ("status", "Discharging\n"),
                ("capacity", "61\n"),
                ("energy_now", "1000\n"),
                ("power_now", "500\n"),
            ],
        );
        let mut collector = PowerSupplyCollector::with_fs(fs, &root);
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        collector.reader.fs.lists = 0;
        collector.reader.fs.opens = 0;
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 500,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(collector.reader.fs.lists, 0);
        assert_eq!(collector.reader.fs.opens, 0);
    }

    #[test]
    fn collector_rediscovery_uses_overshot_sixty_second_clock_deadline() {
        let root = PathBuf::from("/fake/power");
        let mut fs = FakeFs::default();
        fake_supply(
            &mut fs,
            &root,
            "BAT0",
            &[("type", "Battery\n"), ("status", "Discharging\n")],
        );
        let mut collector = PowerSupplyCollector::with_fs(fs, &root);
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        collector.reader.fail_next_read_for_test();
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 120,
                    now_ms: 60_001,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(collector.reader.fs.lists, 2);
    }

    #[test]
    fn unknown_or_missing_type_entries_are_absent_and_publish_no_rows() {
        for attrs in [vec![("type", "Unknown\n"), ("online", "1\n")], vec![]] {
            let root = PathBuf::from("/fake/power");
            let mut fs = FakeFs::default();
            fake_supply(&mut fs, &root, "OTHER0", &attrs);
            let mut collector = PowerSupplyCollector::with_fs(fs, &root);
            assert_eq!(
                collector.discover(&Env::default()),
                Health::Absent(Absent::NoHardware)
            );
            let mut frame = MonitorFrame::default();
            collector
                .sample(&TickCtx { tick: 1, now_ms: 1 }, &mut frame)
                .unwrap();
            assert!(frame.parts.power_supply.supplies.is_empty());
        }
    }

    #[test]
    fn device_battery_and_noncritical_errno_errors_degrade_fields() {
        let root = PathBuf::from("/fake/power");
        let mut fs = FakeFs::default();
        fake_supply(
            &mut fs,
            &root,
            "BAT-HID",
            &[
                ("type", "Battery\n"),
                ("scope", "Device\n"),
                ("status", "Discharging\n"),
                ("capacity", "44\n"),
            ],
        );
        fake_supply(
            &mut fs,
            &root,
            "BAT0",
            &[
                ("type", "Battery\n"),
                ("status", "Discharging\n"),
                ("capacity", "82\n"),
            ],
        );
        fs.failure_codes
            .insert(root.join("BAT-HID/status"), libc::EIO);
        fs.failure_codes
            .insert(root.join("BAT-HID/capacity"), libc::ENODATA);
        fs.failure_codes.insert(root.join("BAT0/status"), libc::EIO);
        let mut reader = SupplyReader::with_fs(fs, &root);
        assert_eq!(reader.discover(), Health::Ok);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies.len(), 2);
        assert_eq!(part.supplies[0].status, None);
        assert_eq!(part.supplies[0].capacity_pct, None);
        assert_eq!(part.supplies[1].status, None);
        assert_eq!(part.supplies[1].capacity_pct, Some(82));
        let mut rows = Vec::new();
        reader.read_base(&mut rows, false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].capacity, Some(82));
        assert!(matches!(
            reader.read_base(&mut rows, true),
            Err(CollectError::Gone)
        ));
    }

    #[test]
    fn other_online_supply_types_are_kept_and_read_as_adapters() {
        let root = PathBuf::from("/fake/power");
        let mut fs = FakeFs::default();
        fake_supply(
            &mut fs,
            &root,
            "BAT0",
            &[
                ("type", "Battery\n"),
                ("status", "Discharging\n"),
                ("capacity", "80\n"),
                ("energy_now", "500\n"),
                ("power_now", "5\n"),
            ],
        );
        fake_supply(
            &mut fs,
            &root,
            "USB-PD",
            &[("type", "USB_PD_DRP\n"), ("online", "1\n")],
        );
        let mut reader = SupplyReader::with_fs(fs, &root);
        assert_eq!(reader.discover(), Health::Ok);
        let mut rows = Vec::new();
        reader.read_base(&mut rows, true).unwrap();
        assert!(rows[0].on_ac);
        assert_eq!(reader.counters().adapter_reads, 1);
        assert_eq!(reader.counters().reads, 5);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies[1].kind, SupplyKind::Usb);
    }

    #[test]
    fn battery_is_kept_ahead_of_usb_rows_when_supply_cap_is_reached() {
        let root = PathBuf::from("/fake/power");
        let mut fs = FakeFs::default();
        for index in 0..17 {
            let name = format!("USB{index}");
            let dir = root.join(name);
            fs.dirs.push(dir.clone());
            fs.files.insert(dir.join("type"), b"USB_DCP\n".to_vec());
            fs.files.insert(dir.join("online"), b"0\n".to_vec());
        }
        fake_supply(
            &mut fs,
            &root,
            "BAT0",
            &[("type", "Battery\n"), ("capacity", "70\n")],
        );
        let mut reader = SupplyReader::with_fs(fs, &root);
        assert_eq!(reader.discover(), Health::Ok);
        let mut part = PowerSupplyPart::default();
        reader.read_full(&mut part).unwrap();
        assert_eq!(part.supplies.len(), MAX_SUPPLIES);
        assert_eq!(part.supplies[0].name.as_ref(), "BAT0");
    }
}
