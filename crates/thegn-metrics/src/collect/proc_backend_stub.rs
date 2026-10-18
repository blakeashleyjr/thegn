//! Cross-platform placeholder for the Linux-only native process backend.

use crate::procs::ProcSnapshot;

pub(crate) struct ProcBackend;

impl ProcBackend {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn reset(&mut self, primed: &mut bool) {
        *primed = false;
    }

    pub(crate) fn sample(
        &mut self,
        _request: super::ProcSampleRequest<'_>,
        _primed: &mut bool,
    ) -> Option<ProcSnapshot> {
        None
    }
}
