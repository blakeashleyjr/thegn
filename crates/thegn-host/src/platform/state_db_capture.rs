//! Linux-first inspected namespace seam. No stock SQLite VFS can promise a
//! descriptor-bound filename open against hostile same-UID path replacement.

#[cfg(target_os = "linux")]
#[path = "state_db_capture_linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::capture;

#[cfg(not(target_os = "linux"))]
pub(crate) fn capture(
    _path: &std::path::Path,
) -> Result<
    crate::state_host_capture::StateHostCapture,
    crate::state_host_capture::StateHostReadError,
> {
    Err(crate::state_host_capture::StateHostReadError::Unsupported)
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    #[test]
    fn unsupported_platform_never_uses_a_tolerant_opener() {
        assert!(matches!(
            super::capture(std::path::Path::new("unused")),
            Err(crate::state_host_capture::StateHostReadError::Unsupported)
        ));
    }
}
