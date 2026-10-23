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
//! [`store`] is the inverse: it persists a UI-entered token to the keyring, or
//! uses a `0600` file only after a completed keyring failure, and returns the
//! SecretRef string to write into config.

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
const SECRETS_LOCK: &str = "secrets.lock";
// Bounded so a UI-loop caller can never hang on another process's lock, but long
// enough that ordinary CLI/worker contention (each op is a short fsync'd write)
// waits its turn instead of failing as busy.
const SECRETS_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

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
/// preferring the OS keyring and falling back to a `0600` file only after a
/// completed keyring failure. A timeout is an error because the keyring may
/// still commit the write. Returns the SecretRef to store in config
/// (`keyring:<name>` or `file:<path>`), so the token itself never lands in
/// `config.toml`.
pub fn store(name: &str, token: &str) -> Result<String> {
    if let Ok(mut m) = presence_memo().lock() {
        m.clear();
    }
    store_with_dir(name, token, keyring_set, secrets_dir_path)
}

#[cfg(test)]
fn store_in(
    dir: &std::path::Path,
    name: &str,
    token: &str,
    set: impl FnOnce(&str, &str) -> KeyringOutcome<Result<()>>,
) -> Result<String> {
    store_with_dir(name, token, set, || Ok(dir.to_path_buf()))
}

fn store_with_dir(
    name: &str,
    token: &str,
    set: impl FnOnce(&str, &str) -> KeyringOutcome<Result<()>>,
    dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<String> {
    match set(name, token) {
        KeyringOutcome::Completed(Ok(())) => return Ok(format!("keyring:{name}")),
        KeyringOutcome::TimedOut => anyhow::bail!(
            "keyring write outcome unknown for `{name}`; nothing was written to a file; retry once the keyring responds"
        ),
        KeyringOutcome::NotStarted(reason) => anyhow::bail!(
            "keyring write was not started for `{name}`: {reason}; nothing was written to a file"
        ),
        KeyringOutcome::Completed(Err(error)) => {
            tracing::debug!(target: "thegn::secret", %error, "keyring write failed; using owner-only file")
        }
    }
    let dir = dir()?;
    let _lock = lock_secrets_in(&dir)?;
    let private = open_secrets_in(&dir)?;
    let token_name = resolve_token_name(&private, name)?;
    write_account_sidecar_with(&private, &token_name, name)?;
    write_private_with(&private, &token_name, token)?;
    private.sync()?;
    let path = private.path().join(token_name);
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
    let dir = secrets_dir()?;
    let _lock = lock_secrets_in(&dir)?;
    let private = open_secrets_in(&dir)?;
    let token_name = resolve_token_name(&private, name)?;
    write_account_sidecar_with(&private, &token_name, name)?;
    write_private_with(&private, &token_name, token)?;
    private.sync()?;
    let path = private.path().join(token_name);
    Ok(format!("file:{}", path.display()))
}

/// Remove a stored secret (best-effort, both backends) — used when an env is
/// deleted. Never errors on a missing entry.
pub fn forget(name: &str) {
    let report = forget_with_report(name);
    match &report.keyring {
        ForgetKeyringOutcome::Failed(error) => {
            tracing::warn!(target: "thegn::secret", %error, "keyring secret removal failed")
        }
        ForgetKeyringOutcome::OutcomeUnknown => {
            tracing::warn!(target: "thegn::secret", "keyring secret removal outcome is unknown")
        }
        ForgetKeyringOutcome::Removed => {}
    }
    if let ForgetFileOutcome::Failed(error) = &report.file {
        tracing::warn!(target: "thegn::secret", %error, "refused file-backed secret removal");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetKeyringOutcome {
    /// The keyring removal call completed successfully.
    Removed,
    /// The keyring removal call completed with an error.
    Failed(String),
    /// The call timed out, so the keyring may or may not have removed it.
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetFileOutcome {
    /// A file-backed secret or sidecar was removed.
    Removed,
    /// No file-backed secret or sidecar existed.
    Absent,
    /// The file backend could not complete the removal.
    Failed(String),
}

/// Removal outcomes for both secret backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetReport {
    /// Result from the OS keyring.
    pub keyring: ForgetKeyringOutcome,
    /// Result from the owner-only file backend.
    pub file: ForgetFileOutcome,
}

impl ForgetReport {
    pub(crate) fn ensure_file_removed(&self, name: &str) -> Result<()> {
        if let ForgetFileOutcome::Failed(error) = &self.file {
            anyhow::bail!("file-backed secret removal failed for `{name}`: {error}");
        }
        Ok(())
    }
}

/// Remove a stored secret and retain the outcome from each backend for callers
/// that need to report uncertainty (notably `thegn secret rm`).
pub fn forget_with_report(name: &str) -> ForgetReport {
    if let Ok(mut m) = presence_memo().lock() {
        m.clear();
    }
    let dir = match secrets_dir_path() {
        Ok(dir) => dir,
        Err(error) => {
            let keyring = keyring_removal(keyring_del(name));
            return ForgetReport {
                keyring,
                file: ForgetFileOutcome::Failed(error.to_string()),
            };
        }
    };
    forget_in(&dir, name, keyring_del)
}

fn forget_in(
    dir: &std::path::Path,
    name: &str,
    del: impl FnOnce(&str) -> KeyringOutcome<Result<()>>,
) -> ForgetReport {
    let keyring = keyring_removal(del(name));
    // Do not derive and delete the pre-hash fallback filename. That historical
    // sanitizer was not injective (`a/b` and `a_b` both became `a_b.token`), so
    // automatic legacy cleanup could delete a different account's credential.
    // Existing `file:` refs remain directly resolvable and can be removed by
    // their explicit path during a user-directed migration.
    let file = match lock_secrets_in(dir) {
        Err(error) => ForgetFileOutcome::Failed(error.to_string()),
        Ok(_lock) => match open_secrets_in(dir) {
            Err(error) => ForgetFileOutcome::Failed(error.to_string()),
            Ok(private) => match file_entry_exists(&private, name) {
                Err(error) => ForgetFileOutcome::Failed(error.to_string()),
                Ok(existed) => match file_del_with(&private, name) {
                    Ok(()) if existed => ForgetFileOutcome::Removed,
                    Ok(()) => ForgetFileOutcome::Absent,
                    Err(error) => ForgetFileOutcome::Failed(error.message().to_string()),
                },
            },
        },
    };
    ForgetReport { keyring, file }
}

fn keyring_removal(outcome: KeyringOutcome<Result<()>>) -> ForgetKeyringOutcome {
    match outcome {
        KeyringOutcome::Completed(Ok(())) => ForgetKeyringOutcome::Removed,
        KeyringOutcome::Completed(Err(error)) => ForgetKeyringOutcome::Failed(error.to_string()),
        KeyringOutcome::TimedOut => ForgetKeyringOutcome::OutcomeUnknown,
        KeyringOutcome::NotStarted(reason) => ForgetKeyringOutcome::Failed(reason.to_string()),
    }
}

fn file_entry_exists(private: &thegn_core::fsperm::PrivateDir, name: &str) -> Result<bool> {
    let current = token_name(name)?;
    let legacy = legacy_token_name(name)?;
    Ok(private.entry_exists_nofollow(&current)?
        || private.entry_exists_nofollow(&account_name(&current)?)?
        || private.entry_exists_nofollow(&legacy)?
        || private.entry_exists_nofollow(&account_name(&legacy)?)?)
}

fn secrets_dir_path() -> Result<PathBuf> {
    let cfg = thegn_core::config::Config::path();
    std::path::Path::new(&cfg)
        .parent()
        .map(|parent| parent.join("secrets"))
        .context("config path has no parent")
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
/// A stuck probe is answered "unavailable", which is both true and useful for
/// discovery; write operations still decide independently whether a completed
/// keyring failure permits the `0600` file backend.
///
/// Every operation runs on one owned worker. A wedged OS call holds that one
/// thread; the circuit breaker rejects later requests instead of piling up
/// threads or queue entries.
fn probe_keyring_bounded() -> bool {
    matches!(
        run_keyring_op_with_timeout("probe", KEYRING_PROBE_TIMEOUT, probe_keyring),
        KeyringOutcome::Completed(true)
    )
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
        // answered the same way: unavailable. A later write still requires its
        // own completed keyring failure before it may use the file backend.
        Err(_) => false,
    }
}

enum KeyringOutcome<T> {
    Completed(T),
    TimedOut,
    NotStarted(&'static str),
}

const KEYRING_QUEUE: usize = 32;
const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const ABANDONED: u8 = 2;

struct Job {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    run: Box<dyn FnOnce() + Send>,
}

struct KeyringWorker {
    sender: std::sync::mpsc::SyncSender<Job>,
    receiver: std::sync::Mutex<Option<std::sync::mpsc::Receiver<Job>>>,
    busy_since: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    #[cfg(test)]
    threads_started: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl KeyringWorker {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(KEYRING_QUEUE);
        Self {
            sender,
            receiver: std::sync::Mutex::new(Some(receiver)),
            busy_since: std::sync::Arc::new(std::sync::Mutex::new(None)),
            thread: std::sync::Mutex::new(None),
            #[cfg(test)]
            threads_started: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn ensure_started(&self) {
        let Ok(mut receiver) = self.receiver.lock() else {
            return;
        };
        if let Some(receiver) = receiver.take() {
            let busy_since = self.busy_since.clone();
            #[cfg(test)]
            self.threads_started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let thread = thegn_core::thread_name::spawn_named("thegn-keyring", move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive);
                while let Ok(job) = receiver.recv() {
                    if job
                        .state
                        .compare_exchange(
                            QUEUED,
                            STARTED,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    if let Ok(mut busy) = busy_since.lock() {
                        *busy = Some(std::time::Instant::now());
                    }
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.run)).is_err() {
                        tracing::debug!(target: "thegn::secret", "keyring operation panicked");
                    }
                    if let Ok(mut busy) = busy_since.lock() {
                        *busy = None;
                    }
                }
            });
            if let Ok(mut owned_thread) = self.thread.lock() {
                *owned_thread = Some(thread);
            }
        }
    }

    fn run<T: Send + 'static>(
        &self,
        operation: &'static str,
        timeout: std::time::Duration,
        op: impl FnOnce() -> T + Send + 'static,
    ) -> KeyringOutcome<T> {
        self.ensure_started();
        if let Ok(busy) = self.busy_since.lock()
            && busy.is_some_and(|started| started.elapsed() > timeout)
        {
            tracing::debug!(target: "thegn::secret", operation, "keyring worker busy; request not started");
            return KeyringOutcome::NotStarted(
                "keyring busy: an earlier operation has not returned",
            );
        }

        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(QUEUED));
        let job_state = state.clone();
        let job = Job {
            state: job_state,
            run: Box::new(move || {
                let result = op();
                if let Err(error) = reply_tx.send(result) {
                    tracing::debug!(target: "thegn::secret", %error, "keyring caller stopped waiting");
                }
            }),
        };
        if let Err(error) = self.sender.try_send(job) {
            return match error {
                std::sync::mpsc::TrySendError::Full(_) => {
                    tracing::debug!(target: "thegn::secret", operation, "keyring queue full; request not started");
                    KeyringOutcome::NotStarted("keyring queue full")
                }
                std::sync::mpsc::TrySendError::Disconnected(_) => {
                    tracing::debug!(target: "thegn::secret", operation, "keyring worker unavailable; request not started");
                    KeyringOutcome::NotStarted("keyring worker unavailable")
                }
            };
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(value) => KeyringOutcome::Completed(value),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if state
                    .compare_exchange(
                        QUEUED,
                        ABANDONED,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    tracing::debug!(target: "thegn::secret", operation, "keyring request expired before start");
                    KeyringOutcome::NotStarted("keyring did not start the operation in time")
                } else {
                    tracing::warn!(target: "thegn::secret", operation, cap_ms = timeout.as_millis() as u64, "keyring operation timed out — outcome unknown");
                    KeyringOutcome::TimedOut
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if state.load(std::sync::atomic::Ordering::SeqCst) == STARTED {
                    tracing::warn!(target: "thegn::secret", operation, "keyring operation disconnected after start — outcome unknown");
                    KeyringOutcome::TimedOut
                } else {
                    KeyringOutcome::NotStarted("keyring worker unavailable")
                }
            }
        }
    }
}

fn keyring_worker() -> &'static KeyringWorker {
    static WORKER: std::sync::OnceLock<KeyringWorker> = std::sync::OnceLock::new();
    WORKER.get_or_init(KeyringWorker::new)
}

fn run_keyring_op_with_timeout<T: Send + 'static>(
    operation: &'static str,
    timeout: std::time::Duration,
    op: impl FnOnce() -> T + Send + 'static,
) -> KeyringOutcome<T> {
    keyring_worker().run(operation, timeout, op)
}

fn keyring_get(account: &str) -> Result<Option<String>> {
    let account = account.to_string();
    match run_keyring_op_with_timeout("get", KEYRING_PROBE_TIMEOUT, move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)
            .ok()
            .and_then(|entry| entry.get_password().ok())
            .filter(|value| !value.trim().is_empty())
    }) {
        KeyringOutcome::Completed(value) => Ok(value),
        KeyringOutcome::TimedOut => Err(anyhow::anyhow!(
            "keyring get timed out after {}ms",
            KEYRING_PROBE_TIMEOUT.as_millis()
        )),
        KeyringOutcome::NotStarted(reason) => Err(anyhow::anyhow!(reason)),
    }
}

fn keyring_set(account: &str, token: &str) -> KeyringOutcome<Result<()>> {
    let account = account.to_string();
    let token = token.to_string();
    run_keyring_op_with_timeout("set", KEYRING_PROBE_TIMEOUT, move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)
            .context("keyring entry")?
            .set_password(&token)
            .context("keyring set")
    })
}

fn keyring_del(account: &str) -> KeyringOutcome<Result<()>> {
    let account = account.to_string();
    run_keyring_op_with_timeout("delete", KEYRING_PROBE_TIMEOUT, move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account).context("keyring entry")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("keyring delete"),
        }
    })
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

fn open_secrets_in(dir: &std::path::Path) -> Result<thegn_core::fsperm::PrivateDir> {
    secrets_dir_at(dir)?;
    thegn_core::fsperm::open_private_dir(dir)
        .with_context(|| format!("open private secrets directory {}", dir.display()))
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
    let private = open_secrets_in(dir)?;
    Ok(private.path().join(resolve_token_name(&private, account)?))
}

fn resolve_token_name(private: &thegn_core::fsperm::PrivateDir, account: &str) -> Result<String> {
    let v2 = token_name(account)?;
    if private.entry_exists_nofollow(&v2)? || private.entry_exists_nofollow(&account_name(&v2)?)? {
        return Ok(v2);
    }
    let legacy = legacy_token_name(account)?;
    if private.entry_exists_nofollow(&legacy)? {
        account_matches_or_missing_with(private, &legacy, account)?;
        return Ok(legacy);
    }
    Ok(v2)
}

fn token_name(account: &str) -> Result<String> {
    token_path_in(std::path::Path::new("."), account)?
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
        .context("secret token path has no filename")
}

fn legacy_token_name(account: &str) -> Result<String> {
    legacy_token_path_in(std::path::Path::new("."), account)?
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
        .context("legacy secret token path has no filename")
}

#[cfg(test)]
fn account_path(token: &std::path::Path) -> PathBuf {
    token.with_extension("account")
}

fn account_name(token: &str) -> Result<String> {
    std::path::Path::new(token)
        .with_extension("account")
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
        .context("secret token path has no sidecar filename")
}

#[cfg(test)]
fn write_account_sidecar(token_path: &std::path::Path, account: &str) -> Result<()> {
    let parent = token_path.parent().context("secret path has no parent")?;
    let private = open_secrets_in(parent)?;
    let token_name = token_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .context("secret path has no filename")?;
    write_account_sidecar_with(&private, token_name, account)
}

fn write_account_sidecar_with(
    private: &thegn_core::fsperm::PrivateDir,
    token_name: &str,
    account: &str,
) -> Result<()> {
    let sidecar_name = account_name(token_name)?;
    match read_account_sidecar_with(private, &sidecar_name) {
        Ok(existing) if existing != account => anyhow::bail!("secret account digest collision"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match private.open_regular_nofollow(token_name) {
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
    private.write_owner_only_atomic(&sidecar_name, account.as_bytes())?;
    Ok(())
}

fn read_account_sidecar_with(
    private: &thegn_core::fsperm::PrivateDir,
    name: &str,
) -> std::io::Result<String> {
    use std::io::Read;
    let file = private.open_regular_nofollow(name)?;
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

fn account_matches_or_missing_with(
    private: &thegn_core::fsperm::PrivateDir,
    token_name: &str,
    account: &str,
) -> Result<()> {
    match read_account_sidecar_with(private, &account_name(token_name)?) {
        Ok(existing) if existing != account => anyhow::bail!("secret account digest collision"),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("validate secret account sidecar"),
    }
}

#[cfg(test)]
fn write_private(path: &std::path::Path, token: &str) -> Result<()> {
    let parent = path.parent().context("secret path has no parent")?;
    let private = open_secrets_in(parent)?;
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .context("secret path has no filename")?;
    write_private_with(&private, name, token)
}

fn write_private_with(
    private: &thegn_core::fsperm::PrivateDir,
    name: &str,
    token: &str,
) -> Result<()> {
    private
        .write_owner_only_atomic(name, token.trim().as_bytes())
        .map(|_| ())
        .with_context(|| {
            format!(
                "write owner-only secret {}",
                private.path().join(name).display()
            )
        })
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
                 explicit `keyring:` refs are unavailable; broker writes may use an owner-only file after a completed keyring failure"
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
        match keyring_set(account, value) {
            KeyringOutcome::Completed(result) => {
                result.map_err(|error| SecretError::denied(error.to_string()))
            }
            KeyringOutcome::TimedOut => Err(SecretError::denied(
                "keyring outcome unknown: write timed out; retry once the keyring responds",
            )),
            KeyringOutcome::NotStarted(reason) => Err(SecretError::unavailable(reason)),
        }
    }
    fn del(&self, account: &str) -> Result<(), SecretError> {
        match keyring_del(account) {
            KeyringOutcome::Completed(result) => {
                result.map_err(|error| SecretError::denied(error.to_string()))
            }
            KeyringOutcome::TimedOut => Err(SecretError::denied(
                "keyring outcome unknown: removal timed out; retry once the keyring responds",
            )),
            KeyringOutcome::NotStarted(reason) => Err(SecretError::unavailable(reason)),
        }
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

#[cfg(test)]
fn file_get_at(path: &std::path::Path, account: &str) -> Result<String, SecretError> {
    let dir = path
        .parent()
        .ok_or_else(|| SecretError::denied("secret path has no parent"))?;
    let private = open_secrets_in(dir).map_err(|e| SecretError::denied(e.to_string()))?;
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| SecretError::denied("secret path has no filename"))?;
    file_get_with(&private, name, account)
}

fn file_get_with(
    private: &thegn_core::fsperm::PrivateDir,
    token_name: &str,
    account: &str,
) -> Result<String, SecretError> {
    use std::io::Read;
    account_matches_or_missing_with(private, token_name, account)
        .map_err(|e| SecretError::denied(e.to_string()))?;
    let file = private.open_regular_nofollow(token_name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SecretError::not_found(account.to_string())
        } else {
            SecretError::denied(format!("open secret: {error}"))
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|e| SecretError::denied(e.to_string()))?;
    if !thegn_core::fsperm::owned_by_current_user(&metadata) {
        return Err(SecretError::denied("secret file is owned by another user"));
    }
    let path = private.path().join(token_name);
    if thegn_core::fsperm::ensure_single_link_regular_file(&metadata).is_err() {
        return Err(SecretError::denied(format!(
            "secret file {} has other hard links; refusing to read it (remove the extra link, then store the secret again)",
            path.display()
        )));
    }
    if let Some(mode) = thegn_core::fsperm::mode_bits_from_metadata(&metadata)
        && mode & 0o077 != 0
    {
        return Err(SecretError::denied(format!(
            "secret file {} is accessible to other users (mode {mode:o}); refusing to read it (run `chmod 600` on it, or store the secret again)",
            path.display()
        )));
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
    let private = open_secrets_in(dir).map_err(|e| SecretError::unavailable(e.to_string()))?;
    let name =
        resolve_token_name(&private, account).map_err(|e| SecretError::denied(e.to_string()))?;
    file_get_with(&private, &name, account)
}

fn file_set_at(dir: &std::path::Path, account: &str, value: &str) -> Result<(), SecretError> {
    let _lock =
        lock_secrets_in(dir).map_err(|error| SecretError::unavailable(error.to_string()))?;
    let private = open_secrets_in(dir).map_err(|e| SecretError::unavailable(e.to_string()))?;
    file_set_with(&private, account, value)
}

fn file_set_with(
    private: &thegn_core::fsperm::PrivateDir,
    account: &str,
    value: &str,
) -> Result<(), SecretError> {
    let name =
        resolve_token_name(private, account).map_err(|e| SecretError::denied(e.to_string()))?;
    write_account_sidecar_with(private, &name, account)
        .map_err(|e| SecretError::denied(e.to_string()))?;
    write_private_with(private, &name, value).map_err(|e| SecretError::denied(e.to_string()))?;
    private
        .sync()
        .map_err(|e| SecretError::denied(e.to_string()))
}

fn file_del_at(dir: &std::path::Path, account: &str) -> Result<(), SecretError> {
    let _lock =
        lock_secrets_in(dir).map_err(|error| SecretError::unavailable(error.to_string()))?;
    let private = open_secrets_in(dir).map_err(|e| SecretError::unavailable(e.to_string()))?;
    file_del_with(&private, account)
}

fn file_del_with(
    private: &thegn_core::fsperm::PrivateDir,
    account: &str,
) -> Result<(), SecretError> {
    let current = token_name(account).map_err(|e| SecretError::denied(e.to_string()))?;
    let legacy = legacy_token_name(account).map_err(|e| SecretError::denied(e.to_string()))?;
    for name in [&current, &legacy] {
        account_matches_or_missing_with(private, name, account)
            .map_err(|e| SecretError::denied(e.to_string()))?;
    }
    for name in [&current, &legacy] {
        private
            .remove_file(name)
            .map_err(|e| SecretError::denied(format!("remove secret: {e}")))?;
        remove_sidecar_with(private, name)
            .map_err(|e| SecretError::denied(format!("remove secret sidecar: {e}")))?;
    }
    private
        .sync()
        .map_err(|e| SecretError::denied(e.to_string()))
}

fn remove_sidecar_with(
    private: &thegn_core::fsperm::PrivateDir,
    token_name: &str,
) -> std::io::Result<()> {
    let sidecar_name = account_name(token_name)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    private.remove_file(&sidecar_name).map(|_| ())
}

fn file_list_at(dir: &std::path::Path) -> Result<Vec<String>, SecretError> {
    let _lock =
        lock_secrets_in(dir).map_err(|error| SecretError::unavailable(error.to_string()))?;
    let private = open_secrets_in(dir).map_err(|e| SecretError::unavailable(e.to_string()))?;
    file_list_with(&private)
}

fn file_list_with(private: &thegn_core::fsperm::PrivateDir) -> Result<Vec<String>, SecretError> {
    // Legacy tokens without an exact-identity sidecar are deliberately omitted.
    let entries = private
        .file_names()
        .map_err(|e| SecretError::unavailable(format!("read secret inventory: {e}")))?;
    let mut names = Vec::new();
    for entry in entries {
        if std::path::Path::new(&entry)
            .extension()
            .and_then(|v| v.to_str())
            != Some("account")
        {
            continue;
        }
        let sidecar_name = entry
            .to_str()
            .ok_or_else(|| SecretError::denied("invalid non-UTF-8 secret account sidecar name"))?;
        let account = read_account_sidecar_with(private, sidecar_name)
            .map_err(|e| SecretError::denied(format!("invalid secret account sidecar: {e}")))?;
        let v2 = token_name(&account).map_err(|e| SecretError::denied(e.to_string()))?;
        let legacy = legacy_token_name(&account).map_err(|e| SecretError::denied(e.to_string()))?;
        let token =
            if account_name(&v2).map_err(|e| SecretError::denied(e.to_string()))? == sidecar_name {
                v2
            } else if account_name(&legacy).map_err(|e| SecretError::denied(e.to_string()))?
                == sidecar_name
            {
                legacy
            } else {
                return Err(SecretError::denied(
                    "secret account sidecar identity mismatch",
                ));
            };
        match private.open_regular_nofollow(&token) {
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

fn open_dir_lock(
    dir: &std::path::Path,
    lock_name: &str,
    what: &str,
) -> Result<std::fs::File, String> {
    let file = open_dir_lock_file(dir, lock_name, what)?;
    file.lock().map_err(|e| format!("lock {what}: {e}"))?;
    Ok(file)
}

fn open_dir_lock_file(
    dir: &std::path::Path,
    lock_name: &str,
    what: &str,
) -> Result<std::fs::File, String> {
    let path = dir.join(lock_name);
    let file = loop {
        match thegn_core::fsperm::open_regular_file_nofollow(&path) {
            Ok(file) => break file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match thegn_core::fsperm::create_owner_only_exclusive(&path) {
                    Ok(file) => break file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(format!("open {what} lock: {error}")),
                }
            }
            Err(error) => return Err(format!("open {what} lock: {error}")),
        }
    };
    if !thegn_core::fsperm::owned_by_current_user(
        &file
            .metadata()
            .map_err(|e| format!("inspect {what} lock: {e}"))?,
    ) {
        return Err(format!("{what} lock is owned by another user"));
    }
    Ok(file)
}

#[derive(Debug)]
enum SecretsLockError {
    Busy,
    Other(String),
}

impl std::fmt::Display for SecretsLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("secrets store busy"),
            Self::Other(error) => f.write_str(error),
        }
    }
}

impl std::error::Error for SecretsLockError {}

/// Held for one file-backend operation. Threads of this process queue on the
/// mutex first, so they never starve each other while polling the bounded
/// cross-process file lock (fields drop in order: file lock, then mutex).
struct SecretsLock {
    _file: std::fs::File,
    _process: std::sync::MutexGuard<'static, ()>,
}

static SECRETS_PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_secrets_in(dir: &std::path::Path) -> Result<SecretsLock, SecretsLockError> {
    let process = SECRETS_PROCESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    secrets_dir_at(dir).map_err(|error| SecretsLockError::Other(error.to_string()))?;
    let file =
        open_dir_lock_file(dir, SECRETS_LOCK, "file secrets").map_err(SecretsLockError::Other)?;
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(SecretsLock {
                    _file: file,
                    _process: process,
                });
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let elapsed = started.elapsed();
                if elapsed >= SECRETS_LOCK_WAIT {
                    return Err(SecretsLockError::Busy);
                }
                std::thread::sleep(
                    std::time::Duration::from_millis(5).min(SECRETS_LOCK_WAIT - elapsed),
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(SecretsLockError::Other(format!(
                    "lock file secrets: {error}"
                )));
            }
        }
    }
}

fn with_index_lock<T>(
    dir: &std::path::Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _lock = open_dir_lock(dir, "mcp-index.lock", "MCP secret index")?;
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
    fn keyring_worker_bounded_operation_times_out_as_typed_outcome() {
        let worker = std::sync::Arc::new(KeyringWorker::new());
        assert!(matches!(
            worker.run("warm-up", std::time::Duration::from_secs(2), || true),
            KeyringOutcome::Completed(true)
        ));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        let result = worker.run("test", std::time::Duration::from_millis(200), move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            true
        });
        let elapsed = started.elapsed();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(matches!(result, KeyringOutcome::TimedOut));
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "elapsed: {elapsed:?}"
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn keyring_worker_thousand_calls_use_one_thread_and_fail_fast() {
        let worker = std::sync::Arc::new(KeyringWorker::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let blocked_worker = worker.clone();
        let blocker = std::thread::spawn(move || {
            blocked_worker.run("blocker", std::time::Duration::from_secs(5), move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                true
            })
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let started = std::time::Instant::now();
        let mut not_started = 0_usize;
        let mut timed_out = 0_usize;
        let mut completed = 0_usize;
        for _ in 0..1000 {
            match worker.run("burst", std::time::Duration::from_millis(10), || true) {
                KeyringOutcome::NotStarted(_) => not_started += 1,
                KeyringOutcome::TimedOut => timed_out += 1,
                KeyringOutcome::Completed(_) => completed += 1,
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(
            worker
                .threads_started
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(not_started >= 990, "not started: {not_started}");
        assert_eq!(not_started + timed_out, 1000, "completed: {completed}");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "elapsed: {elapsed:?}"
        );
        release_tx.send(()).unwrap();
        assert!(matches!(
            blocker.join().unwrap(),
            KeyringOutcome::Completed(true)
        ));
        assert!(matches!(
            worker.run("after-release", std::time::Duration::from_secs(2), || true),
            KeyringOutcome::Completed(true)
        ));
    }

    #[test]
    fn keyring_worker_abandoned_request_never_runs() {
        let worker = std::sync::Arc::new(KeyringWorker::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let blocked_worker = worker.clone();
        let blocker = std::thread::spawn(move || {
            blocked_worker.run("blocker", std::time::Duration::from_secs(5), move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                true
            })
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let operation_reached = reached.clone();
        assert!(matches!(
            worker.run(
                "abandoned",
                std::time::Duration::from_millis(20),
                move || {
                    operation_reached.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    true
                }
            ),
            KeyringOutcome::NotStarted(_)
        ));
        release_tx.send(()).unwrap();
        assert!(matches!(
            blocker.join().unwrap(),
            KeyringOutcome::Completed(true)
        ));
        let drain_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while worker
            .busy_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
            && std::time::Instant::now() < drain_deadline
        {
            std::thread::yield_now();
        }
        assert!(matches!(
            worker.run("barrier", std::time::Duration::from_secs(2), || true),
            KeyringOutcome::Completed(true)
        ));
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn keyring_worker_started_request_that_times_out_is_unknown() {
        let worker = std::sync::Arc::new(KeyringWorker::new());
        assert!(matches!(
            worker.run("warm-up", std::time::Duration::from_secs(2), || true),
            KeyringOutcome::Completed(true)
        ));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let running_worker = worker.clone();
        let started = std::time::Instant::now();
        let request = std::thread::spawn(move || {
            running_worker.run(
                "started-timeout",
                std::time::Duration::from_millis(200),
                move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    true
                },
            )
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let result = request.join().unwrap();
        let elapsed = started.elapsed();
        assert!(matches!(result, KeyringOutcome::TimedOut));
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "elapsed: {elapsed:?}"
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn keyring_worker_panic_reports_unknown_and_keeps_worker_alive() {
        let worker = KeyringWorker::new();
        assert!(matches!(
            worker.run("warm-up", std::time::Duration::from_secs(2), || true),
            KeyringOutcome::Completed(true)
        ));
        assert!(matches!(
            worker.run("panic", std::time::Duration::from_secs(2), || -> bool {
                panic!("test keyring panic")
            }),
            KeyringOutcome::TimedOut
        ));
        assert!(matches!(
            worker.run("after-panic", std::time::Duration::from_secs(2), || true),
            KeyringOutcome::Completed(true)
        ));
    }

    #[test]
    fn timed_out_keyring_set_writes_no_file_fallback() {
        let temp = private_temp();
        let result = store_in(temp.path(), "acct", "tok", |_, _| KeyringOutcome::TimedOut);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn keyring_worker_not_started_write_never_falls_back_to_file() {
        let temp = private_temp();
        let result = store_in(temp.path(), "acct", "tok", |_, _| {
            KeyringOutcome::NotStarted("x")
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not started"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn completed_keyring_set_failure_still_falls_back_to_file() {
        let temp = private_temp();
        let result = store_in(temp.path(), "acct", "tok", |_, _| {
            KeyringOutcome::Completed(Err(anyhow::anyhow!("denied")))
        })
        .unwrap();
        assert!(result.starts_with("file:"));
        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), "tok");
    }

    #[test]
    fn completed_keyring_set_success_returns_keyring_ref() {
        let temp = private_temp();
        let result = store_in(temp.path(), "acct", "tok", |_, _| {
            KeyringOutcome::Completed(Ok(()))
        })
        .unwrap();
        assert_eq!(result, "keyring:acct");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn timed_out_keyring_delete_is_reported_unknown() {
        let temp = private_temp();
        file_set_at(temp.path(), "acct", "tok").unwrap();
        let report = forget_in(temp.path(), "acct", |_| KeyringOutcome::TimedOut);
        assert_eq!(report.keyring, ForgetKeyringOutcome::OutcomeUnknown);
        assert_eq!(report.file, ForgetFileOutcome::Removed);
        assert!(
            matches!(file_get_in(temp.path(), "acct"), Err(error) if error.message() == "acct")
        );
    }

    #[test]
    fn completed_keyring_delete_reports_removed() {
        let temp = private_temp();
        let report = forget_in(temp.path(), "acct", |_| KeyringOutcome::Completed(Ok(())));
        assert_eq!(report.keyring, ForgetKeyringOutcome::Removed);
        assert_eq!(report.file, ForgetFileOutcome::Absent);
    }

    #[test]
    fn failed_file_removal_makes_report_an_error() {
        let report = ForgetReport {
            keyring: ForgetKeyringOutcome::Removed,
            file: ForgetFileOutcome::Failed("permission denied".to_string()),
        };
        let result = report.ensure_file_removed("acct");
        assert!(
            matches!(result, Err(error) if error.to_string().contains("file-backed secret removal failed for `acct`"))
        );
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
    fn account_sidecar_name_requires_a_filename() {
        assert!(account_name("/").is_err());
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
    fn file_store_refuses_a_symlinked_secrets_dir() {
        let temp = private_temp();
        let outside = temp.path().join("outside");
        let secrets = temp.path().join("secrets");
        std::fs::create_dir(&outside).unwrap();
        thegn_core::fsperm::ensure_private_dir(&outside).unwrap();
        let canary = token_path_in(&outside, "acct").unwrap();
        std::fs::write(&canary, b"canary").unwrap();
        if thegn_core::fsperm::create_symlink(&outside, &secrets, true).is_err() {
            return;
        }

        assert!(file_get_in(&secrets, "acct").is_err());
        assert!(file_set_at(&secrets, "acct", "replacement").is_err());
        assert!(file_del_at(&secrets, "acct").is_err());
        assert_eq!(std::fs::read(&canary).unwrap(), b"canary");
        assert!(canary.exists());
    }

    #[test]
    fn file_ops_after_open_do_not_follow_a_swapped_dir() {
        let temp = private_temp();
        let secrets = temp.path().join("secrets");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&secrets).unwrap();
        std::fs::create_dir(&outside).unwrap();
        thegn_core::fsperm::ensure_private_dir(&secrets).unwrap();
        thegn_core::fsperm::ensure_private_dir(&outside).unwrap();
        let canary = token_path_in(&outside, "acct").unwrap();
        std::fs::write(&canary, b"canary").unwrap();
        write_account_sidecar(&canary, "acct").unwrap();
        let private = open_secrets_in(&secrets).unwrap();

        std::fs::rename(&secrets, temp.path().join("moved")).unwrap();
        if thegn_core::fsperm::create_symlink(&outside, &secrets, true).is_err() {
            return;
        }
        file_del_with(&private, "acct").unwrap();

        assert_eq!(std::fs::read(&canary).unwrap(), b"canary");
        assert_eq!(std::fs::read(account_path(&canary)).unwrap(), b"acct");
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
        thegn_core::fsperm::restrict_to_owner(&oversized).unwrap();
        assert!(matches!(
            file_get_at(&oversized, "oversized"),
            Err(ref error) if error.message().contains("size limit")
        ));
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
    fn secrets_lock_blocks_file_set_until_released() {
        let temp = private_temp();
        let dir = temp.path().to_path_buf();
        let lock = open_dir_lock(&dir, SECRETS_LOCK, "file secrets").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            file_set_at(&dir, "acct", "v1")
        });

        started_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let entries: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(entries.iter().all(|entry| {
            let name = entry.to_string_lossy();
            !name.ends_with(".token") && !name.ends_with(".account")
        }));

        drop(lock);
        worker.join().unwrap().unwrap();
        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), "v1");
    }

    #[test]
    fn secrets_lock_blocks_file_delete_until_released() {
        let temp = private_temp();
        file_set_at(temp.path(), "acct", "v1").unwrap();
        let dir = temp.path().to_path_buf();
        let lock = open_dir_lock(&dir, SECRETS_LOCK, "file secrets").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            file_del_at(&dir, "acct")
        });

        started_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), "v1");

        drop(lock);
        worker.join().unwrap().unwrap();
        assert!(file_get_in(temp.path(), "acct").is_err());
    }

    #[test]
    fn secrets_lock_store_creates_missing_directory() {
        let temp = private_temp();
        let secrets = temp.path().join("secrets");
        let result = store_in(&secrets, "acct", "tok", |_, _| {
            KeyringOutcome::Completed(Err(anyhow::anyhow!("denied")))
        })
        .unwrap();

        assert!(result.starts_with("file:"));
        assert_eq!(file_get_in(&secrets, "acct").unwrap(), "tok");
    }

    #[test]
    fn secrets_lock_forget_creates_missing_directory_and_reports_absent() {
        let temp = private_temp();
        let secrets = temp.path().join("secrets");
        let report = forget_in(&secrets, "acct", |_| KeyringOutcome::Completed(Ok(())));

        assert_eq!(report.file, ForgetFileOutcome::Absent);
        assert!(secrets.join(SECRETS_LOCK).exists());
    }

    #[test]
    fn secrets_lock_does_not_write_through_symlinked_root() {
        let temp = private_temp();
        let outside = temp.path().join("outside");
        let secrets = temp.path().join("secrets");
        std::fs::create_dir(&outside).unwrap();
        thegn_core::fsperm::ensure_private_dir(&outside).unwrap();
        thegn_core::fsperm::create_symlink(&outside, &secrets, true).unwrap();

        assert!(file_set_at(&secrets, "acct", "tok").is_err());
        assert!(!outside.join(SECRETS_LOCK).exists());
    }

    #[test]
    fn concurrent_file_set_and_delete_keep_token_and_sidecar_paired() {
        // Positive control: this checks the final invariant without asserting
        // that the unsynchronized implementation must lose a particular race.
        let temp = private_temp();
        let dir = temp.path().to_path_buf();
        let workers: Vec<_> = (0..8)
            .map(|worker_id| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for iteration in 0..20 {
                        file_set_at(&dir, "shared", &format!("{worker_id}-{iteration}")).unwrap();
                        file_del_at(&dir, "shared").unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let token = token_name("shared").unwrap();
        let sidecar = account_name(&token).unwrap();
        let private = open_secrets_in(temp.path()).unwrap();
        assert_eq!(
            private.entry_exists_nofollow(&token).unwrap(),
            private.entry_exists_nofollow(&sidecar).unwrap()
        );
        assert!(file_list_at(temp.path()).is_ok());
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

    #[test]
    fn file_get_refuses_a_group_readable_token() {
        const VALUE: &str = "s3cret-value-Zq9";

        let temp = private_temp();
        file_set_at(temp.path(), "acct", VALUE).unwrap();
        let token = token_path_in(temp.path(), "acct").unwrap();
        match thegn_core::fsperm::set_mode_for_test(&token, 0o640) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return,
            Err(error) => panic!("could not set fixture mode: {error}"),
        }
        let error = match file_get_in(temp.path(), "acct") {
            Err(error) => error,
            Ok(_) => panic!("group-readable token was accepted"),
        };
        assert!(error.message().contains("accessible to other users"));
        assert!(!error.message().contains(VALUE));
        assert!(error.message().contains("chmod 600"));

        thegn_core::fsperm::set_mode_for_test(&token, 0o600).unwrap();
        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), VALUE);
    }

    #[test]
    fn file_get_refuses_a_hard_linked_token() {
        const VALUE: &str = "s3cret-value-Zq9";

        let temp = private_temp();
        file_set_at(temp.path(), "acct", VALUE).unwrap();
        let token = token_path_in(temp.path(), "acct").unwrap();
        let extra_link = temp.path().join("extra-link");
        match std::fs::hard_link(&token, &extra_link) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return;
            }
            Err(error) => panic!("could not create hard-link fixture: {error}"),
        }
        let error = match file_get_in(temp.path(), "acct") {
            Err(error) => error,
            Ok(_) => panic!("multiply linked token was accepted"),
        };
        assert!(error.message().contains("other hard links"));
        assert!(!error.message().contains(VALUE));
        assert!(error.message().contains("remove the extra link"));

        std::fs::remove_file(extra_link).unwrap();
        assert_eq!(file_get_in(temp.path(), "acct").unwrap(), VALUE);
    }

    #[test]
    fn file_get_refuses_a_world_writable_token() {
        const VALUE: &str = "s3cret-value-Zq9";

        let temp = private_temp();
        file_set_at(temp.path(), "acct", VALUE).unwrap();
        let token = token_path_in(temp.path(), "acct").unwrap();
        match thegn_core::fsperm::set_mode_for_test(&token, 0o602) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return,
            Err(error) => panic!("could not set fixture mode: {error}"),
        }
        let error = match file_get_in(temp.path(), "acct") {
            Err(error) => error,
            Ok(_) => panic!("world-writable token was accepted"),
        };
        assert!(error.message().contains("accessible to other users"));
        assert!(!error.message().contains(VALUE));
    }
}
