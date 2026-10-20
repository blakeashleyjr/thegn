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

/// Refuse executables that can be replaced by an unrelated local user.
pub(crate) fn executable_trust(metadata: &std::fs::Metadata) -> Result<(), String> {
    platform_executable_trust(metadata)
}

/// Platform-specific executable identity fields (Unix uid/mode, absent on Windows).
pub(crate) fn executable_identity_metadata(
    metadata: &std::fs::Metadata,
) -> (Option<u32>, Option<u32>) {
    platform_executable_identity_metadata(metadata)
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
pub(crate) fn set_mode_for_test(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    platform_set_mode_for_test(path, mode)
}

#[cfg(test)]
mod native_tests;
