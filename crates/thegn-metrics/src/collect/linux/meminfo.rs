//! Held-fd memory and swap collector.

use std::{
    io,
    path::{Path, PathBuf},
};

use super::{procfile::ProcFile, swap_zram::ZramFiles};
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::meminfo::MeminfoPart,
    parse::{
        meminfo::{self, MemField, MeminfoRaw},
        swaps::{self, SwapDev},
    },
    schedule::Cadence,
};

#[derive(Debug)]
pub struct MeminfoCollector {
    meminfo_path: PathBuf,
    swaps_path: PathBuf,
    block_root: PathBuf,
    meminfo: Option<ProcFile>,
    swaps_file: Option<ProcFile>,
    zram_files: ZramFiles,
    raw: MeminfoRaw,
    swaps: Vec<SwapDev>,
    countdown: u8,
}

impl MeminfoCollector {
    pub fn new() -> Self {
        Self::with_roots("/proc/meminfo", "/proc/swaps", "/sys/block")
    }
    pub fn with_roots(
        meminfo: impl AsRef<Path>,
        swaps: impl AsRef<Path>,
        block: impl AsRef<Path>,
    ) -> Self {
        Self {
            meminfo_path: meminfo.as_ref().to_path_buf(),
            swaps_path: swaps.as_ref().to_path_buf(),
            block_root: block.as_ref().to_path_buf(),
            meminfo: None,
            swaps_file: None,
            zram_files: ZramFiles::default(),
            raw: MeminfoRaw::default(),
            swaps: Vec::new(),
            countdown: 0,
        }
    }
    fn open_optional(path: &Path) -> Result<Option<ProcFile>, io::Error> {
        match ProcFile::open(path) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}
impl Default for MeminfoCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for MeminfoCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Meminfo
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        match ProcFile::open(&self.meminfo_path) {
            Ok(file) => self.meminfo = Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Health::Absent(Absent::NotOnThisOs);
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Health::Absent(Absent::NoPermission);
            }
            Err(_) => return Health::Warming.after_error(0),
        }
        self.swaps_file = Self::open_optional(&self.swaps_path).unwrap_or(None);
        self.zram_files.discover(&self.block_root);
        Health::Warming
    }
    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let read_result = match self.meminfo.as_mut() {
            Some(file) => match file.read_all() {
                Ok(bytes) => {
                    meminfo::parse(bytes, &mut self.raw);
                    Ok(())
                }
                Err(error) => Err((error, file.needs_reopen())),
            },
            None => return Err(CollectError::Gone),
        };
        if let Err((error, reopen)) = read_result {
            if reopen {
                self.meminfo = None;
            }
            return Err(error.into());
        }
        if self.raw.get(MemField::MemTotal).is_none() {
            return Err(CollectError::Parse("meminfo: no MemTotal"));
        }
        if self.countdown == 0 {
            if let Some(file) = self.swaps_file.as_mut() {
                let read_result = match file.read_all() {
                    Ok(bytes) => {
                        swaps::parse(bytes, &mut self.swaps);
                        Ok(())
                    }
                    Err(_) => Err(file.needs_reopen()),
                };
                if let Err(reopen) = read_result {
                    if reopen {
                        self.swaps_file = None;
                    }
                    self.swaps.clear();
                }
            } else {
                self.swaps.clear();
            }
            self.zram_files.read_all();
            self.countdown = 5;
        }
        self.countdown = self.countdown.saturating_sub(1);
        let part: &mut MeminfoPart = &mut frame.parts.meminfo;
        part.raw = self.raw;
        part.swaps.clone_from(&self.swaps);
        part.swaps_present = !self.swaps.is_empty();
        self.zram_files.publish(&mut part.zram);
        frame.sampled.insert(CollectorId::Meminfo);
        Ok(())
    }
    fn close(&mut self) {
        self.meminfo = None;
        self.swaps_file = None;
        self.zram_files.clear();
        self.countdown = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::MeminfoCollector;
    use crate::collect::parse::meminfo::MemField;
    use crate::collect::{
        Collector, TickCtx,
        env::Env,
        frame::MonitorFrame,
        health::{Absent, Health},
    };
    use std::fs;
    use std::os::unix::fs::FileExt;

    #[test]
    fn held_meminfo_reads_rewrites_and_absent_swaps_are_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let proc_dir = dir.path().join("proc");
        let block = dir.path().join("block");
        fs::create_dir_all(&proc_dir).unwrap();
        fs::create_dir_all(&block).unwrap();
        fs::create_dir_all(block.join("zram0/mm_stat")).unwrap();
        let path = proc_dir.join("meminfo");
        fs::write(&path, b"MemTotal: 10 kB\nMemAvailable: 4 kB\n").unwrap();
        let mut collector = MeminfoCollector::with_roots(&path, proc_dir.join("swaps"), &block);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        assert!(!frame.parts.meminfo.swaps_present);
        assert!(frame.parts.meminfo.raw.get(MemField::MemTotal).is_some());
        assert_eq!(frame.parts.meminfo.zram[0].mm, None);
        let buffer_ptr = collector
            .meminfo
            .as_mut()
            .unwrap()
            .read_all()
            .unwrap()
            .as_ptr();
        let writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.set_len(0).unwrap();
        writer
            .write_at(b"MemTotal: 20 kB\nMemAvailable: 5 kB\n", 0)
            .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            collector
                .meminfo
                .as_mut()
                .unwrap()
                .read_all()
                .unwrap()
                .as_ptr(),
            buffer_ptr
        );
        assert_eq!(frame.parts.meminfo.used_bytes(), Some(15 * 1024));
        collector.close();
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_err());
    }

    #[test]
    fn missing_meminfo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut collector =
            MeminfoCollector::with_roots(dir.path().join("missing"), dir.path(), dir.path());
        assert_eq!(
            collector.discover(&Env::default()),
            Health::Absent(Absent::NotOnThisOs)
        );
    }
}
