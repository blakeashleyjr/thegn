//! Linux `/proc/uptime` collector.
use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    parse::uptime,
    schedule::Cadence,
};
use std::path::{Path, PathBuf};

pub struct UptimeCollector {
    path: PathBuf,
    file: Option<ProcFile>,
}
impl UptimeCollector {
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            file: None,
        }
    }
    fn read(&mut self) -> Result<uptime::Uptime, CollectError> {
        let file = self.file.as_mut().ok_or(CollectError::Gone)?;
        let bytes = match file.read_all() {
            Ok(bytes) => bytes,
            Err(_error) if file.needs_reopen() => return Err(CollectError::Gone),
            Err(error) => return Err(error.into()),
        };
        uptime::parse(bytes).ok_or(CollectError::Parse("/proc/uptime"))
    }
}
impl Collector for UptimeCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Uptime
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.close();
        match ProcFile::open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                Health::Ok
            }
            Err(_) => Health::Absent(Absent::NoHardware),
        }
    }
    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        frame.parts.uptime.value = Some(self.read()?);
        frame.sampled.insert(CollectorId::Uptime);
        Ok(())
    }
    fn close(&mut self) {
        self.file = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn held_uptime_file_updates_between_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uptime");
        fs::write(&path, b"10.00 20.00").unwrap();
        let mut collector = UptimeCollector::at(&path);
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        let mut frame = MonitorFrame::default();
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        assert_eq!(frame.parts.uptime.value.unwrap().up_centi, 1000);
        fs::write(&path, b"11.00 21.00").unwrap();
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        assert_eq!(frame.parts.uptime.value.unwrap().up_centi, 1100);
    }
}
