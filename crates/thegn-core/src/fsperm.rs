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
    restrict(path, 0o600, false)
}

/// Restrict a directory at `path` to the owning user (`chmod 0700` on unix —
/// the traverse bit matters; the Windows DACL treatment is identical to files).
pub fn restrict_dir_to_owner(path: &Path) -> std::io::Result<()> {
    restrict(path, 0o700, true)
}

/// Whether `path` is owner-only — the read side of [`restrict_to_owner`] /
/// [`restrict_dir_to_owner`], so callers and tests can assert the contract on
/// either platform instead of reaching for `PermissionsExt` (which does not
/// exist on Windows).
///
/// Unix: no group or other bits are set (`mode & 0o077 == 0`).
/// Windows: the DACL carries no **inherited** ACEs — exactly what
/// `icacls /inheritance:r` establishes. Reading it back through `icacls`
/// mirrors the write path's subprocess approach.
pub fn is_restricted_to_owner(path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        Ok(mode & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("icacls").arg(path).output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "icacls exited {:?} for {}",
                out.status.code(),
                path.display()
            )));
        }
        Ok(icacls_says_owner_only(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(true)
    }
}

/// Decide owner-only-ness from `icacls <path>` output. An ACE is any
/// `PRINCIPAL:(perms)` token; `(I)` marks it inherited. Owner-only means at
/// least one ACE and none inherited.
///
/// Pure so it is unit-tested on every platform (the Linux coverage gate sees
/// the Windows branch's logic even though the syscall side only runs there).
pub(crate) fn icacls_says_owner_only(stdout: &str) -> bool {
    let mut saw_ace = false;
    for line in stdout.lines() {
        // The trailing "Successfully processed N files" summary is not an ACE.
        if line.trim_start().starts_with("Successfully processed") {
            continue;
        }
        if line.contains(":(") {
            saw_ace = true;
            if line.contains("(I)") {
                return false;
            }
        }
    }
    saw_ace
}

/// Whether `path` has already been restricted by this process — Windows only.
///
/// Unix applies the mode with a `chmod` syscall, which is cheap enough to
/// repeat. Windows has no mode bits, so [`restrict`] shells out to `icacls`,
/// and the callers are on *hot* paths: `Db::open` re-applies the DACL on every
/// open, and the host opens the DB ad hoc from hundreds of sites. Measured on
/// an idle release build, that was **29 `icacls.exe` spawns per 20 seconds** —
/// a process create plus LSA/RPC round trip each — and a large share of the
/// ~20%-of-a-core idle burn (see docs/windows-native-audit.md, W2).
///
/// The DACL is persistent filesystem state, so re-applying it to a path this
/// process already restricted is redundant. The trade-off, deliberately taken:
/// if something *else* loosens the permissions mid-run, we will not re-tighten
/// them until the next launch. These are best-effort hardening calls on files
/// thegn itself owns, so once per process per path is the right contract.
#[cfg(windows)]
fn already_restricted(path: &Path) -> bool {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    static DONE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let done = DONE.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = match done.lock() {
        Ok(g) => g,
        // A poisoned lock here is not worth failing a best-effort chmod over.
        Err(p) => p.into_inner(),
    };
    // `insert` returns false when the path was already present.
    !guard.insert(path.to_path_buf())
}

#[cfg_attr(windows, allow(unused_variables))]
fn restrict(path: &Path, unix_mode: u32, is_dir: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(unix_mode))
    }
    #[cfg(windows)]
    {
        if already_restricted(path) {
            return Ok(());
        }
        let user = std::env::var("USERNAME")
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "USERNAME unset"))?;
        // A DIRECTORY grant MUST carry the inheritance flags `(OI)(CI)`
        // (object-inherit, container-inherit). `/inheritance:r` strips the
        // inherited ACEs, so without them the ACL applies to the directory
        // object alone and everything created inside it afterwards lands with
        // an EMPTY DACL — unreadable by anyone, including thegn itself. That is
        // not the unix `chmod 0700` this seam is modelled on, where the mode
        // governs the directory and new files get the process umask.
        //
        // Observed: `$XDG_STATE_HOME/thegn/logs` became inaccessible after the
        // state dir was restricted, so the log file could not be read back.
        let grant = if is_dir {
            format!("{user}:(OI)(CI)F")
        } else {
            format!("{user}:F")
        };
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &grant])
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinct temp path per test — `process::id()` alone collides when two
    /// tests in the same binary run concurrently.
    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("thegn-fsperm-{}-{tag}", std::process::id()))
    }

    #[test]
    fn restricts_files_to_owner_only() {
        let p = tmp("file");
        std::fs::write(&p, b"secret").unwrap();
        restrict_to_owner(&p).unwrap();
        assert!(
            is_restricted_to_owner(&p).unwrap(),
            "file must be owner-only after restrict_to_owner"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn restricts_dirs_to_owner_only() {
        let d = tmp("dir");
        std::fs::create_dir_all(&d).unwrap();
        restrict_dir_to_owner(&d).unwrap();
        assert!(
            is_restricted_to_owner(&d).unwrap(),
            "dir must be owner-only after restrict_dir_to_owner"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Restricting a directory must not orphan what is created inside it.
    ///
    /// The Windows arm strips inherited ACEs, so a grant without `(OI)(CI)`
    /// left every subsequently-created child with an EMPTY DACL — thegn locked
    /// itself out of its own `logs/` directory. Unix has never had this problem
    /// (`chmod 0700` governs the directory; children get the umask), which is
    /// exactly why the bug survived: the seam looked symmetric and wasn't.
    #[test]
    fn a_restricted_dir_still_admits_files_created_inside_it() {
        let d = tmp("dir-inherit");
        std::fs::create_dir_all(&d).unwrap();
        restrict_dir_to_owner(&d).unwrap();

        // Created AFTER the restrict — this is the case that broke.
        let nested = d.join("logs");
        std::fs::create_dir_all(&nested).unwrap();
        let f = nested.join("thegn.log");
        std::fs::write(&f, b"hello").unwrap();

        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "hello",
            "a file created inside a restricted dir must still be readable"
        );
        assert!(
            std::fs::read_dir(&nested).is_ok(),
            "a directory created inside a restricted dir must still be listable"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // The icacls parse is pure, so both platforms exercise it — the Windows
    // branch's decision logic stays covered by the Linux coverage gate.

    #[test]
    fn icacls_owner_only_when_no_inherited_aces() {
        let out = "C:\\s\\token.txt ACCOUNTS\\blakea:(F)\r\n\r\n\
                   Successfully processed 1 files; Failed processing 0 files\r\n";
        assert!(icacls_says_owner_only(out));
    }

    #[test]
    fn icacls_rejects_inherited_aces() {
        let out = "C:\\s\\token.txt ACCOUNTS\\blakea:(F)\r\n\
                   NT AUTHORITY\\SYSTEM:(I)(F)\r\n\
                   BUILTIN\\Administrators:(I)(F)\r\n";
        assert!(
            !icacls_says_owner_only(out),
            "an inherited ACE means the DACL was never stripped"
        );
    }

    #[test]
    fn icacls_rejects_output_with_no_aces() {
        assert!(
            !icacls_says_owner_only(
                "Successfully processed 0 files; Failed processing 1 files\r\n"
            ),
            "no ACE at all is not evidence of an owner-only DACL"
        );
        assert!(!icacls_says_owner_only(""));
    }
}
