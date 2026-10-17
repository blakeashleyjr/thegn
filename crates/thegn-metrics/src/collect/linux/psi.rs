//! Linux PSI collector over held `/proc/pressure` files.
use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::psi::PsiResource,
    parse::psi,
    schedule::Cadence,
};
use std::path::{Path, PathBuf};

pub struct PsiCollector {
    root: PathBuf,
    files: [Option<ProcFile>; 4],
    previous: [Option<(u64, Option<u64>, Option<u64>)>; 4],
}
impl PsiCollector {
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            root: path.as_ref().to_owned(),
            files: [None, None, None, None],
            previous: [None; 4],
        }
    }
    fn names() -> [&'static str; 4] {
        ["cpu", "memory", "io", "irq"]
    }
    fn resource(&mut self, index: usize, now_ms: u64) -> Result<PsiResource, CollectError> {
        let Some(file) = self.files[index].as_mut() else {
            return Ok(PsiResource::default());
        };
        let bytes = match file.read_all().map(<[u8]>::to_vec) {
            Ok(bytes) => bytes,
            Err(_error) if file.needs_reopen() => return Err(CollectError::Gone),
            Err(error) => return Err(error.into()),
        };
        let parsed = psi::parse(&bytes);
        if parsed.some.is_none() && parsed.full.is_none() {
            return Err(CollectError::Parse("pressure"));
        }
        let (some_stall_centi, full_stall_centi) = match self.previous[index] {
            Some((previous_now, previous_some, previous_full)) => {
                let elapsed = now_ms.saturating_sub(previous_now);
                (
                    parsed.some.zip(previous_some).and_then(|(line, previous)| {
                        psi::stall_pct_centi(previous, line.total_us, elapsed)
                    }),
                    parsed.full.zip(previous_full).and_then(|(line, previous)| {
                        psi::stall_pct_centi(previous, line.total_us, elapsed)
                    }),
                )
            }
            None => (None, None),
        };
        self.previous[index] = Some((
            now_ms,
            parsed.some.map(|line| line.total_us),
            parsed.full.map(|line| line.total_us),
        ));
        Ok(PsiResource {
            present: true,
            file: parsed,
            some_stall_centi,
            full_stall_centi,
        })
    }
}
impl Collector for PsiCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Psi
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(2)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        for (index, name) in Self::names().into_iter().enumerate() {
            if let Ok(file) = ProcFile::open(self.root.join(name)) {
                self.files[index] = Some(file);
            }
        }
        if self.files.iter().any(Option::is_some) {
            Health::Ok
        } else {
            Health::Absent(Absent::NoHardware)
        }
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let cpu = self.resource(0, ctx.now_ms)?;
        let memory = self.resource(1, ctx.now_ms)?;
        let io = self.resource(2, ctx.now_ms)?;
        let irq = self.resource(3, ctx.now_ms)?;
        frame.parts.psi = crate::collect::model::psi::PsiPart {
            cpu,
            memory,
            io,
            irq,
        };
        frame.sampled.insert(CollectorId::Psi);
        Ok(())
    }
    fn close(&mut self) {
        self.files = [None, None, None, None];
        self.previous = [None; 4];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn partial_directory_is_ok_and_missing_resources_are_not_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cpu"),
            b"some avg10=1 avg60=2 avg300=3 total=100\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("io"),
            b"full total=40 avg300=0 avg10=0 avg60=0\n",
        )
        .unwrap();
        let mut collector = PsiCollector::at(dir.path());
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 0,
                    now_ms: 10,
                },
                &mut frame,
            )
            .unwrap();
        assert!(frame.parts.psi.cpu.present);
        assert!(!frame.parts.psi.memory.present);
    }
    #[test]
    fn empty_directory_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut collector = PsiCollector::at(dir.path());
        assert_eq!(
            collector.discover(&Env::default()),
            Health::Absent(Absent::NoHardware)
        );
    }

    #[test]
    fn collector_reports_some_stall_after_two_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cpu");
        fs::write(&path, b"some avg10=0 avg60=0 avg300=0 total=100\n").unwrap();
        let mut collector = PsiCollector::at(dir.path());
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
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
        assert_eq!(frame.parts.psi.cpu.some_stall_centi, None);

        fs::write(&path, b"some avg10=0 avg60=0 avg300=0 total=50100\n").unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(frame.parts.psi.cpu.some_stall_centi, Some(500));
    }

    #[test]
    fn garbage_pressure_file_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cpu"), b"not pressure data\n").unwrap();
        let mut collector = PsiCollector::at(dir.path());
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        assert!(matches!(
            collector.sample(&TickCtx::default(), &mut MonitorFrame::default()),
            Err(CollectError::Parse("pressure"))
        ));
    }
}
