//! Held-fd Linux collector for `/proc/stat`.

use std::io;
use std::path::{Path, PathBuf};

use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::cpu_stat::CpuStatDelta,
    parse::stat::{StatRaw, parse_stat},
    schedule::Cadence,
};

#[derive(Debug)]
pub struct CpuStatCollector {
    path: PathBuf,
    file: Option<ProcFile>,
    raw: StatRaw,
    delta: CpuStatDelta,
}

impl CpuStatCollector {
    pub fn new() -> Self {
        Self::with_path("/proc/stat")
    }

    pub fn with_path(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: None,
            raw: StatRaw::default(),
            delta: CpuStatDelta::new(),
        }
    }
}

impl Default for CpuStatCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for CpuStatCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CpuStat
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, _env: &Env) -> Health {
        if self.file.is_some() {
            return Health::Warming;
        }
        match ProcFile::open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                Health::Warming
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Health::Absent(Absent::NotOnThisOs)
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Health::Absent(Absent::NoPermission)
            }
            Err(_) => Health::Warming.after_error(0),
        }
    }

    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let Some(file) = self.file.as_mut() else {
            return Err(CollectError::Gone);
        };
        let read_result = match file.read_all() {
            Ok(bytes) => {
                parse_stat(bytes, &mut self.raw);
                Ok(())
            }
            Err(error) => Err((error, file.needs_reopen())),
        };
        if let Err((error, reopen)) = read_result {
            if reopen {
                self.file = None;
            }
            return Err(error.into());
        }
        if self.raw.total.is_none() {
            return Err(CollectError::Parse("stat: no cpu line"));
        }
        self.delta
            .update(&self.raw, ctx.now_ms, &mut frame.parts.cpu_stat);
        frame.sampled.insert(CollectorId::CpuStat);
        Ok(())
    }

    fn close(&mut self) {
        self.file = None;
    }
}

#[cfg(test)]
mod tests {
    use super::CpuStatCollector;
    use crate::collect::{
        CollectError, Collector, TickCtx,
        env::Env,
        frame::MonitorFrame,
        health::{Absent, Health},
    };
    use std::fs::{self, File};
    use std::os::unix::fs::FileExt;
    use std::time::Duration;

    fn sample(collector: &mut CpuStatCollector, now_ms: u64) -> Result<(), CollectError> {
        collector.sample(
            &TickCtx {
                tick: now_ms,
                now_ms,
            },
            &mut MonitorFrame::default(),
        )
    }

    #[test]
    fn held_fd_reads_rewritten_file_and_marks_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stat");
        fs::write(&path, b"cpu 10 0 0 10\ncpu0 10 0 0 10\n").unwrap();
        let mut collector = CpuStatCollector::with_path(&path);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 0,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        assert!(frame.sampled.contains(crate::collect::CollectorId::CpuStat));
        let writer = File::options().write(true).open(&path).unwrap();
        writer.set_len(0).unwrap();
        writer
            .write_at(b"cpu 20 0 0 20\ncpu0 20 0 0 20\n", 0)
            .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame.parts.cpu_stat.cores[0].share.map(|share| share.user),
            Some(500)
        );
    }

    #[test]
    fn missing_path_and_missing_cpu_line_have_distinct_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut missing = CpuStatCollector::with_path(dir.path().join("missing"));
        assert_eq!(
            missing.discover(&Env::default()),
            Health::Absent(Absent::NotOnThisOs)
        );

        let path = dir.path().join("no-cpu");
        fs::write(&path, b"ctxt 12\n").unwrap();
        let mut malformed = CpuStatCollector::with_path(path);
        assert_eq!(malformed.discover(&Env::default()), Health::Warming);
        assert!(matches!(
            sample(&mut malformed, 1000),
            Err(CollectError::Parse("stat: no cpu line"))
        ));
    }

    #[test]
    fn live_proc_stat_cpu_mode_shares_sum_to_one_thousand() {
        let mut collector = CpuStatCollector::new();
        if collector.discover(&Env::default()) != Health::Warming {
            return;
        }
        let mut frame = MonitorFrame::default();
        if collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .is_err()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(110));
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 1110,
                },
                &mut frame,
            )
            .unwrap();
        for core in frame.parts.cpu_stat.cores.iter().filter(|core| core.online) {
            let Some(share) = core.share else { continue };
            let sum = i32::from(share.user)
                + i32::from(share.nice)
                + i32::from(share.system)
                + i32::from(share.iowait)
                + i32::from(share.irq)
                + i32::from(share.softirq)
                + i32::from(share.steal)
                + i32::from(share.guest)
                + i32::from(share.idle);
            assert!(
                (991..=1009).contains(&sum),
                "cpu{} share sum was {sum}",
                core.id
            );
        }
    }
}
