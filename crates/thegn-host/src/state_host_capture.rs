//! Standalone THE-603 capture helper; deliberately not wired into startup.
//!
//! Success is captured SQL data under the documented stable-namespace model,
//! never launch authority. The future host adapter must independently exclude
//! workload-writable state and own off-loop scheduling/coalescing/deadlines.
//! Ordinary SQLite WAL/SHM activity is permitted; zero-write callers cannot use
//! this operation. No immutable fallback, migration or global install occurs.

use std::{fmt, path::Path};
use thegn_core::{
    host_db_capture::HostCaptureReadError, host_definition_snapshot::HostDefinitionsSnapshot,
};

#[derive(Debug)]
pub(crate) enum StateHostCapture {
    Absent,
    Present(HostDefinitionsSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateHostReadError {
    InvalidPath,
    Unsupported,
    NonRegular,
    Changed,
    OrphanedSidecar,
    Unavailable,
    Database(HostCaptureReadError),
}

impl fmt::Display for StateHostReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "state capture path is invalid",
            Self::Unsupported => "state capture namespace is unsupported",
            Self::NonRegular => "state capture path has an invalid file type",
            Self::Changed => "state capture path changed during observation",
            Self::OrphanedSidecar => "state database sidecars exist without a base file",
            Self::Unavailable => "state capture path could not be inspected",
            Self::Database(_) => "state database could not be captured",
        })
    }
}

impl std::error::Error for StateHostReadError {}

/// Blocking internal operation. Metadata checks are not authorization: the
/// future caller must establish the stable, non-workload-writable namespace
/// assumption separately. No such production caller exists in this component.
pub(crate) fn capture(path: &Path) -> Result<StateHostCapture, StateHostReadError> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(StateHostReadError::InvalidPath);
    }
    crate::platform::state_db_capture::capture(path)
}
