use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FlightKind {
    Refresh,
    RunDetail,
    LogDrilldown { job_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FlightKey {
    pub(crate) active_path: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) kind: FlightKind,
    pub(crate) generation: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct FlightToken {
    pub(crate) key: FlightKey,
    pub(crate) force: bool,
    pub(crate) cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FlightAdmission {
    Started,
    Busy,
    Queued,
}

#[derive(Default)]
pub(crate) struct CiFlight {
    pub(crate) running: Option<FlightToken>,
    pub(crate) queued: Option<FlightToken>,
}

impl CiFlight {
    pub(crate) fn submit(&mut self, request: FlightToken) -> FlightAdmission {
        let Some(running) = self.running.as_mut() else {
            self.running = Some(request);
            return FlightAdmission::Started;
        };
        if running.key == request.key {
            running.force |= request.force;
            return FlightAdmission::Busy;
        }
        running.cancel.store(true, Ordering::Relaxed);
        if let Some(queued) = self.queued.as_ref()
            && queued.key != request.key
        {
            queued.cancel.store(true, Ordering::Relaxed);
        }
        let force = self
            .queued
            .as_ref()
            .filter(|queued| queued.key == request.key)
            .is_some_and(|queued| queued.force)
            || request.force;
        self.queued = Some(FlightToken { force, ..request });
        FlightAdmission::Queued
    }

    pub(crate) fn finish(&mut self) -> Option<FlightToken> {
        let next = self.queued.take();
        self.running = next.as_ref().map(|token| FlightToken {
            key: token.key.clone(),
            force: token.force,
            cancel: Arc::clone(&token.cancel),
        });
        next
    }

    pub(crate) fn clear(&mut self) {
        self.running = None;
        self.queued = None;
    }
}
