//! Base-tier Linux CPU usage from the shared `/proc/stat` parser.

use std::io;
use std::path::{Path, PathBuf};

use crate::collect::linux::procfile::ProcFile;
use crate::collect::parse::stat::{CpuTicks, StatRaw, parse_stat};

const REOPEN_EVERY: u64 = 5;

#[derive(Debug)]
pub(crate) struct BaseCpu {
    path: PathBuf,
    file: Option<ProcFile>,
    raw: StatRaw,
    previous_global: Option<CpuTicks>,
    previous_cpus: Vec<Option<CpuTicks>>,
    tick: u64,
    #[cfg(test)]
    read_count: u64,
    #[cfg(test)]
    open_count: u64,
}

impl BaseCpu {
    pub(crate) fn new() -> Self {
        Self::with_path("/proc/stat")
    }

    fn with_path(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: None,
            raw: StatRaw::default(),
            previous_global: None,
            previous_cpus: Vec::new(),
            tick: 0,
            #[cfg(test)]
            read_count: 0,
            #[cfg(test)]
            open_count: 0,
        }
    }

    pub(crate) fn fill(&mut self, primed: &mut bool) -> (Option<u8>, Vec<u8>) {
        let tick = self.tick;
        self.tick = self.tick.saturating_add(1);
        if self.file.is_none() && tick.is_multiple_of(REOPEN_EVERY) {
            self.file = self.open_file();
        }
        let read = self.read_once();
        let bytes = match read {
            Some(Ok(bytes)) => Some(bytes),
            Some(Err(_error)) => {
                if self.file.as_ref().is_some_and(ProcFile::needs_reopen) {
                    self.file = None;
                }
                None
            }
            None => None,
        };
        let Some(bytes) = bytes else {
            *primed = true;
            return (None, Vec::new());
        };
        self.fill_bytes(primed, &bytes)
    }

    fn open_file(&mut self) -> Option<ProcFile> {
        #[cfg(test)]
        {
            self.open_count = self.open_count.saturating_add(1);
        }
        match ProcFile::open(&self.path) {
            Ok(file) => Some(file),
            Err(_error) => None,
        }
    }

    fn read_once(&mut self) -> Option<io::Result<Vec<u8>>> {
        let file = self.file.as_mut()?;
        #[cfg(test)]
        {
            self.read_count = self.read_count.saturating_add(1);
        }
        Some(file.read_all().map(|bytes| bytes.to_vec()))
    }

    fn fill_bytes(&mut self, primed: &mut bool, bytes: &[u8]) -> (Option<u8>, Vec<u8>) {
        parse_stat(bytes, &mut self.raw);
        let Some(global) = self.raw.total else {
            *primed = true;
            return (None, Vec::new());
        };

        let Some(previous_global) = self.previous_global.replace(global) else {
            self.update_previous_cpus();
            *primed = true;
            return (None, Vec::new());
        };

        let global_pct = usage(previous_global, global).map(pct_u8);
        let max_id = self.raw.cpus.iter().map(|(id, _)| usize::from(*id)).max();
        let Some(max_id) = max_id else {
            self.update_previous_cpus();
            *primed = true;
            return (global_pct, Vec::new());
        };
        let mut cores = vec![0; max_id + 1];
        for (id, current) in &self.raw.cpus {
            let index = usize::from(*id);
            if let Some(previous) = self.previous_cpus.get(index).copied().flatten()
                && let Some(value) = usage(previous, *current)
            {
                cores[index] = pct_u8(value);
            }
        }
        self.update_previous_cpus();
        *primed = true;
        (global_pct, cores)
    }

    fn update_previous_cpus(&mut self) {
        if let Some((last_id, _)) = self.raw.cpus.last() {
            self.previous_cpus.resize(usize::from(*last_id) + 1, None);
        }
        for (id, ticks) in &self.raw.cpus {
            let index = usize::from(*id);
            if self.previous_cpus.len() <= index {
                self.previous_cpus.resize(index + 1, None);
            }
            self.previous_cpus[index] = Some(*ticks);
        }
    }
}

fn usage(previous: CpuTicks, current: CpuTicks) -> Option<f32> {
    let previous = previous.corrected();
    let current = current.corrected();
    let delta = current.checked_delta(previous)?;
    let virtual_delta = current
        .guest
        .saturating_add(current.guest_nice)
        .saturating_sub(previous.guest.saturating_add(previous.guest_nice));
    let current_total = current
        .user
        .saturating_add(current.nice)
        .saturating_add(current.system)
        .saturating_add(current.irq)
        .saturating_add(current.softirq)
        .saturating_add(current.idle)
        .saturating_add(current.iowait)
        .saturating_add(current.steal)
        .saturating_add(current.guest)
        .saturating_add(current.guest_nice);
    let previous_total = previous
        .user
        .saturating_add(previous.nice)
        .saturating_add(previous.system)
        .saturating_add(previous.irq)
        .saturating_add(previous.softirq)
        .saturating_add(previous.idle)
        .saturating_add(previous.iowait)
        .saturating_add(previous.steal)
        .saturating_add(previous.guest)
        .saturating_add(previous.guest_nice);
    let total = current_total.saturating_sub(previous_total).max(1) as f32;
    let nice = delta.nice as f32 / total;
    let user = delta.user as f32 / total;
    let system = delta
        .system
        .saturating_add(delta.irq)
        .saturating_add(delta.softirq) as f32
        / total;
    let steal_and_guest = delta.steal.saturating_add(virtual_delta) as f32 / total;
    Some(((nice + user + system + steal_and_guest) * 100.0).min(100.0))
}

fn pct_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

#[cfg(test)]
#[path = "base_cpu_tests.rs"]
mod tests;
