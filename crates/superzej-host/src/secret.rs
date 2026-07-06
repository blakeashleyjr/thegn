//! Layered secret backend for provider tokens.
//!
//! A provider's `[env.<name>.provider] api_key_env` is a **SecretRef** resolved
//! through a priority chain so a token can live wherever the user wants — and so
//! the setup UI has somewhere to *store* what it collects (before this, tokens
//! were env-vars only, with nowhere to persist a UI-entered value):
//!
//! - `keyring:<account>` — the OS keyring (Secret Service / macOS Keychain /
//!   Windows Credential Manager), via the pure-Rust `keyring` crate. On a headless
//!   box with no Secret Service this fails softly and the caller falls back.
//! - `env:VAR` / `file:PATH` — delegated to
//!   [`superzej_core::config::expand_env_ref`] (unchanged behavior).
//! - a bare string (`FLY_API_TOKEN`) — an **env var name**, matching the historic
//!   `api_key_env` meaning, so existing configs keep working untouched.
//!
//! [`store`] is the inverse: it persists a UI-entered token to the keyring (best),
//! else a `0600` file, and returns the SecretRef string to write into config.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The keyring "service" all superzej secrets live under.
const KEYRING_SERVICE: &str = "superzej";

/// Resolve a [`SecretRef`](self) string to a token value. `None` when the ref is
/// empty or the secret can't be found (a missing env var, unreadable file, or
/// unavailable/absent keyring entry) — callers treat that as "not configured".
pub fn resolve(secret_ref: &str) -> Option<String> {
    let r = secret_ref.trim();
    if r.is_empty() {
        return None;
    }
    if let Some(account) = r.strip_prefix("keyring:") {
        return keyring_get(account.trim());
    }
    if r.starts_with("env:") || r.starts_with("file:") {
        return superzej_core::config::expand_env_ref(r);
    }
    // Bare name → env var (historic `api_key_env` semantics).
    std::env::var(r).ok().filter(|s| !s.trim().is_empty())
}

/// Persist a UI/CLI-entered `token` for `name` (e.g. an env name like `fly-dev`),
/// preferring the OS keyring and falling back to a `0600` file. Returns the
/// SecretRef to store in config (`keyring:<name>` or `file:<path>`), so the token
/// itself never lands in `config.toml`.
pub fn store(name: &str, token: &str) -> Result<String> {
    if keyring_set(name, token).is_ok() {
        return Ok(format!("keyring:{name}"));
    }
    let path = secrets_file(name)?;
    write_private(&path, token)?;
    Ok(format!("file:{}", path.display()))
}

/// Remove a stored secret (best-effort, both backends) — used when an env is
/// deleted. Never errors on a missing entry.
pub fn forget(name: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, name) {
        let _ = entry.delete_credential();
    }
    if let Ok(path) = secrets_file(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether an OS keyring is actually usable here (so the UI can tell the user
/// where a token will land, and tests can skip the keyring leg on a headless CI).
// Wired into the TUI "Add environment" wizard (Layer 3) to show the storage hint.
#[allow(dead_code)]
pub fn keyring_available() -> bool {
    // A round-trip on a throwaway account is the only honest probe.
    let probe = "__superzej_keyring_probe__";
    match keyring::Entry::new(KEYRING_SERVICE, probe) {
        Ok(e) => {
            let ok = e.set_password("1").is_ok();
            if ok {
                let _ = e.delete_credential();
            }
            ok
        }
        Err(_) => false,
    }
}

fn keyring_get(account: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, account)
        .ok()?
        .get_password()
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn keyring_set(account: &str, token: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, account)
        .context("keyring entry")?
        .set_password(token)
        .context("keyring set")
}

/// `$XDG_CONFIG_HOME/superzej/secrets/<name>.token` — alongside the config file,
/// so it moves with `SUPERZEJ_DIR`/XDG isolation used by tests + `just start`.
fn secrets_file(name: &str) -> Result<PathBuf> {
    let cfg = superzej_core::config::Config::path();
    let dir = std::path::Path::new(&cfg)
        .parent()
        .map(|p| p.join("secrets"))
        .context("config path has no parent")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    set_mode(&dir, 0o700);
    // Sanitize so a name never escapes the dir.
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(dir.join(format!("{safe}.token")))
}

fn write_private(path: &std::path::Path, token: &str) -> Result<()> {
    std::fs::write(path, token.trim().as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    set_mode(path, 0o600);
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_env_and_bare_and_empty() {
        // SAFETY: single-threaded test; unique var name.
        unsafe { std::env::set_var("SZ_SECRET_TEST_TOK", "s3cr3t") };
        // bare name → env var
        assert_eq!(resolve("SZ_SECRET_TEST_TOK").as_deref(), Some("s3cr3t"));
        // explicit env: ref
        assert_eq!(resolve("env:SZ_SECRET_TEST_TOK").as_deref(), Some("s3cr3t"));
        // empty / unset
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("SZ_SECRET_DEFINITELY_UNSET_XYZ"), None);
        unsafe { std::env::remove_var("SZ_SECRET_TEST_TOK") };
    }

    #[test]
    fn resolve_file_ref_reads_token() {
        let f = std::env::temp_dir().join(format!("sz-secret-test-{}.tok", std::process::id()));
        std::fs::write(&f, "  filetoken\n").unwrap();
        assert_eq!(
            resolve(&format!("file:{}", f.display())).as_deref(),
            Some("filetoken")
        );
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn keyring_prefix_missing_entry_is_none() {
        // An account we never set resolves to None (never panics, even with no
        // Secret Service available).
        assert_eq!(resolve("keyring:__sz_never_set_account__"), None);
    }
}
