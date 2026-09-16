//! Owner-only file permissions, cross-platform — the "0600 for secrets" seam.
//!
//! Unix is a chmod. Windows has no mode bits; the equivalent is an owner-only
//! DACL, applied through the platform PowerShell ACL API rather than a page of
//! unsafe `SetNamedSecurityInfoW` plumbing. The ACL is rebuilt from an empty,
//! inheritance-protected DACL so pre-existing explicit grants cannot survive.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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
        return std::fs::set_permissions(path, permissions);
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
