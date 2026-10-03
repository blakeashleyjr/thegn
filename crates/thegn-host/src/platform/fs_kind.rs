//! Which kind of filesystem a path lives on, for one question: can an
//! inotify/FSEvents watch be TRUSTED to see every change there?
//!
//! Network and userspace filesystems (NFS, SMB/CIFS, FUSE, 9p, AFP, WebDAV)
//! deliver no events for changes made by other clients or behind the kernel's
//! back, so a change generation built on them would stay put while the tree
//! moves. [`unwatchable`] says "do not claim coverage here". It is an
//! ALLOWLIST of known-local filesystems: an unlisted type is unwatchable.
//!
//! Fail toward today's behaviour: anything it cannot positively classify as a
//! local filesystem -- a `statfs` failure, a platform without this seam -- is
//! reported unwatchable, which only costs the optimisation.

use std::path::Path;

/// Whether `path` is on a filesystem whose change events cannot be trusted (or
/// whose type could not be determined).
#[cfg(target_os = "linux")]
pub(crate) fn unwatchable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: `c` is a valid NUL-terminated path; `st` is a properly sized,
    // writable out-parameter that statfs fully initialises on success.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return true;
    }
    // Bit-widths of f_type differ across arches; compare as u64.
    !is_local_magic(st.f_type as u64)
}

/// Linux `f_type` magics of LOCAL filesystems whose change events can be trusted.
/// An allowlist: anything not listed (network, FUSE, 9p, unknown) is unwatchable.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_local_magic(magic: u64) -> bool {
    matches!(
        magic & 0xFFFF_FFFF,
        0xEF53          // ext2/3/4
        | 0x5846_5342   // xfs
        | 0x9123_683E   // btrfs
        | 0x0102_1994   // tmpfs
        | 0x794C_7630   // overlayfs
        | 0x2FC1_2FC1   // zfs
        | 0xF2F5_2010   // f2fs
        | 0xCA45_1A4E // bcachefs
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn unwatchable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: as above.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return true;
    }
    let name: Vec<u8> = st
        .f_fstypename
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    !is_local_fstype(&String::from_utf8_lossy(&name))
}

/// macOS `f_fstypename` values of local filesystems (allowlist).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn is_local_fstype(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "apfs" | "hfs")
}

/// No classification available: never claim coverage.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn unwatchable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_local_filesystems_are_watchable() {
        // ext4 / xfs / btrfs / tmpfs / overlayfs / zfs / f2fs / bcachefs.
        for m in [
            0xEF53u64,
            0x5846_5342,
            0x9123_683E,
            0x0102_1994,
            0x794C_7630,
            0x2FC1_2FC1,
            0xF2F5_2010,
            0xCA45_1A4E,
        ] {
            assert!(is_local_magic(m), "{m:#x}");
        }
        // NFS, SMB, CIFS, SMB2, FUSE, 9p, AFS, Coda and any unknown magic.
        for m in [
            0x6969u64,
            0x517B,
            0xFF53_4D42,
            0xFE53_4D42,
            0x6573_5546,
            0x0102_1997,
            0x5346_414F,
            0x7375_7245,
            0xDEAD_BEEF,
        ] {
            assert!(!is_local_magic(m), "{m:#x}");
        }
        assert!(is_local_fstype("apfs") && is_local_fstype("HFS"));
        assert!(!is_local_fstype("nfs") && !is_local_fstype("macfuse") && !is_local_fstype("?"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_path_cannot_be_classified_so_it_is_unwatchable() {
        assert!(unwatchable(Path::new("/definitely/not/here/tg718")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_temp_dir_is_classified_without_panicking() {
        // tmpfs or a local disk on every CI/dev box; the point is it answers.
        std::hint::black_box(unwatchable(&std::env::temp_dir()));
    }
}
