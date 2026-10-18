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

/// Whether an accept error is a platform-specific resource exhaustion error.
pub(crate) fn is_transient_accept_resource_error(raw_os_error: Option<i32>) -> bool {
    platform_is_transient_accept_resource_error(raw_os_error)
}

#[cfg(test)]
pub(crate) fn accept_resource_error_for_test() -> std::io::Error {
    platform_accept_resource_error_for_test()
}

#[cfg(test)]
mod native_tests;
