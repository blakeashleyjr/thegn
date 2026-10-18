//! AMD GPU engine and memory readings from held sysfs descriptors.
//!
//! Sampling performs at most 5 preads per card on ordinary ticks and 7 on
//! DPM ticks. With `DEVICE_CAP` cards, that is at most 40 and 56 preads per
//! tick respectively. DPM reads follow the five-tick cadence; the collector
//! has no CLOCKS panel visibility mask yet.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::collect::model::gpu::RuntimeStatus;
use crate::collect::model::gpu_engines::{DpmLevels, GpuEngineRow, GpuSlot};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    parse::pp_dpm,
    schedule::Cadence,
};

const DEVICE_CAP: usize = 8;
const VALUE_CAP: usize = 1024;

pub trait GpuAmdFs: Send {
    type Handle: Send;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize>;
    fn open(&mut self, path: &Path) -> io::Result<Self::Handle>;
    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize>;
}

pub struct RealFs;
impl GpuAmdFs for RealFs {
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

struct Card<H> {
    runtime: H,
    gfx_busy: Option<H>,
    vcn_busy: Option<H>,
    vram_used: Option<H>,
    gtt_used: Option<H>,
    sclk: Option<H>,
    mclk: Option<H>,
    row: GpuEngineRow,
}

pub struct GpuAmdCollector<F: GpuAmdFs = RealFs> {
    fs: F,
    drm_root: PathBuf,
    cards: Vec<Card<F::Handle>>,
    discovered: bool,
    truncated: bool,
}

impl GpuAmdCollector<RealFs> {
    pub fn new() -> Self {
        Self::at("/sys/class/drm")
    }
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::with_fs(RealFs, root)
    }
}

impl<F: GpuAmdFs> GpuAmdCollector<F> {
    pub fn with_fs(fs: F, root: impl Into<PathBuf>) -> Self {
        Self {
            fs,
            drm_root: root.into(),
            cards: Vec::with_capacity(DEVICE_CAP),
            discovered: false,
            truncated: false,
        }
    }

    fn read_small(&mut self, path: &Path) -> Option<Vec<u8>> {
        let mut bytes = [0_u8; VALUE_CAP];
        let length = self.fs.read_small(path, &mut bytes).ok()?.min(bytes.len());
        Some(bytes[..length].to_vec())
    }

    fn discover_cards(&mut self) -> bool {
        self.cards.clear();
        self.truncated = false;
        let Ok(entries) = self.fs.list(&self.drm_root) else {
            return false;
        };
        let mut candidates = entries
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
        candidates.sort_by_key(|(index, _)| *index);
        for (_, card_path) in candidates {
            let device = card_path.join("device");
            let Some(uevent) = self.read_small(&device.join("uevent")) else {
                continue;
            };
            let Some(slot) = text_value(&uevent, b"PCI_SLOT_NAME") else {
                continue;
            };
            let Some(gpu_slot) = GpuSlot::new(slot) else {
                continue;
            };
            if text_value(&uevent, b"DRIVER") != Some("amdgpu")
                || self.cards.iter().any(|card| card.row.slot.as_str() == slot)
            {
                continue;
            }
            if self.cards.len() == DEVICE_CAP {
                self.truncated = true;
                continue;
            }
            let runtime = match self.fs.open(&device.join("power/runtime_status")) {
                Ok(handle) => handle,
                Err(_) => continue,
            };
            let open = |fs: &mut F, name: &str| fs.open(&device.join(name)).ok();
            let vram_total = self.read_number(&device.join("mem_info_vram_total"));
            let gtt_total = self.read_number(&device.join("mem_info_gtt_total"));
            self.cards.push(Card {
                runtime,
                gfx_busy: open(&mut self.fs, "gpu_busy_percent"),
                vcn_busy: open(&mut self.fs, "vcn_busy_percent"),
                vram_used: open(&mut self.fs, "mem_info_vram_used"),
                gtt_used: open(&mut self.fs, "mem_info_gtt_used"),
                sclk: open(&mut self.fs, "pp_dpm_sclk"),
                mclk: open(&mut self.fs, "pp_dpm_mclk"),
                row: GpuEngineRow {
                    slot: gpu_slot,
                    vram_total,
                    gtt_total,
                    ..GpuEngineRow::default()
                },
            });
        }
        !self.cards.is_empty()
    }

    fn read_number(&mut self, path: &Path) -> Option<u64> {
        parse_u64(&self.read_small(path)?)
    }

    fn update_number(&mut self, handle: Option<&F::Handle>, output: &mut Option<u64>) -> bool {
        let Some(handle) = handle else { return false };
        let mut bytes = [0_u8; 64];
        match self.fs.pread(handle, &mut bytes) {
            Ok(length) => *output = parse_u64(&bytes[..length.min(bytes.len())]),
            Err(error) if is_gone(&error) => return true,
            Err(_) => {}
        }
        false
    }

    fn update_busy(&mut self, handle: Option<&F::Handle>, output: &mut Option<u8>) -> bool {
        let Some(handle) = handle else { return false };
        let mut bytes = [0_u8; 64];
        match self.fs.pread(handle, &mut bytes) {
            Ok(length) => {
                *output = parse_u64(&bytes[..length.min(bytes.len())])
                    .and_then(|value| u8::try_from(value).ok())
                    .filter(|value| *value <= 100);
            }
            Err(error) if is_gone(&error) => return true,
            Err(_) => {}
        }
        false
    }

    fn update_dpm(&mut self, handle: Option<&F::Handle>, output: &mut DpmLevels) -> bool {
        let Some(handle) = handle else { return false };
        let mut bytes = [0_u8; VALUE_CAP];
        match self.fs.pread(handle, &mut bytes) {
            Ok(length) => {
                if let Some(parsed) = pp_dpm::parse(&bytes[..length.min(bytes.len())]) {
                    *output = parsed;
                }
            }
            Err(error) if is_gone(&error) => return true,
            Err(_) => {}
        }
        false
    }

    fn sample_card(&mut self, card: &mut Card<F::Handle>, tick: u64) -> bool {
        let mut status = [0_u8; 64];
        match self.fs.pread(&card.runtime, &mut status) {
            Ok(length) => {
                card.row.runtime_status = match trim(&status[..length.min(status.len())]) {
                    b"active" => RuntimeStatus::Active,
                    b"suspended" => RuntimeStatus::Suspended,
                    b"suspending" => RuntimeStatus::Suspending,
                    b"unsupported" => RuntimeStatus::Unsupported,
                    _ => RuntimeStatus::Unknown,
                };
            }
            Err(error) if is_gone(&error) => return true,
            Err(_) => {
                card.row.runtime_status = RuntimeStatus::Unknown;
                clear_hot_fields(&mut card.row);
                return false;
            }
        }
        if !matches!(
            card.row.runtime_status,
            RuntimeStatus::Active | RuntimeStatus::Unsupported
        ) {
            clear_hot_fields(&mut card.row);
            return false;
        }
        let gone = self.update_busy(card.gfx_busy.as_ref(), &mut card.row.gfx_busy_pct)
            || self.update_busy(card.vcn_busy.as_ref(), &mut card.row.vcn_busy_pct)
            || self.update_number(card.vram_used.as_ref(), &mut card.row.vram_used)
            || self.update_number(card.gtt_used.as_ref(), &mut card.row.gtt_used);
        if gone {
            return true;
        }
        if tick.is_multiple_of(5) {
            return self.update_dpm(card.sclk.as_ref(), &mut card.row.sclk)
                || self.update_dpm(card.mclk.as_ref(), &mut card.row.mclk);
        }
        false
    }
}

impl Default for GpuAmdCollector<RealFs> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: GpuAmdFs> Collector for GpuAmdCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::GpuEngines
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.discovered = true;
        if self.discover_cards() {
            Health::Warming
        } else {
            Health::Absent(Absent::NoHardware)
        }
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if !self.discovered {
            return Err(CollectError::Gone);
        }
        let mut cards = std::mem::take(&mut self.cards);
        let card_count = cards.len();
        cards.retain_mut(|card| !self.sample_card(card, ctx.tick));
        let removed = cards.len() != card_count;
        self.cards = cards;
        if removed && self.cards.is_empty() {
            return Err(CollectError::Gone);
        }
        let output = &mut frame.parts.gpu_engines;
        if output.devices.len() != self.cards.len() {
            output.devices.clear();
            output
                .devices
                .extend(self.cards.iter().map(|card| card.row));
        } else {
            for (published, card) in output.devices.iter_mut().zip(&self.cards) {
                *published = card.row;
            }
        }
        output.truncated = self.truncated;
        frame.sampled.insert(CollectorId::GpuEngines);
        Ok(())
    }
    fn close(&mut self) {
        self.cards.clear();
        self.discovered = false;
    }
}

fn is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ENODEV)
}

fn clear_hot_fields(row: &mut GpuEngineRow) {
    row.gfx_busy_pct = None;
    row.vcn_busy_pct = None;
    row.vram_used = None;
    row.gtt_used = None;
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    let bytes = trim(bytes);
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_u64, |value, byte| {
        if byte.is_ascii_digit() {
            value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))
        } else {
            None
        }
    })
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn text_value<'a>(uevent: &'a [u8], key: &[u8]) -> Option<&'a str> {
    uevent.split(|byte| *byte == b'\n').find_map(|line| {
        let split = line.iter().position(|byte| *byte == b'=')?;
        (trim(&line[..split]) == key)
            .then(|| std::str::from_utf8(trim(&line[split + 1..])).ok())
            .flatten()
    })
}

#[cfg(test)]
#[path = "gpu_amd_tests.rs"]
mod tests;
