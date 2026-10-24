//! Client-side account switching for coding-agent CLIs (roadmap item 656).
//!
//! Codex and Claude Code each locate their *entire* credential home from a
//! single env var — Codex honors `CODEX_HOME` (default `~/.codex`) and Claude
//! Code honors `CLAUDE_CONFIG_DIR` (default `~/.claude`). thegn registers N
//! credential homes per provider and injects the chosen one's env var at
//! agent-launch time, so the user's real `~/.codex` / `~/.claude` is never
//! touched and a switch only affects newly launched agents.
//!
//! Accounts come from two places, merged here:
//!   * config `[[accounts]]` (adopt an existing dir, or a managed dir to log into)
//!   * the DB `accounts` table (created by the in-app "Add account" login flow)
//!
//! The *active* account is resolved by precedence (most specific first):
//! worktree override (`ui_state[account:<p>:wt:<path>]`) → workspace config
//! (`[workspace.<slug>] accounts.<p>`) → workspace pointer
//! (`ui_state[account:<p>:ws:<slug>]`) → global active (`ui_state[account:<p>]`)
//! → none (the CLI falls back to its own default home — backwards compatible).

use crate::config::Config;
use crate::db::Db;
use crate::store::{AccountStore, WorkspaceStore};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

const OWNER_MANIFEST: &str = ".thegn-account-owner";
const OWNER_MANIFEST_LIMIT: u64 = 4 * 1024;

/// A managed credential home could not be safely established or verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountHomeError {
    /// A managed path component or manifest could not be safely opened or written.
    UnsafePath { path: PathBuf, error: String },
    /// The existing manifest belongs to another exact provider/account pair.
    ForeignManifest { provider: String, name: String },
    /// The existing manifest is not a valid versioned owner record.
    MalformedManifest { path: PathBuf },
    /// A managed account row points somewhere other than its canonical home.
    RelocatedManagedHome { stored: PathBuf, expected: PathBuf },
    /// A new managed account's canonical home already exists (it may hold a forgotten account's
    /// login).
    HomeExists { path: PathBuf },
    /// A manifest-less legacy home may belong to another account with the same old slug.
    AmbiguousLegacyHome { path: PathBuf, aliases: Vec<String> },
    /// The requested provider is not account-switchable.
    UnknownProvider { provider: String },
}

impl std::fmt::Display for AccountHomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafePath { path, error } => {
                write!(f, "unsafe managed account path {}: {error}", path.display())
            }
            Self::ForeignManifest { provider, name } => {
                write!(
                    f,
                    "managed account home belongs to {provider} account {name:?}"
                )
            }
            Self::MalformedManifest { path } => {
                write!(
                    f,
                    "malformed managed account owner manifest at {}",
                    path.display()
                )
            }
            Self::RelocatedManagedHome { stored, expected } => write!(
                f,
                "managed account home {} is not the expected managed home {}; log in to the account again to recreate it (the old directory is left untouched)",
                stored.display(),
                expected.display()
            ),
            Self::HomeExists { path } => write!(
                f,
                "managed account home {} already exists and may hold another account's login; refusing to reuse it for a new account",
                path.display()
            ),
            Self::AmbiguousLegacyHome { path, aliases } => write!(
                f,
                "managed account home {} has no owner record and may hold the login of {} (older thegn versions used this directory for them too); refusing to claim it: move it aside, then log in again",
                path.display(),
                aliases.join(", ")
            ),
            Self::UnknownProvider { provider } => {
                write!(f, "unknown account provider {provider:?}")
            }
        }
    }
}

impl std::error::Error for AccountHomeError {}

fn owner_manifest(provider_id: &str, name: &str) -> Vec<u8> {
    format!(
        "thegn-account-home v1\nprovider={provider_id}\nname={}\n",
        encode_hex(name.as_bytes())
    )
    .into_bytes()
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            Some(digit(pair[0])? * 16 + digit(pair[1])?)
        })
        .collect()
}

fn parse_owner_manifest(bytes: &[u8]) -> Option<(&str, String)> {
    if !bytes.ends_with(b"\n") {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.split_terminator('\n');
    if lines.next()? != "thegn-account-home v1" {
        return None;
    }
    let provider = lines.next()?.strip_prefix("provider=")?;
    let name_hex = lines.next()?.strip_prefix("name=")?;
    if lines.next().is_some() {
        return None;
    }
    let name = String::from_utf8(decode_hex(name_hex)?).ok()?;
    Some((provider, name))
}

/// Establish or verify the private, owner-bound credential home for an account.
/// Legacy managed homes without a manifest are claimed on first use.
pub fn ensure_managed_home(provider_id: &str, name: &str) -> Result<PathBuf, AccountHomeError> {
    ensure_managed_home_claiming(provider_id, name, &[])
}

/// Establish the credential home for a NEW managed account. Unlike
/// [`ensure_managed_home`], an existing entry at the canonical path is never
/// adopted: it may hold a removed account's login.
pub fn create_managed_home(provider_id: &str, name: &str) -> Result<PathBuf, AccountHomeError> {
    if provider(provider_id).is_none() {
        return Err(AccountHomeError::UnknownProvider {
            provider: provider_id.to_owned(),
        });
    }
    let path = managed_dir(provider_id, name);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => return Err(AccountHomeError::HomeExists { path }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AccountHomeError::UnsafePath {
                path,
                error: error.to_string(),
            });
        }
    }
    // The existence check and creation are separate operations; the rooted
    // private-directory helper still refuses unsafe path components.
    ensure_managed_home_claiming(provider_id, name, &[])
}

fn ensure_managed_home_claiming(
    provider_id: &str,
    name: &str,
    legacy_aliases: &[String],
) -> Result<PathBuf, AccountHomeError> {
    if provider(provider_id).is_none() {
        return Err(AccountHomeError::UnknownProvider {
            provider: provider_id.to_owned(),
        });
    }
    let anchor = util::xdg_state_home().join("thegn");
    let anchor_error = |error: std::io::Error| AccountHomeError::UnsafePath {
        path: anchor.clone(),
        error: error.to_string(),
    };
    std::fs::create_dir_all(&anchor).map_err(anchor_error)?;
    let rel = Path::new("accounts")
        .join(provider_id)
        .join(home_component(name));
    let private = crate::fsperm::ensure_private_dir_beneath(&anchor, &rel).map_err(|error| {
        AccountHomeError::UnsafePath {
            path: anchor.join(rel),
            error: error.to_string(),
        }
    })?;
    let path = private.path().to_path_buf();
    let manifest = match private.open_regular_nofollow(OWNER_MANIFEST) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(OWNER_MANIFEST_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| AccountHomeError::UnsafePath {
                    path: path.join(OWNER_MANIFEST),
                    error: error.to_string(),
                })?;
            if bytes.len() > OWNER_MANIFEST_LIMIT as usize {
                return Err(AccountHomeError::MalformedManifest {
                    path: path.join(OWNER_MANIFEST),
                });
            }
            Some(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            if private
                .entry_exists_nofollow(OWNER_MANIFEST)
                .map_err(|exists_error| AccountHomeError::UnsafePath {
                    path: path.join(OWNER_MANIFEST),
                    error: exists_error.to_string(),
                })?
            {
                return Err(AccountHomeError::UnsafePath {
                    path: path.join(OWNER_MANIFEST),
                    error: error.to_string(),
                });
            }
            None
        }
    };
    match manifest {
        Some(bytes) => {
            let Some((manifest_provider, manifest_name)) = parse_owner_manifest(&bytes) else {
                return Err(AccountHomeError::MalformedManifest {
                    path: path.join(OWNER_MANIFEST),
                });
            };
            if manifest_provider != provider_id || manifest_name != name {
                return Err(AccountHomeError::ForeignManifest {
                    provider: manifest_provider.to_owned(),
                    name: manifest_name,
                });
            }
        }
        None => {
            if !legacy_aliases.is_empty() {
                let entries =
                    private
                        .file_names()
                        .map_err(|error| AccountHomeError::UnsafePath {
                            path: path.clone(),
                            error: error.to_string(),
                        })?;
                if !entries.is_empty() {
                    return Err(AccountHomeError::AmbiguousLegacyHome {
                        path,
                        aliases: legacy_aliases.to_vec(),
                    });
                }
            }
            private
                .write_owner_only_atomic(OWNER_MANIFEST, &owner_manifest(provider_id, name))
                .map_err(|error| AccountHomeError::UnsafePath {
                    path: path.join(OWNER_MANIFEST),
                    error: error.to_string(),
                })?;
        }
    }
    Ok(path)
}

/// A `[[accounts]]` entry — one credential home for a coding-agent provider
/// (`codex`/`claude`), used by client-side account switching. thegn points
/// the agent's credential-home env var (`CODEX_HOME` / `CLAUDE_CONFIG_DIR`) at
/// the chosen account on launch, so the user's real `~/.codex` / `~/.claude` is
/// never modified. `dir` omitted ⇒ thegn manages the dir under the state dir
/// (use "Add account" to log in); `dir` set ⇒ adopt an existing login dir.
/// (A config type; it lives here with its domain logic and is re-exported by
/// `config` so `config::Account` paths keep working.)
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Account {
    pub name: String,
    pub provider: String,
    /// Credential home directory (`~` expanded). When absent, thegn manages a
    /// dir at `$XDG_STATE_HOME/thegn/accounts/<provider>/<identity>/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// A coding-agent CLI whose credentials live in a relocatable home directory.
///
/// This is a **projection** of the [`crate::harness`] seam: the per-vendor facts
/// (`home_env`, `default_dir`, `auth_marker`, `login_argv`) live in one place —
/// the harness impl — and this account-switching layer reads them from there.
/// The struct is kept so the many account/bundle/sandbox callers are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// Stable id used in config + DB (`"codex"`, `"claude"`).
    pub id: &'static str,
    /// Env var that relocates the credential home.
    pub home_env: &'static str,
    /// The CLI's default home dir basename under `$HOME` (display / detection).
    pub default_dir: &'static str,
    /// argv that performs an interactive login into the home dir.
    pub login_argv: &'static [&'static str],
    /// File whose presence under the home dir proves a successful login.
    pub auth_marker: &'static str,
}

impl Provider {
    /// Project a relocatable-home harness into a `Provider`. `None` for a
    /// harness with no relocatable credential home (`aider`, `antigravity`) —
    /// those are not account-switchable and never appeared in this table.
    fn from_harness(h: &'static dyn crate::harness::Harness) -> Option<Provider> {
        let home = h.home().filter(crate::harness::HomeSpec::is_relocatable)?;
        Some(Provider {
            id: h.id(),
            home_env: home.home_env,
            default_dir: home.default_dir,
            login_argv: h.login_argv(),
            auth_marker: home.auth_marker,
        })
    }
}

/// The account-switchable providers, projected from the closed harness registry
/// (`[codex, claude]` — the order the pickers and usage grouping rely on).
/// Extend by adding a harness impl in [`crate::harness`], not this list.
pub fn providers() -> &'static [Provider] {
    static PROVIDERS: std::sync::LazyLock<Vec<Provider>> = std::sync::LazyLock::new(|| {
        crate::harness::HARNESSES
            .iter()
            .copied()
            .filter_map(Provider::from_harness)
            .collect()
    });
    &PROVIDERS
}

/// Look up a provider by id.
pub fn provider(id: &str) -> Option<&'static Provider> {
    providers().iter().find(|p| p.id == id)
}

/// Infer the provider from a command line's program basename
/// (`/usr/bin/codex --foo` → codex).
pub fn infer_provider(command: &str) -> Option<&'static Provider> {
    let prog = command.split_whitespace().next()?;
    // Both separators + `.exe` so `C:\…\codex.exe` infers like `/usr/bin/codex`.
    let base = crate::util::basename(prog);
    provider(base.strip_suffix(".exe").unwrap_or(base))
}

/// The provider for an agent `choice`: an explicit `provider` field on the
/// matching `[[agents]]`/`[[tools]]` entry wins, else inferred from its command.
pub fn provider_for(cfg: &Config, choice: &str) -> Option<&'static Provider> {
    let nc = cfg
        .agents
        .iter()
        .chain(cfg.tools.iter())
        .find(|a| a.name == choice)?;
    if let Some(id) = nc.provider.as_deref() {
        return provider(id);
    }
    infer_provider(&nc.command)
}

/// Root of every thegn-managed account dir (`$XDG_STATE_HOME/thegn/accounts`).
pub fn managed_root() -> PathBuf {
    util::xdg_state_home().join("thegn").join("accounts")
}

/// Canonical, lossless component used for every managed account-home path.
/// Existing canonical slugs keep their paths; other names carry a digest of
/// the exact configured name so slug aliases cannot share credential bytes.
pub fn home_component(name: &str) -> String {
    let slug = util::slugify(name);
    if !slug.is_empty() && slug == name {
        return slug;
    }
    use sha2::{Digest, Sha256};
    let prefix = if slug.is_empty() { "acct" } else { &slug };
    let prefix = &prefix[..prefix.len().min(40)];
    let digest = Sha256::digest(name.as_bytes());
    let hex = format!("{digest:x}");
    format!("{prefix}_{}", &hex[..16])
}

/// The managed credential-home dir for an account thegn owns
/// (`$XDG_STATE_HOME/thegn/accounts/<provider>/<identity>/`).
pub fn managed_dir(provider_id: &str, name: &str) -> PathBuf {
    managed_root().join(provider_id).join(home_component(name))
}

/// One resolved account: where its creds live and whether it is logged in.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub name: String,
    pub provider: String,
    pub dir: PathBuf,
    /// `true` when thegn manages the dir (vs an adopted/config dir).
    pub managed: bool,
    /// `true` when the provider's auth marker exists under `dir`.
    pub authed: bool,
}

fn marked_authed(provider_id: &str, dir: &std::path::Path) -> bool {
    match provider(provider_id) {
        Some(p) => dir.join(p.auth_marker).exists(),
        None => false,
    }
}

/// The credential-home dir for a named account: config entries first
/// (adopted `dir`, or its managed dir), then the DB-managed table.
pub fn account_dir(cfg: &Config, db: &Db, provider_id: &str, name: &str) -> Option<PathBuf> {
    let entries = resolved_entries(cfg, db, provider_id);
    let aliases = legacy_aliases(&entries, name);
    let shared = shared_dirs(
        &entries
            .iter()
            .map(|(n, d, _)| (n.clone(), d.clone()))
            .collect::<Vec<_>>(),
    );
    if shared.contains(name) {
        crate::msg::warn(&format!(
            "account '{name}' shares a credential directory with another account; refusing to use it"
        ));
        return None;
    }
    let (_, dir, managed) = entries.into_iter().find(|(n, _, _)| n == name)?;
    if managed && let Err(error) = managed_home_check(provider_id, name, &dir) {
        crate::msg::warn(&format!(
            "account '{name}' has a relocated managed credential home; refusing to use it: {error}"
        ));
        return None;
    }
    if managed {
        match ensure_managed_home_claiming(provider_id, name, &aliases) {
            Ok(verified) => Some(verified),
            Err(error) => {
                crate::msg::warn(&format!(
                    "account '{name}' has an unsafe credential home; refusing to use it: {error}"
                ));
                None
            }
        }
    } else {
        Some(dir)
    }
}

fn legacy_aliases(entries: &[(String, PathBuf, bool)], name: &str) -> Vec<String> {
    let component = home_component(name);
    let mut aliases: Vec<_> = entries
        .iter()
        .filter(|(other, _, managed)| {
            other != name && *managed && util::slugify(other) == component
        })
        .map(|(other, _, _)| other.clone())
        .collect();
    aliases.sort();
    aliases.dedup();
    aliases
}

fn managed_home_check(
    provider_id: &str,
    name: &str,
    stored: &Path,
) -> Result<(), AccountHomeError> {
    let expected = managed_dir(provider_id, name);
    if stored == expected {
        Ok(())
    } else {
        Err(AccountHomeError::RelocatedManagedHome {
            stored: stored.to_path_buf(),
            expected,
        })
    }
}

fn resolved_entries(cfg: &Config, db: &Db, provider_id: &str) -> Vec<(String, PathBuf, bool)> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for account in cfg.accounts.iter().filter(|a| a.provider == provider_id) {
        let dir = match &account.dir {
            Some(d) => PathBuf::from(util::expand_tilde(d)),
            None => managed_dir(provider_id, &account.name),
        };
        seen.insert(account.name.clone());
        entries.push((account.name.clone(), dir, account.dir.is_none()));
    }
    for (name, dir, managed) in db.list_accounts(provider_id).unwrap_or_default() {
        if seen.insert(name.clone()) {
            entries.push((name, PathBuf::from(dir), managed));
        }
    }
    entries
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DirKey {
    /// Same filesystem object (follows symlinks; catches bind mounts).
    Fs(String),
    /// The directory does not exist yet: compare lexically.
    Lexical(PathBuf),
}

fn dir_key(dir: &Path) -> DirKey {
    match util::file_identity(dir) {
        Some(id) => DirKey::Fs(id),
        None => DirKey::Lexical(dir.components().collect()),
    }
}

/// Names participating in a shared physical directory. Existing directories
/// are compared by filesystem identity (including symlinks, `..` spellings,
/// and bind mounts); missing directories are compared by path components
/// (`.` and trailing slashes are normalized; `..` and symlinks are not).
/// Repeated records for the same exact logical name do not constitute a
/// collision.
fn shared_dirs(entries: &[(String, PathBuf)]) -> HashSet<String> {
    let mut owners: std::collections::HashMap<DirKey, HashSet<&str>> =
        std::collections::HashMap::new();
    for (name, dir) in entries {
        owners.entry(dir_key(dir)).or_default().insert(name);
    }
    owners
        .into_values()
        .filter(|names| names.len() > 1)
        .flat_map(|names| names.into_iter().map(str::to_owned))
        .collect()
}

/// The agent's effective on-host config dir when thegn does **not** manage
/// an account for it: the inherited `home_env` env var (tilde-expanded) if set,
/// else `~/<default_dir>`. Returns `Some` only when the dir already exists — we
/// carve an existing dir writable in the sandbox, we don't fabricate one from a
/// stray env var. Used to overmount the agent's config dir read-write under a
/// read-only `$HOME` so it can write its runtime state (`session-env`, `todos`,
/// shell snapshots) — parity with the managed-account path.
pub fn effective_config_dir(p: &Provider) -> Option<String> {
    let env_val = std::env::var(p.home_env).ok(); // best-effort: optional input: the variable may legitimately be unset
    let home = std::env::var("HOME").ok(); // best-effort: optional input: the variable may legitimately be unset
    resolve_config_dir(env_val.as_deref(), home.as_deref(), p.default_dir)
}

/// Pure core of [`effective_config_dir`]: pick the config dir from the resolved
/// env var (tilde-expanded) or `<home>/<default_dir>`, returning it only when it
/// exists on disk. Split out so the env reads stay at the edge and the choice is
/// unit-testable without mutating process-global env.
fn resolve_config_dir(
    env_val: Option<&str>,
    home: Option<&str>,
    default_dir: &str,
) -> Option<String> {
    let dir = match env_val {
        Some(v) if !v.is_empty() => util::expand_tilde(v),
        _ => format!("{}/{default_dir}", home.filter(|h| !h.is_empty())?),
    };
    std::path::Path::new(&dir).is_dir().then_some(dir)
}

/// The full merged account list for a provider (config + DB-managed), for the
/// picker. Config entries take precedence on name collisions.
pub fn list(cfg: &Config, db: &Db, provider_id: &str) -> Vec<AccountInfo> {
    let entries = resolved_entries(cfg, db, provider_id);
    let shared = shared_dirs(
        &entries
            .iter()
            .map(|(n, d, _)| (n.clone(), d.clone()))
            .collect::<Vec<_>>(),
    );
    if !shared.is_empty() {
        let mut names: Vec<_> = shared.iter().cloned().collect();
        names.sort();
        crate::msg::warn(&format!(
            "refusing coding-agent accounts with shared credential directories: {}",
            names.join(", ")
        ));
    }
    entries
        .into_iter()
        .filter(|(name, _, _)| !shared.contains(name))
        .map(|(name, dir, managed)| AccountInfo {
            authed: marked_authed(provider_id, &dir),
            managed,
            name,
            provider: provider_id.to_string(),
            dir,
        })
        .collect()
}

// --- active-account pointers (over the ui_state KV store) -------------------

fn scope_global(p: &str) -> String {
    format!("account:{p}")
}
fn scope_ws(p: &str, slug: &str) -> String {
    format!("account:{p}:ws:{slug}")
}
fn scope_wt(p: &str, worktree: &str) -> String {
    format!("account:{p}:wt:{worktree}")
}

/// The active account name for `provider`, by precedence, with every pointer
/// read checked: a failed read is an error, never a fall-through to a broader
/// scope (that would select another account's credentials).
pub fn active_name_checked(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    provider_id: &str,
) -> anyhow::Result<Option<String>> {
    use anyhow::Context as _;

    if let Some(n) = db
        .get_ui_state(&scope_wt(provider_id, worktree), "active")
        .with_context(|| format!("read the {provider_id} account pinned to worktree {worktree}"))?
    {
        return Ok(Some(n));
    }
    if let Some(slug) = slug {
        // THE-515: the trusted overlay is selected by the one refusing
        // resolver. `slug` is still the tab namespace until RepositoryId
        // binding lands. A refused overlay REFUSES: no workspace pointer and
        // no global account stand in for the block the user pinned.
        match cfg.workspace_overlay_for_tab_slug(db, slug) {
            crate::workspace_overlay::WorkspaceOverlay::Refused(_) => return Ok(None),
            overlay => {
                if let Some(n) = overlay.overlay().and_then(|w| w.accounts.get(provider_id)) {
                    return Ok(Some(n.clone()));
                }
            }
        }
        if let Some(n) = db
            .get_ui_state(&scope_ws(provider_id, slug), "active")
            .with_context(|| format!("read the {provider_id} account pinned to workspace {slug}"))?
        {
            return Ok(Some(n));
        }
    }
    db.get_ui_state(&scope_global(provider_id), "active")
        .with_context(|| format!("read the global {provider_id} account"))
}

/// The active account *name* for `provider`, by precedence. `None` ⇒ no
/// selection (the CLI uses its own default home). Lenient: a failed read is
/// `None` (no selection), never a broader scope's account; launch decisions
/// that must not guess use [`active_name_checked`].
pub fn active_name(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    provider_id: &str,
) -> Option<String> {
    // best-effort: display-only lenient read; launch decisions use active_name_checked
    active_name_checked(cfg, db, worktree, slug, provider_id)
        .ok()
        .flatten()
}

/// Where an active selection can be pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    Global,
    Workspace,
    Worktree,
}

/// Set the active account for `provider` at the given binding scope.
pub fn set_active(
    db: &Db,
    bind: Bind,
    worktree: &str,
    slug: Option<&str>,
    provider_id: &str,
    name: &str,
) -> anyhow::Result<()> {
    let scope = match bind {
        Bind::Global => scope_global(provider_id),
        Bind::Workspace => scope_ws(provider_id, slug.unwrap_or_default()),
        Bind::Worktree => scope_wt(provider_id, worktree),
    };
    db.set_ui_state(&scope, "active", name)?;
    Ok(())
}

/// Resolve the credential-home dir to inject for the active account, if any.
pub fn resolve_dir(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    provider_id: &str,
) -> Option<PathBuf> {
    let name = active_name(cfg, db, worktree, slug, provider_id)?;
    account_dir(cfg, db, provider_id, &name)
}

/// The `(env_var, dir)` to inject when launching `choice` in `worktree`, or
/// `None` when the agent has no provider or no active account. Records
/// `last_used` on the resolved account.
pub fn launch_env(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    choice: &str,
) -> Option<(String, PathBuf)> {
    // best-effort: legacy display callers retain the lenient behavior; launches use launch_env_checked.
    launch_env_checked(cfg, db, worktree, slug, choice)
        .ok()
        .flatten()
}

/// Resolve the active account home for a launch, preserving failures while
/// reading the scoped account pointer so a caller cannot fall through to a
/// different account's credentials.
pub fn launch_env_checked(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    choice: &str,
) -> anyhow::Result<Option<(String, PathBuf)>> {
    let Some(p) = provider_for(cfg, choice) else {
        return Ok(None);
    };
    let Some(name) = active_name_checked(cfg, db, worktree, slug, p.id)? else {
        return Ok(None);
    };
    let Some(dir) = account_dir(cfg, db, p.id, &name) else {
        return Ok(None);
    };
    let _ = db.touch_account(p.id, &name, util::now()); // best-effort: cache write: last-used touch; git/forge stays the source of truth
    Ok(Some((p.home_env.to_string(), dir)))
}

/// A short chip label for the resolved account (`"codex:work"`), or `None`.
pub fn chip_label(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    choice: &str,
) -> Option<String> {
    let p = provider_for(cfg, choice)?;
    let name = active_name(cfg, db, worktree, slug, p.id)?;
    Some(format!("{}:{}", p.id, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, NamedCommand};

    fn agent(name: &str, command: &str, provider: Option<&str>) -> NamedCommand {
        NamedCommand {
            name: name.into(),
            command: command.into(),
            hints: vec![],
            provider: provider.map(|s| s.into()),
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            drawer_scope: None,
            drawer_cwd: None,
        }
    }

    #[test]
    fn infer_provider_from_command_basename() {
        assert_eq!(infer_provider("codex --foo").map(|p| p.id), Some("codex"));
        assert_eq!(
            infer_provider("/usr/bin/claude").map(|p| p.id),
            Some("claude")
        );
        assert_eq!(infer_provider("vim ."), None);
        assert_eq!(infer_provider(""), None);
    }

    #[test]
    fn provider_for_prefers_explicit_then_infers() {
        let mut cfg = Config::default();
        // Explicit provider field overrides the command basename.
        cfg.agents.push(agent("cc", "my-wrapper", Some("claude")));
        // No explicit field → infer from the command.
        cfg.agents.push(agent("codex", "codex resume", None));
        cfg.agents.push(agent("plain", "bash", None));
        assert_eq!(provider_for(&cfg, "cc").map(|p| p.id), Some("claude"));
        assert_eq!(provider_for(&cfg, "codex").map(|p| p.id), Some("codex"));
        assert_eq!(provider_for(&cfg, "plain"), None);
        assert_eq!(provider_for(&cfg, "missing"), None);
    }

    #[test]
    fn account_dir_prefers_config_then_db() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "adopted".into(),
            provider: "codex".into(),
            dir: Some("~/x/.codex-work".into()),
        });
        cfg.accounts.push(Account {
            name: "managed-cfg".into(),
            provider: "codex".into(),
            dir: None,
        });
        // Config: explicit dir is tilde-expanded.
        assert_eq!(
            account_dir(&cfg, &db, "codex", "adopted"),
            Some(PathBuf::from(util::expand_tilde("~/x/.codex-work")))
        );
        // Config without dir → the managed path.
        assert_eq!(
            account_dir(&cfg, &db, "codex", "managed-cfg"),
            Some(managed_dir("codex", "managed-cfg"))
        );
        // Unknown → None, until registered in the DB.
        assert_eq!(account_dir(&cfg, &db, "codex", "db-only"), None);
        db.put_account("codex", "db-only", "/var/creds", true, 1)
            .unwrap();
        assert_eq!(account_dir(&cfg, &db, "codex", "db-only"), None);
    }

    #[test]
    fn legacy_home_with_a_slug_alias_is_not_claimed() {
        let state = crate::testenv::IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let private =
            crate::fsperm::ensure_private_dir_beneath(&anchor, Path::new("accounts/codex/work"))
                .unwrap();
        let home = private.path().to_path_buf();
        std::fs::write(home.join("auth.json"), b"credential bytes").unwrap();

        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.extend([
            Account {
                name: "work".into(),
                provider: "codex".into(),
                dir: None,
            },
            Account {
                name: "Work".into(),
                provider: "codex".into(),
                dir: None,
            },
        ]);

        assert_eq!(account_dir(&cfg, &db, "codex", "work"), None);
        assert!(!home.join(OWNER_MANIFEST).exists());
        assert!(matches!(
            ensure_managed_home_claiming("codex", "work", &["Work".to_string()]),
            Err(AccountHomeError::AmbiguousLegacyHome { aliases, .. })
                if aliases == vec!["Work".to_string()]
        ));
    }

    #[test]
    fn legacy_home_alias_list_is_exact() {
        let entries = [
            ("work".to_string(), PathBuf::new(), true),
            ("Work".to_string(), PathBuf::new(), true),
            ("WORK".to_string(), PathBuf::new(), false),
            ("work/x".to_string(), PathBuf::new(), true),
        ];

        assert_eq!(legacy_aliases(&entries, "work"), vec!["Work".to_string()]);
    }

    #[test]
    fn legacy_home_without_aliases_is_still_claimed() {
        let state = crate::testenv::IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let private =
            crate::fsperm::ensure_private_dir_beneath(&anchor, Path::new("accounts/codex/work"))
                .unwrap();
        let home = private.path().to_path_buf();

        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "work".into(),
            provider: "codex".into(),
            dir: None,
        });

        assert_eq!(account_dir(&cfg, &db, "codex", "work"), Some(home.clone()));
        assert_eq!(
            std::fs::read(home.join(OWNER_MANIFEST)).unwrap(),
            owner_manifest("codex", "work")
        );
    }

    #[test]
    fn legacy_home_alias_with_an_empty_new_home_is_claimed() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.extend([
            Account {
                name: "work".into(),
                provider: "codex".into(),
                dir: None,
            },
            Account {
                name: "Work".into(),
                provider: "codex".into(),
                dir: None,
            },
        ]);

        assert_eq!(
            account_dir(&cfg, &db, "codex", "work"),
            Some(managed_dir("codex", "work"))
        );
        assert_eq!(
            std::fs::read(managed_dir("codex", "work").join(OWNER_MANIFEST)).unwrap(),
            owner_manifest("codex", "work")
        );
    }

    #[test]
    fn legacy_home_with_a_manifest_and_an_alias_verifies() {
        let _state = crate::testenv::IsolatedState::new();
        let home = ensure_managed_home("codex", "work").unwrap();
        std::fs::write(home.join("auth.json"), b"credential bytes").unwrap();

        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.extend([
            Account {
                name: "work".into(),
                provider: "codex".into(),
                dir: None,
            },
            Account {
                name: "Work".into(),
                provider: "codex".into(),
                dir: None,
            },
        ]);

        assert_eq!(account_dir(&cfg, &db, "codex", "work"), Some(home));
    }

    #[test]
    fn home_components_are_distinct_for_separator_and_case_aliases() {
        let components: HashSet<_> = ["work/a", "work-a", "work_a", "Work-A"]
            .into_iter()
            .map(home_component)
            .collect();
        assert_eq!(components.len(), 4);
        let punctuation = home_component("!!!");
        let empty = home_component("");
        assert!(!punctuation.is_empty());
        assert!(!empty.is_empty());
        assert_ne!(punctuation, empty);
        assert_eq!(home_component("acct1"), "acct1");
    }

    #[test]
    fn colliding_db_rows_are_refused() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        db.put_account("codex", "first", "/shared/creds", true, 1)
            .unwrap();
        db.put_account("codex", "second", "/shared/creds", true, 2)
            .unwrap();

        assert_eq!(account_dir(&cfg, &db, "codex", "first"), None);
        assert_eq!(account_dir(&cfg, &db, "codex", "second"), None);
        let listed = list(&cfg, &db, "codex");
        assert!(
            !listed
                .iter()
                .any(|account| { account.name == "first" || account.name == "second" })
        );
    }

    #[test]
    fn colliding_config_and_db_accounts_are_refused() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "cfg".into(),
            provider: "codex".into(),
            dir: Some("/shared/creds".into()),
        });
        db.put_account("codex", "dbrow", "/shared/creds", true, 1)
            .unwrap();

        assert_eq!(account_dir(&cfg, &db, "codex", "cfg"), None);
        assert_eq!(account_dir(&cfg, &db, "codex", "dbrow"), None);
        let listed = list(&cfg, &db, "codex");
        assert!(
            !listed
                .iter()
                .any(|account| account.name == "cfg" || account.name == "dbrow")
        );
    }

    #[test]
    fn same_name_config_and_db_records_are_not_a_collision() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "same".into(),
            provider: "codex".into(),
            dir: Some("/shared/creds".into()),
        });
        db.put_account("codex", "same", "/shared/creds", true, 1)
            .unwrap();

        assert_eq!(
            account_dir(&cfg, &db, "codex", "same"),
            Some(PathBuf::from("/shared/creds"))
        );
        let listed = list(&cfg, &db, "codex");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "same");
    }

    #[test]
    fn symlinked_spelling_of_one_dir_is_refused() {
        let fixture = crate::testenv::symlinked_tempdir();
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "cfg".into(),
            provider: "codex".into(),
            dir: Some(fixture.real_path().to_string_lossy().into_owned()),
        });
        let link_dir = fixture.path().to_string_lossy().into_owned();
        db.put_account("codex", "dbrow", &link_dir, false, 1)
            .unwrap();

        assert_eq!(account_dir(&cfg, &db, "codex", "cfg"), None);
        assert_eq!(account_dir(&cfg, &db, "codex", "dbrow"), None);
        let listed = list(&cfg, &db, "codex");
        assert!(
            !listed
                .iter()
                .any(|account| account.name == "cfg" || account.name == "dbrow")
        );
    }

    #[test]
    fn dot_dot_spelling_of_one_dir_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let creds = temp.path().join("creds");
        std::fs::create_dir_all(creds.join("sub")).unwrap();
        let dot_dot = creds.join("sub").join("..");
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "cfg".into(),
            provider: "codex".into(),
            dir: Some(creds.to_string_lossy().into_owned()),
        });
        let dot_dot_dir = dot_dot.to_string_lossy().into_owned();
        db.put_account("codex", "dbrow", &dot_dot_dir, false, 1)
            .unwrap();

        assert_eq!(account_dir(&cfg, &db, "codex", "cfg"), None);
        assert_eq!(account_dir(&cfg, &db, "codex", "dbrow"), None);
        let listed = list(&cfg, &db, "codex");
        assert!(
            !listed
                .iter()
                .any(|account| account.name == "cfg" || account.name == "dbrow")
        );
    }

    #[test]
    fn different_existing_account_dirs_are_not_refused() {
        let temp = tempfile::tempdir().unwrap();
        let first_dir = temp.path().join("first");
        let second_dir = temp.path().join("second");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "first".into(),
            provider: "codex".into(),
            dir: Some(first_dir.to_string_lossy().into_owned()),
        });
        cfg.accounts.push(Account {
            name: "second".into(),
            provider: "codex".into(),
            dir: Some(second_dir.to_string_lossy().into_owned()),
        });
        let db = Db::open_memory().unwrap();

        assert_eq!(account_dir(&cfg, &db, "codex", "first"), Some(first_dir));
        assert_eq!(account_dir(&cfg, &db, "codex", "second"), Some(second_dir));
    }

    /// A live main checkout (`.git` directory) at `dir/<name>`.
    fn checkout(dir: &std::path::Path, name: &str) -> String {
        let path = dir.join(name);
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn foo_config() -> Config {
        let mut cfg = Config::default();
        cfg.workspace
            .entry("foo".into())
            .or_default()
            .accounts
            .insert("codex".into(), "work".into());
        cfg
    }

    #[test]
    fn two_live_same_named_checkouts_refuse_the_account_instead_of_falling_back() {
        // THE-515: neither registration wins `[project.foo]`, and the global /
        // pointer accounts must not stand in for the refused block.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let a = checkout(dir.path(), "a/foo");
        let b = checkout(dir.path(), "b/foo");
        assert_eq!(db.slug_for_repo(&a, "foo").unwrap(), "foo");
        assert_eq!(db.slug_for_repo(&b, "foo").unwrap(), "foo-2");
        let cfg = foo_config();
        set_active(&db, Bind::Global, "/wt", Some("foo"), "codex", "g").unwrap();
        set_active(&db, Bind::Workspace, "/wt", Some("foo"), "codex", "wsp").unwrap();
        assert_eq!(active_name(&cfg, &db, "/wt", Some("foo"), "codex"), None);
        assert_eq!(active_name(&cfg, &db, "/wt2", Some("foo-2"), "codex"), None);
        let refusal = cfg.workspace_overlay_for_tab_slug(&db, "foo-2");
        let text = refusal.refusal().expect("refused").to_string();
        assert!(text.contains("Rename or delete"), "{text}");
        // An explicit per-worktree pin is the user's own choice and stays.
        set_active(&db, Bind::Worktree, "/wt", Some("foo"), "codex", "pinned").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("foo"), "codex").as_deref(),
            Some("pinned")
        );
    }

    #[test]
    fn stale_registry_row_does_not_lock_out_a_single_repo() {
        // A scratch clone registered first and since deleted holds `foo`; the
        // real repo got `foo-2`. It must still get its own block.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let gone = dir
            .path()
            .join("scratch/foo")
            .to_string_lossy()
            .into_owned();
        db.slug_for_repo(&gone, "foo").unwrap();
        let real = checkout(dir.path(), "code/foo");
        assert_eq!(db.slug_for_repo(&real, "foo").unwrap(), "foo-2");
        // A plain dir workspace and a linked worktree named `foo` don't count.
        let plain = dir.path().join("plain/foo");
        std::fs::create_dir_all(&plain).unwrap();
        db.slug_for_repo(&plain.to_string_lossy(), "foo").unwrap();
        let linked = dir.path().join("wt/foo");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /x\n").unwrap();
        db.slug_for_repo(&linked.to_string_lossy(), "foo").unwrap();
        let cfg = foo_config();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("foo-2"), "codex").as_deref(),
            Some("work")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_second_spelling_is_the_same_repo() {
        let dir = crate::testenv::symlinked_tempdir();
        let db = Db::open_memory().unwrap();
        let real = checkout(dir.real_path(), "foo");
        let alias = dir.path().join("foo");
        db.slug_for_repo(&alias.to_string_lossy(), "foo").unwrap();
        assert_eq!(db.slug_for_repo(&real, "foo").unwrap(), "foo-2");
        let cfg = foo_config();
        for slug in ["foo", "foo-2"] {
            assert_eq!(
                active_name(&cfg, &db, "/wt", Some(slug), "codex").as_deref(),
                Some("work"),
                "{slug}"
            );
        }
    }

    #[test]
    fn nameless_repository_does_not_get_the_repo_block_via_its_tab_slug() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.slug_for_repo("/src/日本語", "repo").unwrap(), "repo");
        let mut cfg = Config::default();
        cfg.workspace
            .entry("repo".into())
            .or_default()
            .accounts
            .insert("codex".into(), "work".into());
        assert_eq!(active_name(&cfg, &db, "/wt", Some("repo"), "codex"), None);
    }

    #[test]
    fn active_name_precedence() {
        let db = Db::open_memory().unwrap();
        // The tab slug is resolved through the registry, so the scope has to
        // be a registered repository (THE-515) — its directory name is the key.
        db.slug_for_repo("/src/repo", "repo").unwrap();
        let mut cfg = Config::default();
        cfg.workspace.entry("repo".into()).or_default();
        // Nothing set anywhere.
        assert_eq!(active_name(&cfg, &db, "/wt", Some("repo"), "codex"), None);

        // Global is the weakest.
        set_active(&db, Bind::Global, "/wt", Some("repo"), "codex", "g").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("repo"), "codex").as_deref(),
            Some("g")
        );

        // Workspace pointer beats global.
        set_active(&db, Bind::Workspace, "/wt", Some("repo"), "codex", "wsp").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("repo"), "codex").as_deref(),
            Some("wsp")
        );

        // Workspace *config* beats the workspace pointer.
        cfg.workspace
            .get_mut("repo")
            .unwrap()
            .accounts
            .insert("codex".into(), "wscfg".into());
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("repo"), "codex").as_deref(),
            Some("wscfg")
        );

        // Worktree override is the strongest.
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "codex", "wt").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("repo"), "codex").as_deref(),
            Some("wt")
        );
        // A different worktree is unaffected by the override.
        assert_eq!(
            active_name(&cfg, &db, "/other", Some("repo"), "codex").as_deref(),
            Some("wscfg")
        );
    }

    #[test]
    fn launch_env_resolves_provider_and_var() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.agents.push(agent("claude", "claude", None));
        cfg.accounts.push(Account {
            name: "work".into(),
            provider: "claude".into(),
            dir: Some("/creds/claude-work".into()),
        });
        // No active account yet → nothing injected.
        assert!(launch_env(&cfg, &db, "/wt", None, "claude").is_none());

        set_active(&db, Bind::Global, "/wt", None, "claude", "work").unwrap();
        let (var, dir) = launch_env(&cfg, &db, "/wt", None, "claude").unwrap();
        assert_eq!(var, "CLAUDE_CONFIG_DIR");
        assert_eq!(dir, PathBuf::from("/creds/claude-work"));

        // A non-provider agent never injects.
        cfg.agents.push(agent("sh", "bash", None));
        assert!(launch_env(&cfg, &db, "/wt", None, "sh").is_none());
    }

    #[test]
    fn launch_env_checked_propagates_an_unreadable_pointer() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.agents.push(agent("claude", "claude", None));
        cfg.accounts.push(Account {
            name: "work".into(),
            provider: "claude".into(),
            dir: Some("/creds/claude-work".into()),
        });
        db.conn().execute_batch("DROP TABLE ui_state;").unwrap();
        assert!(launch_env_checked(&cfg, &db, "/wt", None, "claude").is_err());

        let db = Db::open_memory().unwrap();
        set_active(&db, Bind::Global, "/wt", None, "claude", "work").unwrap();
        let resolved = launch_env_checked(&cfg, &db, "/wt", None, "claude").unwrap();
        assert_eq!(
            resolved,
            Some((
                "CLAUDE_CONFIG_DIR".into(),
                PathBuf::from("/creds/claude-work")
            ))
        );
    }

    #[test]
    fn marked_authed_checks_marker_and_unknown_provider() {
        let tmp = std::env::temp_dir().join(format!("tg-acct-mark-{}", util::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // No marker yet → not authed.
        assert!(!marked_authed("claude", &tmp));
        // Drop the provider's marker → authed.
        std::fs::write(tmp.join(".credentials.json"), b"{}").unwrap();
        assert!(marked_authed("claude", &tmp));
        // Unknown provider has no marker concept → never authed.
        assert!(!marked_authed("nope", &tmp));
        std::fs::remove_dir_all(&tmp).ok(); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn resolve_config_dir_prefers_env_then_default_and_needs_existence() {
        let base = std::env::temp_dir().join(format!("tg-acct-cfg-{}", util::now()));
        let env_dir = base.join("explicit");
        let home = base.join("home");
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let (env_s, home_s) = (
            env_dir.to_string_lossy().into_owned(),
            home.to_string_lossy().into_owned(),
        );

        // Explicit env var (existing dir) wins over the default.
        assert_eq!(
            resolve_config_dir(Some(&env_s), Some(&home_s), ".claude"),
            Some(env_s.clone())
        );
        // No env var → falls back to <home>/<default_dir> when it exists.
        assert_eq!(
            resolve_config_dir(None, Some(&home_s), ".claude"),
            Some(format!("{home_s}/.claude"))
        );
        // Empty env var is treated as unset → default fallback.
        assert_eq!(
            resolve_config_dir(Some(""), Some(&home_s), ".claude"),
            Some(format!("{home_s}/.claude"))
        );
        // Non-existent dir → None (we carve existing dirs, never fabricate).
        assert_eq!(resolve_config_dir(None, Some(&home_s), ".codex"), None);
        assert_eq!(
            resolve_config_dir(
                Some(&base.join("missing").to_string_lossy()),
                None,
                ".claude"
            ),
            None
        );
        // No env var and no HOME → None.
        assert_eq!(resolve_config_dir(None, None, ".claude"), None);
        std::fs::remove_dir_all(&base).ok(); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn list_merges_config_and_db_with_dedup_and_auth() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();

        // A config-adopted account whose dir contains the auth marker.
        let authed_dir = std::env::temp_dir().join(format!("tg-acct-list-{}", util::now()));
        std::fs::create_dir_all(&authed_dir).unwrap();
        std::fs::write(authed_dir.join("auth.json"), b"{}").unwrap();
        cfg.accounts.push(Account {
            name: "adopted".into(),
            provider: "codex".into(),
            dir: Some(authed_dir.to_string_lossy().into_owned()),
        });
        // A config account with no dir → managed, not authed.
        cfg.accounts.push(Account {
            name: "managed".into(),
            provider: "codex".into(),
            dir: None,
        });
        // An account of a different provider must be filtered out.
        cfg.accounts.push(Account {
            name: "other".into(),
            provider: "claude".into(),
            dir: None,
        });

        // DB rows: one new, one that collides with a config name (skipped).
        db.put_account("codex", "db-only", "/var/db-creds", true, 1)
            .unwrap();
        db.put_account("codex", "adopted", "/should/be/ignored", false, 2)
            .unwrap();

        let out = list(&cfg, &db, "codex");
        let names: Vec<&str> = out.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["adopted", "managed", "db-only"]);

        let adopted = &out[0];
        assert!(!adopted.managed); // has an explicit dir
        assert!(adopted.authed); // marker present
        assert_eq!(adopted.dir, authed_dir);

        let managed = &out[1];
        assert!(managed.managed); // config dir is None
        assert!(!managed.authed);
        assert_eq!(managed.dir, managed_dir("codex", "managed"));

        let db_only = &out[2];
        assert!(db_only.managed); // DB row marked managed=true
        assert!(!db_only.authed);
        assert_eq!(db_only.dir, PathBuf::from("/var/db-creds"));
        assert_eq!(db_only.provider, "codex");

        std::fs::remove_dir_all(&authed_dir).ok(); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn resolve_dir_follows_active_selection() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "work".into(),
            provider: "codex".into(),
            dir: Some("/creds/codex-work".into()),
        });
        // No active selection → nothing to resolve.
        assert_eq!(resolve_dir(&cfg, &db, "/wt", None, "codex"), None);

        set_active(&db, Bind::Global, "/wt", None, "codex", "work").unwrap();
        assert_eq!(
            resolve_dir(&cfg, &db, "/wt", None, "codex"),
            Some(PathBuf::from("/creds/codex-work"))
        );
    }

    #[test]
    fn set_active_workspace_and_worktree_scopes() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.workspace.entry("repo".into()).or_default();

        // Workspace pointer with a missing slug falls back to the empty scope.
        set_active(&db, Bind::Workspace, "/wt", None, "codex", "ws-default").unwrap();
        // Worktree binding is keyed on the worktree path.
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "codex", "wt-pin").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("repo"), "codex").as_deref(),
            Some("wt-pin")
        );
        // The empty-slug workspace pointer is visible for that empty slug.
        assert_eq!(
            active_name(&cfg, &db, "/elsewhere", Some(""), "codex").as_deref(),
            Some("ws-default")
        );
    }

    #[test]
    fn active_name_checked_propagates_an_unreadable_pointer() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        set_active(&db, Bind::Global, "/wt", None, "codex", "g").unwrap();
        db.conn().execute_batch("DROP TABLE ui_state").unwrap();

        assert!(active_name_checked(&cfg, &db, "/wt", None, "codex").is_err());
    }

    #[test]
    fn active_name_checked_names_workspace_and_global_read_failures() {
        let cfg = Config::default();
        let workspace_db = Db::open_memory().unwrap();
        workspace_db
            .conn()
            .execute(
                "INSERT INTO ui_state(scope, key, value) VALUES (?1, 'active', X'FF')",
                [&scope_ws("codex", "repo")],
            )
            .unwrap();
        let workspace_error =
            active_name_checked(&cfg, &workspace_db, "/wt", Some("repo"), "codex").unwrap_err();
        assert!(
            format!("{workspace_error:#}")
                .contains("read the codex account pinned to workspace repo")
        );

        let global_db = Db::open_memory().unwrap();
        global_db
            .conn()
            .execute(
                "INSERT INTO ui_state(scope, key, value) VALUES (?1, 'active', X'FF')",
                [&scope_global("codex")],
            )
            .unwrap();
        let global_error = active_name_checked(&cfg, &global_db, "/wt", None, "codex").unwrap_err();
        assert!(format!("{global_error:#}").contains("read the global codex account"));
    }

    #[test]
    fn active_name_checked_matches_active_name_on_a_readable_db() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        set_active(&db, Bind::Global, "/wt", None, "codex", "g").unwrap();
        set_active(&db, Bind::Worktree, "/wt", None, "codex", "p").unwrap();

        for (worktree, expected) in [
            ("/wt", Some("p")),
            ("/other", Some("g")),
            ("/empty", Some("g")),
        ] {
            let checked = active_name_checked(&cfg, &db, worktree, None, "codex").unwrap();
            assert_eq!(checked.as_deref(), expected);
            assert_eq!(checked, active_name(&cfg, &db, worktree, None, "codex"));
        }
        let empty_db = Db::open_memory().unwrap();
        let checked = active_name_checked(&cfg, &empty_db, "/wt", None, "codex").unwrap();
        assert_eq!(checked, None);
        assert_eq!(checked, active_name(&cfg, &empty_db, "/wt", None, "codex"));
    }

    #[test]
    fn active_name_is_none_when_the_pointer_store_is_unreadable() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        set_active(&db, Bind::Global, "/wt", None, "codex", "g").unwrap();
        db.conn().execute_batch("DROP TABLE ui_state").unwrap();

        assert_eq!(active_name(&cfg, &db, "/wt", None, "codex"), None);
    }

    #[test]
    fn chip_label_formats_provider_and_name() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.agents.push(agent("codex", "codex", None));
        // No active account → no chip.
        assert_eq!(chip_label(&cfg, &db, "/wt", None, "codex"), None);

        set_active(&db, Bind::Global, "/wt", None, "codex", "work").unwrap();
        assert_eq!(
            chip_label(&cfg, &db, "/wt", None, "codex").as_deref(),
            Some("codex:work")
        );

        // A non-provider agent yields no chip.
        cfg.agents.push(agent("sh", "bash", None));
        assert_eq!(chip_label(&cfg, &db, "/wt", None, "sh"), None);
    }

    #[test]
    fn providers_projection_matches_the_pre_seam_table() {
        // Behaviour-identity: the `PROVIDERS` const this replaced held exactly
        // these two rows, in this order. The facts now come from the harness
        // seam; this pins that the projection did not change them.
        let got: Vec<(&str, &str, &str, &str, &[&str])> = providers()
            .iter()
            .map(|p| (p.id, p.home_env, p.default_dir, p.auth_marker, p.login_argv))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "codex",
                    "CODEX_HOME",
                    ".codex",
                    "auth.json",
                    &["codex", "login"][..]
                ),
                (
                    "claude",
                    "CLAUDE_CONFIG_DIR",
                    ".claude",
                    ".credentials.json",
                    &["claude"][..]
                ),
            ]
        );
        // Non-relocatable harnesses (aider/antigravity) are not account
        // providers — the projection filtered them, as the old table did.
        assert!(provider("aider").is_none());
        assert!(provider("antigravity").is_none());
    }

    #[test]
    fn managed_dir_is_under_state_home() {
        let d = managed_dir("codex", "My Work!");
        assert!(
            d.starts_with(
                util::xdg_state_home()
                    .join("thegn")
                    .join("accounts")
                    .join("codex")
            )
        );
        // Non-canonical names keep a readable slug prefix and exact-name hash.
        assert!(
            d.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("my-work_")
        );
    }

    #[test]
    fn managed_home_claims_then_verifies() {
        let _state = crate::testenv::IsolatedState::new();
        let home = ensure_managed_home("claude", "primary").unwrap();
        let manifest = std::fs::read(home.join(OWNER_MANIFEST)).unwrap();
        assert_eq!(manifest, owner_manifest("claude", "primary"));
        assert_eq!(ensure_managed_home("claude", "primary").unwrap(), home);
        assert_ne!(
            managed_dir("claude", "primary"),
            managed_dir("claude", "different")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(home.join(OWNER_MANIFEST))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn managed_home_refuses_a_foreign_manifest() {
        let state = crate::testenv::IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let private = crate::fsperm::ensure_private_dir_beneath(
            &anchor,
            Path::new("accounts/claude/primary"),
        )
        .unwrap();
        let home = private.path().to_path_buf();
        std::fs::write(home.join(OWNER_MANIFEST), owner_manifest("claude", "other")).unwrap();
        assert!(matches!(
            ensure_managed_home("claude", "primary"),
            Err(AccountHomeError::ForeignManifest { provider, name })
                if provider == "claude" && name == "other"
        ));
    }

    #[test]
    fn managed_home_refuses_a_malformed_manifest() {
        let state = crate::testenv::IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let private =
            crate::fsperm::ensure_private_dir_beneath(&anchor, Path::new("accounts/codex/primary"))
                .unwrap();
        let home = private.path();
        std::fs::write(home.join(OWNER_MANIFEST), b"not an owner record\n").unwrap();
        assert!(matches!(
            ensure_managed_home("codex", "primary"),
            Err(AccountHomeError::MalformedManifest { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn managed_home_refuses_a_symlinked_manifest() {
        use std::os::unix::fs::symlink;

        let state = crate::testenv::IsolatedState::new();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let private =
            crate::fsperm::ensure_private_dir_beneath(&anchor, Path::new("accounts/codex/primary"))
                .unwrap();
        let home = private.path();
        let outside = state.root().join("outside-manifest");
        std::fs::write(&outside, owner_manifest("codex", "primary")).unwrap();
        symlink(&outside, home.join(OWNER_MANIFEST)).unwrap();
        assert!(matches!(
            ensure_managed_home("codex", "primary"),
            Err(AccountHomeError::UnsafePath { .. })
        ));
        assert_eq!(
            std::fs::read(outside).unwrap(),
            owner_manifest("codex", "primary")
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_home_refuses_a_symlinked_provider_dir() {
        use std::os::unix::fs::symlink;

        let state = crate::testenv::IsolatedState::new();
        let account_root = state.state_home().join("thegn/accounts");
        std::fs::create_dir_all(state.state_home().join("thegn")).unwrap();
        crate::fsperm::ensure_private_dir_beneath(
            &state.state_home().join("thegn"),
            Path::new("accounts"),
        )
        .unwrap();
        let outside = state.root().join("outside-provider");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, account_root.join("claude")).unwrap();
        assert!(matches!(
            ensure_managed_home("claude", "primary"),
            Err(AccountHomeError::UnsafePath { .. })
        ));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn managed_home_refuses_creation_failure() {
        let state = crate::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn")).unwrap();
        std::fs::write(state.state_home().join("thegn/accounts"), b"blocker").unwrap();
        assert!(matches!(
            ensure_managed_home("codex", "primary"),
            Err(AccountHomeError::UnsafePath { .. })
        ));
    }

    #[test]
    fn account_dir_refuses_a_managed_home_that_fails_verification() {
        let state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let anchor = state.state_home().join("thegn");
        std::fs::create_dir_all(&anchor).unwrap();
        let private =
            crate::fsperm::ensure_private_dir_beneath(&anchor, Path::new("accounts/codex/primary"))
                .unwrap();
        let home = private.path().to_path_buf();
        std::fs::write(
            home.join(OWNER_MANIFEST),
            owner_manifest("codex", "someone-else"),
        )
        .unwrap();
        db.put_account("codex", "primary", &home.to_string_lossy(), true, 1)
            .unwrap();
        assert_eq!(
            account_dir(&Config::default(), &db, "codex", "primary"),
            None
        );
    }

    #[test]
    fn unknown_provider_is_refused_before_any_effect() {
        let state = crate::testenv::IsolatedState::new();
        assert!(matches!(
            ensure_managed_home("../../outside", "primary"),
            Err(AccountHomeError::UnknownProvider { provider }) if provider == "../../outside"
        ));
        assert!(!state.state_home().join("thegn").exists());
    }

    #[test]
    fn managed_row_pointing_elsewhere_is_refused() {
        let state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let stored = state.root().join("edited-managed-home");
        std::fs::create_dir(&stored).unwrap();
        crate::fsperm::restrict_dir_to_owner(&stored).unwrap();
        db.put_account("codex", "primary", &stored.to_string_lossy(), true, 1)
            .unwrap();

        assert_eq!(
            account_dir(&Config::default(), &db, "codex", "primary"),
            None
        );
        assert!(stored.is_dir());
        assert!(!managed_dir("codex", "primary").exists());
    }

    #[test]
    fn managed_row_with_a_legacy_slug_path_is_refused() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let stored = managed_root().join("codex").join("work");
        std::fs::create_dir_all(&stored).unwrap();
        crate::fsperm::restrict_dir_to_owner(&stored).unwrap();
        db.put_account("codex", "Work", &stored.to_string_lossy(), true, 1)
            .unwrap();

        assert_eq!(account_dir(&Config::default(), &db, "codex", "Work"), None);
        assert!(stored.is_dir());
        assert!(!managed_dir("codex", "Work").exists());
    }

    #[test]
    fn managed_row_check_reports_both_paths() {
        let _state = crate::testenv::IsolatedState::new();
        let stored = Path::new("/elsewhere");

        assert!(matches!(
            managed_home_check("codex", "primary", stored),
            Err(AccountHomeError::RelocatedManagedHome { stored: actual, expected })
                if actual == stored && expected == managed_dir("codex", "primary")
        ));
    }

    #[test]
    fn managed_row_adopted_dir_still_resolves() {
        let state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let adopted = state.root().join("adopted-home");
        std::fs::create_dir(&adopted).unwrap();
        db.put_account("codex", "primary", &adopted.to_string_lossy(), false, 1)
            .unwrap();

        assert_eq!(
            account_dir(&Config::default(), &db, "codex", "primary"),
            Some(adopted)
        );
    }

    #[test]
    fn create_managed_home_refuses_an_existing_home() {
        let _state = crate::testenv::IsolatedState::new();
        let existing = ensure_managed_home("claude", "acct1").unwrap();

        assert!(matches!(
            create_managed_home("claude", "acct1"),
            Err(AccountHomeError::HomeExists { path }) if path == managed_dir("claude", "acct1")
        ));
        assert_eq!(existing, managed_dir("claude", "acct1"));
    }

    #[test]
    fn create_managed_home_refuses_a_leftover_manifestless_dir() {
        let state = crate::testenv::IsolatedState::new();
        let home = managed_dir("codex", "acct3");
        std::fs::create_dir_all(&home).unwrap();
        let auth = home.join("auth.json");
        std::fs::write(&auth, b"forgotten login").unwrap();

        assert!(matches!(
            create_managed_home("codex", "acct3"),
            Err(AccountHomeError::HomeExists { path }) if path == home
        ));
        assert_eq!(std::fs::read(auth).unwrap(), b"forgotten login");

        let accounts = state.state_home().join("thegn/accounts");
        std::fs::create_dir_all(&accounts).unwrap();
        let blocked_provider = accounts.join("claude");
        std::fs::write(&blocked_provider, b"not a directory").unwrap();
        assert!(matches!(
            create_managed_home("claude", "acct1"),
            Err(AccountHomeError::UnsafePath { path, .. }) if path == managed_dir("claude", "acct1")
        ));
    }

    #[test]
    fn create_managed_home_creates_a_fresh_owned_home() {
        let _state = crate::testenv::IsolatedState::new();

        let home = create_managed_home("codex", "acct2").unwrap();

        assert_eq!(home, managed_dir("codex", "acct2"));
        assert_eq!(
            std::fs::read(home.join(OWNER_MANIFEST)).unwrap(),
            owner_manifest("codex", "acct2")
        );
        assert!(matches!(
            create_managed_home("unknown", "acct1"),
            Err(AccountHomeError::UnknownProvider { provider }) if provider == "unknown"
        ));
    }
}
