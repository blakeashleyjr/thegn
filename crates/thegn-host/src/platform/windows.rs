//! Windows impls of the platform seam.
//!
//! No signals here: "terminate" is `TerminateProcess` (hard kill, no graceful
//! window — the unix side's SIGTERM handlers never run on Windows), and
//! shutdown notification listens to console control events (Ctrl+C / window
//! close / system shutdown) instead of SIGTERM/SIGHUP.
//!
//! Process-tree kills ride Job Objects with `KILL_ON_JOB_CLOSE`: terminating
//! the job reaps the whole tree, and merely *dropping* the last
//! [`GroupHandle`] does too — better orphan hygiene than unix pgids (a thegn
//! that dies mid-run takes its spawned trees with it).

use std::io;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(test)]
static FAIL_JOB_ASSIGN_FOR_TEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl<R: std::io::Read + std::os::windows::io::AsRawHandle> super::GatePipe for R {
    fn set_nonblocking(&self) -> io::Result<()> {
        gate_pipe_handle_nonblocking(std::os::windows::io::AsRawHandle::as_raw_handle(self))
    }

    fn read_available(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match gate_pipe_available(std::os::windows::io::AsRawHandle::as_raw_handle(self))? {
            0 => Err(io::ErrorKind::WouldBlock.into()),
            _ => std::io::Read::read(self, bytes),
        }
    }
}

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

pub(crate) fn os_path_from_git_bytes(bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    let path = String::from_utf8(bytes.to_vec()).map_err(|error| {
        anyhow::anyhow!("Git returned a conflicted path that is not valid UTF-8: {error}")
    })?;
    Ok(std::path::PathBuf::from(path))
}

pub(crate) fn display_git_path(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, GetExitCodeProcess, OpenProcess, OpenThread,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME,
    TerminateProcess, WaitForSingleObject,
};

/// Restores the original stderr handle on drop
/// (see [`super::redirect_stderr_to_logfile`]).
pub struct StderrGuard {
    saved: HANDLE,
    /// Owns the log-file handle `STD_ERROR_HANDLE` now points at; dropped (and
    /// the handle closed) only after the std handle is restored.
    _file: std::fs::File,
}

// SAFETY: the raw HANDLE is only touched in `Drop`; the guard lives on the
// compositor's main thread but the types it's embedded in may assert Send.
unsafe impl Send for StderrGuard {}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        // SAFETY: restoring a std handle we saved earlier; best-effort.
        unsafe {
            SetStdHandle(STD_ERROR_HANDLE, self.saved);
        }
    }
}

impl StderrGuard {
    /// Windows: the crash notice is not rebound to the pre-redirect handle (the
    /// default hook still prints the panic to stderr → the log file). No-op so
    /// the cross-platform call site compiles.
    pub fn register_crash_notice(&self) {}
}

/// Windows terminal restore is left to the normal teardown / termwiz path; the
/// panic-hook fast restore is unix-only for now. A unit stub so the call site
/// is platform-neutral.
pub struct TerminalRestore;

impl TerminalRestore {
    pub fn restore(&self) {}
}

/// Windows: no early raw-termios capture; returns `None` so the hook skips the
/// fast restore and the normal teardown handles it.
pub fn capture_terminal_restore() -> Option<TerminalRestore> {
    None
}

/// Point the process's `STD_ERROR_HANDLE` at `file`, saving the original for
/// the guard's `Drop`. Rust's `std::io::stderr` resolves the std handle per
/// write, so panics/`eprintln!` from any thread land in the log. (C-runtime
/// fd-2 writers are not rebound — thegn has no C code that writes stderr.)
pub(super) fn redirect_stderr_to(file: std::fs::File) -> Option<StderrGuard> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: querying/replacing our own process's std handle slot.
    unsafe {
        let saved = GetStdHandle(STD_ERROR_HANDLE);
        if SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle() as HANDLE) == 0 {
            return None;
        }
        Some(StderrGuard { saved, _file: file })
    }
}

/// Is a process with this pid alive?
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: probing a foreign pid with the narrowest access right; the
    // handle is closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        alive
    }
}

/// Best-effort termination of a single process. Hard kill — Windows has no
/// SIGTERM; the child gets no cleanup window.
pub fn terminate_pid(pid: u32) {
    // SAFETY: terminating an explicit pid; the handle is closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return;
        }
        TerminateProcess(h, 1);
        CloseHandle(h);
    }
}

/// Create a fresh file for the recordings tee. Windows has no unix mode bits;
/// the per-profile state dir already sits under the user's profile, so this is
/// a plain create for now (ACL tightening is later hardening).
pub fn create_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// Open (creating, never truncating) an advisory-lock file; ACLs are inherited.
pub fn open_lock_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Windows ACLs are inherited from the per-profile state directory.
pub fn restrict_dir_owner_only_checked(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Create a random child with an explicit DACL built from the current process
/// token. `tempfile::TempDir` cannot pass SECURITY_ATTRIBUTES to the Windows
/// directory creation call, so bundle custody uses this narrow constructor;
/// the existing fetch and profile-state paths keep their old behavior.
pub(crate) fn create_private_directory(
    root: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID,
        TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor, NO_INHERITANCE,
        SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let root = std::fs::canonicalize(root)?;
    let mut token = std::ptr::null_mut();
    // SAFETY: querying this process's token into an owned handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let result = (|| {
        let mut needed = 0u32;
        // SAFETY: the null probe requests the required TOKEN_USER size.
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let word_count = (needed as usize)
            .checked_add(std::mem::size_of::<usize>() - 1)
            .ok_or_else(|| std::io::Error::other("TOKEN_USER size overflow"))?
            / std::mem::size_of::<usize>();
        let mut token_words = vec![0usize; word_count];
        // SAFETY: token_words is aligned storage with the size returned by
        // the probe.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_words.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the word-aligned buffer and successful query contain
        // TOKEN_USER followed by a valid SID pointer.
        let sid = unsafe { (*(token_words.as_ptr() as *const TOKEN_USER)).User.Sid };
        if sid.is_null() {
            return Err(std::io::Error::other("process token has no user SID"));
        }
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        if sid_len == 0 {
            return Err(std::io::Error::other("process token user SID is invalid"));
        }
        // Keep the SID in its original aligned token buffer. The buffer stays
        // alive through ACL construction and CreateDirectoryW; copying it into
        // a Vec<u8> would discard the allocation's alignment guarantee.
        let sid = sid.cast::<u16>();
        let explicit = EXPLICIT_ACCESS_W {
            grfAccessPermissions: windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid,
            },
        };
        let mut dacl = std::ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &explicit, std::ptr::null(), &mut dacl) };
        if status != 0 || dacl.is_null() {
            unsafe { LocalFree(dacl.cast()) };
            return Err(if status != 0 {
                std::io::Error::from_raw_os_error(status as i32)
            } else {
                std::io::Error::other("private directory DACL construction failed")
            });
        }
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        // SAFETY: descriptor and dacl are valid writable/owned structures.
        let descriptor_ok = unsafe {
            InitializeSecurityDescriptor(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SECURITY_DESCRIPTOR_REVISION,
            ) != 0
                && SetSecurityDescriptorDacl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                    dacl,
                    0,
                ) != 0
                && SetSecurityDescriptorControl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                ) != 0
        };
        if !descriptor_ok {
            unsafe { LocalFree(dacl.cast()) };
            return Err(std::io::Error::last_os_error());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        };
        for _ in 0..16 {
            let mut random = [0u8; 16];
            if let Err(error) = getrandom::fill(&mut random) {
                unsafe { LocalFree(dacl.cast()) };
                return Err(std::io::Error::other(error));
            }
            let name = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let path = root.join(format!("thegn-mq-{name}"));
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: wide path and security attributes remain alive for call.
            if unsafe { CreateDirectoryW(wide.as_ptr(), &attrs) } != 0 {
                unsafe { LocalFree(dacl.cast()) };
                return Ok(path);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                unsafe { LocalFree(dacl.cast()) };
                return Err(error);
            }
        }
        unsafe { LocalFree(dacl.cast()) };
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a private bundle directory",
        ))
    })();
    // SAFETY: token is the handle opened above and is closed on every path.
    unsafe { CloseHandle(token) };
    result
}

/// Give a newly-created temporary directory an explicit protected owner-only
/// DACL, then read back and validate that exact DACL. This is separate from
/// the legacy profile-state helper above, whose inherited ACL semantics remain
/// unchanged for existing callers.
pub(crate) fn secure_private_directory(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
        SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner: PSID = std::ptr::null_mut();
    let mut original_descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the path is NUL-terminated and all output pointers are valid.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut original_descriptor,
        )
    };
    if status != 0 || owner.is_null() {
        unsafe { LocalFree(original_descriptor.cast()) };
        return Err(if status != 0 {
            std::io::Error::from_raw_os_error(status as i32)
        } else {
            std::io::Error::other("private directory has no owner SID")
        });
    }
    let explicit = EXPLICIT_ACCESS_W {
        grfAccessPermissions: windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: windows_sys::Win32::Security::NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: owner.cast(),
        },
    };
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: owner remains valid while original_descriptor is retained; the
    // ACL returned by SetEntriesInAclW is owned by this function.
    let status = unsafe { SetEntriesInAclW(1, &explicit, std::ptr::null(), &mut dacl) };
    unsafe { LocalFree(original_descriptor.cast()) };
    if status != 0 || dacl.is_null() {
        unsafe { LocalFree(dacl.cast()) };
        return Err(if status != 0 {
            std::io::Error::from_raw_os_error(status as i32)
        } else {
            std::io::Error::other("owner-only DACL construction failed")
        });
    }
    // SAFETY: path and the freshly allocated ACL are valid for this call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    unsafe { LocalFree(dacl.cast()) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }

    let mut owner: PSID = std::ptr::null_mut();
    let mut read_dacl: *mut ACL = std::ptr::null_mut();
    let mut security_descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: output pointers are valid and the returned descriptor is freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut read_dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    let mut read_present = 0;
    let mut read_defaulted = 0;
    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    let mut ace_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: all pointers came from the successful read above.
    let valid = unsafe {
        GetSecurityDescriptorControl(security_descriptor, &mut control, &mut revision) != 0
            && GetSecurityDescriptorDacl(
                security_descriptor,
                &mut read_present,
                &mut read_dacl,
                &mut read_defaulted,
            ) != 0
            && read_present != 0
            && !read_dacl.is_null()
            && control & SE_DACL_PROTECTED != 0
            && GetAclInformation(
                read_dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                windows_sys::Win32::Security::AclSizeInformation,
            ) != 0
            && size.AceCount == 1
            && GetAce(read_dacl, 0, &mut ace_ptr) != 0
            && !ace_ptr.is_null()
            && (*(ace_ptr as *const ACCESS_ALLOWED_ACE)).Header.AceType
                == ACCESS_ALLOWED_ACE_TYPE as u8
            && (*(ace_ptr as *const ACCESS_ALLOWED_ACE)).Header.AceFlags == 0
            && (*(ace_ptr as *const ACCESS_ALLOWED_ACE)).Mask
                == windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS
            && EqualSid(
                owner,
                &(*(ace_ptr as *const ACCESS_ALLOWED_ACE)).SidStart as *const u32 as PSID,
            ) != 0
    };
    unsafe { LocalFree(security_descriptor.cast()) };
    if !valid {
        return Err(std::io::Error::other("owner-only DACL validation failed"));
    }
    Ok(())
}

/// POSIX shell wrappers are not a Windows cache transport. Returning an
/// explicit unsupported error lets the portable cache policy fail soft to a
/// direct compiler without materializing a script Windows cannot execute.
pub(crate) fn publish_private_executable(
    _temporary: &std::path::Path,
    _path: &std::path::Path,
    _contents: &[u8],
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "executable shell wrappers are unavailable on Windows",
    ))
}

/// Windows named pipes rely on the current user's inherited endpoint ACL and
/// local pipe identity rather than Unix effective-uid peer credentials.
pub(crate) fn local_control_security(_path: &std::path::Path) -> super::LocalControlSecurity {
    super::LocalControlSecurity {
        auth: "local-pipe-or-token",
        peer_identity: "local-only-named-pipe",
        hardening: "platform-acl",
        error: None,
    }
}

/// Open an existing path without traversing a final-component reparse point.
/// `FILE_FLAG_OPEN_REPARSE_POINT` is kept local to the platform seam rather
/// than enabling another windows-sys feature for one constant.
pub fn open_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Pin a CLI file identity while following ordinary executable symlinks.
/// Metadata-only access does not consume bytes from a replaced special file.
pub(crate) fn open_capability_identity(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().access_mode(0).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "capability identity is not a regular file",
        ));
    }
    Ok(file)
}

/// Purpose-scoped directory identity open. BACKUP_SEMANTICS enables directory
/// handles only here; ordinary file opens keep their existing security flags.
/// Refuse all final-component reparse points, including directory junctions.
pub fn open_directory_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || handle_file_attributes(&file)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::other("not a plain directory identity"));
    }
    Ok(file)
}

/// Windows has no openat equivalent in the Win32 surface used by this crate.
/// Keep the pinned parent descriptor in the operation, and open the leaf with
/// the no-reparse seam; unlinking below acts on that validated leaf handle.
pub(crate) fn platform_open_cleanup_file_at(
    _parent: &std::fs::File,
    parent_path: &std::path::Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::File> {
    open_nofollow(&parent_path.join(name))
}

pub(crate) fn platform_cleanup_file_identity(
    file: &std::fs::File,
) -> std::io::Result<super::CleanupFileIdentity> {
    let (device, high, low, links) = handle_identity_with_links(file)?;
    Ok(super::CleanupFileIdentity {
        device: u64::from(device),
        inode: (u64::from(high) << 32) | u64::from(low),
        links: u64::from(links),
    })
}

pub(crate) fn platform_unlink_cleanup_file_at(
    _parent: &std::fs::File,
    _parent_path: &std::path::Path,
    _name: &std::ffi::OsStr,
    file: &std::fs::File,
) -> std::io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    // SAFETY: `file` is our open, validated regular-file handle and the
    // disposition structure is the documented fixed-size input for this API.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Stable handle identity for retained file/directory custody. This avoids
/// the unstable `std::os::windows::fs::MetadataExt` by using the documented
/// Win32 handle query already available through windows-sys 0.59.
pub(crate) fn handle_identity(file: &std::fs::File) -> std::io::Result<(u32, u32, u32)> {
    let (volume, high, low, _) = handle_identity_with_links(file)?;
    Ok((volume, high, low))
}

fn handle_identity_with_links(file: &std::fs::File) -> std::io::Result<(u32, u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` owns a live kernel handle and `info` is valid output
    // storage for the fixed-size Win32 structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call above initialized all fields on success.
    let info = unsafe { info.assume_init() };
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
        info.nNumberOfLinks,
    ))
}

pub(crate) fn handle_file_attributes(file: &std::fs::File) -> std::io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` owns a live kernel handle and `info` is valid output
    // storage for the fixed-size Win32 structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call above initialized all fields on success.
    Ok(unsafe { info.assume_init() }.dwFileAttributes)
}

#[cfg(test)]
pub fn symlink_file_for_test(
    original: &std::path::Path,
    link: &std::path::Path,
) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

/// No-op on Windows (no unix mode bits to tighten).
pub fn restrict_dir_owner_only(_path: &std::path::Path) {}

/// An owned kill-on-close Job Object handle. Closing the last clone (Drop)
/// reaps every process still in the job.
struct JobInner(HANDLE);

// SAFETY: a Job Object HANDLE is process-global kernel state; using it from
// any thread is fine (the watchdog thread terminates it, the spawner drops it).
unsafe impl Send for JobInner {}
unsafe impl Sync for JobInner {}

impl Drop for JobInner {
    fn drop(&mut self) {
        // SAFETY: closing a handle we own; KILL_ON_JOB_CLOSE reaps the tree.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A spawned child's Job Object (process group on unix) — what
/// [`GroupHandle::terminate`] reaps in one call.
#[derive(Clone)]
pub struct GroupHandle {
    pid: u32,
    /// `None` = degraded (job creation/assignment failed): terminate falls
    /// back to the direct child only.
    job: Option<Arc<JobInner>>,
}

impl GroupHandle {
    /// A handle over an already-known pid — for tests and callers that track
    /// pids themselves (the PTY pane's `Drop` reap, which only ever has the
    /// pid). No job: terminate is direct-child only.
    pub fn from_pid(pid: i32) -> Self {
        Self {
            pid: pid.max(0) as u32,
            job: None,
        }
    }

    /// Terminate the whole job (hard kill — Windows has no graceful TERM
    /// window in this scoped teardown), or just the direct child on the
    /// degraded path. Graceful child shutdown on Windows remains a limitation.
    pub fn terminate(&self) {
        match &self.job {
            // SAFETY: terminating a job whose handle we own.
            Some(j) => unsafe {
                TerminateJobObject(j.0, 1);
            },
            None => terminate_pid(self.pid),
        }
    }
    /// Forcefully terminate the whole Job Object.
    pub fn kill(&self) {
        self.terminate();
    }

    /// Stop applying kill-on-close to this Job Object so a successful helper
    /// may leave a background clipboard owner running after its leader exits.
    /// Returns false only if Windows refused to update the job limits.
    pub fn detach(&self) -> bool {
        let Some(job) = &self.job else {
            return true;
        };
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
        }
    }

    /// Whether the retained Job Object has no active processes.
    pub fn is_empty(&self) -> bool {
        let Some(job) = &self.job else {
            return !pid_alive(i64::from(self.pid));
        };
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the job handle is owned by this GroupHandle and `info` is a
        // correctly-sized writable output structure.
        let ok = unsafe {
            QueryInformationJobObject(
                job.0,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } != 0;
        ok && info.ActiveProcesses == 0
    }
}

/// Put an already spawned PTY child in a kill-on-close Job Object while its
/// owning handle is still available. A failed assignment leaves teardown to
/// the child's own process handle; it never guesses from a persisted PID.
pub(crate) fn pty_group(child: &dyn portable_pty::Child) -> Option<GroupHandle> {
    let pid = child.process_id()?;
    let process = child.as_raw_handle()? as HANDLE;
    // SAFETY: the new job is owned below and the PTY child handle is live for
    // this call. Assignment failure closes the newly-created job.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0;
        let assigned = configured && AssignProcessToJobObject(job, process) != 0;
        if !assigned {
            CloseHandle(job);
            return None;
        }
        Some(GroupHandle {
            pid,
            job: Some(Arc::new(JobInner(job))),
        })
    }
}

/// Spawn `cmd` suspended, assign it to a fresh kill-on-close Job Object, then
/// resume it. No user code runs before containment succeeds; any setup failure
/// returns an error and leaves no live child. `CREATE_SUSPENDED` is set through
/// `CommandExt::creation_flags`, which replaces caller-supplied flags, so
/// callers of this function must not set creation flags themselves.
pub fn spawn_grouped(cmd: &mut Command) -> std::io::Result<(std::process::Child, GroupHandle)> {
    spawn_contained(cmd)
}

/// Spawn an interactive, non-security-sensitive helper with tree cleanup
/// when available. Only Job Object setup failures degrade to direct-child
/// ownership; command spawn failures are returned unchanged.
pub fn spawn_grouped_best_effort(
    cmd: &mut Command,
) -> std::io::Result<(std::process::Child, GroupHandle)> {
    match spawn_contained_with_stage(cmd) {
        Ok(children) => Ok(children),
        Err(failure) if failure.stage == ContainedSpawnStage::Command => Err(failure.error),
        Err(failure) => {
            let error = failure.error;
            tracing::warn!(target: "thegn::platform", %error, "job object containment unavailable; helper runs without tree cleanup");
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0);
            let child = cmd.spawn()?;
            let group = GroupHandle {
                pid: child.id(),
                job: None,
            };
            Ok((child, group))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContainedSpawnStage {
    Command,
    Job,
}

struct ContainedSpawnFailure {
    stage: ContainedSpawnStage,
    error: io::Error,
}

fn spawn_contained(cmd: &mut Command) -> io::Result<(Child, GroupHandle)> {
    spawn_contained_with_stage(cmd).map_err(|failure| failure.error)
}

fn spawn_contained_with_stage(
    cmd: &mut Command,
) -> Result<(Child, GroupHandle), ContainedSpawnFailure> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;

    // SAFETY: the fresh Job Object is either transferred to JobInner or
    // explicitly closed on every failure path.
    let job = unsafe {
        let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if handle.is_null() {
            return Err(ContainedSpawnFailure {
                stage: ContainedSpawnStage::Job,
                error: io::Error::last_os_error(),
            });
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = io::Error::last_os_error();
            CloseHandle(handle);
            return Err(ContainedSpawnFailure {
                stage: ContainedSpawnStage::Job,
                error,
            });
        }
        handle
    };

    cmd.creation_flags(CREATE_SUSPENDED);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            // SAFETY: the configured job is still empty.
            unsafe { CloseHandle(job) };
            return Err(ContainedSpawnFailure {
                stage: ContainedSpawnStage::Command,
                error,
            });
        }
    };

    #[cfg(test)]
    let force_assignment_failure =
        FAIL_JOB_ASSIGN_FOR_TEST.swap(false, std::sync::atomic::Ordering::SeqCst);
    #[cfg(not(test))]
    let force_assignment_failure = false;

    // SAFETY: the child is suspended and cannot create descendants before
    // assignment. On success, JobInner takes ownership of `job` below.
    let assigned = !force_assignment_failure
        && unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) } != 0;
    if !assigned {
        let error = if force_assignment_failure {
            io::Error::other("forced job assignment failure (test)")
        } else {
            io::Error::last_os_error()
        };
        let _ = child.kill(); // best-effort: fail-closed spawn cleanup
        #[expect(
            clippy::disallowed_methods,
            reason = "failed containment assignment; reap the suspended child before returning the spawn failure"
        )]
        let _ = child.wait();
        // SAFETY: assignment failed, so the job remains empty.
        unsafe { CloseHandle(job) };
        return Err(ContainedSpawnFailure {
            stage: ContainedSpawnStage::Job,
            error,
        });
    }

    let group = GroupHandle {
        pid: child.id(),
        job: Some(Arc::new(JobInner(job))),
    };
    if let Err(error) = resume_suspended_gate_process(child.id()) {
        group.kill();
        #[expect(
            clippy::disallowed_methods,
            reason = "failed to resume only after containment succeeded; reap before refusing spawn"
        )]
        let _ = child.wait();
        return Err(ContainedSpawnFailure {
            stage: ContainedSpawnStage::Job,
            error,
        });
    }
    Ok((child, group))
}

/// Windows' kill-on-close Job Object already ends the tree with thegn.
pub fn spawn_grouped_die_with_parent(
    cmd: &mut Command,
) -> std::io::Result<(std::process::Child, GroupHandle)> {
    spawn_grouped(cmd)
}

/// Gate spawn uses the shared fail-closed suspended-create/assign/resume path.
fn resume_suspended_gate_process(pid: u32) -> io::Result<()> {
    // SAFETY: snapshot creation takes no borrowed pointers and returns an owned
    // kernel handle which is closed on every path below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    // SAFETY: `entry` is initialized with its required size and writable.
    let mut found = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    let mut thread_id = None;
    while found {
        if entry.th32OwnerProcessID == pid {
            if thread_id.replace(entry.th32ThreadID).is_some() {
                unsafe { CloseHandle(snapshot) };
                return Err(io::Error::other(
                    "suspended gate process exposed more than one startup thread",
                ));
            }
        }
        // SAFETY: `entry` remains a correctly-sized writable THREADENTRY32.
        found = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: release the snapshot acquired above.
    unsafe { CloseHandle(snapshot) };
    let thread_id =
        thread_id.ok_or_else(|| io::Error::other("suspended gate startup thread was not found"))?;
    // SAFETY: open the unique thread belonging to the suspended child process.
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: resume exactly the startup thread we opened and then close it.
    let previous = unsafe { ResumeThread(thread) };
    unsafe { CloseHandle(thread) };
    if previous == u32::MAX {
        Err(io::Error::last_os_error())
    } else if previous != 1 {
        Err(io::Error::other(format!(
            "gate startup thread had unexpected suspend count {previous}"
        )))
    } else {
        Ok(())
    }
}

pub fn spawn_gate_grouped(
    cmd: &mut Command,
) -> std::io::Result<(std::process::Child, GroupHandle)> {
    spawn_contained(cmd)
}

/// Query available anonymous-pipe bytes so the gate reader can poll and honor
/// cancellation even when an escaped descendant keeps the writer open.
pub fn gate_pipe_available(handle: std::os::windows::io::RawHandle) -> io::Result<usize> {
    let mut available = 0u32;
    // SAFETY: PeekNamedPipe reads metadata from the owned child pipe handle and
    // writes one DWORD to our initialized out parameter.
    let ok = unsafe {
        PeekNamedPipe(
            handle as HANDLE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available as usize)
    }
}

fn gate_pipe_handle_nonblocking(_handle: std::os::windows::io::RawHandle) -> io::Result<()> {
    // Windows anonymous pipes have no nonblocking mode. Gate readers use
    // PeekNamedPipe before each read and poll cancellation between reads.
    Ok(())
}

/// Windows `try_wait` observes exit while the retained process and Job Object
/// handles continue to pin ownership of the child and its descendants.
pub fn gate_child_exited(child: &mut Child) -> io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

/// Spawn a native clipboard helper using the existing direct-child behavior.
///
/// Process-tree containment for clipboard helpers is intentionally deferred to
/// THE-274. Deadlines still apply, but this seam must not quietly expand this
/// issue into a second Windows containment implementation.
pub fn spawn_clipboard_helper(
    cmd: &mut Command,
) -> std::io::Result<(std::process::Child, GroupHandle)> {
    let child = cmd.spawn()?;
    let group = GroupHandle::from_pid(child.id() as i32);
    Ok((child, group))
}

/// A desktop helper together with its owned direct child and optional Job
/// Object. The child remains owned until process-tree cleanup and wait finish.
pub struct DesktopChild {
    child: Child,
    group: GroupHandle,
    state: DesktopChildState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DesktopChildState {
    Live,
    ObservedExited,
    Reaped,
    Uncertain,
}

impl DesktopChild {
    /// Spawn a notifier and retain its Job Object when available; notifiers
    /// explicitly allow degraded direct-child ownership on Job Object failure.
    pub fn spawn(cmd: &mut Command) -> io::Result<Self> {
        let (child, group) = spawn_grouped_best_effort(cmd)?;
        Ok(Self {
            child,
            group,
            state: DesktopChildState::Live,
        })
    }

    /// Observe process exit without consuming the direct-child wait status.
    pub fn poll_exit(&mut self) -> io::Result<bool> {
        match self.state {
            DesktopChildState::ObservedExited => return Ok(true),
            DesktopChildState::Reaped => {
                return Err(io::Error::other("desktop child was already reaped"));
            }
            DesktopChildState::Uncertain => {
                return Err(io::Error::other(
                    "desktop child wait ownership is uncertain",
                ));
            }
            DesktopChildState::Live => {}
        }
        use std::os::windows::io::AsRawHandle;

        // SAFETY: waiting with zero timeout on the owned process handle does
        // not consume its wait state.
        let result = unsafe { WaitForSingleObject(self.child.as_raw_handle() as HANDLE, 0) };
        match result {
            WAIT_OBJECT_0 => {
                self.state = DesktopChildState::ObservedExited;
                Ok(true)
            }
            windows_sys::Win32::Foundation::WAIT_TIMEOUT => Ok(false),
            _ => {
                self.state = DesktopChildState::Uncertain;
                Err(io::Error::last_os_error())
            }
        }
    }

    /// Terminate through the owned Job Object, or through the owned direct
    /// Child on the documented degraded path, then reap that same Child.
    #[expect(
        clippy::disallowed_methods,
        reason = "owned child wait runs only on the dedicated desktop dispatcher thread"
    )]
    pub fn terminate_and_wait(&mut self) -> io::Result<()> {
        match self.state {
            DesktopChildState::Live => {
                self.poll_exit()?;
            }
            DesktopChildState::ObservedExited => {}
            DesktopChildState::Reaped => {
                return Err(io::Error::other("desktop child was already reaped"));
            }
            DesktopChildState::Uncertain => {
                return Err(io::Error::other(
                    "desktop child wait ownership is uncertain",
                ));
            }
        }
        let exited = self.state == DesktopChildState::ObservedExited;
        let termination_error = if let Some(job) = self.group.job.as_ref() {
            // SAFETY: the Job Object handle is owned by this child wrapper.
            (unsafe { TerminateJobObject(job.0, 1) } == 0).then(io::Error::last_os_error)
        } else if exited {
            // The direct child has already exited. There is no owned Job Object
            // to clean up, and Child::kill would spuriously report failure.
            None
        } else {
            // No raw PID fallback: the std Child remains the direct owner.
            self.child.kill().err()
        };
        match self.child.wait() {
            Ok(_) => self.state = DesktopChildState::Reaped,
            Err(error) => {
                self.state = DesktopChildState::Uncertain;
                return Err(error);
            }
        }
        if let Some(error) = termination_error {
            return Err(error);
        }
        Ok(())
    }

    /// Reap only through the still-owned direct child. Used by quarantine and
    /// never attempts a PID or Job Object signal after ownership is uncertain.
    #[expect(
        clippy::disallowed_methods,
        reason = "owned child wait runs only on the dedicated desktop dispatcher thread"
    )]
    pub fn reap_owned(&mut self) -> io::Result<()> {
        if self.state == DesktopChildState::Reaped {
            return Err(io::Error::other("desktop child was already reaped"));
        }
        match self.child.wait() {
            Ok(_) => {
                self.state = DesktopChildState::Reaped;
                Ok(())
            }
            Err(error) => {
                self.state = DesktopChildState::Uncertain;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod desktop_child_tests {
    use super::*;

    #[test]
    fn desktop_child_terminates_owned_job_before_reap() {
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let mut child = DesktopChild::spawn(&mut command).unwrap();
        assert!(child.group.job.is_some());
        child.terminate_and_wait().unwrap();
        assert!(child.poll_exit().is_err());
    }

    #[test]
    fn desktop_child_uses_direct_child_when_job_setup_is_degraded() {
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let child = command.spawn().unwrap();
        let pid = child.id();
        let mut child = DesktopChild {
            child,
            group: GroupHandle { pid, job: None },
            state: DesktopChildState::Live,
        };
        child.terminate_and_wait().unwrap();
        assert!(child.poll_exit().is_err());
    }

    #[test]
    fn degraded_fast_exit_reaps_without_kill() {
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "exit 0"]);
        let child = command.spawn().unwrap();
        let pid = child.id();
        let mut child = DesktopChild {
            child,
            group: GroupHandle { pid, job: None },
            state: DesktopChildState::Live,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut observed_exit = false;
        while std::time::Instant::now() < deadline {
            if child.poll_exit().unwrap() {
                observed_exit = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(observed_exit, "fast child did not reach ObservedExited");
        child.terminate_and_wait().unwrap();
        assert!(child.poll_exit().is_err());
    }
}

/// No `rlimit` on Windows; the fd-limit report prints this as "unlimited".
pub fn rlim_infinity() -> u64 {
    u64::MAX
}

/// macOS-only sysctl; nothing analogous here.
pub fn max_files_per_proc() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    static SPAWN_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The whole tree dies on `terminate()`: spawn `cmd /C ping -n 30 …`
    /// (cmd.exe parent + ping child), terminate the job, and verify the
    /// direct child is gone.
    #[test]
    fn job_terminate_reaps_the_tree() {
        let _guard = SPAWN_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (mut child, group) = spawn_grouped(&mut cmd).expect("spawn under job");
        assert!(group.job.is_some(), "job assignment must succeed on CI");
        group.terminate();
        let status = child.wait().expect("wait");
        assert!(!status.success(), "terminated tree exits nonzero");
    }

    /// KILL_ON_JOB_CLOSE: dropping the last handle (no explicit terminate)
    /// also reaps the tree — the orphan-hygiene guarantee.
    #[test]
    fn dropping_the_last_handle_reaps_the_tree() {
        let _guard = SPAWN_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (child, group) = spawn_grouped(&mut cmd).expect("spawn under job");
        assert!(group.job.is_some(), "job assignment must succeed on CI");
        let pid = child.id() as i64;
        drop(group);
        drop(child); // not reaped via wait(); the job close must kill it
        // The kernel reaps asynchronously; give it a moment.
        for _ in 0..50 {
            if !pid_alive(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("child survived job-handle drop");
    }

    #[test]
    fn forced_assignment_failure_returns_err_and_leaves_no_child() {
        let _guard = SPAWN_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        FAIL_JOB_ASSIGN_FOR_TEST.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        assert!(spawn_grouped(&mut cmd).is_err());
    }

    #[test]
    fn assignment_failure_marker_child_never_runs() {
        let _guard = SPAWN_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = tempfile::tempdir().expect("private marker directory");
        let marker = temp.path().join("marker");
        let marker_arg = marker.to_string_lossy().into_owned();
        let marker_script = format!("echo x> \"{marker_arg}\"");
        let mut marker_cmd = Command::new("cmd.exe");
        marker_cmd.args(["/C", marker_script.as_str()]);
        FAIL_JOB_ASSIGN_FOR_TEST.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok((mut child, group)) = spawn_grouped(&mut marker_cmd) {
            group.terminate();
            child
                .wait()
                .expect("reap unexpectedly started marker child");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!marker.exists(), "suspended child must not run user code");
    }

    #[test]
    fn best_effort_degrades_on_forced_assignment_failure() {
        let _guard = SPAWN_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        FAIL_JOB_ASSIGN_FOR_TEST.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (mut child, group) =
            spawn_grouped_best_effort(&mut cmd).expect("best-effort helper spawn");
        assert!(group.job.is_none());
        group.terminate();
        child.wait().expect("wait for degraded helper");
    }

    #[test]
    fn grandchild_started_immediately_is_in_the_job() {
        let _guard = SPAWN_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut cmd = Command::new("cmd.exe");
        cmd.args([
            "/C",
            "start /B ping -n 30 127.0.0.1 > NUL & ping -n 30 127.0.0.1 > NUL",
        ]);
        let (mut child, group) = spawn_grouped(&mut cmd).expect("spawn under job");
        let job = group.job.as_ref().expect("job assignment must succeed").0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut observed_grandchild = false;
        while std::time::Instant::now() < deadline {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `job` is retained by `group`, and `info` is writable.
            let queried = unsafe {
                QueryInformationJobObject(
                    job,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            } != 0;
            assert!(queried, "query job accounting");
            if info.ActiveProcesses >= 2 {
                observed_grandchild = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            observed_grandchild,
            "grandchild must be assigned before execution"
        );
        group.terminate();
        child.wait().expect("wait for contained parent");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !group.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(group.is_empty(), "job tree must be reaped");
    }
}

/// Compositor shutdown: on Ctrl+C / console close / system shutdown set `flag`
/// and pulse `waker` so the blocking `poll_input` returns and the loop exits
/// gracefully. Must be called inside a tokio runtime.
pub fn install_shutdown_signal(flag: Arc<AtomicBool>, waker: termwiz::terminal::TerminalWaker) {
    tokio::spawn(async move {
        use tokio::signal::windows;
        let (Ok(mut ctrl_c), Ok(mut close), Ok(mut shut)) = (
            windows::ctrl_c(),
            windows::ctrl_close(),
            windows::ctrl_shutdown(),
        ) else {
            return;
        };
        tokio::select! {
            _ = ctrl_c.recv() => {}
            _ = close.recv() => {}
            _ = shut.recv() => {}
        }
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

/// Register daemon console-control events synchronously before the pipe bind.
pub fn register_shutdown_signal() -> impl std::future::Future<Output = ()> + Send {
    let events = (
        tokio::signal::windows::ctrl_c(),
        tokio::signal::windows::ctrl_close(),
        tokio::signal::windows::ctrl_shutdown(),
    );
    async move {
        if let (Ok(mut ctrl_c), Ok(mut close), Ok(mut shut)) = events {
            tokio::select! {
                _ = ctrl_c.recv() => {}
                _ = close.recv() => {}
                _ = shut.recv() => {}
            }
        } else {
            std::future::pending::<()>().await;
        }
    }
}

/// Forward a registered console-control event to the daemon shutdown token.
pub fn spawn_shutdown_notifier(
    shutdown: tokio_util::sync::CancellationToken,
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        signal.await;
        shutdown.cancel();
    });
}

/// Create a file symbolic link `link` → `target` (needs the symlink privilege
/// or Developer Mode; callers treat failure as "unsupported here").
#[allow(dead_code)] // test support: the dispatch done-gate tests build a symlinked artifact
pub fn symlink_file(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

/// Open a regular-file candidate without following a final reparse point.
/// `FILE_FLAG_OPEN_REPARSE_POINT` is the Windows equivalent of Unix
/// `O_NOFOLLOW`; callers still validate the opened descriptor's metadata.
pub fn open_read_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(path)
}
