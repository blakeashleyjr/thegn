//! Comment/format-preserving config authoring — the *write* half of the config
//! system (the read/resolve half is `config.rs` + `config_resolve.rs`).
//!
//! `thegn env create`/`edit`, `config set`, and the TUI setup wizard all funnel
//! through here so they edit the user's `config.toml` **in place** — via
//! `toml_edit` — without clobbering comments or reformatting the file. Two rules
//! mirror the trust model in `config_resolve.rs`:
//!
//! - `[env.<name>]` tables are authored in the **global** config only (a repo
//!   `.thegn.toml` may *select* an env with a top-level `env = "…"` but never
//!   *define* one — see [`select_env_in_repo`]).
//! - Tokens are never written here: [`EnvSpec::api_key_env`] holds a *SecretRef*
//!   (`keyring:` / `file:` / `env:`), produced by the host's secret backend.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, value};

fn shared_hard_link(metadata: &std::fs::Metadata) -> bool {
    link_count(metadata) > 1
}

#[cfg(unix)]
fn link_count(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(unix))]
fn link_count(_metadata: &std::fs::Metadata) -> u64 {
    // std does not expose a stable Windows link count. Reparse/link handling is
    // deferred until a platform-specific metadata seam is available.
    1
}

fn reject_shared_hard_link(metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        !shared_hard_link(metadata),
        "config file has {} hard links; refusing to replace it — edit it directly or remove the extra link",
        link_count(metadata)
    );
    Ok(())
}

/// A typed environment to author into the global config. Optional fields are
/// only written when `Some`, so `edit` upserts without dropping untouched keys.
#[derive(Debug, Clone, Default)]
pub struct EnvSpec {
    pub name: String,
    /// `"local"` | `"ssh"` | `"provider"`.
    pub placement: String,
    /// e.g. `"in_env"` — where files live (optional).
    pub data: Option<String>,
    // provider placement
    pub provider: Option<String>,
    pub api_key_env: Option<String>,
    pub region: Option<String>,
    pub size: Option<String>,
    pub template: Option<String>,
    pub max_instances: Option<i64>,
    pub max_lifetime_secs: Option<i64>,
    pub auto_provision: Option<bool>,
    // ssh placement
    pub ssh_host: Option<String>,
    // local placement
    pub sandbox_backend: Option<String>,
}

fn read_doc(path: &Path) -> Result<(DocumentMut, Option<Vec<u8>>)> {
    // Only a genuinely-absent file is an empty document. Any OTHER read error
    // (EACCES, EIO, a symlink loop, a file that exists but can't be read) must
    // NOT be swallowed as "empty": the caller mutates and writes the doc back, so
    // treating an unreadable-but-present config as empty would clobber the user's
    // entire config.toml with just the one key being set.
    let bytes = match crate::fsperm::open_regular_file_nofollow(path) {
        Ok(mut file) => {
            let metadata = file.metadata().context("inspect config file")?;
            anyhow::ensure!(
                crate::fsperm::owned_by_current_user(&metadata),
                "config file is not owned by the current user"
            );
            reject_shared_hard_link(&metadata)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .with_context(|| format!("read {}", path.display()))?;
            Some(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(config_path_error(path, error)),
    };
    let text = bytes
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()
        .context("config is not UTF-8")?
        .unwrap_or("");
    text.parse::<DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))
        .map(|doc| (doc, bytes))
}

fn write_doc(path: &Path, doc: &DocumentMut, expected: Option<&[u8]>) -> Result<()> {
    write_document_fenced(path, doc, expected)
}

/// Publish an edited config document only if its source generation is unchanged.
/// `expected` is `None` only when the destination was absent at read time. Writers
/// using this funnel are serialized by a sidecar lock. A writer that edits the
/// file directly can still race the final generation check and replacement.
/// If replacement succeeds but syncing the containing directory fails, this
/// returns success with a warning: the new bytes are visible, but may not
/// survive a crash.
pub fn write_document_fenced(
    path: &Path,
    doc: &DocumentMut,
    expected: Option<&[u8]>,
) -> Result<()> {
    write_document_fenced_checked(path, doc, expected, |_, _| Ok(()))
}

fn write_document_fenced_checked(
    path: &Path,
    doc: &DocumentMut,
    expected: Option<&[u8]>,
    check: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<()> {
    let prior = expected
        .map(std::str::from_utf8)
        .transpose()
        .context("config is not UTF-8")?
        .unwrap_or("");
    let next = doc.to_string();
    crate::config_duration::introduced(
        &crate::config_duration::errors_for_str(prior),
        crate::config_duration::errors_for_str(&next),
    )
    .map_err(anyhow::Error::msg)?;
    crate::custom_cmd::validate_write(prior, &next).map_err(anyhow::Error::msg)?;
    write_doc_fenced(path, doc, expected, check)
}

fn config_lock_path(path: &Path) -> Result<std::path::PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().context("config path has no file name")?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(name);
    lock_name.push(".lock");
    Ok(parent.join(lock_name))
}

fn lock_config_writer(path: &Path) -> Result<std::fs::File> {
    let lock_path = config_lock_path(path)?;
    let lock_file = match crate::fsperm::create_owner_only_exclusive(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = crate::fsperm::open_regular_file_nofollow(&lock_path)
                .with_context(|| format!("open config writer lock {}", lock_path.display()))?;
            let metadata = file.metadata().context("inspect config writer lock")?;
            anyhow::ensure!(
                crate::fsperm::owned_by_current_user(&metadata),
                "config writer lock is not owned by the current user"
            );
            file
        }
        Err(error) => return Err(error).context("create private config writer lock"),
    };
    match lock_file.try_lock() {
        Ok(()) => Ok(lock_file),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("config is being written by another thegn writer; retry")
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).context("lock config writer sidecar")
        }
    }
}

/// Create an exact-byte config seed under the same writer lock as edits.
/// Existing destinations are left untouched; only a missing parent is created
/// private, matching the fenced writer's parent policy.
pub fn seed_if_absent(path: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match std::fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::fsperm::ensure_private_dir(parent)
                .with_context(|| format!("create private config directory {}", parent.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", parent.display())),
    }

    let _writer_lock = lock_config_writer(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => return Ok(false),
        Ok(_) => anyhow::bail!("config path exists but is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    crate::fsperm::write_owner_only_new(path, bytes)
        .with_context(|| format!("create private config seed {}", path.display()))?;
    if let Err(error) = crate::fsperm::sync_parent_dir(parent) {
        crate::msg::warn(&format!(
            "config seed was written, but its directory could not be synced; the change may not survive a crash: {error} ({})",
            parent.display()
        ));
    }
    Ok(true)
}

fn write_doc_fenced(
    path: &Path,
    doc: &DocumentMut,
    expected: Option<&[u8]>,
    check: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match std::fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::fsperm::ensure_private_dir(parent)
                .with_context(|| format!("create private config directory {}", parent.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", parent.display())),
    }
    let _writer_lock = lock_config_writer(path)?;
    let current = match crate::fsperm::open_regular_file_nofollow(path) {
        Ok(mut file) => {
            let metadata = file.metadata().context("inspect config destination")?;
            anyhow::ensure!(
                crate::fsperm::owned_by_current_user(&metadata),
                "config destination is not owned by the current user"
            );
            reject_shared_hard_link(&metadata)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .context("read config generation")?;
            Some(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(config_path_error(path, error)),
    };
    anyhow::ensure!(
        current.as_deref() == expected,
        "config changed during edit; retry the operation"
    );

    let prior = expected
        .map(std::str::from_utf8)
        .transpose()
        .context("config is not UTF-8")?
        .unwrap_or("");
    let next = doc.to_string();
    check(prior, &next)?;

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = parent.join(format!(
        ".config-write-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = crate::fsperm::create_owner_only_exclusive(&tmp)
        .context("create private config temporary")?;
    let mut pending = ConfigTemp(Some(tmp.clone()));
    file.write_all(next.as_bytes())
        .context("write config temporary")?;
    file.sync_all().context("sync config temporary")?;
    drop(file);
    // Recheck directly before replacement; a conflict preserves the old file.
    let (current, existing_metadata) = match crate::fsperm::open_regular_file_nofollow(path) {
        Ok(mut file) => {
            let metadata = file
                .metadata()
                .context("inspect config during generation check")?;
            anyhow::ensure!(
                crate::fsperm::owned_by_current_user(&metadata),
                "config destination is not owned by the current user"
            );
            reject_shared_hard_link(&metadata)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .context("recheck config generation")?;
            (Some(bytes), Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(error) => return Err(config_path_error(path, error)),
    };
    anyhow::ensure!(
        current.as_deref() == expected,
        "config changed during edit; retry the operation"
    );
    if let Some(metadata) = existing_metadata {
        crate::fsperm::preserve_owner_only_permissions(&metadata, &tmp)
            .context("preserve owner-only config permissions")?;
    }
    crate::fsperm::rename_replace(&tmp, path).context("replace config")?;
    pending.0 = None;
    if let Err(error) = crate::fsperm::sync_parent_dir(parent) {
        crate::msg::warn(&format!(
            "config was written, but its directory could not be synced; the change may not survive a crash: {error} ({})",
            parent.display()
        ));
    }
    Ok(())
}

fn config_path_error(path: &Path, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::InvalidInput {
        anyhow::anyhow!(
            "config path {} is a symlink or not a regular file; refusing to replace it - edit the link target directly",
            path.display()
        )
    } else {
        anyhow::Error::from(error).context(format!("access config {}", path.display()))
    }
}

struct ConfigTemp(Option<std::path::PathBuf>);
impl Drop for ConfigTemp {
    fn drop(&mut self) {
        if let Some(path) = &self.0
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %error, "could not remove config temporary");
        }
    }
}

/// Get-or-create a child table named `key` under `parent`, marking it implicit
/// (so an empty table isn't emitted as a bare `[header]`).
fn subtable<'a>(parent: &'a mut Table, key: &str) -> &'a mut Table {
    let entry = parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    if !entry.is_table() {
        *entry = Item::Table(Table::new());
    }
    entry.as_table_mut().expect("just ensured table")
}

/// Create or update `[env.<name>]` (+ its `.provider`/`.ssh`/`.sandbox` subtable)
/// in the global config at `config_path`, preserving all other content + comments.
pub fn upsert_env(config_path: &Path, spec: &EnvSpec) -> Result<()> {
    anyhow::ensure!(!spec.name.trim().is_empty(), "env name is empty");
    let (mut doc, expected) = read_doc(config_path)?;
    let root = doc.as_table_mut();
    let env = subtable(root, "env");
    env.set_implicit(true);
    let et = subtable(env, &spec.name);
    et.insert("placement", value(&spec.placement));
    if let Some(d) = &spec.data {
        et.insert("data", value(d));
    }
    match spec.placement.as_str() {
        "provider" => {
            let p = subtable(et, "provider");
            put_str(p, "provider", &spec.provider);
            put_str(p, "api_key_env", &spec.api_key_env);
            put_str(p, "region", &spec.region);
            put_str(p, "size", &spec.size);
            put_str(p, "template", &spec.template);
            if let Some(n) = spec.max_instances {
                p.insert("max_instances", value(n));
            }
            if let Some(n) = spec.max_lifetime_secs {
                p.insert("max_lifetime_secs", value(n));
            }
            if let Some(b) = spec.auto_provision {
                p.insert("auto_provision", value(b));
            }
        }
        "ssh" => {
            let s = subtable(et, "ssh");
            put_str(s, "host", &spec.ssh_host);
        }
        "local" => {
            if spec.sandbox_backend.is_some() {
                let sb = subtable(et, "sandbox");
                put_str(sb, "backend", &spec.sandbox_backend);
            }
        }
        other => anyhow::bail!("unknown placement {other:?} (expected local|ssh|provider)"),
    }
    write_doc(config_path, &doc, expected.as_deref())
}

fn put_str(tbl: &mut Table, key: &str, v: &Option<String>) {
    if let Some(s) = v.as_ref().filter(|s| !s.trim().is_empty()) {
        tbl.insert(key, value(s.as_str()));
    }
}

/// Remove `[env.<name>]` from the global config. Ok if it wasn't there.
pub fn remove_env(config_path: &Path, name: &str) -> Result<()> {
    let (mut doc, expected) = read_doc(config_path)?;
    if let Some(env) = doc.get_mut("env").and_then(Item::as_table_mut) {
        env.remove(name);
    }
    write_doc(config_path, &doc, expected.as_deref())
}

/// Infer the TOML scalar type of a raw CLI string so `config set foo.enabled
/// true` writes a real bool (not the string `"true"`). Booleans and plain
/// integers/floats are typed; everything else stays a quoted string. This
/// matters because most config fields are typed (`bool`/`u64`/…): writing a
/// quoted string into one makes `toml::from_str::<Config>` hard-error, and
/// `load_layered` then discards the *entire* file and reverts to defaults — so a
/// single mistyped `config set` used to silently wipe the user's whole config.
fn typed_value(val: &str) -> Item {
    match val.trim() {
        "true" => return value(true),
        "false" => return value(false),
        t => {
            // An array or inline table: parse it as TOML so array-typed keys
            // (`regenerate_paths`, `repo_roots`, …) are settable at all. Without
            // this every value was written as a string, so the write was
            // correctly rejected by the post-write validation and there was NO
            // CLI path to set a sequence — it required hand-editing the file.
            // Only attempted for bracket/brace-shaped input, so a plain string
            // that happens to contain a bracket is unaffected.
            let bracketed = (t.starts_with('[') && t.ends_with(']'))
                || (t.starts_with('{') && t.ends_with('}'));
            if bracketed && let Ok(v) = t.parse::<toml_edit::Value>() {
                return Item::Value(v);
            }
            if let Ok(n) = t.parse::<i64>() {
                return value(n);
            }
            // Require a decimal point / exponent before treating as float, so a
            // long integer id isn't reinterpreted (and lossily rounded) as f64.
            if (t.contains('.') || t.contains('e') || t.contains('E'))
                && let Ok(f) = t.parse::<f64>()
                && f.is_finite()
            {
                return value(f);
            }
        }
    }
    value(val)
}

/// Set one dotted key (`a.b.c = value`), creating intermediate tables. The
/// value's type is inferred (see `typed_value`): `true`/`false` and bare
/// integers/floats are written with their native TOML type, everything else as a
/// string. The general `config set` counterpart to `config get`.
pub fn set_key(config_path: &Path, dotted: &str, val: &str) -> Result<()> {
    set_key_checked(config_path, dotted, val, |_, _| Ok(()))
}

/// Set one dotted key and run `check` against the exact prior and proposed
/// config contents while the writer lock is held, before anything is published.
pub fn set_key_checked(
    config_path: &Path,
    dotted: &str,
    val: &str,
    check: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<()> {
    let dotted = crate::config_compat::canonical_key(dotted);
    let parts: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty key");
    let (mut doc, expected) = read_doc(config_path)?;
    let mut tbl = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        tbl = subtable(tbl, seg);
    }
    tbl.insert(parts[parts.len() - 1], typed_value(val));
    write_document_fenced_checked(config_path, &doc, expected.as_deref(), check)
}

/// Set one dotted key to a string **array** (`repo_roots = ["a", "b"]`),
/// creating intermediate tables. The array-valued sibling of [`set_key`].
pub fn set_string_array(config_path: &Path, dotted: &str, items: &[String]) -> Result<()> {
    let dotted = crate::config_compat::canonical_key(dotted);
    let parts: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty key");
    let (mut doc, expected) = read_doc(config_path)?;
    let mut tbl = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        tbl = subtable(tbl, seg);
    }
    let mut arr = toml_edit::Array::new();
    for it in items {
        arr.push(it.as_str());
    }
    tbl.insert(parts[parts.len() - 1], value(arr));
    write_doc(config_path, &doc, expected.as_deref())
}

/// Create or update a `[host.<name>]` ssh host (+ its `.ssh` subtable) in the
/// global config, preserving comments. `ssh` is the `user@box[:port]` form the
/// env/terminal wizards use; untouched sibling keys are kept on re-upsert.
pub fn upsert_host(config_path: &Path, name: &str, ssh: &str) -> Result<()> {
    anyhow::ensure!(!name.trim().is_empty(), "host name is empty");
    anyhow::ensure!(!ssh.trim().is_empty(), "ssh target is empty");
    let (mut doc, expected) = read_doc(config_path)?;
    let root = doc.as_table_mut();
    let hosts = subtable(root, "host");
    hosts.set_implicit(true);
    let ht = subtable(hosts, name.trim());
    ht.insert("reach", value("ssh"));
    let st = subtable(ht, "ssh");
    st.insert("host", value(ssh.trim()));
    write_doc(config_path, &doc, expected.as_deref())
}

/// Insert `key = "<v>"` only when `v` is non-blank (trimmed).
fn put_str_ref(tbl: &mut Table, key: &str, v: &str) {
    if !v.trim().is_empty() {
        tbl.insert(key, value(v.trim()));
    }
}

/// Upsert one entry into an array-of-tables at the dotted `array` path (e.g.
/// `"forges"` → `[[forges]]`, or `"issues.issue_accounts"` →
/// `[[issues.issue_accounts]]`), keyed by its `name` field — updates the
/// matching entry in place or appends a new one, preserving comments and
/// sibling entries. `fill` sets the entry's fields (`name` is written for you).
fn upsert_named_array_entry(
    config_path: &Path,
    array: &str,
    name: &str,
    fill: impl FnOnce(&mut Table),
) -> Result<()> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "{array} entry name is empty");
    let parts: Vec<&str> = array.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty array path");
    let (mut doc, expected) = read_doc(config_path)?;
    let mut parent = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        parent.set_implicit(true);
        parent = subtable(parent, seg);
    }
    let entry = parent
        .entry(parts[parts.len() - 1])
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    if !entry.is_array_of_tables() {
        *entry = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let aot = entry.as_array_of_tables_mut().expect("just ensured aot");
    let idx = aot
        .iter()
        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name));
    let tbl = match idx {
        Some(i) => aot.get_mut(i).expect("index in range"),
        None => {
            aot.push(Table::new());
            aot.iter_mut().last().expect("just pushed")
        }
    };
    tbl.insert("name", value(name));
    fill(tbl);
    write_doc(config_path, &doc, expected.as_deref())
}

/// Remove the entry named `name` from the array-of-tables at the dotted `array`
/// path. Ok if absent.
pub fn remove_array_entry(config_path: &Path, array: &str, name: &str) -> Result<()> {
    let parts: Vec<&str> = array.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty array path");
    let (mut doc, expected) = read_doc(config_path)?;
    let mut node: &mut Table = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        match node.get_mut(seg).and_then(Item::as_table_mut) {
            Some(t) => node = t,
            None => return write_doc(config_path, &doc, expected.as_deref()), // path absent → nothing to remove
        }
    }
    if let Some(aot) = node
        .get_mut(parts[parts.len() - 1])
        .and_then(Item::as_array_of_tables_mut)
    {
        let idx = aot
            .iter()
            .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name.trim()));
        if let Some(i) = idx {
            aot.remove(i);
        }
    }
    write_doc(config_path, &doc, expected.as_deref())
}

/// Create or update a `[[issue_accounts]]` entry (keyed by `name`), preserving
/// comments + sibling entries. `token_ref` must be a SecretRef / `env:` ref
/// (never a raw token — see the module trust model); empty fields are omitted.
#[allow(clippy::too_many_arguments)] // one call site; a struct would cost more than it saves
pub fn upsert_issue_account(
    config_path: &Path,
    name: &str,
    provider: &str,
    token_ref: &str,
    team_id: &str,
    workspace_slug: &str,
    base_url: &str,
    email: &str,
    project_key: &str,
) -> Result<()> {
    upsert_named_array_entry(config_path, "issues.issue_accounts", name, |t| {
        t.insert("provider", value(provider));
        put_str_ref(t, "token", token_ref);
        put_str_ref(t, "team_id", team_id);
        put_str_ref(t, "workspace_slug", workspace_slug);
        put_str_ref(t, "base_url", base_url);
        put_str_ref(t, "email", email);
        put_str_ref(t, "project_key", project_key);
    })
}

/// Rewrite ONLY the `token` of an existing `[[issue_accounts]]` entry (keyed by
/// `name`), preserving every other field + comments. Used by
/// `thegn secret migrate` to replace a pasted plaintext token with the
/// SecretRef the broker returns.
pub fn set_issue_account_token(config_path: &Path, name: &str, token_ref: &str) -> Result<()> {
    upsert_named_array_entry(config_path, "issues.issue_accounts", name, |t| {
        put_str_ref(t, "token", token_ref);
    })
}

/// Return whether `name` is an existing `[[calendar.accounts]]` entry in this
/// config file. Inline arrays and absent entries are not considered writable.
pub fn calendar_account_in_file(config_path: &Path, name: &str) -> Result<bool> {
    let (doc, _) = read_doc(config_path)?;
    Ok(doc
        .get("calendar")
        .and_then(Item::as_table)
        .and_then(|calendar| calendar.get("accounts"))
        .and_then(Item::as_array_of_tables)
        .is_some_and(|accounts| {
            accounts
                .iter()
                .any(|account| account.get("name").and_then(|v| v.as_str()) == Some(name.trim()))
        }))
}

/// Return the token string for `name`'s `[[calendar.accounts]]` entry in this
/// config file. Inline arrays, absent entries, and non-string tokens return
/// `None`.
pub fn calendar_account_token_in_file(config_path: &Path, name: &str) -> Result<Option<String>> {
    let (doc, _) = read_doc(config_path)?;
    Ok(doc
        .get("calendar")
        .and_then(Item::as_table)
        .and_then(|calendar| calendar.get("accounts"))
        .and_then(Item::as_array_of_tables)
        .and_then(|accounts| {
            accounts
                .iter()
                .find(|account| account.get("name").and_then(|v| v.as_str()) == Some(name.trim()))
        })
        .and_then(|account| account.get("token"))
        .and_then(Item::as_str)
        .map(str::to_owned))
}

/// Rewrite only the token of an existing `[[calendar.accounts]]` entry.
/// Unlike the issue-account upsert, this never creates a partial account when
/// the account is defined in another config layer.
pub fn set_calendar_account_token(config_path: &Path, name: &str, token_ref: &str) -> Result<()> {
    let (mut doc, expected) = read_doc(config_path)?;
    let account = doc
        .get_mut("calendar")
        .and_then(Item::as_table_mut)
        .and_then(|calendar| calendar.get_mut("accounts"))
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|accounts| {
            accounts
                .iter_mut()
                .find(|account| account.get("name").and_then(|v| v.as_str()) == Some(name.trim()))
        });
    let Some(account) = account else {
        anyhow::bail!(
            "calendar account {name:?} is not defined in {}; edit the config layer that defines it",
            config_path.display()
        );
    };
    put_str_ref(account, "token", token_ref);
    write_doc(config_path, &doc, expected.as_deref())
}

/// Create or update a `[[forges]]` entry (keyed by `name`). `token_ref` must be
/// a SecretRef / `env:` ref; empty `host`/`token` are omitted.
pub fn upsert_forge(
    config_path: &Path,
    name: &str,
    kind: &str,
    host: &str,
    token_ref: &str,
) -> Result<()> {
    upsert_named_array_entry(config_path, "forges", name, |t| {
        t.insert("kind", value(kind));
        put_str_ref(t, "host", host);
        put_str_ref(t, "token", token_ref);
    })
}

/// In a **repo** `.thegn.toml`, select an env with a top-level `env = "<name>"`
/// (repos select, never define — this refuses to write any `[env.*]` table).
pub fn select_env_in_repo(repo_toml: &Path, name: &str) -> Result<()> {
    let (mut doc, expected) = read_doc(repo_toml)?;
    doc.as_table_mut().insert("env", value(name));
    write_doc(repo_toml, &doc, expected.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_follow_config_errors_explain_symlink_refusal() {
        let error = config_path_error(
            Path::new("config.toml"),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "is a symlink"),
        );
        assert!(error.to_string().contains("edit the link target directly"));
    }

    #[cfg(unix)]
    #[test]
    fn read_doc_does_not_treat_an_unreadable_file_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("unreadable.toml");
        std::fs::write(&p, "[sandbox]\nbackend = \"bwrap\"\n").unwrap();
        // Make the existing file unreadable: a read error that is NOT NotFound.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        // set_key must ERROR rather than clobber the (present) config with an empty
        // doc holding only the new key.
        let r = set_key(&p, "sandbox.enabled", "true");
        // Restore perms so the content check + cleanup work regardless of outcome.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            r.is_err(),
            "unreadable config must not be silently clobbered"
        );
        // The original content is untouched (atomic write never ran).
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains("bwrap"),
            "original config was clobbered: {after:?}"
        );
    }

    #[test]
    fn write_doc_is_atomic_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("atomic.toml");
        set_key(&p, "sandbox.backend", "podman").unwrap();
        assert!(p.exists());
        assert!(!p.with_extension("toml.tmp").exists(), "tmp file leaked");
    }

    #[test]
    fn fenced_write_rejects_changed_generation_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("generation-conflict.toml");
        let original = b"[theme]\npreset = \"prism\"\n";
        std::fs::write(&p, original).unwrap();
        let edited = "[theme]\npreset = \"storm\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        std::fs::write(&p, b"[theme]\npreset = \"user-edit\"\n").unwrap();

        let result = write_document_fenced(&p, &edited, Some(original));
        assert!(result.is_err(), "a stale config generation must conflict");
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"[theme]\npreset = \"user-edit\"\n"
        );
    }

    #[test]
    fn writer_lock_conflict_returns_without_changing_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let original = b"picker = \"list\"\n";
        std::fs::write(&p, original).unwrap();
        let lock_path = config_lock_path(&p).unwrap();
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        held.try_lock().unwrap();

        let result = set_key(&p, "picker", "auto");
        assert!(result.is_err(), "a held writer lock must reject the write");
        assert_eq!(std::fs::read(&p).unwrap(), original);
        drop(held);

        set_key(&p, "picker", "auto").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "picker = \"auto\"\n");
    }

    #[test]
    fn seed_rejects_a_held_writer_lock_without_creating_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let lock_path = config_lock_path(&p).unwrap();
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        held.try_lock().unwrap();

        assert!(seed_if_absent(&p, b"picker = \"list\"\n").is_err());
        assert!(!p.exists(), "a rejected seed must leave config absent");
    }

    #[test]
    fn seed_creates_missing_parent_privately_and_leaves_existing_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("new");
        let path = parent.join("config.toml");
        let bytes = b"# exact seed bytes\npicker = \"list\"\n";

        assert!(seed_if_absent(&path, bytes).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(matches!(
            crate::fsperm::mode_bits(&path).unwrap(),
            None | Some(0o600)
        ));
        assert!(matches!(
            crate::fsperm::mode_bits(&parent).unwrap(),
            None | Some(0o700)
        ));

        std::fs::write(&path, b"existing bytes\n").unwrap();
        assert!(!seed_if_absent(&path, bytes).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing bytes\n");
    }

    #[cfg(unix)]
    #[test]
    fn seed_rejects_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let target = dir.path().join("missing-target.toml");
        symlink(&target, &path).unwrap();

        let error = seed_if_absent(&path, b"picker = \"list\"\n").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("config path exists but is not a regular file")
        );
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn set_key_refuses_hard_linked_config_without_changing_either_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let alias = dir.path().join("alias.toml");
        let original = b"picker = \"list\"\n";
        std::fs::write(&path, original).unwrap();
        std::fs::hard_link(&path, &alias).unwrap();

        let result = set_key(&path, "picker", "auto");

        let error = result.expect_err("a shared inode must not be replaced");
        assert!(error.to_string().contains("2 hard links"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(&alias).unwrap(), original);
    }

    #[test]
    fn first_write_creates_private_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("new");
        let path = parent.join("config.toml");

        set_key(&path, "picker", "auto").unwrap();

        assert!(matches!(
            crate::fsperm::mode_bits(&parent).unwrap(),
            None | Some(0o700)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn first_write_leaves_existing_parent_permissions_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("existing");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        set_key(&parent.join("config.toml"), "picker", "auto").unwrap();

        assert_eq!(crate::fsperm::mode_bits(&parent).unwrap(), Some(0o755));
    }

    #[test]
    fn write_rejects_an_uninspectable_parent_path() {
        let doc = "".parse::<DocumentMut>().unwrap();
        let path = Path::new("invalid\0/config.toml");

        assert!(write_document_fenced(path, &doc, None).is_err());
    }

    #[test]
    fn rejected_checked_write_leaves_bytes_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let original = b"picker = \"list\"\n";
        std::fs::write(&p, original).unwrap();

        let result = set_key_checked(&p, "picker", "auto", |prior, next| {
            assert_eq!(prior, std::str::from_utf8(original).unwrap());
            assert!(next.contains("picker = \"auto\""));
            anyhow::bail!("rejected by validation")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&p).unwrap(), original);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".config-write-")
        }));
    }

    #[test]
    fn upsert_provider_env_round_trips_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("provider.toml");
        std::fs::write(&p, "# my thegn config\n[sandbox]\nbackend = \"bwrap\"\n").unwrap();
        let spec = EnvSpec {
            name: "fly-dev".into(),
            placement: "provider".into(),
            data: Some("in_env".into()),
            provider: Some("fly".into()),
            api_key_env: Some("keyring:fly-dev".into()),
            region: Some("iad".into()),
            size: Some("shared-cpu-2x".into()),
            template: Some("image:ghcr.io/x:v1".into()),
            max_instances: Some(5),
            auto_provision: Some(true),
            ..Default::default()
        };
        upsert_env(&p, &spec).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# my thegn config"), "comment preserved");
        assert!(
            out.contains("backend = \"bwrap\""),
            "existing table preserved"
        );
        // Re-parse and verify the env landed.
        let doc = out.parse::<DocumentMut>().unwrap();
        let prov = &doc["env"]["fly-dev"]["provider"];
        assert_eq!(
            doc["env"]["fly-dev"]["placement"].as_str(),
            Some("provider")
        );
        assert_eq!(prov["provider"].as_str(), Some("fly"));
        assert_eq!(prov["api_key_env"].as_str(), Some("keyring:fly-dev"));
        assert_eq!(prov["region"].as_str(), Some("iad"));
        assert_eq!(prov["max_instances"].as_integer(), Some(5));
        assert_eq!(prov["auto_provision"].as_bool(), Some(true));

        // edit: change size only, other fields stay.
        let edit = EnvSpec {
            name: "fly-dev".into(),
            placement: "provider".into(),
            size: Some("performance-1x".into()),
            ..Default::default()
        };
        upsert_env(&p, &edit).unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["env"]["fly-dev"]["provider"]["size"].as_str(),
            Some("performance-1x")
        );
        assert_eq!(
            doc["env"]["fly-dev"]["provider"]["region"].as_str(),
            Some("iad"),
            "untouched key kept"
        );

        remove_env(&p, "fly-dev").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(
            doc.get("env").and_then(|e| e.get("fly-dev")).is_none(),
            "env removed"
        );
    }

    #[test]
    fn repo_select_writes_only_a_string() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("repo.toml");
        select_env_in_repo(&p, "fly-dev").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert_eq!(out.trim(), "env = \"fly-dev\"");
    }

    #[test]
    fn set_string_array_round_trips_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("array.toml");
        std::fs::write(&p, "# keep me\nworktrees_dir = \"~/wt\"\n").unwrap();
        set_string_array(&p, "repo_roots", &["~/code".into(), "~/oss".into()]).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# keep me"), "comment preserved");
        let doc = out.parse::<DocumentMut>().unwrap();
        let arr = doc["repo_roots"].as_array().unwrap();
        let items: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(items, ["~/code", "~/oss"]);
        // Overwrite replaces the whole array.
        set_string_array(&p, "repo_roots", &["~/work".into()]).unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["repo_roots"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn upsert_host_writes_reach_and_ssh_and_keeps_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("host.toml");
        std::fs::write(
            &p,
            "# cfg\n[host.other]\nreach = \"ssh\"\n[host.other.ssh]\nhost = \"me@old\"\n",
        )
        .unwrap();
        upsert_host(&p, "build-box", "me@build.example.com:2222").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# cfg"), "comment preserved");
        let doc = out.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["host"]["build-box"]["reach"].as_str(), Some("ssh"));
        assert_eq!(
            doc["host"]["build-box"]["ssh"]["host"].as_str(),
            Some("me@build.example.com:2222")
        );
        assert_eq!(
            doc["host"]["other"]["ssh"]["host"].as_str(),
            Some("me@old"),
            "sibling host untouched"
        );
        // Re-upsert updates in place (no duplicate table).
        upsert_host(&p, "build-box", "me@new").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["host"]["build-box"]["ssh"]["host"].as_str(),
            Some("me@new")
        );
    }

    #[test]
    fn upsert_host_rejects_empty_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("host-empty.toml");
        assert!(upsert_host(&p, " ", "me@box").is_err());
        assert!(upsert_host(&p, "box", "  ").is_err());
    }

    #[test]
    fn upsert_issue_account_appends_updates_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("issue-accts.toml");
        std::fs::write(&p, "# keep\n[issues]\nprovider = \"none\"\n").unwrap();
        upsert_issue_account(&p, "work", "linear", "keyring:work", "TEAM", "", "", "", "").unwrap();
        upsert_issue_account(&p, "home", "linear", "env:LIN", "", "", "", "", "").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# keep"), "comment preserved");
        let doc = out.parse::<DocumentMut>().unwrap();
        let aot = doc["issues"]["issue_accounts"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(aot.len(), 2, "two distinct accounts of one provider");
        assert_eq!(aot.get(0).unwrap()["name"].as_str(), Some("work"));
        assert_eq!(aot.get(0).unwrap()["token"].as_str(), Some("keyring:work"));
        assert_eq!(aot.get(0).unwrap()["team_id"].as_str(), Some("TEAM"));
        // The pre-existing `[issues] provider` scalar is preserved.
        assert_eq!(doc["issues"]["provider"].as_str(), Some("none"));
        // Empty scope fields are omitted.
        assert!(aot.get(1).unwrap().get("team_id").is_none());

        // Upsert by name updates in place (no duplicate).
        upsert_issue_account(&p, "work", "linear", "keyring:work2", "T2", "", "", "", "").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let aot = doc["issues"]["issue_accounts"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(aot.len(), 2, "upsert did not duplicate");
        assert_eq!(aot.get(0).unwrap()["token"].as_str(), Some("keyring:work2"));

        // Remove by name.
        remove_array_entry(&p, "issues.issue_accounts", "home").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["issues"]["issue_accounts"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn calendar_account_token_rewrites_only_the_named_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("calendar-accts.toml");
        std::fs::write(
            &p,
            "# keep\n[[calendar.accounts]]\nname = \"work\"\nprovider = \"ics_url\"\nurl = \"https://example.test/work.ics\"\ntoken = \"lit\"\n\n[[calendar.accounts]]\nname = \"home\"\ntoken = \"lit2\"\n",
        )
        .unwrap();

        set_calendar_account_token(&p, "work", "file:/x").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# keep"));
        let doc = out.parse::<DocumentMut>().unwrap();
        let accounts = doc["calendar"]["accounts"].as_array_of_tables().unwrap();
        assert_eq!(accounts.get(0).unwrap()["token"].as_str(), Some("file:/x"));
        assert_eq!(
            accounts.get(0).unwrap()["url"].as_str(),
            Some("https://example.test/work.ics")
        );
        assert_eq!(accounts.get(1).unwrap()["token"].as_str(), Some("lit2"));
        toml::from_str::<crate::config::Config>(&out).unwrap();
    }

    #[test]
    fn calendar_account_token_refuses_an_absent_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("calendar-absent.toml");
        std::fs::write(
            &p,
            "[calendar]\n[[calendar.accounts]]\nname = \"home\"\ntoken = \"lit2\"\n",
        )
        .unwrap();
        let before = std::fs::read(&p).unwrap();

        assert!(!calendar_account_in_file(&p, "work").unwrap());
        assert!(set_calendar_account_token(&p, "work", "file:/x").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), before);
    }

    #[test]
    fn upsert_forge_writes_kind_and_host() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("forges.toml");
        upsert_forge(&p, "ghe", "ghe", "git.corp.example", "env:GHE").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let f = doc["forges"].as_array_of_tables().unwrap().get(0).unwrap();
        assert_eq!(f["name"].as_str(), Some("ghe"));
        assert_eq!(f["kind"].as_str(), Some("ghe"));
        assert_eq!(f["host"].as_str(), Some("git.corp.example"));
    }

    #[test]
    fn upsert_issue_account_rejects_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("issue-empty.toml");
        assert!(upsert_issue_account(&p, " ", "linear", "", "", "", "", "", "").is_err());
    }

    #[test]
    fn set_key_creates_nested_tables() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("set.toml");
        set_key(&p, "sandbox.backend", "docker").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["sandbox"]["backend"].as_str(), Some("docker"));
    }

    #[test]
    fn set_key_writes_canonical_names_for_legacy_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("set-canonical.toml");
        set_key(&p, "workspaces_dir", "/projects").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["projects_dir"].as_str(), Some("/projects"));
        assert!(doc.get("workspaces_dir").is_none());
    }

    #[test]
    fn set_key_writes_arrays_and_inline_tables() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("set_array.toml");
        // The regression: every value was written as a TOML string, so an
        // array-typed key could not be set from the CLI at all — the write was
        // (correctly) rejected by validation and the only fix was hand-editing.
        set_key(
            &p,
            "merge_queue.regenerate_paths",
            r#"["Cargo.lock", "pnpm-lock.yaml"]"#,
        )
        .unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let arr = doc["merge_queue"]["regenerate_paths"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr.get(0).unwrap().as_str(), Some("Cargo.lock"));
        // And the whole file still deserializes — the point of the exercise.
        let body = std::fs::read_to_string(&p).unwrap();
        let cfg: crate::config::Config = toml::from_str(&body).unwrap();
        assert_eq!(cfg.merge_queue.regenerate_paths.len(), 2);
    }

    #[test]
    fn set_key_keeps_bracketed_prose_as_a_string() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("set_prose.toml");
        // Only well-formed TOML gets the array treatment; a value that merely
        // starts with a bracket must stay the string the user typed.
        set_key(&p, "base_branch", "[not valid toml").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["base_branch"].as_str(), Some("[not valid toml"));
    }

    #[test]
    fn set_key_infers_scalar_types_and_config_stays_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("set_typed.toml");
        set_key(&p, "sandbox.enabled", "false").unwrap();
        set_key(&p, "pr.ttl_secs", "120").unwrap();
        set_key(&p, "sandbox.backend", "podman").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        // Native types, not quoted strings.
        assert_eq!(doc["sandbox"]["enabled"].as_bool(), Some(false));
        assert_eq!(doc["pr"]["ttl_secs"].as_integer(), Some(120));
        assert_eq!(doc["sandbox"]["backend"].as_str(), Some("podman"));
        // The whole file must still deserialize as a Config — the failure the bug
        // caused (typed field := quoted string ⇒ load_layered discards everything).
        toml::from_str::<crate::config::Config>(&text)
            .expect("config with a bool/int set must still parse");
    }
}
