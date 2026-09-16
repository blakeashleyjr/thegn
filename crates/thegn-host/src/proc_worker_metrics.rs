//! The production process collector, shared with the opt-in measured worker.
use super::{Collector, Factory, ProcSnapshot};
use std::sync::Arc;

struct LiveCollector {
    sampler: thegn_metrics::ProcSampler,
    pane_pids: crate::hydrate::PanePids,
    daemon_pid: Arc<std::sync::atomic::AtomicU32>,
}
impl Collector for LiveCollector {
    fn sample(&mut self) -> ProcSnapshot {
        use std::sync::atomic::Ordering;
        let pids = self
            .pane_pids
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        self.sampler.set_pane_pids(pids.to_vec());
        self.sampler
            .set_daemon_pid(match self.daemon_pid.load(Ordering::Relaxed) {
                0 => None,
                pid => Some(pid),
            });
        #[cfg(test)]
        crate::proc_workload::event(crate::proc_workload::Event::SampleStart);
        let snapshot = {
            let _guard = crate::perf::measure(crate::perf::Subsys::Stats);
            self.sampler.sample()
        };
        #[cfg(test)]
        crate::proc_workload::event(crate::proc_workload::Event::SampleEnd);
        snapshot
    }
    fn reset(&mut self) {
        self.sampler.reset();
        #[cfg(test)]
        crate::proc_workload::event(crate::proc_workload::Event::Reset);
    }
}
pub(super) fn factory(
    pane_pids: crate::hydrate::PanePids,
    daemon_pid: Arc<std::sync::atomic::AtomicU32>,
    rows: usize,
) -> Factory {
    Box::new(move || {
        Box::new(LiveCollector {
            sampler: thegn_metrics::ProcSampler::new(rows),
            pane_pids,
            daemon_pid,
        })
    })
}
