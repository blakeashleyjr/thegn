//! Resident-process ownership seam. All OS-specific lifecycle code stays here.
//! Unix groups and Windows direct-child handles do not certify escaped trees.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

/// Whether metadata describes a file the current platform can execute.
/// Unix checks execute mode bits; Windows applies its normal process lookup
/// rules after regular-file validation.
pub(crate) fn is_executable_file(metadata: &std::fs::Metadata) -> bool {
    platform_is_executable_file(metadata)
}

#[cfg(test)]
mod native_tests;
