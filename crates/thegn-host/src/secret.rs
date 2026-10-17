//! Layered secret backend for provider tokens.
//!
//! A provider's `[env.<name>.provider] api_key_env` is a **SecretRef** resolved
//! through a priority chain so a token can live wherever the user wants — and so
//! the setup UI has somewhere to *store* what it collects (before this, tokens
//! were env-vars only, with nowhere to persist a UI-entered value):
//!
//! - `keyring:<account>` — the OS keyring (Secret Service / macOS Keychain /
//!   Windows Credential Manager), via the pure-Rust `keyring` crate. On a headless
//!   box with no Secret Service explicit reads report unavailable; broker writes
//!   may choose the owner-only file backend instead.
//! - `env:VAR` / `file:PATH` — delegated to
//!   [`thegn_core::config::expand_env_ref`] (unchanged behavior).
//! - a bare string (`FLY_API_TOKEN`) — an **env var name**, matching the historic
//!   `api_key_env` meaning, so existing configs keep working untouched.
//!
//! [`store`] is the inverse: it persists a UI-entered token to the keyring (best),
//! else a `0600` file, and returns the SecretRef string to write into config.

use std::path::PathBuf;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use thegn_core::secret_audit::{SecretAudit, SecretOutcome};
use thegn_core::secret_store::{SecretBackendCaps, SecretBackendKind, SecretError, SecretStore};
use thegn_core::secretref::{BareAs, SecretRef};

const MAX_SECRET_BYTES: u64 = 64 * 1024;
const MAX_ACCOUNT_BYTES: usize = 1024;
const MAX_INDEX_BYTES: u64 = 64 * 1024;
const MAX_INDEX_ENTRIES: usize = 4096;

/// The keyring "service" all thegn secrets live under.
const KEYRING_SERVICE: &str = "thegn";

/// Resolve a [`SecretRef`](self) string to a token value, with the historic
/// bare-as-env-name semantics of the provider `api_key_env` family. `None` when
/// the ref is empty or the secret can't be found — callers treat that as "not
/// configured". Every non-empty resolution emits a value-free audit event
/// (`thegn::secret::audit`) tagged with the generic `host` consumer; call
/// [`resolve_for`] to attribute it to a specific component.
pub fn resolve(secret_ref: &str) -> Option<String> {
    resolve_for(secret_ref, "host")
}

/// [`resolve`] with an explicit consumer tag for the audit trail
/// (`provider:fly`, `snapshot`, …). Bare strings are env-var **names** (the
/// `api_key_env` family). For fields whose bare string is a literal value
/// (issue/CI tokens), use [`resolve_ref_for`] with [`BareAs::Literal`].
pub fn resolve_for(secret_ref: &str, consumer: &str) -> Option<String> {
    resolve_ref_for(&SecretRef::parse(secret_ref, BareAs::EnvName), consumer)
}

/// The typed broker chokepoint: resolve a [`SecretRef`] through the right
/// backend and emit exactly one value-free audit event. This is the single
/// place a secret value is fetched host-side; every string overload funnels
/// here.
///
/// Degrades gracefully: an unavailable keyring answers `None` within the bounded
/// probe deadline (see [`keyring_available`]) rather than wedging, and the audit
/// outcome distinguishes `missing` from `unavailable`.
pub fn resolve_ref_for(r: &SecretRef, consumer: &str) -> Option<String> {
    resolve_ref_for_outcome(r, consumer).ok().flatten()
}

/// Resolution status for a configured reference. Errors contain only the
/// component/outcome class; they never include the reference operand or value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretResolveFailure {
    Missing,
    Unavailable,
    Denied,
}

impl std::fmt::Display for SecretResolveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Missing => "configured secret is missing",
                Self::Unavailable => "secret backend is unavailable",
                Self::Denied => "secret backend denied access",
            }
        )
    }
}

/// Resolve with distinct missing, unavailable, and denied outcomes. An empty
/// unconfigured reference returns `Ok(None)` without an audit event.
pub fn resolve_ref_for_outcome(
    r: &SecretRef,
    consumer: &str,
) -> Result<Option<String>, SecretResolveFailure> {
    resolve_ref_for_outcome_with(r, consumer, keyring_get, keyring_available)
}

fn resolve_ref_for_outcome_with(
    r: &SecretRef,
    consumer: &str,
    keyring_get: impl Fn(&str) -> Result<Option<String>>,
    keyring_available: impl Fn() -> bool,
) -> Result<Option<String>, SecretResolveFailure> {
    // An unconfigured/empty ref is "not set" — no fetch, no audit noise.
    if !r.is_configured() {
        return Ok(None);
    }
    let (value, outcome) = match r {
        SecretRef::Keyring { account } => match keyring_get(account) {
            Ok(Some(v)) => (Some(v), SecretOutcome::Resolved),
            Ok(None) if keyring_available() => (None, SecretOutcome::Missing),
            Ok(None) | Err(_) => (None, SecretOutcome::Unavailable),
        },
        SecretRef::Env { var } => match std::env::var(var).ok().filter(|s| !s.trim().is_empty()) {
            Some(v) => (Some(v), SecretOutcome::Resolved),
            None => (None, SecretOutcome::Missing),
        },
        SecretRef::File { path } => {
            let p = thegn_core::util::expand_tilde(path);
            match std::fs::read_to_string(&p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                Some(v) => (Some(v), SecretOutcome::Resolved),
                None => (None, SecretOutcome::Missing),
            }
        }
        SecretRef::Literal(_) => match r.expose_literal().filter(|s| !s.trim().is_empty()) {
            Some(v) => (Some(v.to_string()), SecretOutcome::Resolved),
            None => (None, SecretOutcome::Missing),
        },
    };
    SecretAudit::new(r, consumer, outcome).record();
    match outcome {
        SecretOutcome::Resolved => Ok(value),
        SecretOutcome::Missing => Err(SecretResolveFailure::Missing),
        SecretOutcome::Unavailable => Err(SecretResolveFailure::Unavailable),
        SecretOutcome::Denied => Err(SecretResolveFailure::Denied),
    }
}

/// TTL-memoized PRESENCE check for the hydration path: `env_snapshots` asks
/// "does a token resolve?" for every provider env on every hydration (~5s
/// cadence), and a `keyring:` ref costs a real Secret Service / Keychain
/// round-trip each time (continuous Keychain traffic on macOS; a DBus timeout
/// per env on a locked/absent Secret Service). Caches only the boolean —
/// never the secret value — for 60s. [`store`]/[`forget`] clear the memo.
pub fn resolve_present_cached(secret_ref: &str) -> bool {
    use std::time::{Duration, Instant};
    let now = Instant::now();
    if let Ok(mut guard) = presence_memo().lock() {
        if let Some((v, at)) = guard.get(secret_ref)
            && now.duration_since(*at) < Duration::from_secs(60)
        {
            return *v;
        }
        let v = resolve(secret_ref).is_some();
        guard.insert(secret_ref.to_string(), (v, now));
        return v;
    }
    resolve(secret_ref).is_some()
}

/// Whether a typed [`SecretRef`] resolves, using the cached presence check —
/// names/backends only, never fetching a value into a printable place. A
/// literal is "present" iff configured (it carries its own value). For
/// `secret list` / `secret audit` / doctor presence rows.
pub fn present(r: &SecretRef) -> bool {
    match r {
        SecretRef::Literal(_) => r.is_configured(),
        _ => match r.to_config_string() {
            Some(s) => resolve_present_cached(&s),
            None => false,
        },
    }
}

fn presence_memo()
-> &'static std::sync::Mutex<std::collections::HashMap<String, (bool, std::time::Instant)>> {
    static MEMO: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (bool, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    MEMO.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Persist a UI/CLI-entered `token` for `name` (e.g. an env name like `fly-dev`),
/// preferring the OS keyring and falling back to a `0600` file. Returns the
/// SecretRef to store in config (`keyring:<name>` or `file:<path>`), so the token
/// itself never lands in `config.toml`.
pub fn store(name: &str, token: &str) -> Result<String> {
    if let Ok(mut m) = presence_memo().lock() {
        m.clear();
    }
    if keyring_set(name, token).is_ok() {
        return Ok(format!("keyring:{name}"));
    }
    let path = secrets_file(name)?;
    write_account_sidecar(&path, name)?;
    write_private(&path, token)?;
    thegn_core::fsperm::sync_parent_dir(path.parent().context("secret path has no parent")?)?;
    Ok(format!("file:{}", path.display()))
}

/// Persist a token to a `0600` **file** only (never the keyring), returning a
/// `file:<path>` ref. Used by `secret migrate` for fields whose runtime
/// resolution does not go through the keyring-capable broker and so still
/// resolve via `expand_env_ref` (`env:`/`file:`/literal, but not `keyring:`) —
/// migrating those to a file keeps them resolvable, with no silent breakage.
///
/// **Issue-tracker tokens are no longer in that set** (THE-72): `[issues.*]`
/// api_key/token fields resolve through `thegn_svc::issue::secret`, whose
/// `keyring:` arm calls back into [`resolve_for`] via the resolver installed in
/// `main.rs`, so a `keyring:` ref works for them now. `secret migrate` still
/// files them here — a `file:` ref is correct and unchanged for existing
/// configs; pointing it at [`store`] instead is a separate, opt-in change. CI
/// tokens are what genuinely still require this path.
pub fn store_file(name: &str, token: &str) -> Result<String> {
    if let Ok(mut m) = presence_memo().lock() {
        m.clear();
    }
    let path = secrets_file(name)?;
    write_account_sidecar(&path, name)?;
    write_private(&path, token)?;
    thegn_core::fsperm::sync_parent_dir(path.parent().context("secret path has no parent")?)?;
    Ok(format!("file:{}", path.display()))
}

/// Remove a stored secret (best-effort, both backends) — used when an env is
/// deleted. Never errors on a missing entry.
pub fn forget(name: &str) {
    if let Ok(mut m) = presence_memo().lock() {
        m.clear();
    }
    if let Err(error) = keyring_del(name) {
        tracing::debug!(target: "thegn::secret", %error, "best-effort keyring secret cleanup failed");
    }
    // Do not derive and delete the pre-hash fallback filename. That historical
    // sanitizer was not injective (`a/b` and `a_b` both became `a_b.token`), so
    // automatic legacy cleanup could delete a different account's credential.
    // Existing `file:` refs remain directly resolvable and can be removed by
    // their explicit path during a user-directed migration.
    match secrets_dir() {
        Ok(dir) => {
            if let Err(error) = file_del_at(&dir, name) {
                tracing::warn!(%error, "refused file-backed secret removal");
            }
        }
        Err(error) => tracing::warn!(%error, "secrets dir unavailable for removal"),
    }
}

/// Whether an OS keyring is actually usable here (so the UI can tell the user
/// where a token will land, and tests can skip the keyring leg on a headless CI).
///
/// **Memoized**, on the same terms as the sandbox runtime probe
/// (`sandbox_backend::available`): a `true` is cached for the process, a `false`
/// only briefly, so a keychain the user unlocks mid-session is picked up rather
/// than written off for good.
///
/// The memo is not an optimisation, it is what makes this callable at all. The
/// probe is a real write + delete against the OS credential store, and on macOS
/// that goes through the legacy `SecKeychain` API, which serialises every caller
/// on one process-wide lock inside `KCCursorImpl::next`. Uncached, N concurrent
/// callers convoy on it: `cargo test` (16 threads) wedged for **minutes** with a
/// dozen threads parked in `__psynch_mutexwait` under
/// `SecKeychainFindGenericPassword` — which is why the pre-push gate, the only
/// thing protecting main, could not complete a run on a Mac.
pub fn keyring_available() -> bool {
    /// How long a negative answer stands before we ask the OS again. Long enough
    /// to collapse a burst of callers (onboarding probes several steps), short
    /// enough that unlocking the keychain takes effect without a restart.
    const NEGATIVE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

    static MEMO: std::sync::OnceLock<std::sync::Mutex<Option<(bool, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    let memo = MEMO.get_or_init(|| std::sync::Mutex::new(None));

    // Hold the lock ACROSS the probe, deliberately: it makes concurrent callers
    // wait for one answer instead of each starting its own keychain round-trip.
    // That is the convoy this exists to prevent, and the probe is bounded by the
    // OS call itself.
    let Ok(mut slot) = memo.lock() else {
        return probe_keyring_bounded();
    };
    if let Some((v, at)) = *slot
        && (v || at.elapsed() < NEGATIVE_TTL)
    {
        return v;
    }
    let v = probe_keyring_bounded();
    *slot = Some((v, std::time::Instant::now()));
    v
}

/// How long to wait for the OS credential store to answer before calling it
/// unusable. A local keychain round-trip is milliseconds; anything near this is
/// not slow, it is stuck.
const KEYRING_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// [`probe_keyring`] with a hard deadline. `false` when the store doesn't answer.
///
/// The deadline is load-bearing, not defensive. macOS's keychain call can block
/// **forever** when it wants to show an authorization prompt and there is no GUI
/// session to show it in — a headless test runner, an ssh session, a launchd
/// context. Observed directly: one thread parked in `SecKeychainFindGenericPassword`
/// for the entire run while every other caller queued behind it, which is what
/// stopped `cargo test` from ever completing on this Mac.
///
/// A stuck probe is answered "unavailable", which is both true and useful:
/// [`store`] already falls back to a `0600` file, so the feature degrades
/// instead of hanging.
///
/// The probe thread is deliberately **detached** rather than joined — it may be
/// blocked in Security.framework indefinitely, and joining it would reintroduce
/// exactly the hang this removes. Same trade as `sandbox::output_with_timeout`'s
/// detached reap.
fn probe_keyring_bounded() -> bool {
    run_keyring_op_with_timeout("probe", KEYRING_PROBE_TIMEOUT, probe_keyring).unwrap_or(false)
}

/// The unbounded probe: a round-trip on a throwaway account is the only honest
/// answer — a keyring can be present, reachable, and still refuse to store.
/// Always call it through [`probe_keyring_bounded`].
fn probe_keyring() -> bool {
    let probe = "__thegn_keyring_probe__";
    match keyring::Entry::new(KEYRING_SERVICE, probe) {
        Ok(e) => {
            let ok = e.set_password("1").is_ok();
            if ok {
                let _ = e.delete_credential(); // best-effort: probe cleanup: a failed delete leaves a stray probe credential
            }
            ok
        }
        // No entry handle at all (no credential store, or one that refuses to
        // open) — indistinguishable from a store that cannot hold a secret, and
        // answered the same way: unavailable, so `store` falls back to a file.
        Err(_) => false,
    }
}

fn run_keyring_op_with_timeout<T: Send + 'static>(
    operation: &'static str,
    timeout: std::time::Duration,
    op: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    thegn_core::thread_name::spawn_named("thegn-keyring", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive); // Interactive: synchronous UI callers wait up to 2s.
        // best-effort: the receiver is gone if the operation exceeded its cap.
        let _ = tx.send(op());
    });
    rx.recv_timeout(timeout).map_err(|_| {
        tracing::warn!(
            target: "thegn::secret",
            operation,
            cap_ms = timeout.as_millis() as u64,
            "keyring operation timed out — treating the OS keyring as unavailable"
        );
        anyhow::anyhow!(
            "keyring {operation} timed out after {}ms",
            timeout.as_millis()
        )
    })
}

fn keyring_get(account: &str) -> Result<Option<String>> {
    let account = account.to_string();
    run_keyring_op_with_timeout("get", KEYRING_PROBE_TIMEOUT, move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)
            .ok()
            .and_then(|entry| entry.get_password().ok())
            .filter(|value| !value.trim().is_empty())
    })
}

fn keyring_set(account: &str, token: &str) -> Result<()> {
    let account = account.to_string();
    let token = token.to_string();
    run_keyring_op_with_timeout("set", KEYRING_PROBE_TIMEOUT, move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)
            .context("keyring entry")?
            .set_password(&token)
            .context("keyring set")
    })?
}

fn keyring_del(account: &str) -> Result<()> {
    let account = account.to_string();
    run_keyring_op_with_timeout("delete", KEYRING_PROBE_TIMEOUT, move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account).context("keyring entry")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("keyring delete"),
        }
    })?
}

/// `$XDG_CONFIG_HOME/thegn/secrets/<name>.token` — alongside the config file,
/// so it moves with `THEGN_DIR`/XDG isolation used by tests + `just start`.
fn secrets_file(name: &str) -> Result<PathBuf> {
    let cfg = thegn_core::config::Config::path();
    let dir = std::path::Path::new(&cfg)
        .parent()
        .map(|p| p.join("secrets"))
        .context("config path has no parent")?;
    secrets_dir_at(&dir)?;
    resolve_token_path_in(&dir, name)
}

fn secrets_dir() -> Result<PathBuf> {
    let cfg = thegn_core::config::Config::path();
    let dir = std::path::Path::new(&cfg)
        .parent()
        .map(|p| p.join("secrets"))
        .context("config path has no parent")?;
    secrets_dir_at(&dir)?;
    Ok(dir)
}

fn secrets_dir_at(dir: &std::path::Path) -> Result<()> {
    thegn_core::fsperm::ensure_private_dir(dir)
        .with_context(|| format!("prepare private secrets directory {}", dir.display()))
}

fn token_path_in(dir: &std::path::Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.len() > MAX_ACCOUNT_BYTES {
        anyhow::bail!("secret account must contain 1..={MAX_ACCOUNT_BYTES} UTF-8 bytes");
    }
    // Keep a readable prefix; the sidecar below verifies the exact account for
    // this hashed filename, including aliases such as `a/b` and `a_b`.
    let safe = safe_account_prefix(name);
    let mut hasher = Sha256::new();
    hasher.update(b"thegn-secret-account-v2\0");
    hasher.update(name.as_bytes());
    let identity = format!("{:x}", hasher.finalize());
    Ok(dir.join(format!("{safe}-{identity}.token")))
}

fn legacy_token_path_in(dir: &std::path::Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.len() > MAX_ACCOUNT_BYTES {
        anyhow::bail!("secret account must contain 1..={MAX_ACCOUNT_BYTES} UTF-8 bytes");
    }
    let safe = safe_account_prefix(name);
    let identity = thegn_core::util::short_hash(name, 16);
    Ok(dir.join(format!("{safe}-{identity}.token")))
}

fn safe_account_prefix(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if safe.is_empty() {
        "secret".into()
    } else {
        safe
    }
}

fn resolve_token_path_in(dir: &std::path::Path, account: &str) -> Result<PathBuf> {
    let v2 = token_path_in(dir, account)?;
    if path_entry_exists(&v2)? || path_entry_exists(&account_path(&v2))? {
        return Ok(v2);
    }
    let legacy = legacy_token_path_in(dir, account)?;
    if path_entry_exists(&legacy)? {
        account_matches_or_missing(&legacy, account)?;
        return Ok(legacy);
    }
    Ok(v2)
}

fn path_entry_exists(path: &std::path::Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect secret path {}", path.display())),
    }
}

fn account_path(token: &std::path::Path) -> PathBuf {
    token.with_extension("account")
}

fn write_account_sidecar(token_path: &std::path::Path, account: &str) -> Result<()> {
    let sidecar = account_path(token_path);
    match read_account_sidecar(&sidecar) {
        Ok(existing) if existing != account => anyhow::bail!("secret account digest collision"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match thegn_core::fsperm::open_regular_file_nofollow(token_path) {
                // An explicit write for this exact account adopts a legacy token.
                // The caller replaces it after the sidecar is durably written.
                Ok(_) => {}
                Err(token_error) if token_error.kind() == std::io::ErrorKind::NotFound => {}
                Err(token_error) => {
                    return Err(token_error).context("validate existing secret file");
                }
            }
        }
        Err(error) => return Err(error).context("read existing secret account sidecar"),
    }
    thegn_core::fsperm::write_owner_only_atomic(&sidecar, account.as_bytes())?;
    thegn_core::fsperm::sync_parent_dir(sidecar.parent().context("sidecar has no parent")?)?;
    Ok(())
}

fn read_account_sidecar(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let file = thegn_core::fsperm::open_regular_file_nofollow(path)?;
    if !thegn_core::fsperm::owned_by_current_user(&file.metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "foreign-owned secret account sidecar",
        ));
    }
    let mut bytes = Vec::new();
    file.take((MAX_ACCOUNT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > MAX_ACCOUNT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid secret account sidecar size",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn account_matches_or_missing(token_path: &std::path::Path, account: &str) -> Result<()> {
    match read_account_sidecar(&account_path(token_path)) {
        Ok(existing) if existing != account => anyhow::bail!("secret account digest collision"),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("validate secret account sidecar"),
    }
}

fn write_private(path: &std::path::Path, token: &str) -> Result<()> {
    thegn_core::fsperm::write_owner_only_atomic(path, token.trim().as_bytes())
        .with_context(|| format!("write owner-only secret {}", path.display()))
}

// --- the SecretStore seam, backed by the functions above --------------------
//
// The seam vocabulary lives in `thegn_core::secret_store`; the real work
// (keyring FFI, file I/O, env reads) is here because core is substrate-free.
// `thegn doctor` renders one Probe row per backend, and `exec` is shown
// reserved.

use thegn_core::seam::{Availability, Probe, ProbeReport};

/// The OS keyring backend (Secret Service / Keychain / Credential Manager).
pub struct KeyringStore;
impl Probe for KeyringStore {
    fn probe(&self) -> ProbeReport {
        let avail = if keyring_available() {
            Availability::Ready
        } else {
            Availability::Unavailable(
                "no usable OS credential store (headless / locked / absent); \
                 explicit `keyring:` refs are unavailable; broker writes may use an owner-only file"
                    .into(),
            )
        };
        ProbeReport::new("secret", "keyring", avail)
    }
}
impl SecretStore for KeyringStore {
    fn kind(&self) -> SecretBackendKind {
        SecretBackendKind::Keyring
    }
    fn caps(&self) -> SecretBackendCaps {
        SecretBackendCaps {
            writable: true,
            listable: false,
        }
    }
    fn get(&self, account: &str) -> Result<String, SecretError> {
        match keyring_get(account) {
            Ok(Some(v)) => Ok(v),
            Ok(None) if keyring_available() => Err(SecretError::not_found(account.to_string())),
            Ok(None) | Err(_) => Err(SecretError::unavailable("no usable OS credential store")),
        }
    }
    fn set(&self, account: &str, value: &str) -> Result<(), SecretError> {
        keyring_set(account, value).map_err(|e| SecretError::denied(e.to_string()))
    }
    fn del(&self, account: &str) -> Result<(), SecretError> {
        keyring_del(account).map_err(|e| SecretError::denied(e.to_string()))
    }
}

/// The `0600`-file backend (also covers `file:PATH` targets like agenix/sops).
pub struct FileStore;
impl Probe for FileStore {
    fn probe(&self) -> ProbeReport {
        let avail = match secrets_file("__probe__") {
            Ok(_) => Availability::Ready,
            Err(e) => Availability::Unavailable(format!("secrets dir unusable: {e}")),
        };
        ProbeReport::new("secret", "file", avail)
    }
}
impl SecretStore for FileStore {
    fn kind(&self) -> SecretBackendKind {
        SecretBackendKind::File
    }
    fn caps(&self) -> SecretBackendCaps {
        SecretBackendCaps {
            writable: true,
            listable: true,
        }
    }
    fn get(&self, account: &str) -> Result<String, SecretError> {
        let dir = secrets_dir().map_err(|e| SecretError::unavailable(e.to_string()))?;
        file_get_in(&dir, account)
    }
    fn set(&self, account: &str, value: &str) -> Result<(), SecretError> {
        let dir = secrets_dir().map_err(|e| SecretError::unavailable(e.to_string()))?;
        file_set_at(&dir, account, value)
    }
    fn del(&self, account: &str) -> Result<(), SecretError> {
        let dir = secrets_dir().map_err(|e| SecretError::unavailable(e.to_string()))?;
        file_del_at(&dir, account)
    }
    fn list(&self) -> Result<Vec<String>, SecretError> {
        let dir = secrets_dir().map_err(|e| SecretError::unavailable(e.to_string()))?;
        file_list_at(&dir)
    }
}

fn file_get_at(path: &std::path::Path, account: &str) -> Result<String, SecretError> {
    use std::io::Read;
    account_matches_or_missing(path, account).map_err(|e| SecretError::denied(e.to_string()))?;
    let file = thegn_core::fsperm::open_regular_file_nofollow(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SecretError::not_found(account.to_string())
        } else {
            SecretError::denied(format!("open secret: {error}"))
        }
    })?;
    if !thegn_core::fsperm::owned_by_current_user(
        &file
            .metadata()
            .map_err(|e| SecretError::denied(e.to_string()))?,
    ) {
        return Err(SecretError::denied("secret file is owned by another user"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SECRET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| SecretError::denied(e.to_string()))?;
    if bytes.len() as u64 > MAX_SECRET_BYTES {
        return Err(SecretError::denied("secret file exceeds size limit"));
    }
    let value = String::from_utf8(bytes).map_err(|e| SecretError::denied(e.to_string()))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(SecretError::not_found(account.to_string()))
    } else {
        Ok(value)
    }
}

fn file_get_in(dir: &std::path::Path, account: &str) -> Result<String, SecretError> {
    let path =
        resolve_token_path_in(dir, account).map_err(|e| SecretError::denied(e.to_string()))?;
    file_get_at(&path, account)
}

fn file_set_at(dir: &std::path::Path, account: &str, value: &str) -> Result<(), SecretError> {
    let path =
        resolve_token_path_in(dir, account).map_err(|e| SecretError::denied(e.to_string()))?;
    write_account_sidecar(&path, account).map_err(|e| SecretError::denied(e.to_string()))?;
    write_private(&path, value).map_err(|e| SecretError::denied(e.to_string()))?;
    thegn_core::fsperm::sync_parent_dir(dir).map_err(|e| SecretError::denied(e.to_string()))
}

fn file_del_at(dir: &std::path::Path, account: &str) -> Result<(), SecretError> {
    let paths = [
        token_path_in(dir, account).map_err(|e| SecretError::denied(e.to_string()))?,
        legacy_token_path_in(dir, account).map_err(|e| SecretError::denied(e.to_string()))?,
    ];
    for path in &paths {
        account_matches_or_missing(path, account)
            .map_err(|e| SecretError::denied(e.to_string()))?;
    }
    for path in &paths {
        match std::fs::remove_file(path) {
            Ok(()) => remove_sidecar(path).map_err(|e| SecretError::denied(e.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_sidecar(path).map_err(|e| SecretError::denied(e.to_string()))?;
            }
            Err(error) => return Err(SecretError::denied(format!("remove secret: {error}"))),
        }
    }
    thegn_core::fsperm::sync_parent_dir(dir).map_err(|e| SecretError::denied(e.to_string()))
}

fn remove_sidecar(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(account_path(path)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn file_list_at(dir: &std::path::Path) -> Result<Vec<String>, SecretError> {
    // Legacy tokens without an exact-identity sidecar are deliberately omitted.
    let entries = std::fs::read_dir(dir)
        .map_err(|e| SecretError::unavailable(format!("read secret inventory: {e}")))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|e| SecretError::unavailable(format!("read secret inventory entry: {e}")))?;
        if entry.path().extension().and_then(|v| v.to_str()) != Some("account") {
            continue;
        }
        let account = read_account_sidecar(&entry.path())
            .map_err(|e| SecretError::denied(format!("invalid secret account sidecar: {e}")))?;
        let v2 = token_path_in(dir, &account).map_err(|e| SecretError::denied(e.to_string()))?;
        let legacy =
            legacy_token_path_in(dir, &account).map_err(|e| SecretError::denied(e.to_string()))?;
        let token = if v2.with_extension("account") == entry.path() {
            v2
        } else if legacy.with_extension("account") == entry.path() {
            legacy
        } else {
            return Err(SecretError::denied(
                "secret account sidecar identity mismatch",
            ));
        };
        match thegn_core::fsperm::open_regular_file_nofollow(&token) {
            Ok(file)
                if thegn_core::fsperm::owned_by_current_user(
                    &file
                        .metadata()
                        .map_err(|e| SecretError::denied(e.to_string()))?,
                ) =>
            {
                if !names.contains(&account) {
                    names.push(account)
                }
            }
            Ok(_) => return Err(SecretError::denied("secret file is owned by another user")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(SecretError::denied(format!(
                    "invalid secret token: {error}"
                )));
            }
        }
    }
    names.sort();
    Ok(names)
}

/// The environment-variable backend (read-only).
pub struct EnvStore;
impl Probe for EnvStore {
    fn probe(&self) -> ProbeReport {
        // Env is always available; the ref's own var may still be unset.
        ProbeReport::new("secret", "env", Availability::Ready)
    }
}
impl SecretStore for EnvStore {
    fn kind(&self) -> SecretBackendKind {
        SecretBackendKind::Env
    }
    fn caps(&self) -> SecretBackendCaps {
        SecretBackendCaps::default()
    }
    fn get(&self, account: &str) -> Result<String, SecretError> {
        std::env::var(account)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| SecretError::not_found(account.to_string()))
    }
}

/// The doctor `Secrets` section: one probe per backend kind (with `exec` shown
/// reserved). Cheap and synchronous by the Probe contract — the keyring row
/// reuses the bounded, memoized availability check.
pub fn probes() -> Vec<ProbeReport> {
    vec![
        KeyringStore.probe(),
        FileStore.probe(),
        EnvStore.probe(),
        thegn_core::secret_store::reserved_probe(SecretBackendKind::Exec),
    ]
}

// ── MCP proxy credential custody ────────────────────────────────────────────
//
// The mcp-proxy hub resolves an upstream's `env` refs only at spawn (inside the
// daemon / standalone shim), so agents never see the keys. This reuses the
// canonical `SecretStore` seam above (`KeyringStore`) and adds only the two
// MCP-specific bits: a resolver whose bare-string default is LITERAL (the
// historic `[mcp_servers.<name>] env` meaning, unlike provider `api_key_env`
// which is bare-as-env-name), and a names-only sidecar index so
// `thegn mcp secret list` can enumerate (the keyring cannot).
// (`SecretStore`, `SecretRef`, `BareAs` are already imported at module top.)

/// Resolve one MCP upstream `env` value to its concrete value, at spawn.
///
/// `keyring:` routes through the shared keyring service; a missing entry (or an
/// unavailable keyring) is a clear, actionable error, never a plaintext
/// fallback or a hang. Bare strings are LITERAL values.
pub fn resolve_mcp_env(value: &str, consumer: &str) -> Result<String, String> {
    let reference = SecretRef::parse(value, BareAs::Literal);
    resolve_ref_for(&reference, consumer).ok_or_else(|| match &reference {
        SecretRef::Keyring { account } => format!(
            "keyring entry `{account}` is not set — add it with \
             `thegn mcp secret set {account} <value>` (agents never see the value)"
        ),
        _ => format!(
            "secret ref `{}` is unreadable, unset, or empty",
            reference.audit_name()
        ),
    })
}

/// `thegn mcp secret set` — store under the shared keyring service and record
/// the name in the MCP index (so `list` can enumerate). The value never lands
/// in config or argv.
pub fn mcp_secret_set(account: &str, value: &str) -> Result<(), String> {
    KeyringStore
        .set(account, value)
        .map_err(|e| e.message().to_string())?;
    index_add(account)?;
    Ok(())
}

/// `thegn mcp secret rm` — remove from the keyring and the index.
pub fn mcp_secret_rm(account: &str) -> Result<(), String> {
    KeyringStore
        .del(account)
        .map_err(|error| error.message().to_string())?;
    index_remove(account)?;
    Ok(())
}

/// `thegn mcp secret list` — the names thegn has stored (never values).
pub fn mcp_secret_list() -> Result<Vec<String>, String> {
    index_read()
}

/// The keyring backend's doctor probe (custody row for the mcp-proxy section).
pub fn mcp_keyring_probe() -> thegn_core::seam::ProbeReport {
    KeyringStore.probe()
}

/// The names-only index path (`secrets/mcp-index.json`).
fn index_read() -> Result<Vec<String>, String> {
    let dir = secrets_dir().map_err(|e| e.to_string())?;
    index_read_in(&dir)
}

fn index_read_in(dir: &std::path::Path) -> Result<Vec<String>, String> {
    let path = dir.join("mcp-index.json");
    let file = match thegn_core::fsperm::open_regular_file_nofollow(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read MCP secret index: {error}")),
    };
    use std::io::Read;
    if !thegn_core::fsperm::owned_by_current_user(&file.metadata().map_err(|e| e.to_string())?) {
        return Err("MCP secret index is owned by another user".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err("MCP secret index exceeds size limit".into());
    }
    let names: Vec<String> =
        serde_json::from_slice(&bytes).map_err(|e| format!("corrupt MCP secret index: {e}"))?;
    if names.len() > MAX_INDEX_ENTRIES
        || names
            .iter()
            .any(|name| name.is_empty() || name.len() > MAX_ACCOUNT_BYTES)
    {
        return Err("MCP secret index has invalid entry count or account name".into());
    }
    Ok(names)
}

fn index_write_in(dir: &std::path::Path, names: &[String]) -> Result<(), String> {
    if names.len() > MAX_INDEX_ENTRIES
        || names
            .iter()
            .any(|name| name.is_empty() || name.len() > MAX_ACCOUNT_BYTES)
    {
        return Err("MCP secret index has invalid entry count or account name".into());
    }
    let body = serde_json::to_vec_pretty(names).map_err(|e| e.to_string())?;
    if body.len() as u64 > MAX_INDEX_BYTES {
        return Err("MCP secret index exceeds size limit".into());
    }
    let path = dir.join("mcp-index.json");
    thegn_core::fsperm::write_owner_only_atomic(&path, &body).map_err(|e| e.to_string())?;
    thegn_core::fsperm::sync_parent_dir(dir).map_err(|e| e.to_string())
}

fn with_index_lock<T>(
    dir: &std::path::Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let path = dir.join("mcp-index.lock");
    let file = loop {
        match thegn_core::fsperm::open_regular_file_nofollow(&path) {
            Ok(file) => break file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match thegn_core::fsperm::create_owner_only_exclusive(&path) {
                    Ok(file) => break file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(format!("open MCP secret index lock: {error}")),
                }
            }
            Err(error) => return Err(format!("open MCP secret index lock: {error}")),
        }
    };
    if !thegn_core::fsperm::owned_by_current_user(
        &file
            .metadata()
            .map_err(|e| format!("inspect MCP secret index lock: {e}"))?,
    ) {
        return Err("MCP secret index lock is owned by another user".into());
    }
    file.lock()
        .map_err(|e| format!("lock MCP secret index: {e}"))?;
    action()
}

fn index_add_in(dir: &std::path::Path, account: &str) -> Result<(), String> {
    if account.is_empty() || account.len() > MAX_ACCOUNT_BYTES {
        return Err("invalid MCP secret account name".into());
    }
    with_index_lock(dir, || {
        let mut names = index_read_in(dir)?;
        if !names.iter().any(|name| name == account) {
            names.push(account.to_string());
            names.sort();
            index_write_in(dir, &names)?;
        }
        Ok(())
    })
}

fn index_remove_in(dir: &std::path::Path, account: &str) -> Result<(), String> {
    with_index_lock(dir, || {
        let mut names = index_read_in(dir)?;
        let before = names.len();
        names.retain(|name| name != account);
        if names.len() != before {
            index_write_in(dir, &names)?;
        }
        Ok(())
    })
}

fn index_add(account: &str) -> Result<(), String> {
    let dir = secrets_dir().map_err(|e| e.to_string())?;
    index_add_in(&dir, account)
}

fn index_remove(account: &str) -> Result<(), String> {
    let dir = secrets_dir().map_err(|e| e.to_string())?;
    index_remove_in(&dir, account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct AuditCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for AuditCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mpd_resolution_emits_one_metadata_only_audit_event() {
        let sentinel = "mpd-audit-secret-sentinel";
        let capture = AuditCapture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let resolved = tracing::subscriber::with_default(subscriber, || {
            resolve_ref_for_outcome(&SecretRef::parse(sentinel, BareAs::Literal), "media:mpd")
                .unwrap()
                .unwrap()
        });
        assert_eq!(resolved, sentinel);
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(output.matches("secret resolve").count(), 1, "{output}");
        assert!(output.contains("consumer=media:mpd"), "{output}");
        // Quoted: `tracing`'s fmt layer renders a string-valued field with quotes,
        // so this records as `outcome="resolved"`. `consumer` above is unquoted
        // because it is recorded via `%`/Display rather than as a string.
        assert!(output.contains(r#"outcome="resolved""#), "{output}");
        assert!(
            !output.contains(sentinel),
            "audit output exposed the secret"
        );
    }

    #[test]
    fn typed_broker_distinguishes_missing_from_unavailable_keyring() {
        let reference = SecretRef::Keyring {
            account: "media-mpd-test-account".into(),
        };
        let missing = resolve_ref_for_outcome_with(&reference, "media:mpd", |_| Ok(None), || true);
        let unavailable = resolve_ref_for_outcome_with(
            &reference,
            "media:mpd",
            |_| Err(anyhow::anyhow!("test backend unavailable")),
            || false,
        );
        assert_eq!(missing, Err(SecretResolveFailure::Missing));
        assert_eq!(unavailable, Err(SecretResolveFailure::Unavailable));
        assert_ne!(missing, unavailable);
    }

    #[test]
    fn typed_broker_resolves_env_and_file_refs() {
        // SAFETY: unique test variable is set and removed within this test.
        unsafe { std::env::set_var("TG_MPD_SECRET_TEST", "mpd-test-value") };
        let env = resolve_ref_for_outcome(
            &SecretRef::parse("env:TG_MPD_SECRET_TEST", BareAs::Literal),
            "media:mpd",
        )
        .unwrap();
        unsafe { std::env::remove_var("TG_MPD_SECRET_TEST") };
        assert_eq!(env.as_deref(), Some("mpd-test-value"));

        let file = std::env::temp_dir().join(format!("tg-mpd-secret-{}.tok", std::process::id()));
        std::fs::write(&file, "mpd-file-value").unwrap();
        let from_file = resolve_ref_for_outcome(
            &SecretRef::parse(&format!("file:{}", file.display()), BareAs::Literal),
            "media:mpd",
        )
        .unwrap();
        let _ = std::fs::remove_file(file); // best-effort: test cleanup after broker read
        assert_eq!(from_file.as_deref(), Some("mpd-file-value"));
    }

    #[test]
    fn resolve_env_and_bare_and_empty() {
        // SAFETY: single-threaded test; unique var name.
        unsafe { std::env::set_var("TG_SECRET_TEST_TOK", "s3cr3t") };
        // bare name → env var
        assert_eq!(resolve("TG_SECRET_TEST_TOK").as_deref(), Some("s3cr3t"));
        // explicit env: ref
        assert_eq!(resolve("env:TG_SECRET_TEST_TOK").as_deref(), Some("s3cr3t"));
        // empty / unset
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("TG_SECRET_DEFINITELY_UNSET_XYZ"), None);
        unsafe { std::env::remove_var("TG_SECRET_TEST_TOK") };
    }

    #[test]
    fn resolve_file_ref_reads_token() {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        let f = temp.path().join("secret.tok");
        std::fs::write(&f, "  filetoken\n").unwrap();
        assert_eq!(
            resolve(&format!("file:{}", f.display())).as_deref(),
            Some("filetoken")
        );
    }

    #[test]
    fn keyring_prefix_missing_entry_is_none() {
        // An account we never set resolves to None (never panics, even with no
        // Secret Service available).
        assert_eq!(resolve("keyring:__tg_never_set_account__"), None);
    }

    #[test]
    fn bounded_operation_returns_without_waiting_for_worker() {
        let started = std::time::Instant::now();
        let result =
            run_keyring_op_with_timeout("test", std::time::Duration::from_millis(10), || {
                std::thread::sleep(std::time::Duration::from_secs(1));
                true
            });
        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
    }

    #[test]
    fn fallback_names_do_not_alias_after_sanitization() {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        let slash = token_path_in(temp.path(), "a/b").unwrap();
        let underscore = token_path_in(temp.path(), "a_b").unwrap();
        assert_ne!(slash, underscore);
    }

    #[test]
    fn token_digest_is_domain_separated_sha256_and_legacy_name_is_retained() {
        let temp = private_temp();
        let v2 = token_path_in(temp.path(), "acct").unwrap();
        let legacy = legacy_token_path_in(temp.path(), "acct").unwrap();
        let digest = v2
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit('-')
            .next()
            .unwrap();
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(v2, legacy);
    }

    fn private_temp() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        temp
    }

    #[test]
    fn private_secrets_root_rejects_a_preplanted_symlink() {
        let temp = private_temp();
        let canary = temp.path().join("canary");
        let secrets = temp.path().join("secrets");
        std::fs::create_dir(&canary).unwrap();
        thegn_core::fsperm::ensure_private_dir(&canary).unwrap();
        std::fs::write(canary.join("sentinel"), "unchanged").unwrap();
        thegn_core::fsperm::create_symlink(&canary, &secrets, true).unwrap();
        assert!(secrets_dir_at(&secrets).is_err());
        assert_eq!(
            std::fs::read_to_string(canary.join("sentinel")).unwrap(),
            "unchanged"
        );
    }

    #[test]
    fn conflicting_account_sidecar_refuses_set_get_and_delete() {
        let temp = private_temp();
        let token = token_path_in(temp.path(), "mine").unwrap();
        write_private(&token, "keep-me").unwrap();
        std::fs::write(account_path(&token), "other").unwrap();
        assert!(file_set_at(temp.path(), "mine", "replacement").is_err());
        assert!(file_get_at(&token, "mine").is_err());
        assert!(file_del_at(temp.path(), "mine").is_err());
        assert_eq!(std::fs::read_to_string(token).unwrap(), "keep-me");
    }

    #[test]
    fn file_set_adopts_legacy_token_and_records_exact_account() {
        let temp = private_temp();
        let token = token_path_in(temp.path(), "legacy/account").unwrap();
        write_private(&token, "old-value").unwrap();
        assert!(!account_path(&token).exists());

        file_set_at(temp.path(), "legacy/account", "new-value").unwrap();

        assert_eq!(
            std::fs::read(account_path(&token)).unwrap(),
            b"legacy/account"
        );
        assert_eq!(file_get_at(&token, "legacy/account").unwrap(), "new-value");
    }

    #[test]
    fn legacy_token_stays_sticky_for_get_set_list_and_delete() {
        let temp = private_temp();
        let legacy = legacy_token_path_in(temp.path(), "acct").unwrap();
        write_private(&legacy, "old").unwrap();
        write_account_sidecar(&legacy, "acct").unwrap();

        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), "old");
        assert_eq!(file_list_at(temp.path()).unwrap(), vec!["acct"]);
        file_set_at(temp.path(), "acct", "new").unwrap();
        assert_eq!(file_get_at(&legacy, "acct").unwrap(), "new");
        assert!(!token_path_in(temp.path(), "acct").unwrap().exists());

        let v2 = token_path_in(temp.path(), "acct").unwrap();
        write_private(&v2, "duplicate-generation").unwrap();
        write_account_sidecar(&v2, "acct").unwrap();
        assert_eq!(file_list_at(temp.path()).unwrap(), vec!["acct"]);

        file_del_at(temp.path(), "acct").unwrap();
        assert!(!legacy.exists());
        assert!(!account_path(&legacy).exists());
        assert!(!v2.exists());
        assert!(!account_path(&v2).exists());
    }

    #[test]
    fn legacy_token_without_sidecar_remains_readable() {
        let temp = private_temp();
        let legacy = legacy_token_path_in(temp.path(), "acct").unwrap();
        write_private(&legacy, "old").unwrap();
        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), "old");
    }

    #[test]
    fn mismatched_legacy_sidecar_refuses_get_and_delete_without_removing_bytes() {
        let temp = private_temp();
        let legacy = legacy_token_path_in(temp.path(), "acct").unwrap();
        write_private(&legacy, "keep-me").unwrap();
        std::fs::write(account_path(&legacy), "other").unwrap();

        assert!(file_get_in(temp.path(), "acct").is_err());
        assert!(file_del_at(temp.path(), "acct").is_err());
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "keep-me");
        assert_eq!(
            std::fs::read_to_string(account_path(&legacy)).unwrap(),
            "other"
        );
    }

    #[test]
    fn new_account_is_written_only_at_v2_path() {
        let temp = private_temp();
        let v2 = token_path_in(temp.path(), "new-acct").unwrap();
        let legacy = legacy_token_path_in(temp.path(), "new-acct").unwrap();
        file_set_at(temp.path(), "new-acct", "value").unwrap();
        assert!(v2.exists());
        assert!(account_path(&v2).exists());
        assert!(!legacy.exists());
        assert!(!account_path(&legacy).exists());
    }

    #[test]
    fn no_follow_get_rejects_symlink_and_oversized_secret() {
        let temp = private_temp();
        let canary = temp.path().join("canary");
        std::fs::write(&canary, "do-not-read").unwrap();
        let symlinked = token_path_in(temp.path(), "symlinked").unwrap();
        thegn_core::fsperm::create_symlink(&canary, &symlinked, false).unwrap();
        assert!(file_get_at(&symlinked, "symlinked").is_err());

        let oversized = token_path_in(temp.path(), "oversized").unwrap();
        std::fs::write(&oversized, vec![b'x'; (MAX_SECRET_BYTES + 1) as usize]).unwrap();
        assert!(file_get_at(&oversized, "oversized").is_err());
    }

    #[test]
    fn file_list_sidecars_round_trip_exact_accounts() {
        let temp = private_temp();
        for account in ["a/b", "a_b"] {
            file_set_at(temp.path(), account, &format!("value-{account}")).unwrap();
        }
        assert_eq!(file_list_at(temp.path()).unwrap(), vec!["a/b", "a_b"]);
        for account in ["a/b", "a_b"] {
            let token = token_path_in(temp.path(), account).unwrap();
            assert_eq!(
                file_get_at(&token, account).unwrap(),
                format!("value-{account}")
            );
            file_del_at(temp.path(), account).unwrap();
        }
        assert!(file_list_at(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn file_list_skips_orphan_sidecar_and_lists_complete_account() {
        let temp = private_temp();
        let orphan = token_path_in(temp.path(), "orphan").unwrap();
        std::fs::write(account_path(&orphan), "orphan").unwrap();
        file_set_at(temp.path(), "complete", "value").unwrap();

        assert_eq!(file_list_at(temp.path()).unwrap(), vec!["complete"]);
    }

    #[test]
    fn mcp_index_corruption_is_preserved_and_reported() {
        let temp = private_temp();
        let index = temp.path().join("mcp-index.json");
        std::fs::write(&index, b"not-json").unwrap();
        assert!(index_add_in(temp.path(), "new-name").is_err());
        assert_eq!(std::fs::read(index).unwrap(), b"not-json");
    }

    #[test]
    fn concurrent_mcp_index_additions_keep_every_name() {
        let temp = private_temp();
        let dir = temp.path().to_path_buf();
        let workers: Vec<_> = (0..8)
            .map(|n| {
                let dir = dir.clone();
                std::thread::spawn(move || index_add_in(&dir, &format!("name-{n}")))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        let names = index_read_in(temp.path()).unwrap();
        assert_eq!(names.len(), 8);
        for n in 0..8 {
            assert!(names.contains(&format!("name-{n}")));
        }
    }
}
