//! Linux `/proc/schedstat` collector.
use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    delta::Counter,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::cpu_sched::CoreSchedSample,
    parse::schedstat::{self, CpuSchedRaw, ParseError, SchedstatTable},
    schedule::Cadence,
};
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct CpuSchedCollector {
    path: PathBuf,
    file: Option<ProcFile>,
    raw: SchedstatTable,
    previous: [Option<CpuSchedRaw>; schedstat::MAX_CPUS],
    previous_ms: Option<u64>,
}
impl CpuSchedCollector {
    pub fn new() -> Self {
        Self::at("/proc/schedstat")
    }
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: None,
            raw: SchedstatTable::default(),
            previous: [None; schedstat::MAX_CPUS],
            previous_ms: None,
        }
    }
    fn read(&mut self) -> Result<(), CollectError> {
        let file = self.file.as_mut().ok_or(CollectError::Gone)?;
        match file.read_all() {
            Ok(bytes) => match schedstat::parse(bytes, &mut self.raw) {
                Ok(()) => Ok(()),
                Err(ParseError::Unsupported) => Err(CollectError::Parse("/proc/schedstat version")),
                Err(ParseError::Invalid) => Err(CollectError::Parse("/proc/schedstat")),
            },
            Err(_error) if file.needs_reopen() => Err(CollectError::Gone),
            Err(error) => Err(error.into()),
        }
    }
}
impl Default for CpuSchedCollector {
    fn default() -> Self {
        Self::new()
    }
}
impl Collector for CpuSchedCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CpuSched
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        match ProcFile::open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                match self.read() {
                    Ok(()) => {
                        self.previous = self.raw.cpus;
                        self.previous_ms = None;
                        Health::Warming
                    }
                    Err(CollectError::Parse("/proc/schedstat version")) => {
                        self.file = None;
                        Health::Absent(Absent::NotOnThisOs)
                    }
                    Err(_) => {
                        self.file = None;
                        Health::Warming.after_error(0)
                    }
                }
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
        self.read()?;
        let part = &mut frame.parts.cpu_sched;
        part.cores.clear();
        part.version = Some(self.raw.version);
        for id in 0..self.raw.cpu_count {
            let current = self.raw.cpus[id];
            let old = self.previous[id];
            let delay = current
                .zip(old)
                .and_then(|(new, old)| new.run_delay_ns.checked_sub(old.run_delay_ns));
            let slices = current
                .zip(old)
                .and_then(|(new, old)| new.timeslices.checked_sub(old.timeslices));
            let rate = current
                .zip(old)
                .and_then(|(new, old)| {
                    counter_rate(
                        new.run_delay_ns,
                        old.run_delay_ns,
                        self.previous_ms,
                        ctx.now_ms,
                    )
                })
                .map(|ns_per_s| ns_per_s / 1_000_000.0);
            let average = delay
                .zip(slices)
                .filter(|(_, slices)| *slices > 0)
                .map(|(ns, slices)| ns as f64 / slices as f64 / 1_000_000.0);
            part.cores.push(CoreSchedSample {
                id: id as u16,
                online: current.is_some(),
                run_queue_wait_ms_per_s: rate,
                avg_wait_ms_per_timeslice: average,
            });
        }
        self.previous = self.raw.cpus;
        self.previous_ms = Some(ctx.now_ms);
        frame.sampled.insert(CollectorId::CpuSched);
        Ok(())
    }
    fn close(&mut self) {
        self.file = None;
        self.previous = [None; schedstat::MAX_CPUS];
        self.previous_ms = None;
    }
}

fn counter_rate(current: u64, previous: u64, previous_ms: Option<u64>, now_ms: u64) -> Option<f64> {
    let mut counter = Counter::new();
    match counter.rate(previous, previous_ms?) {
        None => {}
        Some(_) => return None,
    }
    counter.rate(current, now_ms)
}

#[cfg(test)]
mod tests {
    use super::CpuSchedCollector;
    use crate::collect::{
        Collector, TickCtx,
        env::Env,
        frame::MonitorFrame,
        health::{Absent, Health},
    };
    use std::fs;
    #[test]
    fn unknown_version_is_absent_and_rate_uses_counter_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedstat");
        fs::write(&path, b"version 99\n").unwrap();
        let mut collector = CpuSchedCollector::at(&path);
        assert_eq!(
            collector.discover(&Env::default()),
            Health::Absent(Absent::NotOnThisOs)
        );
        fs::write(
            &path,
            b"version 17\ncpu0 0 0 0 0 0 0 10000000 20000000 10\n",
        )
        .unwrap();
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        fs::write(
            &path,
            b"version 17\ncpu0 0 0 0 0 0 0 30000000 40000000 20\n",
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 5000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(frame.parts.cpu_sched.cores[0].run_queue_wait_ms_per_s, None);
        fs::write(
            &path,
            b"version 17\ncpu0 0 0 0 0 0 0 50000000 60000000 30\n",
        )
        .unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 10001,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame.parts.cpu_sched.cores[0].run_queue_wait_ms_per_s,
            Some(3.9992001599680064)
        );
    }
}
