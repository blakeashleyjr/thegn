//! Linux base-tier memory and swap from one held `/proc/meminfo` reader.

use crate::collect::{
    linux::procfile::ProcFile,
    parse::meminfo::{self, MemField, MeminfoRaw},
};

// Keep in sync with sample.rs::SLOW_EVERY so failed opens retry once per slow tier.
const SLOW_EVERY: u8 = 5;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemReading {
    pub(crate) mem: Option<(u64, u64)>,
    pub(crate) free: Option<u64>,
    pub(crate) swap: Option<(u64, u64)>,
}

/// Interpret one parsed `/proc/meminfo` snapshot with sysinfo 0.39.6's Linux
/// memory semantics.
fn reading(raw: &MeminfoRaw) -> MemReading {
    let available = raw.get(MemField::MemAvailable).unwrap_or_else(|| {
        raw.get(MemField::MemFree)
            .unwrap_or(0)
            .saturating_add(raw.get(MemField::Buffers).unwrap_or(0))
            .saturating_add(raw.get(MemField::Cached).unwrap_or(0))
            .saturating_add(raw.get(MemField::SReclaimable).unwrap_or(0))
            .saturating_sub(raw.get(MemField::Shmem).unwrap_or(0))
    });
    let mem = raw
        .get(MemField::MemTotal)
        .map(|total| (total.saturating_sub(available), total));
    let free = raw.get(MemField::MemFree);
    let swap = raw
        .get(MemField::SwapTotal)
        .zip(raw.get(MemField::SwapFree))
        .map(|(total, free)| (total.saturating_sub(free), total));
    MemReading { mem, free, swap }
}

pub(crate) struct BaseMem {
    path: std::path::PathBuf,
    file: Option<ProcFile>,
    raw: MeminfoRaw,
    retry_in: u8,
    #[cfg(test)]
    reads: usize,
}

pub(crate) fn nonzero_total(reading: Option<(u64, u64)>) -> Option<(u64, u64)> {
    reading.filter(|(_, total)| *total > 0)
}

impl BaseMem {
    pub(crate) fn new() -> Self {
        Self::at("/proc/meminfo")
    }

    fn at(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: None,
            raw: MeminfoRaw::default(),
            retry_in: 0,
            #[cfg(test)]
            reads: 0,
        }
    }

    /// Read and parse once. Failed opens or reads stay absent and are retried
    /// at most once per slow-tier period.
    pub(crate) fn fill(&mut self) -> MemReading {
        if self.file.is_none() {
            if self.retry_in > 0 {
                self.retry_in -= 1;
                return MemReading::default();
            }
            match ProcFile::open(&self.path) {
                Ok(file) => self.file = Some(file),
                Err(_) => {
                    self.retry_in = SLOW_EVERY - 1;
                    return MemReading::default();
                }
            }
        }

        let result = match self.file.as_mut() {
            Some(file) => {
                #[cfg(test)]
                {
                    self.reads += 1;
                }
                match file.read_all() {
                    Ok(bytes) => {
                        meminfo::parse(bytes, &mut self.raw);
                        true
                    }
                    Err(_) => false,
                }
            }
            None => return MemReading::default(),
        };
        if !result {
            self.file = None;
            self.raw = MeminfoRaw::default();
            self.retry_in = SLOW_EVERY - 1;
            return MemReading::default();
        }
        reading(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::{BaseMem, MemReading, nonzero_total, reading};
    use crate::collect::parse::meminfo::{MemField, MeminfoRaw, parse};
    use std::fs;

    const CAPTURED_MEMINFO: &[u8] = b"MemTotal:       98134716 kB\n\
MemFree:        15408500 kB\n\
MemAvailable:   46764780 kB\n\
Buffers:              12 kB\n\
Cached:         27946164 kB\n\
Shmem:            206940 kB\n\
SwapTotal:      70038520 kB\n\
SwapFree:       30823012 kB\n";

    fn bytes(kib: u64) -> u64 {
        kib * 1_024
    }

    #[test]
    fn captured_meminfo_matches_exact_sysinfo_memory_and_swap_semantics() {
        let mut raw = MeminfoRaw::default();
        parse(CAPTURED_MEMINFO, &mut raw);
        let result = reading(&raw);
        let total_kib = 98_134_716;
        let available_kib = 46_764_780;
        let free_kib = 15_408_500;
        let swap_total_kib = 70_038_520;
        let swap_free_kib = 30_823_012;
        assert_eq!(
            result,
            MemReading {
                mem: Some((bytes(total_kib - available_kib), bytes(total_kib))),
                free: Some(bytes(free_kib)),
                swap: Some((bytes(swap_total_kib - swap_free_kib), bytes(swap_total_kib))),
            }
        );
    }

    #[test]
    fn missing_available_uses_sysinfo_fallback_and_swap_stays_absent_or_zero() {
        let mut raw = MeminfoRaw::default();
        parse(
            b"MemTotal: 100 kB\nMemFree: 20 kB\nBuffers: 5 kB\nCached: 30 kB\nSReclaimable: 10 kB\nShmem: 4 kB\n",
            &mut raw,
        );
        assert_eq!(
            reading(&raw),
            MemReading {
                mem: Some((bytes(39), bytes(100))),
                free: Some(bytes(20)),
                swap: None,
            }
        );

        parse(
            b"MemTotal: 100 kB\nMemAvailable: 50 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
            &mut raw,
        );
        let result = reading(&raw);
        assert_eq!(result.swap, Some((0, 0)));
        assert_eq!(nonzero_total(result.swap), None);
        assert_eq!(nonzero_total(Some((1, 2))), Some((1, 2)));
        assert_eq!(raw.get(MemField::SwapTotal), Some(0));
    }

    #[test]
    fn fill_reuses_held_file_and_retries_failed_open_at_slow_cadence() {
        let dir = tempfile::tempdir().expect("private fixture directory");
        let path = dir.path().join("meminfo");
        let mut reader = BaseMem::at(&path);
        assert_eq!(reader.fill(), MemReading::default());
        fs::write(&path, CAPTURED_MEMINFO).unwrap();
        for _ in 0..4 {
            assert_eq!(reader.fill(), MemReading::default());
        }
        assert_eq!(reader.fill().mem, reading_from_capture().mem);
        assert_eq!(reader.reads, 1);

        let moved = dir.path().join("held-meminfo");
        fs::rename(&path, moved).unwrap();
        fs::write(
            &path,
            b"MemTotal: 1 kB\nMemAvailable: 0 kB\nMemFree: 1 kB\n",
        )
        .unwrap();
        assert_eq!(reader.fill().mem, reading_from_capture().mem);
        assert_eq!(reader.reads, 2);
    }

    fn reading_from_capture() -> MemReading {
        let mut raw = MeminfoRaw::default();
        parse(CAPTURED_MEMINFO, &mut raw);
        reading(&raw)
    }
}
