//! Candidate-only adapter over the actual owned worker, gate and latest take.
use super::Audit;
use crate::proc_worker::{Control, Phase, ProcessWorker, Settlement};
use std::sync::Arc;
use std::time::Duration;
use thegn_metrics::ProcSnapshot;
pub(super) const LABEL: &str = "THE630/631 candidate; identical paired observation scope";
pub(super) struct Driver {
    worker: Option<ProcessWorker>,
    control: Option<Control>,
    generation: u64,
}
impl Driver {
    pub fn new(audit: Arc<Audit>) -> Self {
        // This ignored workload runs alone. Production's process-static slot
        // keeps custody even if fixture unwind interrupts bounded cleanup.
        let worker = ProcessWorker::spawn_measured(audit).unwrap();
        let control = worker.control();
        Self {
            worker: Some(worker),
            control: Some(control),
            generation: 0,
        }
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        self.generation = self.control.as_ref().unwrap().set_enabled(enabled);
    }
    pub fn acknowledged(&self, enabled: bool) -> bool {
        let status = self.control.as_ref().unwrap().status();
        status.generation == self.generation
            && status.phase
                == if enabled {
                    Phase::Waiting
                } else {
                    Phase::Parked
                }
    }
    pub fn take(&mut self) -> Option<ProcSnapshot> {
        self.control
            .as_ref()?
            .take_latest()
            .map(|publication| publication.snapshot)
    }
    pub fn stop(mut self) {
        self.close();
    }
    fn close(&mut self) {
        self.control.take();
        if let Some(worker) = self.worker.take() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let outcome = runtime.block_on(
                worker.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3)),
            );
            if !std::thread::panicking() {
                assert_eq!(outcome, Settlement::Settled);
            }
        }
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.close();
    }
}
pub(super) fn published(model: &mut crate::chrome::FrameModel) {
    model.process_revision = model.process_revision.checked_add(1).unwrap();
}
