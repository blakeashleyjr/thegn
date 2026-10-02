//! Owner-only file permissions, cross-platform — the "0600 for secrets" seam.
//!
//! Unix is a chmod. Windows has no mode bits; the equivalent is an owner-only
//! DACL, applied through the platform PowerShell ACL API rather than a page of
//! unsafe `SetNamedSecurityInfoW` plumbing. The ACL is rebuilt from an empty,
//! inheritance-protected DACL so pre-existing explicit grants cannot survive.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Open an existing regular file without following its final path component.
///
/// The returned descriptor is checked after opening, so callers can safely
/// read from it without a metadata/open race changing a regular file into a
/// FIFO or device. Unix opens are nonblocking as well as no-follow, preventing
/// a raced FIFO from hanging before that descriptor check.
pub fn open_regular_file_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use nix::fcntl::{OFlag, open};
        use nix::sys::stat::{Mode, SFlag, fstat};

        let fd = open(
            path,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            if error == nix::errno::Errno::ELOOP {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} is a symlink, not a regular file", path.display()),
                )
            } else {
                std::io::Error::from_raw_os_error(error as i32)
            }
        })?;
        let stat = fstat(&fd).map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
        let file_type = SFlag::from_bits_truncate(stat.st_mode);
        if !file_type.contains(SFlag::S_IFREG) {
            let kind = if file_type.contains(SFlag::S_IFIFO) {
                "FIFO"
            } else if file_type.contains(SFlag::S_IFSOCK) {
                "socket"
            } else if file_type.contains(SFlag::S_IFBLK) {
                "block device"
            } else if file_type.contains(SFlag::S_IFCHR) {
                "character device"
            } else if file_type.contains(SFlag::S_IFDIR) {
                "directory"
            } else {
                "non-regular file"
            };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is a {kind}, not a regular file", path.display()),
            ));
        }
        Ok(std::fs::File::from(fd))
    }
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.read(true);
        // Open the reparse point itself; metadata below refuses it instead of
        // allowing a final-component symlink to redirect the read.
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no-follow regular-file opening is unavailable on this platform",
        ))
    }
}

/// Restrict a file at `path` to the owning user: `chmod 0600` on unix; on
/// Windows strip inherited ACEs and grant only the current user full control
/// (a protected DACL containing only the current user with full control).
pub fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    restrict(path, 0o600)
}

/// Restrict a directory at `path` to the owning user (`chmod 0700` on unix —
/// the traverse bit matters; the Windows DACL treatment is identical to files).
pub fn restrict_dir_to_owner(path: &Path) -> std::io::Result<()> {
    restrict(path, 0o700)
}

/// Make a test-only helper executable without leaking platform-specific
/// permission code into a provider fixture. Non-Unix test runners skip shell
/// fixtures because their command format is intentionally Unix-only.
#[cfg(any(test, feature = "test-utils"))]
pub fn make_executable_for_test(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unix shell fixture is unavailable",
        ))
    }
}

/// Create a new file without replacement, restrict it to the owning user, and
/// durably write `bytes`. Unix supplies mode 0600 at creation; other platforms
/// apply their owner-only permission mechanism before content is written.
/// Keeping this platform distinction here lets security-sensitive callers stay
/// substrate-agnostic.
pub fn write_owner_only_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(error) = restrict_to_owner(path) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    if let Err(error) = result {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

/// Make `path` a private (owner-only) real directory, creating it if absent.
///
/// Refuses a symlink or non-directory at `path` (checked with
/// `symlink_metadata` after creation, so a symlink that `create_dir_all`
/// followed is still caught) and, on Unix, a directory owned by another user.
/// A directory we own is tightened to 0700 (Windows: owner-only DACL). Only
/// `path` itself is vetted: ancestors, hardlinks and later replacement of the
/// directory are out of scope (the caller's threat model is a hostile entry
/// inside the directory, not a hostile parent).
pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a symlink, not a directory", path.display()),
        ));
    }
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a directory", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is owned by another user", path.display()),
            ));
        }
    }
    restrict_dir_to_owner(path)
}

/// Atomically replace `path` with owner-only contents.
///
/// The temporary file is created in the destination directory with the strict
/// permissions already applied, so readers can observe either the old private
/// file or the new private file, never a normal-umask intermediate. Windows
/// uses `MoveFileExW` with replace-existing semantics because Rust's standard
/// `rename` does not replace there.
pub fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    restrict_dir_to_owner(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} has no file name", path.display()),
            )
        })?
        .to_string_lossy();
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(
        ".{name}.tmp-{}-{nanos}-{nonce}",
        std::process::id()
    ));
    write_owner_only_new(&temporary, bytes)?;

    #[cfg(windows)]
    let publish = replace_file(&temporary, path);
    #[cfg(not(windows))]
    let publish = std::fs::rename(&temporary, path);
    if let Err(error) = publish {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let success = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The file's unix permission bits (`mode & 0o777`), or `None` on platforms
/// with no mode bits (Windows, where the `restrict_*` calls write an owner-only
/// DACL instead). The read-back companion to `restrict_*`: a caller — or a test
/// — can assert the restriction took without growing a `#[cfg]` of its own, so
/// per-OS knowledge stays inside this seam.
pub fn mode_bits(path: &Path) -> std::io::Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(std::fs::metadata(path)?.permissions().mode() & 0o777))
    }
    #[cfg(not(unix))]
    {
        // Existence still has to hold, so a missing path is an error either way.
        std::fs::metadata(path)?;
        Ok(None)
    }
}

#[cfg_attr(windows, allow(unused_variables))]
fn restrict(path: &Path, unix_mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(unix_mode))
    }
    #[cfg(windows)]
    {
        let powershell = crate::util::which_path("pwsh.exe")
            .or_else(|| crate::util::which_path("powershell.exe"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "PowerShell is required to install an owner-only Windows DACL",
                )
            })?;
        let status = std::process::Command::new(powershell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                windows_acl_script(),
            ])
            // The script is constant. Keeping the path in an environment value
            // avoids command-language interpolation for quotes/metacharacters.
            .env("THEGN_FSPERM_PATH", path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "PowerShell ACL hardening exited {:?} for {}",
                status.code(),
                path.display()
            )))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(windows)]
fn windows_acl_script() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$path = $env:THEGN_FSPERM_PATH
$item = Get-Item -LiteralPath $path
$acl = Get-Acl -LiteralPath $path
$acl.SetAccessRuleProtection($true, $false)
foreach ($rule in @($acl.Access)) { [void]$acl.RemoveAccessRuleAll($rule) }
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$inheritance = [System.Security.AccessControl.InheritanceFlags]::None
if ($item.PSIsContainer) {
  $inheritance = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
}
$rights = [System.Security.AccessControl.FileSystemRights]::FullControl
$propagation = [System.Security.AccessControl.PropagationFlags]::None
$allow = [System.Security.AccessControl.AccessControlType]::Allow
$ownerRule = [System.Security.AccessControl.FileSystemAccessRule]::new($sid, $rights, $inheritance, $propagation, $allow)
$acl.SetOwner($sid)
$acl.SetAccessRule($ownerRule)
Set-Acl -LiteralPath $path -AclObject $acl"#
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn atomic_write_rename_failure_errors_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("target");
        // A non-empty directory at the destination makes rename fail.
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("inner"), b"x").unwrap();
        let res = write_owner_only_atomic(&dest, b"CANARY");
        assert!(res.is_err(), "rename onto a non-empty dir must fail");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["target".to_string()], "no temp left: {names:?}");
    }

    #[test]
    fn ensure_private_dir_refuses_symlink() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }

    #[test]
    fn ensure_private_dir_refuses_file() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(ensure_private_dir(&f).is_err());
    }

    #[test]
    fn ensure_private_dir_tightens_existing_dir() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn ensure_private_dir_creates_fresh_path_0700() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("fresh");
        ensure_private_dir(&d).unwrap();
        assert!(d.is_dir());
        assert_eq!(
            std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn restricts_to_0600_on_unix() {
        let p = std::env::temp_dir().join(format!("thegn-fsperm-{}", std::process::id()));
        std::fs::write(&p, b"secret").unwrap();
        restrict_to_owner(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&p); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn mode_bits_reads_back_the_restriction() {
        let p = std::env::temp_dir().join(format!("thegn-fsperm-read-{}", std::process::id()));
        std::fs::write(&p, b"secret").unwrap();
        restrict_to_owner(&p).unwrap();
        assert_eq!(mode_bits(&p).unwrap(), Some(0o600));
        let _ = std::fs::remove_file(&p); // best-effort: test cleanup: scratch removal must never fail the test
        assert!(mode_bits(&p).is_err(), "a missing path is an error");
    }

    #[test]
    fn nofollow_reader_accepts_regular_files_and_refuses_symlinks() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!(
            "thegn-fsperm-nofollow-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::write(&target, b"calendar").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut bytes = Vec::new();
        open_regular_file_nofollow(&target)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"calendar");
        let error = open_regular_file_nofollow(&link).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nofollow_reader_opens_fifos_nonblocking_then_rejects_by_descriptor() {
        let dir = std::env::temp_dir().join(format!(
            "thegn-fsperm-fifo-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("calendar.ics");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let error = open_regular_file_nofollow(&fifo).unwrap_err();
        assert!(error.to_string().contains("FIFO"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn owner_only_create_is_0600_and_never_replaces() {
        let p = std::env::temp_dir().join(format!(
            "thegn-fsperm-create-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        write_owner_only_new(&p, b"first").unwrap();
        assert_eq!(mode_bits(&p).unwrap(), Some(0o600));
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        assert_eq!(
            write_owner_only_new(&p, b"second").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn owner_only_atomic_replace_remains_0600() {
        let dir = std::env::temp_dir().join(format!(
            "thegn-fsperm-atomic-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret");
        write_owner_only_atomic(&path, b"first").unwrap();
        write_owner_only_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(mode_bits(&path).unwrap(), Some(0o600));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn owner_only_atomic_rejects_paths_without_a_destination_name() {
        let error = write_owner_only_atomic(Path::new("/"), b"secret").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("has no parent directory"));

        let dir = std::env::temp_dir().join(format!(
            "thegn-fsperm-no-name-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let child = dir.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let error = write_owner_only_atomic(&child.join(".."), b"secret").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("has no file name"));
        assert!(std::fs::read_dir(&child).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_atomic_publish_removes_private_temporary_file() {
        let dir = std::env::temp_dir().join(format!(
            "thegn-fsperm-publish-failure-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let destination = dir.join("already-a-directory");
        std::fs::create_dir_all(&destination).unwrap();

        let error = write_owner_only_atomic(&destination, b"secret").unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::IsADirectory
                | std::io::ErrorKind::AlreadyExists
                | std::io::ErrorKind::PermissionDenied
        ));
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![destination.file_name().unwrap()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restricts_dirs_to_0700_on_unix() {
        let d = std::env::temp_dir().join(format!("thegn-fsperm-dir-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        restrict_dir_to_owner(&d).unwrap();
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&d); // best-effort: test cleanup: scratch removal must never fail the test
    }
}
