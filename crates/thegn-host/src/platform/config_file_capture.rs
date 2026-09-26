//! Platform dispatch for bounded, opened configuration-source reads.
//!
//! This is a source-validity adapter, not a same-UID filesystem ownership
//! proof.  Each supported implementation documents its namespace assumption;
//! unsupported targets return a typed refusal rather than falling back to an
//! ordinary pathname read.
//!
//! Platform acceptance boundary: the macOS and Windows config-file readers
//! deliberately refuse a final symlink/reparse point because this chunk does
//! not yet have a reviewed bounded target walker for those platforms. The
//! typed diagnostic tells the operator to select a reviewed ordinary private
//! config file; copying a manager-owned file means it no longer automatically
//! tracks that manager. This limitation applies to config files only. The
//! state DB reader has a separate platform support contract, and this parked
//! adapter does not claim universal startup support.

use std::path::Path;

use crate::config_capture::{ConfigFileReadError, ConfigSourceReader};

pub(crate) struct Reader;

impl ConfigSourceReader for Reader {
    fn read_bounded(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
        read(path, limit)
    }
}

#[cfg(target_os = "linux")]
#[path = "config_file_capture_linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
use linux::read;

// The no-follow opened-file reader shared by every Unix: the macOS
// production reader, and the Linux fallback for filesystems outside the
// Linux O_PATH reader's supported set (see `LinuxStartupReader`).
#[cfg(unix)]
#[path = "config_file_capture_macos.rs"]
mod portable;
#[cfg(target_os = "macos")]
use portable::read;

#[cfg(target_os = "windows")]
#[path = "config_file_capture_windows.rs"]
mod windows;
#[cfg(target_os = "windows")]
use windows::read;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn read(_: &Path, _: usize) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
    Err(ConfigFileReadError::Unsupported)
}

/// The reader production startup and reload use. Linux's reader already
/// walks symlinks component-by-component with `O_PATH`; elsewhere final links
/// are resolved by [`crate::config_capture::FinalLinkResolvingReader`] first so
/// managed (e.g. Nix/home-manager) config symlinks keep working.
pub(crate) fn startup_reader() -> impl ConfigSourceReader {
    #[cfg(target_os = "linux")]
    {
        crate::config_capture::RetryOnChange(LinuxStartupReader)
    }
    #[cfg(not(target_os = "linux"))]
    {
        crate::config_capture::RetryOnChange(crate::config_capture::FinalLinkResolvingReader(
            Reader,
        ))
    }
}

/// Linux startup reader: the component-walking `O_PATH` reader, which only
/// vouches for a fixed set of local filesystems. A config on any other
/// filesystem (ZFS, NFS, overlayfs, FUSE, 9p, bcachefs, …) is read through
/// the shared Unix no-follow reader instead of making startup impossible:
/// final links are resolved and re-checked, the target must be an ordinary
/// file, and length/identity are still checked around a bounded read. Every
/// other refusal of the O_PATH reader stands.
#[cfg(target_os = "linux")]
pub(crate) struct LinuxStartupReader;

#[cfg(target_os = "linux")]
impl ConfigSourceReader for LinuxStartupReader {
    fn read_bounded(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
        match linux::read(path, limit) {
            Err(ConfigFileReadError::UnsupportedFilesystem) => {
                crate::config_capture::FinalLinkResolvingReader(PortableReader)
                    .read_bounded(path, limit)
            }
            other => other,
        }
    }
}

/// The shared Unix no-follow reader as a [`ConfigSourceReader`].
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) struct PortableReader;

#[cfg(unix)]
impl ConfigSourceReader for PortableReader {
    fn read_bounded(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ConfigFileReadError> {
        portable::read(path, limit)
    }
}

pub(crate) struct NativePathKeys {
    pub(crate) home: &'static str,
    pub(crate) home_fallback: &'static str,
    pub(crate) config: &'static str,
    pub(crate) config_fallback: &'static str,
    pub(crate) state: &'static str,
    pub(crate) state_fallback: &'static str,
}

pub(crate) fn native_path_keys() -> NativePathKeys {
    #[cfg(windows)]
    {
        NativePathKeys {
            home: "USERPROFILE",
            home_fallback: "C:\\",
            config: "APPDATA",
            config_fallback: "AppData/Roaming",
            state: "LOCALAPPDATA",
            state_fallback: "AppData/Local",
        }
    }
    #[cfg(not(windows))]
    {
        NativePathKeys {
            home: "HOME",
            home_fallback: "/",
            config: "XDG_CONFIG_HOME",
            config_fallback: ".config",
            state: "XDG_STATE_HOME",
            state_fallback: ".local/state",
        }
    }
}

#[cfg(test)]
pub(crate) fn invalid_native_string() -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xd800])
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::ffi::OsString::from("invalid")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsupported_platforms_refuse_instead_of_using_a_legacy_reader() {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        assert_eq!(
            super::Reader.read_bounded(std::path::Path::new("unused"), 1),
            Err(super::ConfigFileReadError::Unsupported)
        );
    }
}
