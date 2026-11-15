use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FlightKind {
    Refresh,
    RunDetail,
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

/// At most one fetch runs per `CiFlight`; a queued token does not fetch (it is
/// promoted by `finish`). A host therefore holds at most
/// `MAX_RUNNING_FETCHES x thegn_svc::ci::LOG_COLLECTOR_PEAK_BYTES` of streamed
/// log per flight. Every host path that calls the svc log fetch (`logs`):
/// - `ci_refresh.rs` ingestion (`client.logs`): the refresh `CiFlight`, one at a
///   time, further capped by `MAX_LOG_INGEST_CALLS`/`MAX_LOG_INGEST_BYTES` per call.
/// - `actions.rs` run-detail drill (`c.logs`): `CI_DETAIL_FLIGHT`, one at a time.
/// - `cmd/ci.rs` (`provider.logs`): one-shot CLI process, one fetch per invocation.
/// - `daemon/service.rs` `ci.logs`: single-flight per (worktree, run, job) key
///   only, with NO global cap, so distinct jobs fetch concurrently (UNBOUNDED count).
/// - `ci_autofix.rs`: does not fetch; it reads cached log entries only.
pub(crate) const MAX_RUNNING_FETCHES: usize = 1;

#[derive(Default)]
pub(crate) struct CiFlight {
    pub(crate) running: Option<FlightToken>,
    pub(crate) queued: Option<FlightToken>,
}

impl CiFlight {
    pub(crate) fn submit(&mut self, request: FlightToken) -> FlightAdmission {
        debug_assert!(
            usize::from(self.running.is_some()) <= MAX_RUNNING_FETCHES,
            "one fetch per flight"
        );
        let Some(running) = self.running.as_ref() else {
            self.running = Some(request);
            return FlightAdmission::Started;
        };
        if running.key == request.key
            && request.key.kind != FlightKind::Refresh
            && !running.cancel.load(Ordering::Relaxed)
        {
            // The live detail fetch for this exact key already produces this answer.
            return FlightAdmission::Busy;
        }
        if running.key != request.key {
            running.cancel.store(true, Ordering::Relaxed);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn token(key: usize) -> FlightToken {
        FlightToken {
            key: FlightKey {
                active_path: Some(format!("/wt-{key}")),
                run_id: None,
                kind: FlightKind::Refresh,
                generation: None,
            },
            force: false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ops: 0..3 submit key n, 3 finish. Every sequence of up to 6 ops.
    fn run_sequence(ops: &[usize]) {
        let mut flight = CiFlight::default();
        let mut started: usize = 0;
        let mut finished: usize = 0;
        for &op in ops {
            if op < 3 {
                let previous = flight.running.clone();
                let new = token(op);
                let differs = previous.as_ref().is_some_and(|p| p.key != new.key);
                if flight.submit(new) == FlightAdmission::Started {
                    started += 1;
                }
                if differs {
                    assert!(
                        previous.is_some_and(|p| p.cancel.load(Ordering::Relaxed)),
                        "different-key submit must cancel the running token: {ops:?}"
                    );
                }
            } else if flight.running.is_some() {
                finished += 1;
                // `finish` promotes the queued token, which then starts a fetch.
                if flight.finish().is_some() {
                    started += 1;
                }
            }
            assert!(
                started - finished <= MAX_RUNNING_FETCHES,
                "running fetches exceeded the bound: {ops:?}"
            );
            assert_eq!(started - finished, usize::from(flight.running.is_some()));
        }
    }

    #[test]
    fn flight_never_runs_more_than_max_fetches_for_any_small_sequence() {
        const OPS: usize = 4;
        for len in 0..=6usize {
            let total = OPS.pow(len as u32);
            for n in 0..total {
                let mut ops = Vec::with_capacity(len);
                let mut rest = n;
                for _ in 0..len {
                    ops.push(rest % OPS);
                    rest /= OPS;
                }
                run_sequence(&ops);
            }
        }
    }
}
