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
use thegn_core::secret_audit::{SecretAudit, SecretOutcome};
use thegn_core::secret_store::{SecretBackendCaps, SecretBackendKind, SecretError, SecretStore};
use thegn_core::secretref::{BareAs, SecretRef};

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
    // An unconfigured/empty ref is "not set" — no fetch, no audit noise.
    if !r.is_configured() {
        return None;
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
    value
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
    write_private(&path, token)?;
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
    write_private(&path, token)?;
    Ok(format!("file:{}", path.display()))
}

/// Remove a stored secret (best-effort, both backends) — used when an env is
/// deleted. Never errors on a missing entry.
pub fn forget(name: &str) {
    if let Ok(mut m) = presence_memo().lock() {
        m.clear();
    }
    let _ = keyring_del(name); // best-effort: bounded cleanup; absence is success
    if let Ok(path) = secrets_file(name) {
        let _ = std::fs::remove_file(path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
    // Do not derive and delete the pre-hash fallback filename. That historical
    // sanitizer was not injective (`a/b` and `a_b` both became `a_b.token`), so
    // automatic legacy cleanup could delete a different account's credential.
    // Existing `file:` refs remain directly resolvable and can be removed by
    // their explicit path during a user-directed migration.
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
    std::thread::spawn(move || {
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
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    thegn_core::fsperm::restrict_dir_to_owner(&dir)
        .with_context(|| format!("restrict {}", dir.display()))?;
    // Sanitize for readability, then bind the filename to the exact name so
    // aliases such as `a/b` and `a_b` cannot share or delete one another's token.
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
    let safe = if safe.is_empty() { "secret" } else { &safe };
    let identity = thegn_core::util::short_hash(name, 16);
    Ok(dir.join(format!("{safe}-{identity}.token")))
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
        // A bare account name resolves to the config-adjacent secrets file.
        let path = secrets_file(account).map_err(|e| SecretError::unavailable(e.to_string()))?;
        match std::fs::read_to_string(&path) {
            Ok(value) => {
                let value = value.trim().to_string();
                if value.is_empty() {
                    Err(SecretError::not_found(account.to_string()))
                } else {
                    Ok(value)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(SecretError::not_found(account.to_string()))
            }
            Err(error) => Err(SecretError::denied(format!(
                "read {}: {error}",
                path.display()
            ))),
        }
    }
    fn set(&self, account: &str, value: &str) -> Result<(), SecretError> {
        let path = secrets_file(account).map_err(|e| SecretError::unavailable(e.to_string()))?;
        write_private(&path, value).map_err(|e| SecretError::denied(e.to_string()))
    }
    fn del(&self, account: &str) -> Result<(), SecretError> {
        let path = secrets_file(account).map_err(|e| SecretError::unavailable(e.to_string()))?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SecretError::denied(format!(
                "remove {}: {error}",
                path.display()
            ))),
        }
    }
    fn list(&self) -> Result<Vec<String>, SecretError> {
        let path =
            secrets_file("__probe__").map_err(|e| SecretError::unavailable(e.to_string()))?;
        let dir = match path.parent() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };
        let mut names = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_string_lossy().strip_suffix(".token") {
                    names.push(n.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }
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
    index_add(account);
    Ok(())
}

/// `thegn mcp secret rm` — remove from the keyring and the index.
pub fn mcp_secret_rm(account: &str) -> Result<(), String> {
    KeyringStore
        .del(account)
        .map_err(|error| error.message().to_string())?;
    index_remove(account);
    Ok(())
}

/// `thegn mcp secret list` — the names thegn has stored (never values).
pub fn mcp_secret_list() -> Vec<String> {
    index_read()
}

/// The keyring backend's doctor probe (custody row for the mcp-proxy section).
pub fn mcp_keyring_probe() -> thegn_core::seam::ProbeReport {
    KeyringStore.probe()
}

/// The names-only index path (`secrets/mcp-index.json`).
fn mcp_index_path() -> Option<PathBuf> {
    let cfg = thegn_core::config::Config::path();
    let dir = std::path::Path::new(&cfg).parent()?.join("secrets");
    let _ = std::fs::create_dir_all(&dir); // best-effort: dir prep: a later write reports the real failure
    let _ = thegn_core::fsperm::restrict_dir_to_owner(&dir); // best-effort: hardening: a failed chmod must never block the caller
    Some(dir.join("mcp-index.json"))
}

fn index_read() -> Vec<String> {
    let Some(path) = mcp_index_path() else {
        return Vec::new();
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&body).unwrap_or_default()
}

fn index_write(names: &[String]) {
    let Some(path) = mcp_index_path() else { return };
    if let Ok(body) = serde_json::to_string_pretty(names) {
        // best-effort: the index is a convenience for `secret list`, not truth.
        let _ = std::fs::write(&path, body);
    }
}

fn index_add(account: &str) {
    let mut names = index_read();
    if !names.iter().any(|n| n == account) {
        names.push(account.to_string());
        names.sort();
        index_write(&names);
    }
}

fn index_remove(account: &str) {
    let mut names = index_read();
    let before = names.len();
    names.retain(|n| n != account);
    if names.len() != before {
        index_write(&names);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let f = std::env::temp_dir().join(format!("tg-secret-test-{}.tok", std::process::id()));
        std::fs::write(&f, "  filetoken\n").unwrap();
        assert_eq!(
            resolve(&format!("file:{}", f.display())).as_deref(),
            Some("filetoken")
        );
        let _ = std::fs::remove_file(&f); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
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
        let slash = secrets_file("a/b").unwrap();
        let underscore = secrets_file("a_b").unwrap();
        assert_ne!(slash, underscore);
    }
}
