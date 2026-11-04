use anyhow::{Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Injectable so plugins can test credentials without a desktop secret service.
/// No plaintext file fallback is provided.
pub trait CredentialStore {
    fn load(&self, account: &str) -> Result<String>;
    fn save(&self, account: &str, secret: &str) -> Result<()>;
    fn remove(&self, account: &str) -> Result<()>;
}
pub struct OsCredentialStore {
    service: String,
}
impl OsCredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
    pub fn load_optional(&self, account: &str) -> Result<Option<String>> {
        match self.entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("credential store unavailable; reconnect this account"),
        }
    }
    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        if self.service.is_empty() || account.is_empty() {
            bail!("credential namespace is empty");
        }
        Ok(keyring::Entry::new(&self.service, account)?)
    }
}
impl CredentialStore for OsCredentialStore {
    fn load(&self, account: &str) -> Result<String> {
        self.entry(account)?
            .get_password()
            .map_err(|_| anyhow::anyhow!("credential store unavailable; reconnect this account"))
    }
    fn save(&self, account: &str, secret: &str) -> Result<()> {
        self.entry(account)?
            .set_password(secret)
            .map_err(|_| anyhow::anyhow!("cannot save credentials in the OS credential store"))
    }
    fn remove(&self, account: &str) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => bail!("cannot remove stored credentials"),
        }
    }
}

pub trait Browser {
    fn open(&self, url: &url::Url) -> Result<()>;
}
pub struct OsBrowser;
impl Browser for OsBrowser {
    fn open(&self, url: &url::Url) -> Result<()> {
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("browser helper requires an HTTPS URL without embedded credentials");
        }
        let mut command = crate::platform::opener()
            .ok_or_else(|| anyhow::anyhow!("no browser opener on this platform"))?;
        let mut child = command
            .arg(url.as_str())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| anyhow::anyhow!("cannot open authorization browser"))?;
        // Each open call starts a helper that waits for this opener to exit. A
        // desktop opener that stays alive with the browser keeps that helper alive
        // too. It intentionally has no QoS class: qos::set_self is a thegn-host
        // item, not an SDK API.
        drop(thegn_core::thread_name::spawn_named(
            "thegn-sdk-reap",
            move || {
                if let Err(error) = child.wait() {
                    panic!("could not reap authorization browser process: {error}");
                }
            },
        ));
        Ok(())
    }
}

/// Plugin-owned private data, rooted at a host-assigned directory. Filenames
/// are single components; this is intentionally not a general filesystem API.
pub struct PrivateState {
    dir: thegn_core::fsperm::PrivateDir,
}
impl PrivateState {
    pub fn from_host() -> Result<Self> {
        // Keep this literal aligned with the host contract without depending on
        // thegn-svc, which is deliberately outside the plugin SDK dependency set.
        let root = std::env::var_os("THEGN_PLUGIN_STATE_DIR")
            .ok_or_else(|| anyhow::anyhow!("host omitted plugin state directory"))?;
        Self::new(PathBuf::from(root))
    }
    pub fn new(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            bail!("plugin state directory must be absolute");
        }
        thegn_core::fsperm::ensure_private_dir(&root)?;
        let dir = thegn_core::fsperm::open_private_dir(&root)?;
        Ok(Self { dir })
    }
    pub fn root(&self) -> &Path {
        self.dir.path()
    }
    fn check_name(name: &str) -> Result<()> {
        let stem = name
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches([' ', '.'])
            .to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(
                    &stem[3..],
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                );
        if reserved
            || name.ends_with([' ', '.'])
            || name.is_empty()
            || name.len() > 128
            || name == "."
            || name == ".."
            || name
                .chars()
                .any(|c| c == '/' || c == '\\' || c == ':' || c.is_control())
        {
            bail!("invalid private-state filename");
        }
        Ok(())
    }
    pub fn read(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        Self::check_name(name)?;
        let file = match self.dir.open_regular_nofollow(name) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            bail!("private-state file exceeds size limit");
        }
        Ok(Some(bytes))
    }
    pub fn write(&self, name: &str, bytes: &[u8], limit: usize) -> Result<()> {
        if bytes.len() > limit {
            bail!("private-state value exceeds size limit");
        }
        Self::check_name(name)?;
        // Atomic publish renames over the leaf. Check first so it cannot silently
        // replace a planted symlink or a directory where the old path API refused.
        match self.dir.open_regular_nofollow(name) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.dir.write_owner_only_atomic(name, bytes)?;
        Ok(())
    }
    /// Keep a stable lock file for the lifetime of the process. Never unlink a
    /// locked file: another process could otherwise lock a replacement inode.
    pub fn lease(&self, name: &str) -> Result<ProcessLease> {
        Self::check_name(name)?;
        let file = self.dir.open_or_create_regular_nofollow(name)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("another plugin process is using this state directory"))?;
        Ok(ProcessLease { _file: file })
    }
}

pub struct ProcessLease {
    _file: std::fs::File,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn private_state() -> (tempfile::TempDir, PrivateState) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("state");
        (temp, PrivateState::new(root).unwrap())
    }

    #[test]
    fn private_state_bounds_paths_and_content() {
        let (_temp, state) = private_state();
        let root = state.root().to_path_buf();
        assert!(state.write("../escape", b"bad", 16).is_err());
        assert!(state.write("cache", b"oversized", 3).is_err());
        state.write("cache", b"one", 16).unwrap();
        state.write("cache", b"two", 16).unwrap();
        assert_eq!(state.read("cache", 3).unwrap().unwrap(), b"two");
        assert!(state.read("cache", 2).is_err());
        if let Some(mode) = thegn_core::fsperm::mode_bits(&root.join("cache")).unwrap() {
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn private_state_filenames_are_portable_and_cannot_escape_on_windows() {
        let (_temp, state) = private_state();
        for name in [
            "C:outside",
            "cache:stream",
            "CON",
            "nul.txt",
            "COM1",
            "LPT9.log",
            "cache.",
            "cache ",
        ] {
            assert!(state.read(name, 100).is_err());
            assert!(state.write(name, b"value", 100).is_err());
            assert!(state.lease(name).is_err());
        }
        assert!(std::fs::read_dir(state.root()).unwrap().next().is_none());
    }

    #[test]
    fn process_lease_excludes_a_second_owner_then_releases() {
        let (_temp, state) = private_state();
        let lease = state.lease("plugin.lock").unwrap();
        assert!(state.lease("plugin.lock").is_err());
        drop(lease);
        assert!(state.lease("plugin.lock").is_ok());
    }

    #[test]
    fn private_state_rejects_symlinks_without_touching_the_target() {
        let (_temp, state) = private_state();
        let root = state.root().to_path_buf();
        let target = root.join("target");
        let file_link = root.join("cache");
        std::fs::write(&target, b"original").unwrap();
        if thegn_core::fsperm::create_symlink(&target, &file_link, false).is_err() {
            // Some Windows environments do not grant symlink creation privileges.
            return;
        }
        assert!(state.read("cache", 100).is_err());
        assert!(state.write("cache", b"changed", 100).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");

        let root_link = root.with_extension("link");
        if thegn_core::fsperm::create_symlink(&root, &root_link, true).is_err() {
            // Keep the already-exercised leaf-link assertions portable.
            return;
        }
        assert!(PrivateState::new(root_link).is_err());
    }

    #[test]
    fn private_state_write_refuses_a_directory_in_the_way() {
        let (_temp, state) = private_state();
        let blocked = state.root().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let error = state.write("blocked", b"value", 100).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("regular file")
                || message.contains("no-follow regular-file opening is unavailable"),
            "directory leaf should be refused by the no-follow regular-file check: {message}"
        );
        assert!(std::fs::metadata(blocked).unwrap().is_dir());
    }

    #[test]
    fn browser_rejects_unsafe_urls_before_spawning() {
        for url in [
            "file:///etc/passwd",
            "http://example.com",
            "https://user:secret@example.com",
        ] {
            assert!(OsBrowser.open(&url::Url::parse(url).unwrap()).is_err());
        }
    }
}
