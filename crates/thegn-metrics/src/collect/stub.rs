//! Explicit placeholders for collectors whose implementations land in F3b or later.
use super::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    schedule::Cadence,
};

#[derive(Debug, Clone, Copy)]
pub struct NotImplemented(pub CollectorId);
impl Collector for NotImplemented {
    fn id(&self) -> CollectorId {
        self.0
    }
    fn cadence(&self) -> Cadence {
        Cadence::Static
    }
    fn discover(&mut self, _env: &Env) -> Health {
        Health::Absent(Absent::NotImplemented)
    }
    fn sample(&mut self, _ctx: &TickCtx, _frame: &mut MonitorFrame) -> Result<(), CollectError> {
        Ok(())
    }
    fn close(&mut self) {}
}

pub fn stub_collectors() -> Vec<Box<dyn Collector>> {
    CollectorId::ALL
        .into_iter()
        .map(|id| Box::new(NotImplemented(id)) as Box<dyn Collector>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Absent, Health, stub_collectors};
    use crate::{CollectorId, TickCtx, env::Env, frame::MonitorFrame};
    #[test]
    fn stubs_cover_each_id_and_leave_frames_unsampled() {
        let mut collectors = stub_collectors();
        assert_eq!(collectors.len(), CollectorId::ALL.len());
        let mut frame = MonitorFrame::default();
        for (collector, expected) in collectors.iter_mut().zip(CollectorId::ALL) {
            assert_eq!(collector.id(), expected);
            assert_eq!(
                collector.discover(&Env::default()),
                Health::Absent(Absent::NotImplemented)
            );
            collector.sample(&TickCtx::default(), &mut frame).unwrap();
        }
        assert!(frame.sampled.is_empty());
    }
}
