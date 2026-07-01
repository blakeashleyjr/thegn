//! Environment **bundles** — the composition seam (roadmap group AU).
//!
//! A [`crate::config::Bundle`] is a named, declarative unit of env vars +
//! credential/config-dir redirection + per-provider account selection (+ opt-in
//! dotfiles/`.env`/secrets, wired in later phases). This module resolves the
//! bundle(s) bound to a scope and folds them into a [`ResolvedEnv`] that maps
//! 1:1 onto the existing [`crate::sandbox::SandboxSpec`] env fields — **no new
//! sandbox mechanism**.
//!
//! Bundles are the "soft middle" between [`crate::account`] (one env var, one
//! provider, per scope) and heavyweight process-profiles (a whole-process
//! firewall). [`compose`] is called for **every** pane spawn (`choice = None`
//! for a plain shell), so a shell in the `work` worktree sees the work identity,
//! not just agents.
//!
//! **Binding & precedence** mirror [`crate::account`] verbatim, generalized from
//! `account:<p>:…` to `bundle:…` over the `ui_state` KV table. Two composition
//! axes, both low→high with per-key override:
//! - **scope layering** — the bundles bound at global → workspace → worktree are
//!   *merged* (not replaced), so a worktree bundle refines the workspace one.
//! - **`extends`** — within a bundle, named parents merge first.
//!
//! Effective env = curated base ◁ extends chain ◁ global ◁ workspace ◁ worktree.

use crate::account;
use crate::config::{Bundle, Config, expand_env_ref};
use crate::db::Db;
use crate::sandbox::Mount;
use crate::util;
use std::collections::{BTreeMap, HashSet};

/// The resolved product of composing the active bundle(s) for a pane. Maps 1:1
/// onto [`crate::sandbox::SandboxSpec`]`.{env_overrides, env_block, mounts}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedEnv {
    /// `KEY=VALUE` to set in the child (deterministically ordered by key).
    pub overrides: Vec<(String, String)>,
    /// Keys to unset inside the child (e.g. master API key masked by a scoped
    /// one). Empty for a pure bundle; agent-launch may add to it.
    pub block: Vec<String>,
    /// Path-preserving credential/home mounts for the sandbox.
    pub mounts: Vec<Mount>,
    /// Credential-home dirs the caller must `create_dir_all` before launch (the
    /// agent CLI writes tokens/history there). Distinct from `config_dirs`, which
    /// are user-owned and only mounted when they already exist.
    pub ensure_dirs: Vec<String>,
}

impl ResolvedEnv {
    /// The overrides as plain `(KEY, VALUE)` pairs — for a non-sandboxed pane's
    /// caller env (layered on top of the curated base by `spawn_with_env`).
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.overrides.clone()
    }

    /// Fold this resolution into an existing sandbox spec: override env wins over
    /// passthrough, blocked keys are unset, mounts appended (dedup by dest).
    pub fn merge_into_spec(&self, spec: &mut crate::sandbox::SandboxSpec) {
        for (k, v) in &self.overrides {
            spec.env_overrides.insert(k.clone(), v.clone());
        }
        for k in &self.block {
            if !spec.env_block.contains(k) {
                spec.env_block.push(k.clone());
            }
        }
        for m in &self.mounts {
            if !spec.mounts.iter().any(|e| e.dest == m.dest) {
                spec.mounts.push(m.clone());
            }
        }
    }
}

// --- scope pointers (over the ui_state KV store) ---------------------------

fn scope_global() -> String {
    "bundle".to_string()
}
fn scope_ws(slug: &str) -> String {
    format!("bundle:ws:{slug}")
}
fn scope_wt(worktree: &str) -> String {
    format!("bundle:wt:{worktree}")
}

/// The bundle bound at the global scope (`ui_state["bundle"].active`).
fn global_binding(db: &Db) -> Option<String> {
    db.get_ui_state(&scope_global(), "active").ok().flatten()
}

/// The bundle bound at the workspace scope: `[workspace.<slug>].env_bundle`
/// (config) wins over the `ui_state` pointer, matching `account.rs`.
fn workspace_binding(cfg: &Config, db: &Db, slug: Option<&str>) -> Option<String> {
    let slug = slug?;
    if let Some(name) = cfg.workspace.get(slug).and_then(|w| w.env_bundle.clone()) {
        return Some(name);
    }
    db.get_ui_state(&scope_ws(slug), "active").ok().flatten()
}

/// The bundle bound at the worktree scope (strongest single binding).
fn worktree_binding(db: &Db, worktree: &str) -> Option<String> {
    db.get_ui_state(&scope_wt(worktree), "active")
        .ok()
        .flatten()
}

/// The single most-specific bound bundle name (worktree → workspace → global),
/// for the switcher chip + display. `None` ⇒ no bundle bound anywhere.
pub fn active_name(cfg: &Config, db: &Db, worktree: &str, slug: Option<&str>) -> Option<String> {
    worktree_binding(db, worktree)
        .or_else(|| workspace_binding(cfg, db, slug))
        .or_else(|| global_binding(db))
}

/// Where an active bundle selection can be pinned. Same shape as
/// [`crate::account::Bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    Global,
    Workspace,
    Worktree,
}

/// Bind `name` as the active bundle at the given scope.
pub fn set_active(
    db: &Db,
    bind: Bind,
    worktree: &str,
    slug: Option<&str>,
    name: &str,
) -> anyhow::Result<()> {
    let scope = match bind {
        Bind::Global => scope_global(),
        Bind::Workspace => scope_ws(slug.unwrap_or_default()),
        Bind::Worktree => scope_wt(worktree),
    };
    db.set_ui_state(&scope, "active", name)?;
    Ok(())
}

/// Clear the active-bundle binding at the given scope.
pub fn clear_active(db: &Db, bind: Bind, worktree: &str, slug: Option<&str>) -> anyhow::Result<()> {
    let scope = match bind {
        Bind::Global => scope_global(),
        Bind::Workspace => scope_ws(slug.unwrap_or_default()),
        Bind::Worktree => scope_wt(worktree),
    };
    db.del_ui_state(&scope, "active")?;
    Ok(())
}

// --- composition -----------------------------------------------------------

/// Depth-first expand `name`'s `extends` chain into `out` (parents before self),
/// each bundle exactly once (`seen` guards cycles + cross-scope duplicates), in
/// low→high precedence order. Unknown names are skipped with a warning.
fn collect_chain(cfg: &Config, name: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
    if !seen.insert(name.to_string()) {
        return;
    }
    match cfg.bundle.get(name) {
        Some(b) => {
            for parent in &b.extends {
                collect_chain(cfg, parent, seen, out);
            }
            out.push(name.to_string());
        }
        None => crate::msg::warn(&format!("bundle: unknown bundle {name:?} (ignored)")),
    }
}

/// Resolve one raw bundle value.
///
/// - `env:VAR` / `file:PATH` → [`expand_env_ref`].
/// - `<scheme>:<ref>` where `<scheme>` is a configured `[secrets.resolvers]`
///   entry → run the resolver command (only when `allow_secrets`; otherwise the
///   key is *skipped*, never injected as the raw `pass:…` reference). Resolvers
///   run a subprocess, so `allow_secrets` is true only at launch (off the event
///   loop) — unit-test/static callers pass `false`.
/// - anything else → the literal string.
pub fn resolve_value(raw: &str, cfg: &Config, allow_secrets: bool) -> Option<String> {
    let v = raw.trim();
    if v.is_empty() {
        return None;
    }
    if v.starts_with("env:") || v.starts_with("file:") {
        return expand_env_ref(v);
    }
    if let Some((scheme, rest)) = v.split_once(':')
        && let Some(template) = cfg.secrets.resolvers.get(scheme)
    {
        return allow_secrets
            .then(|| run_resolver(scheme, template, v, rest))
            .flatten();
    }
    Some(v.to_string())
}

/// Back-compat static resolver (no subprocess): secret schemes are skipped.
pub fn resolve_value_static(raw: &str, cfg: &Config) -> Option<String> {
    resolve_value(raw, cfg, false)
}

/// Run a `[secrets.resolvers]` command template, substituting placeholders, and
/// return trimmed stdout. Placeholders: `{ref}` (the part after `<scheme>:`),
/// `{value}` (the full raw value incl. scheme — for `op://…`), and `{file}` /
/// `{key}` (`{ref}` split on its last `:`, for `sops`-style refs). Runs via
/// `sh -c` (config is trusted, user-authored). The result is **never persisted
/// or logged**; failure degrades gracefully (warn + `None`) so a launch never
/// blocks on a missing secret backend.
fn run_resolver(scheme: &str, template: &str, value: &str, rest: &str) -> Option<String> {
    let (file, key) = rest.rsplit_once(':').unwrap_or((rest, ""));
    let cmd = template
        .replace("{ref}", rest)
        .replace("{value}", value)
        .replace("{file}", file)
        .replace("{key}", key);
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output()
        .ok()?;
    if !out.status.success() {
        crate::msg::warn(&format!(
            "bundle: secret resolver {scheme:?} failed (exit {:?}); skipping",
            out.status.code()
        ));
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        crate::msg::warn(&format!(
            "bundle: secret resolver {scheme:?} returned empty; skipping"
        ));
        return None;
    }
    Some(s)
}

fn push_mount(mounts: &mut Vec<Mount>, path: &str) {
    if path.is_empty() || mounts.iter().any(|m| m.dest == path) {
        return;
    }
    // Path-preserving, read-write: credential homes (agent login, `git config
    // --global`, `gh` token refresh) are written in place.
    mounts.push(Mount {
        host: path.to_string(),
        dest: path.to_string(),
        ro: false,
        cache: false,
    });
}

fn push_mount_if_exists(mounts: &mut Vec<Mount>, path: &str) {
    if !path.is_empty() && std::path::Path::new(path).exists() {
        push_mount(mounts, path);
    }
}

/// Fold a credential-home dir (account selection): set the provider's home env
/// var, request the dir be created, and mount it path-preserving.
fn fold_cred_dir(
    home_env: &str,
    dir: &str,
    overrides: &mut BTreeMap<String, String>,
    mounts: &mut Vec<Mount>,
    ensure_dirs: &mut Vec<String>,
) {
    push_mount(mounts, dir);
    if !ensure_dirs.iter().any(|d| d == dir) {
        ensure_dirs.push(dir.to_string());
    }
    overrides.insert(home_env.to_string(), dir.to_string());
}

/// The managed HOME dir for a bundle's Tier-2/3 dotfiles
/// (`$XDG_STATE_HOME/superzej/bundles/<slug>/home`).
pub fn managed_home(bundle_name: &str) -> std::path::PathBuf {
    util::xdg_state_home()
        .join("superzej")
        .join("bundles")
        .join(util::slugify(bundle_name))
        .join("home")
}

/// Compose the active bundle(s) for a pane into a [`ResolvedEnv`], resolving
/// secret refs via subprocess. Call at launch (off the event loop). `choice` is
/// the agent name, or `None` for a plain shell pane.
pub fn compose_at_launch(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    choice: Option<&str>,
) -> ResolvedEnv {
    compose_inner(cfg, db, worktree, slug, choice, true)
}

/// Compose the active bundle(s) for a pane into a [`ResolvedEnv`]. `choice` is
/// the agent name, or `None` for a plain shell pane (still gets the bundle
/// identity). Folds env / accounts / config_dirs / Tier-3 `home`; secret refs
/// are **not** dispatched (use [`compose_at_launch`] for that). `.env` opt-in
/// layers on in Phase 1d. `worktree` is the absolute worktree path; `slug` its
/// repo slug.
pub fn compose(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    choice: Option<&str>,
) -> ResolvedEnv {
    compose_inner(cfg, db, worktree, slug, choice, false)
}

/// The ordered, deduped list of bundle names that apply to a scope, low→high:
/// global, then workspace, then worktree — each expanding its `extends` chain
/// first (parents before self). This is the exact fold order [`compose`] uses;
/// exposed so launch-time side effects (Tier-2 dotfile materialization) can
/// iterate the same set.
pub fn active_chain(cfg: &Config, db: &Db, worktree: &str, slug: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    if let Some(g) = global_binding(db) {
        collect_chain(cfg, &g, &mut seen, &mut order);
    }
    if let Some(w) = workspace_binding(cfg, db, slug) {
        collect_chain(cfg, &w, &mut seen, &mut order);
    }
    if let Some(t) = worktree_binding(db, worktree) {
        collect_chain(cfg, &t, &mut seen, &mut order);
    }
    order
}

fn compose_inner(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    choice: Option<&str>,
    allow_secrets: bool,
) -> ResolvedEnv {
    let order = active_chain(cfg, db, worktree, slug);

    let mut overrides: BTreeMap<String, String> = BTreeMap::new();
    let mut mounts: Vec<Mount> = Vec::new();
    let mut ensure_dirs: Vec<String> = Vec::new();

    for name in &order {
        let Some(b) = cfg.bundle.get(name) else {
            continue;
        };
        fold_bundle(
            cfg,
            db,
            name,
            b,
            allow_secrets,
            &mut overrides,
            &mut mounts,
            &mut ensure_dirs,
        );
    }

    // Back-compat: fold the legacy per-provider active account for `choice` if a
    // bundle didn't already select that provider's credential home. Keeps the
    // account-switcher (item 656) working through the unified seam.
    if let Some(choice) = choice
        && let Some(p) = account::provider_for(cfg, choice)
        && !overrides.contains_key(p.home_env)
        && let Some((var, dir)) = account::launch_env(cfg, db, worktree, slug, choice)
    {
        fold_cred_dir(
            &var,
            &dir.to_string_lossy(),
            &mut overrides,
            &mut mounts,
            &mut ensure_dirs,
        );
    }

    // Opt-in `.env` (lowest precedence): only when an active bundle set
    // `dotenv = true`, the worktree `.env` has been allow-listed by content
    // hash, and per-key it (a) is not credential-shaped and (b) does not
    // override a bundle-set value. Fills gaps only.
    if order
        .iter()
        .any(|n| cfg.bundle.get(n).is_some_and(|b| b.dotenv))
    {
        fold_dotenv(db, worktree, &mut overrides);
    }

    ResolvedEnv {
        overrides: overrides.into_iter().collect(),
        block: Vec::new(),
        mounts,
        ensure_dirs,
    }
}

/// Fold one bundle's `env` / `accounts` / `config_dirs` / Tier-3 `home` into the
/// accumulators. (Tier-2 dotfile *materialization* is done separately, off-loop,
/// by [`materialize_dotfiles`]; `.env` folds in Phase 1d.)
#[allow(clippy::too_many_arguments)]
fn fold_bundle(
    cfg: &Config,
    db: &Db,
    name: &str,
    b: &Bundle,
    allow_secrets: bool,
    overrides: &mut BTreeMap<String, String>,
    mounts: &mut Vec<Mount>,
    ensure_dirs: &mut Vec<String>,
) {
    for (k, v) in &b.env {
        if let Some(val) = resolve_value(v, cfg, allow_secrets) {
            overrides.insert(k.clone(), val);
        }
    }
    for (provider, acct) in &b.accounts {
        if let Some(p) = account::provider(provider)
            && let Some(dir) = account::account_dir(cfg, db, provider, acct)
        {
            fold_cred_dir(
                p.home_env,
                &dir.to_string_lossy(),
                overrides,
                mounts,
                ensure_dirs,
            );
        }
    }
    for (k, v) in &b.config_dirs {
        let path = util::expand_tilde(v);
        push_mount_if_exists(mounts, &path);
        overrides.insert(k.clone(), path);
    }
    // Tier-3 synthetic HOME: `"managed"` roots panes at the bundle's managed
    // HOME (materialized separately); `"<path>"` at an explicit dir.
    if !b.home.is_empty() {
        let home = if b.home == "managed" {
            managed_home(name).to_string_lossy().into_owned()
        } else {
            util::expand_tilde(&b.home)
        };
        fold_cred_dir("HOME", &home, overrides, mounts, ensure_dirs);
    }
}

/// Materialize a bundle's Tier-2 dotfiles into its managed HOME, idempotently.
/// I/O — call off the event loop (launch time). Symlinks (or copies, for
/// `template` mode) each top-level entry of `source` into `dest_home`; a
/// `meta.json` records the source content-signature so an unchanged source is a
/// no-op. Best-effort: returns the count materialized, warns and continues on
/// per-entry errors so a launch never fails on a dotfile glitch.
pub fn materialize_dotfiles(
    spec: &crate::config::DotfilesSpec,
    dest_home: &std::path::Path,
) -> usize {
    use crate::config::DotfileMode;
    let source = std::path::PathBuf::from(util::expand_tilde(&spec.source));
    if !source.is_dir() {
        crate::msg::warn(&format!(
            "bundle: dotfiles source {} is not a directory; skipping",
            source.display()
        ));
        return 0;
    }
    if let Err(e) = std::fs::create_dir_all(dest_home) {
        crate::msg::warn(&format!(
            "bundle: cannot create managed HOME {}: {e}",
            dest_home.display()
        ));
        return 0;
    }
    // Idempotency: skip when the source signature (entry names + mtimes) is
    // unchanged since the last materialization.
    let sig = dotfiles_signature(&source, spec.mode);
    let meta = dest_home.join(".superzej-dotfiles.json");
    if std::fs::read_to_string(&meta).ok().as_deref() == Some(sig.as_str()) {
        return 0;
    }
    let mut n = 0;
    let entries = match std::fs::read_dir(&source) {
        Ok(e) => e,
        Err(e) => {
            crate::msg::warn(&format!("bundle: cannot read dotfiles source: {e}"));
            return 0;
        }
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let Some(base) = from.file_name() else {
            continue;
        };
        let to = dest_home.join(base);
        let _ = std::fs::remove_file(&to).or_else(|_| std::fs::remove_dir_all(&to));
        let res = match spec.mode {
            DotfileMode::Symlink => symlink(&from, &to),
            DotfileMode::Template => copy_tree(&from, &to),
        };
        match res {
            Ok(()) => n += 1,
            Err(e) => crate::msg::warn(&format!(
                "bundle: dotfile {} → {}: {e}",
                from.display(),
                to.display()
            )),
        }
    }
    let _ = std::fs::write(&meta, sig);
    n
}

fn dotfiles_signature(source: &std::path::Path, mode: crate::config::DotfileMode) -> String {
    let mut items: Vec<String> = std::fs::read_dir(source)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{name}:{mtime}")
        })
        .collect();
    items.sort();
    format!("{}|{}", mode.as_str(), items.join(","))
}

#[cfg(unix)]
fn symlink(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(from, to)
}
#[cfg(not(unix))]
fn symlink(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    copy_tree(from, to)
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)?.flatten() {
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

// --- opt-in .env ------------------------------------------------------------

/// Env-var name suffixes treated as credential-shaped and **dropped** from a
/// `.env` (a repo's `.env` cannot inject a token into a pane's environment).
const CRED_SUFFIXES: &[&str] = &["_TOKEN", "_KEY", "_SECRET", "_PASSWORD"];

/// Whether `key` looks like a credential (case-insensitive suffix match).
pub fn is_credential_key(key: &str) -> bool {
    let u = key.to_ascii_uppercase();
    CRED_SUFFIXES.iter().any(|s| u.ends_with(s))
}

/// Parse `.env` content into `(KEY, VALUE)` pairs: skips blanks/`#` comments,
/// tolerates a leading `export `, requires a `KEY=VALUE` shape with an
/// identifier-safe key, and strips one layer of matching surrounding quotes.
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        out.push((k.to_string(), v.to_string()));
    }
    out
}

/// Deterministic content signature for the `.env` allowlist (stable across runs;
/// std's `DefaultHasher` uses fixed keys).
fn content_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn dotenv_scope(path: &str) -> String {
    format!("dotenv:allow:{path}")
}

/// Whether the worktree `.env` at `path` (with the given `content`) has been
/// allow-listed at its *current* content hash. A changed file needs re-allowing.
pub fn dotenv_allowed(db: &Db, path: &str, content: &str) -> bool {
    db.get_ui_state(&dotenv_scope(path), "hash")
        .ok()
        .flatten()
        .as_deref()
        == Some(content_hash(content).as_str())
}

/// Allow-list a worktree `.env` at its current content hash (called by the UI on
/// explicit user approval).
pub fn allow_dotenv(db: &Db, path: &str, content: &str) -> anyhow::Result<()> {
    db.set_ui_state(&dotenv_scope(path), "hash", &content_hash(content))?;
    Ok(())
}

/// Fold an allow-listed worktree `.env` into `overrides` at lowest precedence:
/// credential-shaped keys dropped, existing (bundle-set) keys never overridden.
fn fold_dotenv(db: &Db, worktree: &str, overrides: &mut BTreeMap<String, String>) {
    let path = std::path::Path::new(worktree).join(".env");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let path_s = path.to_string_lossy();
    if !dotenv_allowed(db, &path_s, &content) {
        return;
    }
    for (k, v) in parse_dotenv(&content) {
        if is_credential_key(&k) {
            continue;
        }
        overrides.entry(k).or_insert(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Account, Bundle, NamedCommand};

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn bundle_with_env(pairs: &[(&str, &str)]) -> Bundle {
        let mut b = Bundle::default();
        for (k, v) in pairs {
            b.env.insert(k.to_string(), v.to_string());
        }
        b
    }

    #[test]
    fn empty_when_nothing_bound() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        assert!(r.overrides.is_empty() && r.mounts.is_empty() && r.block.is_empty());
    }

    #[test]
    fn resolved_env_pairs_returns_overrides() {
        let r = ResolvedEnv {
            overrides: vec![("A".into(), "1".into())],
            ..Default::default()
        };
        assert_eq!(r.env_pairs(), r.overrides);
    }

    #[test]
    fn bound_but_undefined_bundle_is_ignored() {
        // A binding to a name with no `[bundle.<name>]` definition warns and
        // composes to the identity (no keys), never panics.
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        set_active(&db, Bind::Global, "/wt", None, "ghost").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert!(r.overrides.is_empty());
    }

    #[test]
    fn materialize_template_copies_tree_and_skips_missing_source() {
        use crate::config::{DotfileMode, DotfilesSpec};
        // Missing source → no-op (0), no panic.
        let missing = DotfilesSpec {
            source: "/no/such/dotfiles/dir".into(),
            mode: DotfileMode::Template,
        };
        assert_eq!(
            materialize_dotfiles(&missing, std::path::Path::new("/tmp/sz-none")),
            0
        );

        // Template mode copies a nested tree into the managed HOME.
        let root = std::env::temp_dir().join(format!("sz-tmpl-{}", util::now()));
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("nested").join("f"), b"hi").unwrap();
        let dest = root.join("home");
        let spec = DotfilesSpec {
            source: src.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };
        assert_eq!(materialize_dotfiles(&spec, &dest), 1);
        assert_eq!(std::fs::read(dest.join("nested").join("f")).unwrap(), b"hi");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scope_layering_worktree_refines_workspace_and_global() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.bundle
            .insert("g".into(), bundle_with_env(&[("A", "g"), ("B", "g")]));
        cfg.bundle
            .insert("w".into(), bundle_with_env(&[("B", "w"), ("C", "w")]));
        cfg.bundle
            .insert("t".into(), bundle_with_env(&[("C", "t")]));
        set_active(&db, Bind::Global, "/wt", Some("repo"), "g").unwrap();
        set_active(&db, Bind::Workspace, "/wt", Some("repo"), "w").unwrap();
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "t").unwrap();

        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        // A only in global; B overridden by workspace; C overridden by worktree.
        assert_eq!(get(&r.overrides, "A"), Some("g"));
        assert_eq!(get(&r.overrides, "B"), Some("w"));
        assert_eq!(get(&r.overrides, "C"), Some("t"));
    }

    #[test]
    fn workspace_config_env_bundle_beats_pointer() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.bundle
            .insert("cfg".into(), bundle_with_env(&[("X", "cfg")]));
        cfg.bundle
            .insert("ptr".into(), bundle_with_env(&[("X", "ptr")]));
        cfg.workspace.entry("repo".into()).or_default().env_bundle = Some("cfg".into());
        // A ui_state workspace pointer also exists, but config wins.
        set_active(&db, Bind::Workspace, "/wt", Some("repo"), "ptr").unwrap();
        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "X"), Some("cfg"));
    }

    #[test]
    fn extends_chain_merges_parents_first() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.bundle.insert(
            "base".into(),
            bundle_with_env(&[("A", "base"), ("B", "base")]),
        );
        let mut work = bundle_with_env(&[("B", "work"), ("C", "work")]);
        work.extends = vec!["base".into()];
        cfg.bundle.insert("work".into(), work);
        set_active(&db, Bind::Global, "/wt", None, "work").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&r.overrides, "A"), Some("base")); // inherited
        assert_eq!(get(&r.overrides, "B"), Some("work")); // child overrides parent
        assert_eq!(get(&r.overrides, "C"), Some("work"));
    }

    #[test]
    fn extends_cycle_is_broken() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        let mut a = bundle_with_env(&[("A", "a")]);
        a.extends = vec!["b".into()];
        let mut b = bundle_with_env(&[("B", "b")]);
        b.extends = vec!["a".into()];
        cfg.bundle.insert("a".into(), a);
        cfg.bundle.insert("b".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "a").unwrap();
        // Must terminate and include both keys.
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&r.overrides, "A"), Some("a"));
        assert_eq!(get(&r.overrides, "B"), Some("b"));
    }

    #[test]
    fn accounts_fold_to_provider_home_var() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "work".into(),
            provider: "claude".into(),
            dir: Some("/creds/claude-work".into()),
        });
        let mut b = Bundle::default();
        b.accounts.insert("claude".into(), "work".into());
        cfg.bundle.insert("work".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "work").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(
            get(&r.overrides, "CLAUDE_CONFIG_DIR"),
            Some("/creds/claude-work")
        );
        // Credential home is path-preservingly mounted and requested for creation
        // even though it doesn't exist on disk yet.
        assert!(
            r.mounts
                .iter()
                .any(|m| m.dest == "/creds/claude-work" && !m.ro)
        );
        assert!(r.ensure_dirs.iter().any(|d| d == "/creds/claude-work"));
    }

    #[test]
    fn legacy_account_selection_folds_when_no_bundle_sets_it() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.agents.push(NamedCommand {
            name: "claude".into(),
            command: "claude".into(),
            hints: vec![],
            provider: None,
        });
        cfg.accounts.push(Account {
            name: "work".into(),
            provider: "claude".into(),
            dir: Some("/creds/claude-work".into()),
        });
        account::set_active(&db, account::Bind::Global, "/wt", None, "claude", "work").unwrap();
        // No bundle bound at all — the legacy account still reaches the child.
        let r = compose(&cfg, &db, "/wt", None, Some("claude"));
        assert_eq!(
            get(&r.overrides, "CLAUDE_CONFIG_DIR"),
            Some("/creds/claude-work")
        );
    }

    #[test]
    fn config_dirs_redirect_and_secret_scheme_is_skipped() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.secrets
            .resolvers
            .insert("pass".into(), "pass show {ref}".into());
        let mut b = Bundle::default();
        b.config_dirs
            .insert("GIT_CONFIG_GLOBAL".into(), "/etc/gitconfig-work".into());
        b.env.insert("TOK".into(), "pass:work/tok".into()); // needs Phase 1c → skipped now
        b.env.insert("URL".into(), "https://proxy.internal".into()); // scheme not a resolver
        cfg.bundle.insert("work".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "work").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(
            get(&r.overrides, "GIT_CONFIG_GLOBAL"),
            Some("/etc/gitconfig-work")
        );
        assert_eq!(get(&r.overrides, "URL"), Some("https://proxy.internal"));
        assert_eq!(
            get(&r.overrides, "TOK"),
            None,
            "unresolved secret must not inject raw ref"
        );
    }

    #[test]
    fn secret_resolver_runs_at_launch_and_degrades_on_failure() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        // A safe resolver that just echoes its ref — proves dispatch + capture.
        cfg.secrets
            .resolvers
            .insert("echoref".into(), "printf '%s' 'val-{ref}'".into());
        // A resolver that always fails — must degrade (skip), never block.
        cfg.secrets.resolvers.insert("boom".into(), "exit 3".into());
        let mut b = Bundle::default();
        b.env.insert("TOK".into(), "echoref:secret/id".into());
        b.env.insert("BAD".into(), "boom:whatever".into());
        cfg.bundle.insert("work".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "work").unwrap();

        // Static compose skips secret schemes entirely.
        let stat = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&stat.overrides, "TOK"), None);

        // Launch compose dispatches the resolver…
        let launched = compose_at_launch(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&launched.overrides, "TOK"), Some("val-secret/id"));
        // …and a failing resolver is skipped, not fatal.
        assert_eq!(get(&launched.overrides, "BAD"), None);
    }

    #[test]
    fn tier3_managed_home_folds_home_override_and_ensure() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        let mut b = Bundle::default();
        b.home = "managed".into();
        cfg.bundle.insert("work".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "work").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        let home = managed_home("work").to_string_lossy().into_owned();
        assert_eq!(get(&r.overrides, "HOME"), Some(home.as_str()));
        assert!(r.ensure_dirs.iter().any(|d| d == &home));
        assert!(r.mounts.iter().any(|m| m.dest == home));
    }

    #[test]
    fn materialize_dotfiles_symlinks_and_is_idempotent() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = std::env::temp_dir().join(format!("sz-dotfiles-{}", util::now()));
        let src = root.join("src");
        let dest = root.join("home");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".bashrc"), b"export X=1\n").unwrap();
        let spec = DotfilesSpec {
            source: src.to_string_lossy().into_owned(),
            mode: DotfileMode::Symlink,
        };
        // First run materializes one entry…
        assert_eq!(materialize_dotfiles(&spec, &dest), 1);
        assert!(dest.join(".bashrc").exists());
        // …the second is a no-op (unchanged signature).
        assert_eq!(materialize_dotfiles(&spec, &dest), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_dotenv_handles_comments_quotes_and_export() {
        let pairs =
            parse_dotenv("# comment\n\nexport FOO=bar\nQUOTED=\"a b\"\nSQ='x'\nBAD LINE\n=noKey\n");
        assert_eq!(get(&pairs, "FOO"), Some("bar"));
        assert_eq!(get(&pairs, "QUOTED"), Some("a b"));
        assert_eq!(get(&pairs, "SQ"), Some("x"));
        assert_eq!(pairs.len(), 3, "malformed lines dropped");
    }

    #[test]
    fn credential_key_matches_secret_suffixes() {
        assert!(is_credential_key("GH_TOKEN"));
        assert!(is_credential_key("aws_secret"));
        assert!(is_credential_key("API_KEY"));
        assert!(is_credential_key("DB_PASSWORD"));
        assert!(!is_credential_key("EDITOR"));
        assert!(!is_credential_key("PATH"));
    }

    #[test]
    fn dotenv_gated_by_allow_filters_creds_and_never_overrides_bundle() {
        let db = Db::open_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("sz-dotenv-{}", util::now()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "FOO=fromenv\nSECRET_KEY=leak\n").unwrap();
        let wt = dir.to_string_lossy().into_owned();

        let mut cfg = Config::default();
        // A bundle that opts into .env AND sets FOO itself.
        let mut b = bundle_with_env(&[("FOO", "frombundle")]);
        b.dotenv = true;
        cfg.bundle.insert("work".into(), b);
        set_active(&db, Bind::Global, &wt, None, "work").unwrap();

        // Not yet allow-listed → .env ignored entirely.
        let r = compose(&cfg, &db, &wt, None, None);
        assert_eq!(get(&r.overrides, "FOO"), Some("frombundle"));
        assert_eq!(get(&r.overrides, "SECRET_KEY"), None);

        // After allow: SECRET_KEY still filtered; FOO NOT overridden by .env.
        let content = std::fs::read_to_string(dir.join(".env")).unwrap();
        allow_dotenv(&db, &dir.join(".env").to_string_lossy(), &content).unwrap();
        let r = compose(&cfg, &db, &wt, None, None);
        assert_eq!(
            get(&r.overrides, "FOO"),
            Some("frombundle"),
            "bundle wins over .env"
        );
        assert_eq!(
            get(&r.overrides, "SECRET_KEY"),
            None,
            "credential-shaped key filtered"
        );

        // A gap-only var loads once allowed.
        std::fs::write(
            dir.join(".env"),
            "FOO=fromenv\nEDITOR=vim\nSECRET_KEY=leak\n",
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.join(".env")).unwrap();
        allow_dotenv(&db, &dir.join(".env").to_string_lossy(), &content).unwrap();
        let r = compose(&cfg, &db, &wt, None, None);
        assert_eq!(get(&r.overrides, "EDITOR"), Some("vim"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_and_clear_binding_roundtrip() {
        let db = Db::open_memory().unwrap();
        let cfg = Config::default();
        assert_eq!(active_name(&cfg, &db, "/wt", Some("repo")), None);
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "work").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("repo")).as_deref(),
            Some("work")
        );
        clear_active(&db, Bind::Worktree, "/wt", Some("repo")).unwrap();
        assert_eq!(active_name(&cfg, &db, "/wt", Some("repo")), None);
    }
}
