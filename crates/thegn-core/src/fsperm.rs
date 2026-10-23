//! Owner-only file permissions, cross-platform — the "0600 for secrets" seam.
//!
//! Unix is a chmod. Windows has no mode bits; the equivalent is an owner-only
//! DACL, applied through the platform PowerShell ACL API rather than a page of
//! unsafe `SetNamedSecurityInfoW` plumbing. The ACL is rebuilt from an empty,
//! inheritance-protected DACL so pre-existing explicit grants cannot survive.

use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "fault points live in the unix branch")
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateWriteFault {
    Fchmod,
    Write,
    DirSync,
}

#[cfg(test)]
thread_local! {
    static PRIVATE_WRITE_FAULT: std::cell::Cell<Option<PrivateWriteFault>> = const { std::cell::Cell::new(None) };
    #[cfg(unix)]
    static DIR_SYNCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct PrivateWriteFaultGuard;

#[cfg(test)]
impl Drop for PrivateWriteFaultGuard {
    fn drop(&mut self) {
        PRIVATE_WRITE_FAULT.set(None);
    }
}

#[cfg(test)]
pub(crate) fn with_private_write_fault<T>(fault: PrivateWriteFault, f: impl FnOnce() -> T) -> T {
    PRIVATE_WRITE_FAULT.set(Some(fault));
    let _guard = PrivateWriteFaultGuard;
    f()
}

#[cfg(all(test, unix))]
fn take_private_write_fault(expected: PrivateWriteFault) -> bool {
    if PRIVATE_WRITE_FAULT.get() == Some(expected) {
        PRIVATE_WRITE_FAULT.set(None);
        true
    } else {
        false
    }
}

#[cfg(all(test, unix))]
fn inject_fchmod() -> std::io::Result<()> {
    if take_private_write_fault(PrivateWriteFault::Fchmod) {
        Err(std::io::Error::other("injected fault"))
    } else {
        Ok(())
    }
}

#[cfg(all(not(test), unix))]
#[inline(always)]
fn inject_fchmod() -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
fn inject_write() -> std::io::Result<()> {
    if take_private_write_fault(PrivateWriteFault::Write) {
        Err(std::io::Error::other("injected fault"))
    } else {
        Ok(())
    }
}

#[cfg(all(test, unix))]
fn inject_dir_sync() -> std::io::Result<()> {
    if take_private_write_fault(PrivateWriteFault::DirSync) {
        Err(std::io::Error::other("injected directory sync fault"))
    } else {
        Ok(())
    }
}

#[cfg(all(not(test), unix))]
#[inline(always)]
fn inject_dir_sync() -> std::io::Result<()> {
    Ok(())
}

fn temp_token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos());
    let nanos = nanos.unwrap_or(0);
    let mut tokens = [0_u64; 2];
    for (lane, token) in tokens.iter_mut().enumerate() {
        let state = std::collections::hash_map::RandomState::new();
        let mut hasher = state.build_hasher();
        std::process::id().hash(&mut hasher);
        nanos.hash(&mut hasher);
        counter.hash(&mut hasher);
        if lane == 0 {
            0_u8.hash(&mut hasher);
        } else {
            1_u8.hash(&mut hasher);
        }
        *token = hasher.finish();
    }
    format!("{:016x}{:016x}", tokens[0], tokens[1])
}

#[cfg(all(not(test), unix))]
#[inline(always)]
fn inject_write() -> std::io::Result<()> {
    Ok(())
}

/// Create a file or directory symlink using the platform's native API.
/// Callers that use this for optional capabilities can treat `Unsupported` or
/// `PermissionDenied` as an unavailable platform feature.
pub fn create_symlink(_target: &Path, _link: &Path, _is_dir: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(_target, _link)
    }
    #[cfg(windows)]
    {
        if _is_dir {
            std::os::windows::fs::symlink_dir(_target, _link)
        } else {
            std::os::windows::fs::symlink_file(_target, _link)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are unavailable on this platform",
        ))
    }
}

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

#[cfg(any(test, feature = "test-utils"))]
pub fn set_mode_for_test(
    #[cfg(unix)] path: &Path,
    #[cfg(unix)] mode: u32,
    #[cfg(not(unix))] _path: &Path,
    #[cfg(not(unix))] _mode: u32,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "file modes are unavailable on this platform",
        ))
    }
}

/// Create a new file without replacement, restrict it to the owning user, and
/// durably write `bytes`. Unix supplies mode 0600 at creation; other platforms
/// apply their owner-only permission mechanism before content is written.
/// Keeping this platform distinction here lets security-sensitive callers stay
/// substrate-agnostic.
pub fn write_owner_only_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let _file = write_owner_only_new_handle(path, bytes)?;
    Ok(())
}

fn write_owner_only_new_handle(path: &Path, bytes: &[u8]) -> std::io::Result<std::fs::File> {
    let mut file = create_owner_only_exclusive(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    if let Err(error) = result {
        cleanup_path_if_same(path, file);
        return Err(error);
    }
    Ok(file)
}

/// Exclusively create (never replacing, never following a final-component
/// link) an owner-only file and return the open handle. The primitive behind a
/// git-style `*.lock` file: creation IS the lock acquisition.
pub fn create_owner_only_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    let restrict = {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
    };
    #[cfg(not(unix))]
    let restrict = restrict_to_owner(path);
    if let Err(error) = restrict {
        cleanup_path_if_same(path, file);
        return Err(error);
    }
    Ok(file)
}

#[cfg(unix)]
fn cleanup_path_if_same(path: &Path, file: std::fs::File) {
    use std::os::unix::fs::MetadataExt;
    let expected = match file.metadata() {
        Ok(metadata) => (metadata.dev(), metadata.ino()),
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "unable to identify private temporary for cleanup");
            return;
        }
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if (metadata.dev(), metadata.ino()) == expected => {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(%error, path = %path.display(), "unable to remove private temporary");
            }
        }
        Ok(_) => {
            tracing::debug!(path = %path.display(), "private temporary changed before cleanup; leaving entry")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "unable to inspect private temporary for cleanup")
        }
    }
}

#[cfg(not(unix))]
fn cleanup_path_if_same(path: &Path, file: std::fs::File) {
    drop(file);
    cleanup_path(path);
}

#[cfg(not(unix))]
fn cleanup_path(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %path.display(), "unable to remove private temporary");
    }
}

#[cfg(unix)]
fn finish_directory_sync(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Ok(()) => {
            #[cfg(test)]
            DIR_SYNCS.set(DIR_SYNCS.get().saturating_add(1));
            Ok(())
        }
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                [nix::libc::EINVAL, nix::libc::ENOTSUP, nix::libc::EOPNOTSUPP].contains(&code)
            }) =>
        {
            tracing::debug!(%error, "filesystem does not support directory sync");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn finish_directory_sync(result: std::io::Result<()>) -> std::io::Result<()> {
    result
}

#[cfg(unix)]
#[allow(
    clippy::unnecessary_cast,
    reason = "stat device and inode fields vary in width across Unix targets"
)]
fn unlink_if_same(dir: &std::os::fd::OwnedFd, name: &str, expected: (u64, u64)) {
    use nix::fcntl::AtFlags;
    use nix::sys::stat::fstatat;
    use nix::unistd::{UnlinkatFlags, unlinkat};
    let inspected = fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW);
    match inspected {
        Ok(stat) if (stat.st_dev as u64, stat.st_ino as u64) == expected => {
            if let Err(error) = unlinkat(dir, name, UnlinkatFlags::NoRemoveDir)
                && error != nix::errno::Errno::ENOENT
            {
                tracing::debug!(%error, name, "unable to remove private temporary");
            }
        }
        Ok(_) => tracing::debug!(
            name,
            "private temporary changed before cleanup; leaving entry"
        ),
        Err(nix::errno::Errno::ENOENT) => {}
        Err(error) => {
            tracing::debug!(%error, name, "unable to inspect private temporary for cleanup")
        }
    }
}

/// Whether `meta` describes a file owned by the current user (always true on
/// platforms without a numeric owner, where the ACL seam applies instead).
pub fn owned_by_current_user(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == nix::unistd::geteuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// Refuse a regular file that has another hardlink name. Security-sensitive
/// destinations that will be atomically replaced use this as a tamper signal:
/// replacement would not update the contents visible through the other name.
/// This policy is Unix only; other platforms accept regular files.
pub fn ensure_single_link_regular_file(meta: &std::fs::Metadata) -> std::io::Result<()> {
    if !meta.is_file() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "regular file has multiple hardlinks",
            ));
        }
    }
    Ok(())
}

/// Number of additional hard-link names for a regular file. Other file types
/// and platforms without link counts report zero.
pub fn extra_hard_links(meta: &std::fs::Metadata) -> u64 {
    if !meta.is_file() {
        return 0;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink().saturating_sub(1)
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Explain that replaced file contents may remain reachable through another
/// hard-link name. The message contains only the path and link count.
pub fn hard_link_residue_note(path: &Path, extra: u64) -> Option<String> {
    (extra > 0).then(|| {
        format!(
            "{} has {extra} other hard link(s); the replaced contents stay readable through that link until it is removed",
            path.display()
        )
    })
}

/// List regular files directly in `dir` that have additional hard-link names.
/// Entry metadata is read without following symlinks.
pub fn hard_linked_files(dir: &Path) -> std::io::Result<Vec<(PathBuf, u64)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut linked = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        let extra = extra_hard_links(&metadata);
        if extra > 0 {
            linked.push((path, extra));
        }
    }
    linked.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(linked)
}

/// Carry an existing config's permissions to its replacement while limiting
/// the result to owner-only access. This keeps deliberate read-only configs
/// read-only and drops group/other access on Unix.
pub fn preserve_owner_only_permissions(
    source: &std::fs::Metadata,
    destination: &Path,
) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(destination)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        permissions.set_mode(source.mode() & 0o600);
    }
    #[cfg(not(unix))]
    {
        permissions.set_readonly(source.permissions().readonly());
    }
    std::fs::set_permissions(destination, permissions)
}

/// Atomically rename `from` over `to` (replacing it, never following a
/// final-component link in `to`). Windows needs `MoveFileExW`.
pub fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        replace_file(from, to)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// Sync the directory entry after an atomic replacement. Windows replacement
/// uses `MOVEFILE_WRITE_THROUGH`; Unix explicitly fsyncs the containing dir.
pub fn sync_parent_dir(_parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(_parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// Make `path` a private (owner-only) real directory, creating it if absent.
///
/// Refuses a symlink or non-directory at `path` (checked with
/// `symlink_metadata` after creation, so a symlink that `create_dir_all`
/// followed is still caught) and, on Unix, a directory owned by another user.
/// A directory we own is tightened to 0700 (Windows: owner-only DACL). Only
/// `path` itself is vetted: ancestors and later replacement of the directory
/// are out of scope. Use [`ensure_private_dir_beneath`] when the ancestor path
/// can be hostile or replaced concurrently.
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

/// A private directory held open so later operations stay anchored to the
/// directory inode even if one of its ancestors is renamed or replaced.
#[derive(Debug)]
pub struct PrivateDir {
    path: PathBuf,
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
}

const PRIVATE_TEMP_PREFIX: &str = ".secret.tmp-";

impl PrivateDir {
    /// Return the path used to open this directory. The held handle remains
    /// authoritative if that path is later renamed or replaced.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create an owner-only temporary file relative to this pinned directory.
    /// The returned name can be passed to [`Self::remove_file`] for cleanup.
    #[cfg(unix)]
    pub fn create_owner_only_temp(&self, bytes: &[u8]) -> std::io::Result<(String, PathBuf)> {
        use nix::fcntl::{OFlag, openat};
        use nix::sys::stat::Mode;

        for _ in 0..8 {
            let name = format!("prompt-{}", temp_token());
            let fd = match openat(
                &self.fd,
                name.as_str(),
                OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_WRONLY
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            ) {
                Ok(fd) => fd,
                Err(nix::errno::Errno::EEXIST) => continue,
                Err(error) => return Err(nix_io_error(error)),
            };
            let mut file = std::fs::File::from(fd);
            if let Err(error) = file.write_all(bytes) {
                if let Err(cleanup_error) = self.remove_file(&name) {
                    tracing::debug!(%cleanup_error, "unable to remove incomplete private temporary file");
                }
                return Err(error);
            }
            return Ok((name.clone(), self.path.join(name)));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique private temporary file",
        ))
    }

    /// Open one regular file without following a leaf symlink, relative to
    /// this held directory handle.
    pub fn open_regular_nofollow(&self, name: &str) -> std::io::Result<std::fs::File> {
        let name_path = single_component(name)?;
        #[cfg(unix)]
        {
            use nix::fcntl::{OFlag, openat};
            use nix::sys::stat::{SFlag, fstat};
            let fd = openat(
                &self.fd,
                name_path,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(nix_io_error)?;
            let stat = fstat(&fd).map_err(nix_io_error)?;
            if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "private directory entry is not a regular file",
                ));
            }
            Ok(std::fs::File::from(fd))
        }
        #[cfg(not(unix))]
        {
            open_regular_file_nofollow(&self.path.join(name_path))
        }
    }

    /// Check for an entry without following a leaf symlink.
    pub fn entry_exists_nofollow(&self, name: &str) -> std::io::Result<bool> {
        let name_path = single_component(name)?;
        #[cfg(unix)]
        {
            use nix::fcntl::AtFlags;
            use nix::sys::stat::fstatat;
            match fstatat(&self.fd, name_path, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(_) => Ok(true),
                Err(nix::errno::Errno::ENOENT) => Ok(false),
                Err(error) => Err(nix_io_error(error)),
            }
        }
        #[cfg(not(unix))]
        {
            match std::fs::symlink_metadata(self.path.join(name_path)) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    /// Remove one file entry relative to the held directory, without following
    /// links. Returns false if the entry was already absent.
    pub fn remove_file(&self, name: &str) -> std::io::Result<bool> {
        let name_path = single_component(name)?;
        #[cfg(unix)]
        {
            use nix::unistd::{UnlinkatFlags, unlinkat};
            match unlinkat(&self.fd, name_path, UnlinkatFlags::NoRemoveDir) {
                Ok(()) => Ok(true),
                Err(nix::errno::Errno::ENOENT) => Ok(false),
                Err(error) => Err(nix_io_error(error)),
            }
        }
        #[cfg(not(unix))]
        {
            match std::fs::remove_file(self.path.join(name_path)) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    /// Create or open a lock file beneath this held private directory without
    /// following a leaf symlink. The file is never truncated.
    pub fn open_or_create_regular_nofollow(&self, name: &str) -> std::io::Result<std::fs::File> {
        let name_path = single_component(name)?;
        #[cfg(unix)]
        {
            use nix::fcntl::{OFlag, openat};
            use nix::sys::stat::{SFlag, fstat};
            let fd = openat(
                &self.fd,
                name_path,
                OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::from_bits_truncate(0o600),
            )
            .map_err(|error| {
                if error == nix::errno::Errno::EISDIR {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "private directory entry is not a regular file",
                    )
                } else {
                    nix_io_error(error)
                }
            })?;
            let stat = fstat(&fd).map_err(nix_io_error)?;
            if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "private directory entry is not a regular file",
                ));
            }
            if stat.st_uid != nix::unistd::geteuid().as_raw() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "private directory entry is owned by another user",
                ));
            }
            Ok(std::fs::File::from(fd))
        }
        #[cfg(not(unix))]
        {
            match std::fs::symlink_metadata(self.path.join(name_path)) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "private directory entry is not a regular file",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.path.join(name_path))
        }
    }

    /// List entry names from this pinned directory. Listing errors and entry
    /// iteration errors are returned to the caller.
    pub fn file_names(&self) -> std::io::Result<Vec<std::ffi::OsString>> {
        #[cfg(unix)]
        let entries = {
            use std::os::fd::AsRawFd;
            std::fs::read_dir(format!("/dev/fd/{}", self.fd.as_raw_fd()))?
        };
        #[cfg(not(unix))]
        let entries = std::fs::read_dir(&self.path)?;
        entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }

    /// Sync directory metadata through the held handle.
    pub fn sync(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            nix::unistd::fsync(&self.fd).map_err(nix_io_error)
        }
        #[cfg(not(unix))]
        {
            sync_parent_dir(&self.path)
        }
    }

    /// Remove stale entries with the requested extension and private temporary
    /// residue while keeping metadata lookup and unlink anchored to this
    /// directory's held handle. The returned count includes removed residue.
    /// Symlinks are always removed, directories and special files are kept,
    /// and regular files are removed only once they reach `max_age`.
    pub fn remove_stale_files(
        &self,
        extension: &str,
        now: std::time::SystemTime,
        max_age: std::time::Duration,
    ) -> usize {
        #[cfg(unix)]
        let entries = {
            use std::os::fd::AsRawFd;

            std::fs::read_dir(format!("/dev/fd/{}", self.fd.as_raw_fd()))
        };
        #[cfg(not(unix))]
        let entries = std::fs::read_dir(&self.path);
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!(%error, "unable to list pinned private directory entries");
                return 0;
            }
        };
        let mut removed = 0;
        for entry in entries {
            let name = match entry {
                Ok(entry) => entry.file_name(),
                Err(error) => {
                    tracing::debug!(%error, "unable to read private directory entry name");
                    continue;
                }
            };
            let private_residue = name
                .to_str()
                .is_some_and(|name| name.starts_with(PRIVATE_TEMP_PREFIX));
            if !private_residue
                && Path::new(&name)
                    .extension()
                    .and_then(std::ffi::OsStr::to_str)
                    != Some(extension)
            {
                continue;
            }
            #[cfg(unix)]
            let stale = {
                use nix::fcntl::AtFlags;
                use nix::sys::stat::{SFlag, fstatat};

                let stat = match fstatat(&self.fd, Path::new(&name), AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(error) => {
                        tracing::debug!(%error, "unable to inspect pinned private directory entry");
                        continue;
                    }
                };
                let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
                if kind == SFlag::S_IFLNK {
                    true
                } else if kind != SFlag::S_IFREG || stat.st_mtime < 0 {
                    false
                } else {
                    let modified = std::time::SystemTime::UNIX_EPOCH.checked_add(
                        std::time::Duration::new(stat.st_mtime as u64, stat.st_mtime_nsec as u32),
                    );
                    modified
                        .and_then(|modified| now.duration_since(modified).ok())
                        .is_some_and(|age| age >= max_age)
                }
            };
            #[cfg(not(unix))]
            let stale = {
                let path = self.path.join(&name);
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => true,
                    Ok(metadata) if metadata.is_file() => metadata
                        .modified()
                        .ok()
                        .and_then(|modified| now.duration_since(modified).ok())
                        .is_some_and(|age| age >= max_age),
                    Ok(_) => false,
                    Err(error) => {
                        tracing::debug!(%error, "unable to inspect private directory entry");
                        false
                    }
                }
            };
            if stale {
                #[cfg(unix)]
                let result = {
                    use nix::unistd::{UnlinkatFlags, unlinkat};
                    unlinkat(&self.fd, Path::new(&name), UnlinkatFlags::NoRemoveDir)
                        .map_err(nix_io_error)
                };
                #[cfg(not(unix))]
                let result = std::fs::remove_file(self.path.join(&name));
                match result {
                    Ok(()) => removed += 1,
                    Err(error) => {
                        tracing::debug!(%error, "unable to remove stale private directory entry");
                    }
                }
            }
        }
        removed
    }

    /// Atomically publish owner-only bytes under one filename in this
    /// directory. The unpredictable same-directory temporary is file-synced,
    /// published through the held directory descriptor, then directory-synced.
    /// Failure cleanup removes only the object this call created. A directory
    /// sync error is returned after publication, so the new bytes may already
    /// be visible even when this method returns an error.
    pub fn write_owner_only_atomic(&self, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let name_path = single_component(name)?;
        #[cfg(unix)]
        {
            use nix::fcntl::{OFlag, openat, renameat};
            use nix::sys::stat::{Mode, fchmod, fstat};
            let name = name_path;
            let temporary = format!("{PRIVATE_TEMP_PREFIX}{}", temp_token());
            let fd = openat(
                &self.fd,
                temporary.as_str(),
                OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_WRONLY
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(nix_io_error)?;
            let stat = fstat(&fd).map_err(nix_io_error)?;
            #[allow(
                clippy::unnecessary_cast,
                reason = "stat device and inode fields vary in width across Unix targets"
            )]
            let expected = (stat.st_dev as u64, stat.st_ino as u64);
            let chmod_result = fchmod(&fd, Mode::from_bits_truncate(0o600))
                .map_err(nix_io_error)
                .and_then(|()| inject_fchmod());
            if let Err(error) = chmod_result {
                unlink_if_same(&self.fd, temporary.as_str(), expected);
                return Err(error);
            }
            let mut file = std::fs::File::from(fd);
            let write_result = file
                .write_all(bytes)
                .and_then(|()| inject_write())
                .and_then(|()| file.sync_all());
            if let Err(error) = write_result {
                unlink_if_same(&self.fd, temporary.as_str(), expected);
                return Err(error);
            }
            use nix::fcntl::AtFlags;
            use nix::sys::stat::{SFlag, fstatat};
            match fstatat(&self.fd, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
                    if kind == SFlag::S_IFREG && stat.st_nlink > 1 {
                        #[allow(
                            clippy::unnecessary_cast,
                            reason = "st_nlink is u16 on macOS, u64 on Linux"
                        )]
                        let extra = (stat.st_nlink as u64).saturating_sub(1);
                        if let Some(note) = hard_link_residue_note(&self.path.join(name), extra) {
                            tracing::warn!(target: "thegn::fsperm", path = %self.path.join(name).display(), "{note}");
                        }
                    }
                }
                Err(nix::errno::Errno::ENOENT) => {}
                Err(error) => {
                    tracing::debug!(%error, "unable to inspect replaced private file for hard links")
                }
            }
            if let Err(error) = renameat(&self.fd, temporary.as_str(), &self.fd, name) {
                unlink_if_same(&self.fd, temporary.as_str(), expected);
                return Err(nix_io_error(error));
            }
            inject_dir_sync()?;
            finish_directory_sync(nix::unistd::fsync(&self.fd).map_err(nix_io_error))?;
            Ok(self.path.join(name))
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name_path);
            write_owner_only_atomic(&path, bytes)?;
            Ok(path)
        }
    }
}

fn single_component(name: &str) -> std::io::Result<&Path> {
    let path = Path::new(name);
    if matches!(
        path.components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    ) {
        Ok(path)
    } else {
        Err(invalid_private_name())
    }
}

#[cfg(all(test, not(unix)))]
mod private_write_fault_tests {
    use super::{PrivateWriteFault, with_private_write_fault};

    #[test]
    fn fault_helper_is_available_on_non_unix_test_builds() {
        with_private_write_fault(PrivateWriteFault::Write, || ());
    }
}

/// Open an existing owner-only directory without following its leaf symlink.
/// The leaf is pinned by the returned handle; ancestors are trusted.
pub fn open_private_dir(path: &Path) -> std::io::Result<PrivateDir> {
    #[cfg(unix)]
    {
        use nix::fcntl::{OFlag, open};
        use nix::sys::stat::{Mode, SFlag, fstat};
        use nix::unistd::geteuid;

        let fd = open(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(map_directory_open_error)?;
        let stat = fstat(&fd).map_err(nix_io_error)?;
        // O_DIRECTORY already enforces this on supported Unix systems; keep
        // the metadata check as defence in depth if the open flags ever change.
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFDIR {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not a directory",
            ));
        }
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "directory is not owned by the current user with owner-only permissions",
            ));
        }
        Ok(PrivateDir {
            path: path.to_path_buf(),
            fd,
        })
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is a symlink or not a directory",
            ));
        }
        Ok(PrivateDir {
            path: path.to_path_buf(),
        })
    }
}

/// Create and open a private directory by walking `rel` beneath `anchor`.
/// The anchor is trusted and may itself resolve through a symlink; each
/// component below it is opened without following links and remains pinned.
pub fn ensure_private_dir_beneath(anchor: &Path, rel: &Path) -> std::io::Result<PrivateDir> {
    validate_relative_directory(rel)?;
    #[cfg(unix)]
    {
        use nix::fcntl::{OFlag, open, openat};
        use nix::sys::stat::{Mode, SFlag, fchmod, fstat, mkdirat};
        use nix::unistd::geteuid;
        use std::os::fd::OwnedFd;

        let components = rel
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut current: OwnedFd = open(
            anchor,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(nix_io_error)?;
        for (index, name) in components.iter().enumerate() {
            let name_path = Path::new(name);
            let opened = match openat(
                &current,
                name_path,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(nix::errno::Errno::ENOENT) => {
                    match mkdirat(&current, name_path, Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                        Err(error) => return Err(nix_io_error(error)),
                    }
                    openat(
                        &current,
                        name_path,
                        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(map_directory_open_error)?
                }
                Err(error) => return Err(map_directory_open_error(error)),
            };
            let stat = fstat(&opened).map_err(nix_io_error)?;
            if !SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFDIR) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} is not a directory", name.to_string_lossy()),
                ));
            }
            if stat.st_uid != geteuid().as_raw() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{} is owned by another user", name.to_string_lossy()),
                ));
            }
            if index + 1 < components.len() && stat.st_mode & 0o022 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{} is group/other writable", name.to_string_lossy()),
                ));
            }
            if index + 1 == components.len() {
                fchmod(&opened, Mode::from_bits_truncate(0o700)).map_err(nix_io_error)?;
            }
            current = opened;
        }
        Ok(PrivateDir {
            path: anchor.join(rel),
            fd: current,
        })
    }
    #[cfg(not(unix))]
    {
        let path = anchor.join(rel);
        ensure_private_dir(&path)?;
        Ok(PrivateDir { path })
    }
}

fn invalid_private_name() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "name must be a single normal path component",
    )
}

fn validate_relative_directory(rel: &Path) -> std::io::Result<()> {
    let components = rel.components().collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "relative directory must contain only normal path components",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn nix_io_error(error: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error as i32)
}

#[cfg(unix)]
fn map_directory_open_error(error: nix::errno::Errno) -> std::io::Error {
    if matches!(error, nix::errno::Errno::ELOOP | nix::errno::Errno::ENOTDIR) {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component is a symlink or not a directory",
        )
    } else {
        nix_io_error(error)
    }
}

/// Atomically replace `path` with owner-only contents using an unpredictable
/// same-directory temporary, file and directory sync, and cleanup limited to
/// the object this call created. A directory sync error is returned after
/// publication, so the new bytes may already be visible when this returns an
/// error.
///
/// The temporary file is created in the destination directory with the strict
/// permissions already applied, so readers can observe either the old private
/// file or the new private file, never a normal-umask intermediate. Windows
/// uses `MoveFileExW` with replace-existing semantics because Rust's standard
/// `rename` does not replace there.
pub fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
    let temporary = parent.join(format!(".{name}.tmp-{}", temp_token()));
    let file = write_owner_only_new_handle(&temporary, bytes)?;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let extra = extra_hard_links(&metadata);
            if let Some(note) = hard_link_residue_note(path, extra) {
                tracing::warn!(target: "thegn::fsperm", path = %path.display(), "{note}");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "unable to inspect replaced file for hard links")
        }
    }

    #[cfg(not(unix))]
    {
        drop(file);
    }
    #[cfg(windows)]
    let publish = replace_file(&temporary, path);
    #[cfg(not(windows))]
    let publish = std::fs::rename(&temporary, path);
    if let Err(error) = publish {
        #[cfg(unix)]
        cleanup_path_if_same(&temporary, file);
        #[cfg(not(unix))]
        cleanup_path(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    {
        inject_dir_sync()?;
    }
    finish_directory_sync(sync_parent_dir(parent))?;
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
    Ok(mode_bits_from_metadata(&std::fs::metadata(path)?))
}

/// The permission bits from already-open file metadata, or `None` on
/// platforms with no mode bits.
pub fn mode_bits_from_metadata(_metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(_metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Set a directory's mode to the mode a normal `mkdir` would receive under
/// the current process umask. This uses a sibling directory as a probe, so it
/// does not need to inspect or temporarily change the process-wide umask.
pub fn restore_normal_directory_mode(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not a directory",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let probe = parent.join(format!(".thegn-dir-mode-{}", temp_token()));
    std::fs::create_dir(&probe)?;
    let mode_result = mode_bits(&probe);
    let remove_result = std::fs::remove_dir(&probe);
    remove_result?;
    let mode = mode_result?;
    if let Some(mode) = mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        {
            let _mode = mode;
        }
    }
    Ok(())
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn private_write_temp_names_carry_no_pid_or_time() {
        let first = temp_token();
        let second = temp_token();
        let is_token = |token: &str| {
            token.len() == 32
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        assert!(is_token(&first));
        assert!(is_token(&second));
        assert_ne!(first, second);

        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        let destination = anchor.path().join("secrets/destination");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("inner"), b"x").unwrap();
        assert!(
            dir.write_owner_only_atomic("destination", b"secret")
                .is_err()
        );
        assert!(
            std::fs::read_dir(anchor.path().join("secrets"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PRIVATE_TEMP_PREFIX))
        );
    }

    #[test]
    fn private_write_syncs_the_directory_after_publish() {
        DIR_SYNCS.set(0);
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        dir.write_owner_only_atomic("secret", b"new").unwrap();
        assert_eq!(DIR_SYNCS.get(), 1);
    }

    #[test]
    fn path_atomic_write_syncs_the_parent_directory() {
        DIR_SYNCS.set(0);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("secret");
        write_owner_only_atomic(&destination, b"new").unwrap();
        assert_eq!(DIR_SYNCS.get(), 1);
    }

    #[test]
    fn private_write_dir_sync_failure_is_reported_after_publish() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        let destination = anchor.path().join("secrets/secret");
        let result = with_private_write_fault(PrivateWriteFault::DirSync, || {
            dir.write_owner_only_atomic("secret", b"published")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"published");
        assert!(
            std::fs::read_dir(anchor.path().join("secrets"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PRIVATE_TEMP_PREFIX))
        );
    }

    #[test]
    fn path_atomic_write_dir_sync_failure_is_reported_after_publish() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("secret");
        let result = with_private_write_fault(PrivateWriteFault::DirSync, || {
            write_owner_only_atomic(&destination, b"published")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"published");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn cleanup_leaves_a_substituted_temporary_untouched() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        let original_path = dir.path.join("temporary");
        let replacement_path = dir.path.join("replacement");
        std::fs::write(&original_path, b"original").unwrap();
        std::fs::write(&replacement_path, b"replacement").unwrap();
        let original = std::fs::metadata(&original_path).unwrap();
        let expected_original = (original.dev(), original.ino());
        std::fs::rename(replacement_path, &original_path).unwrap();
        unlink_if_same(&dir.fd, "temporary", expected_original);
        assert_eq!(std::fs::read(&original_path).unwrap(), b"replacement");

        let replacement = std::fs::metadata(&original_path).unwrap();
        unlink_if_same(&dir.fd, "temporary", (replacement.dev(), replacement.ino()));
        assert!(!original_path.exists());
    }

    #[test]
    fn path_cleanup_leaves_a_substituted_temporary_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let original_path = dir.path().join("temporary");
        let replacement_path = dir.path().join("replacement");
        std::fs::write(&original_path, b"original").unwrap();
        std::fs::write(&replacement_path, b"replacement").unwrap();
        let original = std::fs::File::open(&original_path).unwrap();
        std::fs::rename(replacement_path, &original_path).unwrap();

        cleanup_path_if_same(&original_path, original);

        assert_eq!(std::fs::read(&original_path).unwrap(), b"replacement");
    }

    #[test]
    fn owner_only_exclusive_sets_mode_through_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        let _file = create_owner_only_exclusive(&path).unwrap();
        assert_eq!(mode_bits(&path).unwrap(), Some(0o600));
    }

    #[test]
    fn private_write_fchmod_failure_unlinks_the_temporary() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        let destination = anchor.path().join("secrets/secret.env");
        std::fs::write(&destination, b"OLD").unwrap();

        let result = with_private_write_fault(PrivateWriteFault::Fchmod, || {
            dir.write_owner_only_atomic("secret.env", b"CANARY")
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"OLD");
        let entries = std::fs::read_dir(anchor.path().join("secrets"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            entries
                .iter()
                .all(|name| !name.starts_with(PRIVATE_TEMP_PREFIX))
        );
        for entry in entries {
            assert!(
                !std::fs::read(anchor.path().join("secrets").join(entry))
                    .unwrap()
                    .windows(b"CANARY".len())
                    .any(|window| window == b"CANARY")
            );
        }
    }

    #[test]
    fn private_write_write_failure_unlinks_the_temporary() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        let destination = anchor.path().join("secrets/secret.env");
        std::fs::write(&destination, b"OLD").unwrap();

        let result = with_private_write_fault(PrivateWriteFault::Write, || {
            dir.write_owner_only_atomic("secret.env", b"CANARY")
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"OLD");
        let entries = std::fs::read_dir(anchor.path().join("secrets"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            entries
                .iter()
                .all(|name| !name.starts_with(PRIVATE_TEMP_PREFIX))
        );
        for entry in entries {
            assert!(
                !std::fs::read(anchor.path().join("secrets").join(entry))
                    .unwrap()
                    .windows(b"CANARY".len())
                    .any(|window| window == b"CANARY")
            );
        }
    }

    #[test]
    fn extra_hard_links_counts_only_links_beyond_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            extra_hard_links(&std::fs::symlink_metadata(&file).unwrap()),
            0
        );
        assert_eq!(
            extra_hard_links(&std::fs::symlink_metadata(dir.path()).unwrap()),
            0
        );
        std::fs::hard_link(&file, dir.path().join("linked")).unwrap();
        assert_eq!(
            extra_hard_links(&std::fs::symlink_metadata(&file).unwrap()),
            1
        );
    }

    #[test]
    fn hard_link_residue_note_only_describes_additional_links() {
        assert_eq!(hard_link_residue_note(Path::new("secret.env"), 0), None);
        let note = hard_link_residue_note(Path::new("secret.env"), 2).unwrap();
        assert!(note.contains("2 other hard link"));
        assert!(note.contains("secret.env"));
    }

    #[test]
    fn hard_linked_files_lists_only_regular_files_with_external_links() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"single").unwrap();
        std::fs::write(&b, b"linked").unwrap();
        std::fs::hard_link(&b, outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(&b, dir.path().join("d")).unwrap();
        assert_eq!(hard_linked_files(dir.path()).unwrap(), vec![(b, 1)]);
        assert!(
            hard_linked_files(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn private_atomic_replace_preserves_old_contents_through_other_hard_link() {
        let anchor = tempfile::tempdir().unwrap();
        let private = ensure_private_dir_beneath(anchor.path(), Path::new("p")).unwrap();
        let old_link = anchor.path().join("outside");
        private.write_owner_only_atomic("env", b"old").unwrap();
        std::fs::hard_link(private.path.join("env"), &old_link).unwrap();
        private.write_owner_only_atomic("env", b"new").unwrap();
        assert_eq!(std::fs::read(private.path.join("env")).unwrap(), b"new");
        assert_eq!(std::fs::read(old_link).unwrap(), b"old");
    }

    #[test]
    fn path_atomic_replace_preserves_old_contents_through_other_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        let old_link = dir.path().join("outside");
        std::fs::write(&path, b"old").unwrap();
        std::fs::hard_link(&path, &old_link).unwrap();
        write_owner_only_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"new");
        assert_eq!(std::fs::read(old_link).unwrap(), b"old");
    }

    #[test]
    fn private_dir_beneath_creates_owner_only_components() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("a/b")).unwrap();
        assert_eq!(mode_bits(&anchor.path().join("a/b")).unwrap(), Some(0o700));
        assert_eq!(mode_bits(&dir.path).unwrap(), Some(0o700));
    }

    #[test]
    fn private_dir_beneath_allows_a_trusted_symlinked_anchor() {
        let anchor = tempfile::tempdir().unwrap();
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("anchor");
        std::os::unix::fs::symlink(anchor.path(), &link).unwrap();
        ensure_private_dir_beneath(&link, Path::new("secrets")).unwrap();
        assert_eq!(
            mode_bits(&anchor.path().join("secrets")).unwrap(),
            Some(0o700)
        );
    }

    #[test]
    fn private_dir_beneath_refuses_writable_intermediate_directory() {
        let anchor = tempfile::tempdir().unwrap();
        let intermediate = anchor.path().join("a");
        std::fs::create_dir(&intermediate).unwrap();
        std::fs::set_permissions(&intermediate, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            ensure_private_dir_beneath(anchor.path(), Path::new("a/b"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!intermediate.join("b").exists());
    }

    #[test]
    fn private_dir_beneath_refuses_symlinked_components_and_files() {
        let anchor = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), anchor.path().join("a")).unwrap();
        assert_eq!(
            ensure_private_dir_beneath(anchor.path(), Path::new("a/b"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

        std::fs::remove_file(anchor.path().join("a")).unwrap();
        std::fs::create_dir(anchor.path().join("a")).unwrap();
        std::os::unix::fs::symlink(outside.path(), anchor.path().join("a/b")).unwrap();
        assert_eq!(
            ensure_private_dir_beneath(anchor.path(), Path::new("a/b"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );

        std::fs::remove_file(anchor.path().join("a/b")).unwrap();
        std::fs::remove_dir(anchor.path().join("a")).unwrap();
        std::fs::write(anchor.path().join("a"), b"file").unwrap();
        assert_eq!(
            ensure_private_dir_beneath(anchor.path(), Path::new("a/b"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn private_dir_beneath_rejects_non_normal_relative_paths() {
        let anchor = tempfile::tempdir().unwrap();
        for rel in [Path::new(""), Path::new(".."), Path::new("/absolute")] {
            assert_eq!(
                ensure_private_dir_beneath(anchor.path(), rel)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn private_dir_write_stays_anchored_after_parent_replacement() {
        let anchor = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("a/b")).unwrap();
        std::fs::rename(anchor.path().join("a"), anchor.path().join("a_moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), anchor.path().join("a")).unwrap();

        // A path-based write_owner_only_atomic at anchor/a/b would follow the replacement symlink.
        let published = dir
            .write_owner_only_atomic("secret.env", b"CANARY")
            .unwrap();
        let pinned = anchor.path().join("a_moved/b/secret.env");
        assert_eq!(published, anchor.path().join("a/b/secret.env"));
        assert_eq!(std::fs::read(pinned).unwrap(), b"CANARY");
        assert_eq!(
            mode_bits(&anchor.path().join("a_moved/b/secret.env")).unwrap(),
            Some(0o600)
        );
        assert!(!outside.path().join("secret.env").exists());
    }

    #[test]
    fn private_dir_stale_sweep_stays_anchored_after_parent_replacement() {
        let anchor = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        std::fs::write(anchor.path().join("secrets/old.env"), b"pinned").unwrap();
        std::fs::write(outside.path().join("different.env"), b"outside").unwrap();
        let moved = anchor.path().join("secrets_moved");
        std::fs::rename(anchor.path().join("secrets"), &moved).unwrap();
        std::os::unix::fs::symlink(outside.path(), anchor.path().join("secrets")).unwrap();

        assert_eq!(
            dir.remove_stale_files(
                "env",
                std::time::SystemTime::now(),
                std::time::Duration::ZERO,
            ),
            1
        );
        assert!(!moved.join("old.env").exists());
        assert_eq!(
            std::fs::read(outside.path().join("different.env")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn private_dir_stale_sweep_keeps_unix_socket() {
        use std::os::unix::net::UnixListener;

        let parent = tempfile::tempdir().unwrap();
        let dir_path = parent.path().join("private");
        std::fs::create_dir(&dir_path).unwrap();
        std::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let dir = ensure_private_dir_beneath(parent.path(), Path::new("private")).unwrap();
        let socket_path = dir_path.join("sock.env");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        assert_eq!(
            dir.remove_stale_files(
                "env",
                std::time::SystemTime::now(),
                std::time::Duration::ZERO,
            ),
            0
        );
        assert!(socket_path.exists());
    }

    #[test]
    fn private_dir_write_replaces_leaf_symlink_without_touching_victim() {
        let anchor = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        std::os::unix::fs::symlink(&victim, anchor.path().join("secrets/secret.env")).unwrap();
        dir.write_owner_only_atomic("secret.env", b"CANARY")
            .unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(
            std::fs::read(anchor.path().join("secrets/secret.env")).unwrap(),
            b"CANARY"
        );
    }

    #[test]
    fn private_dir_write_replaces_fifo_without_opening_it() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        let fifo = anchor.path().join("secrets/secret.env");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        dir.write_owner_only_atomic("secret.env", b"CANARY")
            .unwrap();
        assert_eq!(std::fs::read(&fifo).unwrap(), b"CANARY");
        assert!(std::fs::metadata(&fifo).unwrap().is_file());
    }

    #[test]
    fn private_dir_write_cleans_temporary_file_when_destination_is_directory() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        std::fs::create_dir(anchor.path().join("secrets/secret.env")).unwrap();
        assert!(
            dir.write_owner_only_atomic("secret.env", b"CANARY")
                .is_err()
        );
        assert_eq!(
            std::fs::read_dir(anchor.path().join("secrets"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn private_dir_write_rejects_non_component_names() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("secrets")).unwrap();
        for name in ["", "../secret", "nested/name", "/absolute"] {
            assert_eq!(
                dir.write_owner_only_atomic(name, b"CANARY")
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn single_link_regular_file_policy_accepts_one_link_and_rejects_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let linked = dir.path().join("linked");
        std::fs::write(&original, b"secret").unwrap();
        ensure_single_link_regular_file(&std::fs::metadata(&original).unwrap()).unwrap();
        ensure_single_link_regular_file(&std::fs::metadata(dir.path()).unwrap()).unwrap();
        std::fs::hard_link(&original, &linked).unwrap();
        let error = ensure_single_link_regular_file(&std::fs::metadata(&original).unwrap())
            .expect_err("multiple hardlinks must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn carried_permissions_keep_read_only_and_drop_group_access() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"old").unwrap();
        std::fs::write(&destination, b"new").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o666)).unwrap();

        let source_metadata = std::fs::metadata(&source).unwrap();
        preserve_owner_only_permissions(&source_metadata, &destination).unwrap();

        let mode = std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o400);
    }

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
    fn restore_normal_directory_mode_uses_current_umask() {
        let temp = tempfile::tempdir().unwrap();
        let normal = temp.path().join("normal");
        let private = temp.path().join("private");
        std::fs::create_dir(&normal).unwrap();
        ensure_private_dir(&private).unwrap();
        assert_eq!(mode_bits(&private).unwrap(), Some(0o700));
        restore_normal_directory_mode(&private).unwrap();
        assert_eq!(mode_bits(&private).unwrap(), mode_bits(&normal).unwrap());
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

    #[test]
    fn private_dir_stale_sweep_removes_old_crash_residue_but_keeps_fresh() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("private")).unwrap();
        let residue = dir.path.join(".secret.tmp-1-2-3");
        let unrelated = dir.path.join("keep.txt");
        std::fs::write(&residue, b"temporary secret").unwrap();
        std::fs::set_permissions(&residue, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        assert_eq!(
            dir.remove_stale_files(
                "env",
                std::time::SystemTime::now(),
                std::time::Duration::from_secs(60 * 60),
            ),
            0
        );
        assert!(residue.exists(), "fresh crash residue must remain");
        assert!(unrelated.exists());

        assert_eq!(
            dir.remove_stale_files(
                "env",
                std::time::SystemTime::now() + std::time::Duration::from_secs(2 * 60 * 60),
                std::time::Duration::from_secs(60 * 60),
            ),
            1
        );
        assert!(!residue.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn private_dir_stale_sweep_keeps_fifo_and_directory_residue() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("private")).unwrap();
        let residue_fifo = dir.path.join(".secret.tmp-9-9-9");
        let env_fifo = dir.path.join("pipe.env");
        let env_directory = dir.path.join("nested.env");
        nix::unistd::mkfifo(
            &residue_fifo,
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        nix::unistd::mkfifo(&env_fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        std::fs::create_dir(&env_directory).unwrap();

        assert_eq!(
            dir.remove_stale_files(
                "env",
                std::time::SystemTime::now(),
                std::time::Duration::ZERO
            ),
            0
        );
        assert!(residue_fifo.exists());
        assert!(env_fifo.exists());
        assert!(env_directory.is_dir());
    }

    #[test]
    fn private_dir_concurrent_writers_never_tear_or_leak() {
        let anchor = tempfile::tempdir().unwrap();
        let dir = ensure_private_dir_beneath(anchor.path(), Path::new("private")).unwrap();
        let destination = dir.path.join("secret.env");
        let writing = std::sync::atomic::AtomicBool::new(true);

        std::thread::scope(|scope| {
            let dir = &dir;
            let destination = &destination;
            let writing = &writing;
            let reader = scope.spawn(move || {
                while writing.load(std::sync::atomic::Ordering::Acquire) {
                    match std::fs::read(destination) {
                        Ok(bytes) => {
                            assert_eq!(bytes.len(), 64 * 1024);
                            assert!(bytes.iter().all(|byte| *byte == bytes[0]));
                            assert!((b'A'..=b'D').contains(&bytes[0]));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => panic!("unable to read published secret: {error}"),
                    }
                }
            });
            let mut writers = Vec::new();
            for writer in 0..4 {
                let payload = vec![b'A' + writer; 64 * 1024];
                writers.push(scope.spawn(move || {
                    for _ in 0..50 {
                        dir.write_owner_only_atomic("secret.env", &payload).unwrap();
                    }
                }));
            }
            for writer in writers {
                writer.join().unwrap();
            }
            writing.store(false, std::sync::atomic::Ordering::Release);
            reader.join().unwrap();
        });

        let content = std::fs::read(&destination).unwrap();
        assert_eq!(content.len(), 64 * 1024);
        assert!(content.iter().all(|byte| *byte == content[0]));
        assert!((b'A'..=b'D').contains(&content[0]));
        assert_eq!(std::fs::read_dir(&dir.path).unwrap().count(), 1);
        assert_eq!(mode_bits(&destination).unwrap(), Some(0o600));
    }

    #[test]
    fn path_atomic_write_replaces_fifo_and_symlink_leaves_without_following() {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let fifo = parent.path().join("secret");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        write_owner_only_atomic(&fifo, b"from fifo replacement").unwrap();
        assert_eq!(std::fs::read(&fifo).unwrap(), b"from fifo replacement");
        assert!(std::fs::metadata(&fifo).unwrap().is_file());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);

        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        std::fs::remove_file(&fifo).unwrap();
        std::os::unix::fs::symlink(&victim, &fifo).unwrap();
        write_owner_only_atomic(&fifo, b"from symlink replacement").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(std::fs::read(&fifo).unwrap(), b"from symlink replacement");
        assert!(std::fs::metadata(&fifo).unwrap().is_file());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
    }

    #[test]
    fn open_private_dir_refuses_a_symlink_leaf_and_group_bits() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        ensure_private_dir(&private).unwrap();
        let link = root.path().join("link");
        create_symlink(&private, &link, true).unwrap();
        assert!(open_private_dir(&link).is_err());

        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o750)).unwrap();
        let error = open_private_dir(&private).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn private_dir_ops_stay_anchored_after_the_path_is_swapped() {
        use std::io::Read;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secrets = root.path().join("secrets");
        ensure_private_dir(&secrets).unwrap();
        let pinned = open_private_dir(&secrets).unwrap();
        assert_eq!(pinned.path(), secrets.as_path());
        pinned
            .write_owner_only_atomic("present", b"inside")
            .unwrap();
        std::fs::write(outside.path().join("x"), b"canary").unwrap();

        std::fs::rename(&secrets, root.path().join("moved")).unwrap();
        create_symlink(outside.path(), &secrets, true).unwrap();

        assert!(!pinned.remove_file("x").unwrap());
        assert_eq!(std::fs::read(outside.path().join("x")).unwrap(), b"canary");
        assert!(pinned.entry_exists_nofollow("present").unwrap());
        assert!(pinned.file_names().unwrap().contains(&"present".into()));
        let mut content = Vec::new();
        pinned
            .open_regular_nofollow("present")
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, b"inside");
        assert!(pinned.remove_file("present").unwrap());
        assert!(!pinned.entry_exists_nofollow("present").unwrap());
        pinned.sync().unwrap();
        assert!(pinned.open_regular_nofollow("../x").is_err());
        assert!(pinned.entry_exists_nofollow("../x").is_err());
        assert!(pinned.remove_file("../x").is_err());
        pinned
            .write_owner_only_atomic("y", b"inside after swap")
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("moved/y")).unwrap(),
            b"inside after swap"
        );
        assert!(!outside.path().join("y").exists());
        std::fs::write(outside.path().join("target"), b"outside").unwrap();
        create_symlink(
            &outside.path().join("target"),
            &root.path().join("moved/link"),
            false,
        )
        .unwrap();
        assert!(pinned.open_regular_nofollow("link").is_err());
    }

    #[test]
    fn open_regular_nofollow_rejects_a_directory_entry() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        ensure_private_dir(&private).unwrap();
        std::fs::create_dir(private.join("child")).unwrap();
        let pinned = open_private_dir(&private).unwrap();

        let error = pinned.open_regular_nofollow("child").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn open_or_create_regular_nofollow_creates_an_owner_only_file_once() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        ensure_private_dir(&private).unwrap();
        let pinned = open_private_dir(&private).unwrap();

        let mut first = pinned.open_or_create_regular_nofollow("lock").unwrap();
        first.write_all(b"x").unwrap();
        let _second = pinned.open_or_create_regular_nofollow("lock").unwrap();

        assert_eq!(std::fs::metadata(private.join("lock")).unwrap().len(), 1);
        assert!(
            mode_bits(&private.join("lock"))
                .unwrap()
                .is_none_or(|mode| mode & 0o777 == 0o600)
        );
    }

    #[test]
    fn open_or_create_regular_nofollow_refuses_a_symlink_and_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        ensure_private_dir(&private).unwrap();
        std::fs::create_dir(private.join("child")).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"untouched").unwrap();
        let pinned = open_private_dir(&private).unwrap();

        assert_eq!(
            pinned
                .open_or_create_regular_nofollow("child")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(pinned.open_or_create_regular_nofollow("../x").is_err());
        if let Err(error) = create_symlink(&outside, &private.join("link"), false) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create test symlink: {error}");
        }

        assert!(pinned.open_or_create_regular_nofollow("link").is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
    }

    #[test]
    fn set_mode_for_test_round_trips_mode_bits() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mode-test");
        std::fs::write(&path, b"fixture").unwrap();
        match set_mode_for_test(&path, 0o640) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return,
            Err(error) => panic!("could not set fixture mode: {error}"),
        }
        assert_eq!(mode_bits(&path).unwrap(), Some(0o640));
    }
}
