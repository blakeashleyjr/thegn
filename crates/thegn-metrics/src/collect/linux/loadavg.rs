//! Linux `/proc/loadavg` collector.
use std::path::{Path, PathBuf};

use super::procfile::ProcFile;
use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    parse::loadavg,
    schedule::Cadence,
};

pub struct LoadavgCollector {
    path: PathBuf,
    file: Option<ProcFile>,
}
impl LoadavgCollector {
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            file: None,
        }
    }
    fn read(&mut self) -> Result<loadavg::Loadavg, CollectError> {
        let file = self.file.as_mut().ok_or(CollectError::Gone)?;
        let bytes = match file.read_all() {
            Ok(bytes) => bytes,
            Err(_error) if file.needs_reopen() => return Err(CollectError::Gone),
            Err(error) => return Err(error.into()),
        };
        loadavg::parse(bytes).ok_or(CollectError::Parse("/proc/loadavg"))
    }
}
impl Collector for LoadavgCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Loadavg
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(2)
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
        frame.parts.loadavg.value = Some(self.read()?);
        frame.sampled.insert(CollectorId::Loadavg);
        Ok(())
    }
    fn close(&mut self) {
        self.file = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::frame::MonitorFrame;
    use std::fs;
    #[test]
    fn held_file_updates_and_bad_data_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loadavg");
        fs::write(&path, b"1.00 2.00 3.00 1/10 20").unwrap();
        let mut c = LoadavgCollector::at(&path);
        assert_eq!(c.discover(&Env::default()), Health::Ok);
        let mut frame = MonitorFrame::default();
        c.sample(&TickCtx::default(), &mut frame).unwrap();
        assert_eq!(frame.parts.loadavg.value.unwrap().load1, 100);
        fs::write(&path, b"4.00 5.00 6.00 1/10 20").unwrap();
        c.sample(&TickCtx::default(), &mut frame).unwrap();
        assert_eq!(frame.parts.loadavg.value.unwrap().load1, 400);
        fs::write(&path, b"bad").unwrap();
        assert!(matches!(
            c.sample(&TickCtx::default(), &mut frame),
            Err(CollectError::Parse(_))
        ));
    }
    #[test]
    fn missing_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = LoadavgCollector::at(dir.path().join("missing"));
        assert_eq!(
            c.discover(&Env::default()),
            Health::Absent(Absent::NoHardware)
        );
    }
}
