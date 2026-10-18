//! Held-fd Linux `/proc/vmstat` collector.

use std::{
    io,
    path::{Path, PathBuf},
};

use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::vmstat::VmstatDelta,
    parse::vmstat::{self, VmstatRaw},
    schedule::Cadence,
};

#[derive(Debug)]
pub struct VmstatCollector {
    path: PathBuf,
    file: Option<ProcFile>,
    raw: VmstatRaw,
    delta: VmstatDelta,
}
impl VmstatCollector {
    pub fn new() -> Self {
        Self::at("/proc/vmstat")
    }
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: None,
            raw: VmstatRaw::default(),
            delta: VmstatDelta::new(),
        }
    }
}
impl Default for VmstatCollector {
    fn default() -> Self {
        Self::new()
    }
}
impl Collector for VmstatCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Vmstat
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
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
        let read_result = match self.file.as_mut() {
            Some(file) => match file.read_all() {
                Ok(bytes) => {
                    vmstat::parse(bytes, &mut self.raw);
                    Ok(())
                }
                Err(error) => Err((error, file.needs_reopen())),
            },
            None => return Err(CollectError::Gone),
        };
        if let Err((error, reopen)) = read_result {
            if reopen {
                self.file = None;
            }
            return Err(error.into());
        }
        self.delta
            .update(&self.raw, ctx.now_ms, &mut frame.parts.vmstat);
        frame.sampled.insert(CollectorId::Vmstat);
        Ok(())
    }
    fn close(&mut self) {
        self.file = None;
        self.delta.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::VmstatCollector;
    use crate::collect::{Collector, TickCtx, env::Env, frame::MonitorFrame, health::Health};
    use std::{fs, os::unix::fs::FileExt};

    #[test]
    fn held_vmstat_updates_rates_and_close_drops_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vmstat");
        fs::write(&path, b"pswpin 10\n").unwrap();
        let mut collector = VmstatCollector::at(&path);
        assert_eq!(collector.discover(&Env::default()), Health::Warming);
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        let writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.set_len(0).unwrap();
        writer.write_at(b"pswpin 20\n", 0).unwrap();
        collector
            .sample(
                &TickCtx {
                    tick: 2,
                    now_ms: 2000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame.parts.vmstat.rates[crate::collect::model::vmstat::VmRate::Pswpin as usize],
            Some(10.0)
        );
        collector.close();
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_err());
    }
}
