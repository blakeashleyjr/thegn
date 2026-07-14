//! Owner-only file permissions, cross-platform — the "0600 for secrets" seam.
//!
//! Unix is a chmod. Windows has no mode bits; the equivalent is an owner-only
//! DACL, applied via `icacls` (ships with Windows) rather than a page of
//! unsafe `SetNamedSecurityInfoW` plumbing — this matches the repo's
//! subprocess-fallback philosophy, and the callers are all best-effort
//! secret-file writes (keyring/Credential Manager is the primary store).

use std::path::Path;

/// Restrict a file at `path` to the owning user: `chmod 0600` on unix; on
/// Windows strip inherited ACEs and grant only the current user full control
/// (`icacls /inheritance:r /grant:r <user>:F`).
pub fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    restrict(path, 0o600)
}

/// Restrict a directory at `path` to the owning user (`chmod 0700` on unix —
/// the traverse bit matters; the Windows DACL treatment is identical to files).
pub fn restrict_dir_to_owner(path: &Path) -> std::io::Result<()> {
    restrict(path, 0o700)
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
        let user = std::env::var("USERNAME")
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "USERNAME unset"))?;
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("{user}:F")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "icacls exited {:?} for {}",
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
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn restricts_dirs_to_0700_on_unix() {
        let d = std::env::temp_dir().join(format!("thegn-fsperm-dir-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        restrict_dir_to_owner(&d).unwrap();
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&d);
    }
}
