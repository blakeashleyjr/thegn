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
use crate::store::{WorkspaceStore, ZoneStore};
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
    /// Zone-owned bundles skipped because this worktree isn't in their zone (the
    /// credential sub-vault firewall). Each entry is `(bundle_name, owner_zone)`.
    /// The launch continues without them; the caller surfaces the denial.
    pub denied: Vec<(String, String)>,
    /// Bundle names that passed zone visibility and were folded, in fold order.
    /// Launch-time bundle effects must use this authorization result.
    pub authorized: Vec<String>,
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
/// `Err(())` = the trusted overlay is refused (THE-515): the caller must not
/// substitute the workspace pointer or the global binding for it.
fn workspace_binding(cfg: &Config, db: &Db, slug: Option<&str>) -> Result<Option<String>, ()> {
    let Some(slug) = slug else {
        return Ok(None);
    };
    let overlay = cfg.workspace_overlay_for_tab_slug(db, slug);
    if overlay.is_refused() {
        return Err(());
    }
    if let Some(name) = overlay.overlay().and_then(|w| w.env_bundle.clone()) {
        return Ok(Some(name));
    }
    Ok(db.get_ui_state(&scope_ws(slug), "active").ok().flatten())
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
    if let Some(name) = worktree_binding(db, worktree) {
        return Some(name);
    }
    match workspace_binding(cfg, db, slug) {
        Err(()) => None,
        Ok(Some(name)) => Some(name),
        Ok(None) => global_binding(db),
    }
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
/// - `env:VAR` / `file:PATH` → `expand_env_ref`.
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
    // D4: bound the resolver subprocess. `launch_spec` runs on the event loop, so
    // a hung secret backend (1password/keyring/dbus) would freeze the compositor
    // indefinitely; cap it so the worst case is a bounded stall + graceful skip.
    let argv = ["sh".to_string(), "-c".to_string(), cmd];
    let (ok, stdout) =
        crate::sandbox::output_with_timeout(&argv, std::time::Duration::from_secs(8))?;
    if !ok {
        crate::msg::warn(&format!(
            "bundle: secret resolver {scheme:?} failed or timed out; skipping"
        ));
        return None;
    }
    let s = stdout.trim().to_string();
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
/// (`$XDG_STATE_HOME/thegn/bundles/<slug>/home`).
pub fn managed_home(bundle_name: &str) -> std::path::PathBuf {
    util::xdg_state_home()
        .join("thegn")
        .join("bundles")
        .join(util::slugify(bundle_name))
        .join("home")
}

/// Resolve a managed home only when the configured bundle name has an
/// unambiguous non-empty legacy slug. The readable legacy layout remains in use
/// for names that have exactly one claimant.
pub fn managed_home_checked(cfg: &Config, bundle_name: &str) -> Option<std::path::PathBuf> {
    let slug = util::slugify(bundle_name);
    if slug.is_empty()
        || slug.len() > 255
        || cfg
            .bundle
            .keys()
            .any(|other| other != bundle_name && util::slugify(other) == slug)
    {
        return None;
    }
    Some(managed_home(bundle_name))
}

/// Dotfile effects for exactly the bundles authorized by composition.
pub fn dotfile_targets(
    cfg: &Config,
    resolved: &ResolvedEnv,
) -> Vec<(crate::config::DotfilesSpec, std::path::PathBuf)> {
    resolved
        .authorized
        .iter()
        .filter_map(|name| {
            let spec = cfg.bundle.get(name)?.dotfiles.as_ref()?;
            let Some(home) = managed_home_checked(cfg, name) else {
                crate::msg::warn(&format!(
                    "bundle: dotfiles HOME for {name:?} is ambiguous or invalid; skipped"
                ));
                return None;
            };
            Some((spec.clone(), home))
        })
        .collect()
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
    // THE-515: a refused trusted overlay drops the global and workspace
    // layers; only the explicit per-worktree binding still applies.
    let workspace = workspace_binding(cfg, db, slug);
    if workspace.is_ok()
        && let Some(g) = global_binding(db)
    {
        collect_chain(cfg, &g, &mut seen, &mut order);
    }
    if let Ok(Some(w)) = workspace {
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
    let mut denied: Vec<(String, String)> = Vec::new();
    let mut authorized: Vec<String> = Vec::new();

    // The worktree's zone (DB-tracked). A zone-owned bundle is a credential
    // sub-vault: only worktrees in that zone may compose it. Checked at fold
    // time so it covers direct, workspace, global, AND `extends`-reachable
    // bindings uniformly. See [`crate::zone`].
    let worktree_zone = db
        .zone_of_worktree(worktree)
        .ok()
        .flatten()
        .map(|z| z.name)
        .unwrap_or_default();

    // Profile-base identity (lowest precedence): the active profile may resolve
    // its credentials from a named identity (`[profiles.<p>] identity = "…"`).
    // Fold it first so bound bundles layer on top per scope; any tool the identity
    // leaves unset falls back to the profile-root default (pinned into the process
    // env at reroot and carried into the pane via the host-env allowlist).
    if let Some(id_name) = cfg
        .profiles
        .get(&crate::profile::name())
        .map(|p| p.identity.clone())
        .filter(|s| !s.is_empty())
    {
        fold_identity(
            cfg,
            db,
            &id_name,
            &mut overrides,
            &mut mounts,
            &mut ensure_dirs,
        );
    }

    for name in &order {
        let Some(b) = cfg.bundle.get(name) else {
            continue;
        };
        if !crate::zone::bundle_visible(&b.zone, &worktree_zone) {
            crate::msg::warn(&format!(
                "bundle: {name:?} is owned by zone {:?}; skipped for this worktree (zone {:?})",
                b.zone, worktree_zone
            ));
            denied.push((name.clone(), b.zone.clone()));
            continue;
        }
        authorized.push(name.clone());
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

    // Directly-bound identities (the identity switcher), highest-precedence per
    // scope (global → workspace → worktree), folded after the bundle chain so an
    // explicit identity switch wins over a bundle-referenced one. Each tool it
    // leaves unset still falls through to the bundle/profile-base identity below.
    for id_name in crate::identity::bound_in_order(db, worktree, slug) {
        fold_identity(
            cfg,
            db,
            &id_name,
            &mut overrides,
            &mut mounts,
            &mut ensure_dirs,
        );
    }

    // Back-compat: fold the legacy per-provider active account for `choice` if a
    // bundle didn't already select that provider's credential home. Keeps the
    // account-switcher (item 656) working through the unified seam. `.or_else`
    // covers an ad-hoc `claude` launch with no matching `[[agents]]` entry.
    if let Some(choice) = choice
        && let Some(p) =
            account::provider_for(cfg, choice).or_else(|| account::infer_provider(choice))
        && !overrides.contains_key(p.home_env)
    {
        if let Some((var, dir)) = account::launch_env(cfg, db, worktree, slug, choice) {
            fold_cred_dir(
                &var,
                &dir.to_string_lossy(),
                &mut overrides,
                &mut mounts,
                &mut ensure_dirs,
            );
        } else if let Some(dir) = account::effective_config_dir(p) {
            // No thegn-managed account, but the agent still writes runtime
            // state (session-env, todos, shell snapshots) into its inherited
            // config dir. Under a read-only $HOME that fails EROFS, so carve it
            // read-write path-preserving — parity with the managed path above.
            fold_cred_dir(
                p.home_env,
                &dir,
                &mut overrides,
                &mut mounts,
                &mut ensure_dirs,
            );
        }
    }

    // Opt-in `.env` (lowest precedence): only when an active bundle set
    // `dotenv = true`, the worktree `.env` has been allow-listed by content
    // hash, and per-key it (a) is not credential-shaped and (b) does not
    // override a bundle-set value. Fills gaps only.
    if order
        .iter()
        .any(|n| !denied.iter().any(|(d, _)| d == n) && cfg.bundle.get(n).is_some_and(|b| b.dotenv))
    {
        fold_dotenv(db, worktree, &mut overrides);
    }

    ResolvedEnv {
        overrides: overrides.into_iter().collect(),
        block: Vec::new(),
        mounts,
        ensure_dirs,
        denied,
        authorized,
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
    // A bundle-referenced identity folds first, so the bundle's own explicit
    // `env`/`config_dirs`/`accounts` below override the named identity per key.
    if !b.identity.is_empty() {
        fold_identity(cfg, db, &b.identity, overrides, mounts, ensure_dirs);
    }
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
            match managed_home_checked(cfg, name) {
                Some(home) => home.to_string_lossy().into_owned(),
                None => {
                    crate::msg::warn(&format!(
                        "bundle: managed HOME for {name:?} is ambiguous or invalid; skipped"
                    ));
                    String::new()
                }
            }
        } else {
            util::expand_tilde(&b.home)
        };
        if !home.is_empty() {
            fold_cred_dir("HOME", &home, overrides, mounts, ensure_dirs);
        }
    }
}

/// Fold a named [`crate::config::IdentityConfig`] into the accumulators. Each
/// per-tool binding it sets overrides the matching credential env var and
/// path-preservingly mounts the dir/file when it already exists; tools it leaves
/// unset are untouched, so a less specific layer (or the profile-root fallback
/// the pane inherits) fills them — the per-tool mix-and-match. Agent `accounts`
/// fold exactly like a bundle's. An unknown identity name warns and is skipped.
fn fold_identity(
    cfg: &Config,
    db: &Db,
    name: &str,
    overrides: &mut BTreeMap<String, String>,
    mounts: &mut Vec<Mount>,
    ensure_dirs: &mut Vec<String>,
) {
    let Some(id) = crate::identity::resolve(cfg, name) else {
        crate::msg::warn(&format!("identity: unknown identity {name:?} (ignored)"));
        return;
    };
    let r = crate::identity::resolved(id);
    if let Some(p) = &r.git_config {
        push_mount_if_exists(mounts, p);
        overrides.insert("GIT_CONFIG_GLOBAL".to_string(), p.clone());
    }
    if let Some(cmd) = r.git_ssh_command() {
        if let Some(key) = &r.git_ssh_key {
            push_mount_if_exists(mounts, key);
        }
        overrides.insert("GIT_SSH_COMMAND".to_string(), cmd);
    }
    if let Some(p) = &r.gh_config {
        push_mount_if_exists(mounts, p);
        overrides.insert("GH_CONFIG_DIR".to_string(), p.clone());
    }
    if let Some(p) = &r.gpg_home {
        push_mount_if_exists(mounts, p);
        overrides.insert("GNUPGHOME".to_string(), p.clone());
    }
    // Per-provider agent accounts (`accounts = { claude = "washu" }`) resolve
    // through `account.rs` to the provider home env var + path-preserving mount,
    // identically to a bundle's `accounts`.
    for (provider, acct) in &id.accounts {
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
}

#[derive(Debug)]
pub enum DotfilesError {
    SourceNotDirectory {
        path: std::path::PathBuf,
    },
    HomeUnusable {
        path: std::path::PathBuf,
        error: std::io::Error,
    },
    SignatureNotRegular {
        path: std::path::PathBuf,
    },
    Sweep(std::io::Error),
    ReadSource(std::io::Error),
    Stage(String),
    Publish(String),
}

impl std::fmt::Display for DotfilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceNotDirectory { path } => write!(
                f,
                "bundle: dotfiles source {} is not a directory; skipping",
                path.display()
            ),
            Self::HomeUnusable { path, error } => {
                write!(
                    f,
                    "bundle: cannot create managed HOME {}: {error}",
                    path.display()
                )
            }
            Self::SignatureNotRegular { path } => write!(
                f,
                "bundle: dotfiles signature {} is not a regular file",
                path.display()
            ),
            Self::Sweep(error) => {
                write!(f, "bundle: cannot sweep dotfile staging residue: {error}")
            }
            Self::ReadSource(error) => write!(f, "bundle: cannot read dotfiles source: {error}"),
            Self::Stage(detail) => write!(f, "bundle: dotfiles staging failed: {detail}"),
            Self::Publish(detail) => write!(f, "bundle: dotfiles publish failed: {detail}"),
        }
    }
}

impl std::error::Error for DotfilesError {}

/// Materialize a bundle's Tier-2 dotfiles into its managed HOME, idempotently.
/// I/O — call off the event loop (launch time). Symlinks (or copies, for
/// `template` mode) each top-level entry of `source` into `dest_home`; a
/// `meta.json` records the source content-signature so an unchanged source is a
/// no-op. A caller must refuse a configured agent/tool launch if this returns
/// an error.
pub fn materialize_dotfiles(
    spec: &crate::config::DotfilesSpec,
    dest_home: &std::path::Path,
    residue_owner_alive: &dyn Fn(u32) -> bool,
) -> Result<usize, DotfilesError> {
    use crate::config::DotfileMode;
    let source = std::path::PathBuf::from(util::expand_tilde(&spec.source));
    if !source.is_dir() {
        return Err(DotfilesError::SourceNotDirectory { path: source });
    }
    ensure_managed_home_directories(dest_home).map_err(|error| DotfilesError::HomeUnusable {
        path: dest_home.to_path_buf(),
        error,
    })?;
    // Idempotency: skip when the source signature (entry names + mtimes) is
    // unchanged since the last materialization.
    let sig = dotfiles_signature(&source, spec.mode);
    let meta = dest_home.join(".thegn-dotfiles.json");
    if let Ok(metadata) = std::fs::symlink_metadata(&meta)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(DotfilesError::SignatureNotRegular { path: meta });
    }
    sweep_dotfile_residue(dest_home, residue_owner_alive).map_err(DotfilesError::Sweep)?;
    if std::fs::read_to_string(&meta).ok().as_deref() == Some(sig.as_str()) {
        return Ok(0);
    }
    let generation = dotfile_generation();
    let mut staged = Vec::new();
    let mut first_error = None;
    let mut budget = CopyBudget { entries: 0 };
    let entries = match std::fs::read_dir(&source) {
        Ok(e) => e,
        Err(e) => return Err(DotfilesError::ReadSource(e)),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                first_error = Some(format!("cannot read dotfiles source entry: {error}"));
                break;
            }
        };
        let from = entry.path();
        let Some(base) = from.file_name() else {
            continue;
        };
        let source_name = base.to_string_lossy();
        if source_name.starts_with(".thegn-stage-") || source_name.starts_with(".thegn-old-") {
            first_error = Some(format!(
                "refusing reserved dotfile source name {}",
                from.display()
            ));
            break;
        }
        let to = dest_home.join(base);
        let stage = dest_home.join(format!(
            ".thegn-stage-{generation}-{}",
            base.to_string_lossy()
        ));
        if spec.mode == DotfileMode::Template {
            match std::fs::symlink_metadata(&from) {
                Ok(metadata)
                    if metadata.file_type().is_symlink()
                        || (!metadata.is_file() && !metadata.is_dir()) =>
                {
                    first_error = Some(format!(
                        "refusing unsupported template entry {}",
                        from.display()
                    ));
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    first_error = Some(format!(
                        "cannot inspect template entry {}: {error}",
                        from.display()
                    ));
                    break;
                }
            }
        }
        staged.push((stage.clone(), to, from.clone()));
        let res = match spec.mode {
            DotfileMode::Symlink => symlink(&from, &stage),
            DotfileMode::Template => copy_tree_bounded(&from, &stage, 0, &mut budget),
        };
        if let Err(error) = res {
            first_error = Some(format!("dotfile {}: {error}", from.display()));
            break;
        }
    }
    if let Some(error) = first_error.take() {
        remove_dotfile_stages(&staged);
        return Err(DotfilesError::Stage(error));
    }
    let mut published = 0;
    for (stage, target, from) in &staged {
        let Some(target_name) = target.file_name() else {
            first_error = Some(format!("dotfile target has no name: {}", target.display()));
            break;
        };
        let old = dest_home.join(format!(
            ".thegn-old-{generation}-{}",
            target_name.to_string_lossy()
        ));
        let stage_is_directory = match std::fs::symlink_metadata(stage) {
            Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
            Err(error) => {
                first_error = Some(format!(
                    "cannot inspect staged dotfile {}: {error}",
                    stage.display()
                ));
                break;
            }
        };
        let old_target_moved = match (stage_is_directory, std::fs::symlink_metadata(target)) {
            (true, Ok(_)) => {
                if let Err(error) = std::fs::rename(target, &old) {
                    first_error = Some(format!(
                        "cannot move dotfile target {} aside: {error}",
                        target.display()
                    ));
                    break;
                }
                true
            }
            (_, Ok(_)) => false,
            (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
            (_, Err(error)) => {
                first_error = Some(format!(
                    "cannot inspect dotfile target {}: {error}",
                    target.display()
                ));
                break;
            }
        };
        if let Err(error) = std::fs::rename(stage, target) {
            let mut publish_error = format!("cannot publish dotfile {}: {error}", from.display());
            if old_target_moved && let Err(restore_error) = std::fs::rename(&old, target) {
                tracing::warn!(target: "thegn::bundle", %restore_error, path = %target.display(), old_path = %old.display(), "could not restore dotfile target after publish failure");
                publish_error.push_str(&format!(
                    "; also could not restore previous target {} from {}: {restore_error}",
                    target.display(),
                    old.display()
                ));
            }
            first_error = Some(publish_error);
            break;
        }
        published += 1;
        if old_target_moved && let Err(error) = remove_path_nofollow(&old) {
            tracing::debug!(target: "thegn::bundle", %error, path = %old.display(), "could not remove replaced dotfile directory");
        }
    }
    if let Some(error) = first_error.take() {
        remove_dotfile_stages(&staged);
        return Err(DotfilesError::Publish(error));
    }
    if let Err(error) = std::fs::write(&meta, sig) {
        tracing::debug!(target: "thegn::bundle", %error, "could not write dotfiles signature");
    }
    Ok(published)
}

fn dotfile_generation() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
    )
}

fn remove_path_nofollow(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
        }
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn sweep_dotfile_residue(
    home: &std::path::Path,
    residue_owner_alive: &dyn Fn(u32) -> bool,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(home)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let generation = name
            .strip_prefix(".thegn-stage-")
            .or_else(|| name.strip_prefix(".thegn-old-"));
        if let Some(generation) = generation {
            let owner_pid = generation
                .split_once('-')
                .and_then(|(pid, _)| u32::from_str_radix(pid, 16).ok());
            if owner_pid.is_some_and(residue_owner_alive) {
                continue;
            }
            remove_path_nofollow(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_dotfile_stages(staged: &[(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)]) {
    for (stage, _, _) in staged {
        if let Err(error) = remove_path_nofollow(stage) {
            tracing::debug!(target: "thegn::bundle", %error, path = %stage.display(), "could not remove dotfile staging entry");
        }
    }
}

const MAX_DOTFILE_DEPTH: usize = 32;
const MAX_DOTFILE_ENTRIES: usize = 10_000;

fn ensure_managed_home_directories(path: &std::path::Path) -> std::io::Result<()> {
    let slug = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed HOME has no bundle dir",
        )
    })?;
    let bundles = slug.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed HOME has no bundles dir",
        )
    })?;
    std::fs::create_dir_all(bundles)?;
    ensure_directory_nofollow(slug)?;
    ensure_directory_nofollow(path)
}

fn ensure_directory_nofollow(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory path is a non-directory or symlink",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(path),
        Err(error) => Err(error),
    }
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
    copy_tree_bounded(from, to, 0, &mut CopyBudget { entries: 0 })
}

struct CopyBudget {
    entries: usize,
}

fn copy_tree_bounded(
    from: &std::path::Path,
    to: &std::path::Path,
    depth: usize,
    budget: &mut CopyBudget,
) -> std::io::Result<()> {
    if depth > MAX_DOTFILE_DEPTH || budget.entries >= MAX_DOTFILE_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "dotfile template exceeds traversal bounds",
        ));
    }
    budget.entries += 1;
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "template symlinks are not followed",
        ));
    }
    if metadata.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree_bounded(
                &entry.path(),
                &to.join(entry.file_name()),
                depth + 1,
                budget,
            )?;
        }
        Ok(())
    } else if metadata.is_file() {
        std::fs::copy(from, to).map(|_| ())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsupported template entry type",
        ))
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

    #[test]
    fn refused_overlay_drops_global_and_workspace_bundles() {
        // THE-515: a shared `foo` key refuses the trusted env bundle, and the
        // global binding must not stand in for it.
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        for name in ["a/foo", "b/foo"] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.join(".git")).unwrap();
            db.slug_for_repo(&path.to_string_lossy(), "foo").unwrap();
        }
        let mut cfg = Config::default();
        cfg.workspace.entry("foo".into()).or_default().env_bundle = Some("work".into());
        set_active(&db, Bind::Global, "/wt", Some("foo"), "global").unwrap();
        assert_eq!(active_name(&cfg, &db, "/wt", Some("foo")), None);
        assert!(active_chain(&cfg, &db, "/wt", Some("foo")).is_empty());
        set_active(&db, Bind::Worktree, "/wt", Some("foo"), "pinned").unwrap();
        assert_eq!(
            active_name(&cfg, &db, "/wt", Some("foo")).as_deref(),
            Some("pinned")
        );
    }

    #[test]
    fn secret_resolver_timeout_kills_a_hung_backend() {
        // D4: `run_resolver` bounds the secret-backend subprocess via
        // `output_with_timeout` so a wedged resolver can't freeze the loop. A
        // command that outlives the deadline is killed → graceful `None`; a fast
        // one returns its stdout.
        use std::time::Duration;
        let hung = ["sh".to_string(), "-c".to_string(), "sleep 5".to_string()];
        assert!(crate::sandbox::output_with_timeout(&hung, Duration::from_millis(150)).is_none());
        let fast = ["sh".to_string(), "-c".to_string(), "printf hi".to_string()];
        let (ok, out) = crate::sandbox::output_with_timeout(&fast, Duration::from_secs(5)).unwrap();
        assert!(ok);
        assert_eq!(out, "hi");
    }

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
        assert!(matches!(
            materialize_dotfiles(&missing, std::path::Path::new("/tmp/tg-none"), &|_| false),
            Err(DotfilesError::SourceNotDirectory { .. })
        ));

        // Template mode copies a nested tree into the managed HOME.
        let root = std::env::temp_dir().join(format!("tg-tmpl-{}", util::now()));
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("nested").join("f"), b"hi").unwrap();
        let dest = root.join("home");
        let spec = DotfilesSpec {
            source: src.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };
        assert_eq!(materialize_dotfiles(&spec, &dest, &|_| false).unwrap(), 1);
        assert_eq!(std::fs::read(dest.join("nested").join("f")).unwrap(), b"hi");
        std::fs::remove_dir_all(&root).ok(); // best-effort: test cleanup: scratch removal must never fail the test
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
        db.slug_for_repo("/src/repo", "repo").unwrap();
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
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            drawer_scope: None,
            drawer_cwd: None,
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
    fn unmanaged_agent_carves_inherited_config_dir_writable() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.agents.push(NamedCommand {
            name: "claude".into(),
            command: "claude".into(),
            hints: vec![],
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            drawer_scope: None,
            drawer_cwd: None,
        });
        // No `[[accounts]]` for claude and no active pointer: thegn doesn't
        // manage it. The agent's inherited config dir (existing on disk) must
        // still be folded read-write so it can write session-env etc. under a
        // read-only $HOME.
        let dir = std::env::temp_dir().join(format!("tg-unmanaged-cfg-{}", crate::util::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_s = dir.to_string_lossy().into_owned();
        // SAFETY: single-threaded test setup; CLAUDE_CONFIG_DIR is only read by
        // `account::effective_config_dir`, reached solely on this unmanaged path.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir_s) };

        let r = compose(&cfg, &db, "/wt", None, Some("claude"));

        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        std::fs::remove_dir_all(&dir).ok(); // best-effort: test cleanup: scratch removal must never fail the test

        assert_eq!(get(&r.overrides, "CLAUDE_CONFIG_DIR"), Some(dir_s.as_str()));
        assert!(
            r.mounts.iter().any(|m| m.dest == dir_s && !m.ro),
            "config dir must be a read-write path-preserving overmount"
        );
        assert!(r.ensure_dirs.contains(&dir_s));
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
        let b = Bundle {
            home: "managed".into(),
            ..Bundle::default()
        };
        cfg.bundle.insert("work".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "work").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        let home = managed_home("work").to_string_lossy().into_owned();
        assert_eq!(get(&r.overrides, "HOME"), Some(home.as_str()));
        assert!(r.ensure_dirs.iter().any(|d| d == &home));
        assert!(r.mounts.iter().any(|m| m.dest == home));
    }

    #[test]
    fn ambiguous_managed_home_aliases_are_not_folded() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        let spec = crate::config::DotfilesSpec {
            source: "/templates".into(),
            ..Default::default()
        };
        for name in ["work/a", "work-a"] {
            cfg.bundle.insert(
                name.into(),
                Bundle {
                    home: "managed".into(),
                    dotfiles: Some(spec.clone()),
                    ..Bundle::default()
                },
            );
        }
        set_active(&db, Bind::Global, "/wt", None, "work/a").unwrap();
        let resolved = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&resolved.overrides, "HOME"), None);
        assert!(resolved.ensure_dirs.is_empty());
        assert!(managed_home_checked(&cfg, "work/a").is_none());
        assert!(dotfile_targets(&cfg, &resolved).is_empty());

        let mut punctuation = Config::default();
        punctuation.bundle.insert("!!!".into(), Bundle::default());
        assert!(managed_home_checked(&punctuation, "!!!").is_none());
        assert!(managed_home_checked(&Config::default(), &"a".repeat(300)).is_none());
        assert!(managed_home_checked(&Config::default(), "work").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn materialize_refuses_symlinked_home_without_touching_target() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let outside = root.path().join("outside");
        let home = root.path().join("home");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(source.join(".bashrc"), "test").unwrap();
        symlink(&outside, &home).unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::HomeUnusable { .. })
        ));
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn materialize_allows_symlinked_ancestor_above_bundles() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let link = root.path().join("link");
        let source = root.path().join("source");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join(".bashrc"), "test").unwrap();
        symlink(&real, &link).unwrap();
        let home = link.join("bundles").join("work").join("home");
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(real.join("bundles/work/home/.bashrc")).unwrap(),
            "test"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_accepts_symlinked_configured_source_root() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let source_link = root.path().join("dotfiles");
        let home = root.path().join("bundles/work/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join(".bashrc"), "test").unwrap();
        symlink(&source, &source_link).unwrap();
        let spec = DotfilesSpec {
            source: source_link.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(home.join(".bashrc")).unwrap(),
            "test"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_refuses_signature_symlink_without_touching_target() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundles/work/home");
        let target = root.path().join("signature-target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(source.join(".bashrc"), "test").unwrap();
        std::fs::write(&target, "keep").unwrap();
        symlink(&target, &home.join(".thegn-dotfiles.json")).unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::SignatureNotRegular { .. })
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert!(!home.join(".bashrc").exists());
    }

    #[test]
    fn materialize_refuses_regular_file_bundle_component() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let bundles = root.path().join("bundles");
        let slug = bundles.join("work");
        let home = slug.join("home");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        std::fs::write(source.join(".bashrc"), "test").unwrap();
        std::fs::write(&slug, "keep").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::HomeUnusable { .. })
        ));
        assert_eq!(std::fs::read_to_string(&slug).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn template_copy_skips_symlinks_and_does_not_cache_partial_success() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let outside = root.path().join("outside");
        let home = root.path().join("home");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(source.join(".bashrc"), "export X=1\n").unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(&outside, &source.join("link")).unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
        assert!(!home.join(".bashrc").exists());
        assert!(!home.join("link/secret.txt").exists());
        assert!(!home.join(".thegn-dotfiles.json").exists());
        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_template_entry_preserves_existing_target() {
        use crate::config::{DotfileMode, DotfilesSpec};
        use std::os::unix::net::UnixListener;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("home");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(source.join(".bashrc"), "new").unwrap();
        std::fs::write(home.join("unsupported"), "keep").unwrap();
        let listener = UnixListener::bind(source.join("unsupported")).unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
        assert_eq!(
            std::fs::read_to_string(home.join("unsupported")).unwrap(),
            "keep"
        );
        assert!(!home.join(".thegn-dotfiles.json").exists());
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_template_symlink_does_not_write_signature() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join(".bashrc"), "export X=1\n").unwrap();
        symlink(&root.path().join("missing"), &source.join("dangling")).unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
        assert!(!home.join(".bashrc").exists());
        assert!(!home.join(".thegn-dotfiles.json").exists());
        assert!(std::fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".thegn-stage-")
        }));
        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
        assert!(!home.join(".bashrc").exists());
        assert!(!home.join(".thegn-dotfiles.json").exists());
        assert!(std::fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".thegn-stage-")
        }));
    }

    #[test]
    fn template_copy_enforces_depth_and_entry_limits() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        std::fs::write(&source, "content").unwrap();

        let mut full_budget = CopyBudget {
            entries: MAX_DOTFILE_ENTRIES,
        };
        assert!(copy_tree_bounded(&source, &dest, 0, &mut full_budget).is_err());

        let mut empty_budget = CopyBudget { entries: 0 };
        assert!(
            copy_tree_bounded(&source, &dest, MAX_DOTFILE_DEPTH + 1, &mut empty_budget).is_err()
        );
    }

    #[test]
    fn dotfile_targets_only_authorized_bundles_and_rejects_ambiguous_homes() {
        use crate::config::DotfilesSpec;
        let mut foreign = zoned_bundle("clientA", "A_KEY", "secret");
        foreign.dotfiles = Some(DotfilesSpec {
            source: "/foreign".into(),
            ..Default::default()
        });
        let mut shared = bundle_with_env(&[("EDITOR", "vim")]);
        shared.dotfiles = Some(DotfilesSpec {
            source: "/shared".into(),
            ..Default::default()
        });
        let (db, cfg) = zoned_setup(
            &[("foreign", foreign), ("shared", shared)],
            "/repo",
            "/repo/wt",
            Some("clientB"),
        );
        set_active(&db, Bind::Worktree, "/repo/wt", Some("repo"), "foreign").unwrap();
        set_active(&db, Bind::Global, "/repo/wt", Some("repo"), "shared").unwrap();
        let resolved = compose(&cfg, &db, "/repo/wt", Some("repo"), None);
        let targets = dotfile_targets(&cfg, &resolved);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0.source, "/shared");
        assert_eq!(resolved.authorized, vec!["shared"]);
    }

    #[test]
    fn materialize_dotfiles_symlinks_and_is_idempotent() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = std::env::temp_dir().join(format!("tg-dotfiles-{}", util::now()));
        let src = root.join("src");
        let dest = root.join("home");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".bashrc"), b"export X=1\n").unwrap();
        let spec = DotfilesSpec {
            source: src.to_string_lossy().into_owned(),
            mode: DotfileMode::Symlink,
        };
        // First run materializes one entry…
        assert_eq!(materialize_dotfiles(&spec, &dest, &|_| false).unwrap(), 1);
        assert!(dest.join(".bashrc").exists());
        // …the second is a no-op (unchanged signature).
        assert_eq!(materialize_dotfiles(&spec, &dest, &|_| false).unwrap(), 0);
        std::fs::remove_dir_all(&root).ok(); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn failed_staging_keeps_the_prior_home() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), "one").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };
        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        let signature = std::fs::read(home.join(".thegn-dotfiles.json")).unwrap();

        std::fs::write(source.join("a.txt"), "two").unwrap();
        let mut deep = source.join("deep");
        std::fs::create_dir(&deep).unwrap();
        for _ in 0..=MAX_DOTFILE_DEPTH {
            deep = deep.join("nested");
            std::fs::create_dir(&deep).unwrap();
        }

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
        assert_eq!(std::fs::read_to_string(home.join("a.txt")).unwrap(), "one");
        assert_eq!(
            std::fs::read(home.join(".thegn-dotfiles.json")).unwrap(),
            signature
        );
        assert!(std::fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".thegn-stage-")
        }));
    }

    #[test]
    fn publish_replaces_entries_and_keeps_runtime_state() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), "one").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };
        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        std::fs::write(home.join(".bash_history"), "runtime").unwrap();
        std::fs::write(source.join("a.txt"), "two").unwrap();
        let modified = std::fs::File::open(source.join("a.txt")).unwrap();
        modified
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3))
            .unwrap();

        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(home.join("a.txt")).unwrap(), "two");
        assert_eq!(
            std::fs::read_to_string(home.join(".bash_history")).unwrap(),
            "runtime"
        );
        assert!(home.join(".thegn-dotfiles.json").is_file());
    }

    #[test]
    fn stale_stage_residue_is_swept() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), "value").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };
        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        std::fs::write(home.join(".thegn-stage-x-a.txt"), "residue").unwrap();

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Ok(0)
        ));
        assert!(!home.join(".thegn-stage-x-a.txt").exists());
    }

    #[test]
    fn directory_target_is_replaced_without_a_gap() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir_all(source.join("cfg")).unwrap();
        std::fs::write(source.join("cfg/new"), "new").unwrap();
        std::fs::create_dir_all(home.join("cfg")).unwrap();
        std::fs::write(home.join("cfg/old"), "old").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 1);
        assert!(!home.join("cfg/old").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("cfg/new")).unwrap(),
            "new"
        );
        assert!(std::fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".thegn-old-")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn directory_stage_replaces_a_file_or_symlink_target() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(source.join("from_file")).unwrap();
        std::fs::create_dir_all(source.join("from_link")).unwrap();
        std::fs::write(source.join("from_file/new"), "from file target").unwrap();
        std::fs::write(source.join("from_link/new"), "from symlink target").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(home.join("from_file"), "old file").unwrap();
        std::fs::write(outside.join("keep"), "keep").unwrap();
        symlink(&outside, &home.join("from_link")).unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(home.join("from_file/new")).unwrap(),
            "from file target"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("from_link/new")).unwrap(),
            "from symlink target"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "keep"
        );
        assert!(std::fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".thegn-old-")
        }));
    }

    #[test]
    fn reserved_dotfile_source_names_fail_staging() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join(".thegn-stage-config"), "reserved").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Stage(_))
        ));
        assert!(!home.join(".thegn-stage-config").exists());
        assert!(!home.join(".thegn-dotfiles.json").exists());
        assert!(std::fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".thegn-stage-")
        }));
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
        let dir = std::env::temp_dir().join(format!("tg-dotenv-{}", util::now()));
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
        std::fs::remove_dir_all(&dir).ok(); // best-effort: test cleanup: scratch removal must never fail the test
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

    // --- zone sub-vault: a zone-owned bundle only composes inside its zone ----

    fn zoned_bundle(zone: &str, k: &str, v: &str) -> Bundle {
        let mut b = bundle_with_env(&[(k, v)]);
        b.zone = zone.to_string();
        b
    }

    /// Put `repo` in `zone`, register a worktree under it, return `(db, cfg)`.
    fn zoned_setup(
        bundles: &[(&str, Bundle)],
        repo: &str,
        wt: &str,
        zone: Option<&str>,
    ) -> (Db, Config) {
        use crate::store::{WorkspaceStore, ZoneStore};
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        for (n, b) in bundles {
            cfg.bundle.insert((*n).to_string(), b.clone());
        }
        db.put_workspace(repo, "ws", "repo").unwrap();
        db.put_worktree("t", repo, wt, "main", None, None).unwrap();
        if let Some(z) = zone {
            let id = db.create_zone(z, 1).unwrap();
            db.assign_workspace_zone(repo, Some(id)).unwrap();
        }
        (db, cfg)
    }

    #[test]
    fn zone_owned_bundle_denied_to_foreign_worktree() {
        // A bundle owned by clientA, a worktree in clientB (direct binding).
        let (db, cfg) = zoned_setup(
            &[("a-secrets", zoned_bundle("clientA", "A_KEY", "secret"))],
            "/repo",
            "/repo/wt",
            Some("clientB"),
        );
        set_active(&db, Bind::Worktree, "/repo/wt", Some("repo"), "a-secrets").unwrap();
        let r = compose(&cfg, &db, "/repo/wt", Some("repo"), None);
        assert!(
            get(&r.overrides, "A_KEY").is_none(),
            "foreign zone bundle skipped"
        );
        assert_eq!(
            r.denied,
            vec![("a-secrets".to_string(), "clientA".to_string())]
        );
    }

    #[test]
    fn zone_owned_bundle_composes_in_own_zone() {
        let (db, cfg) = zoned_setup(
            &[("a-secrets", zoned_bundle("clientA", "A_KEY", "secret"))],
            "/repo",
            "/repo/wt",
            Some("clientA"),
        );
        set_active(&db, Bind::Worktree, "/repo/wt", Some("repo"), "a-secrets").unwrap();
        let r = compose(&cfg, &db, "/repo/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "A_KEY"), Some("secret"));
        assert!(r.denied.is_empty());
    }

    #[test]
    fn unzoned_worktree_denied_zoned_bundle() {
        let (db, cfg) = zoned_setup(
            &[("a-secrets", zoned_bundle("clientA", "A_KEY", "secret"))],
            "/repo",
            "/repo/wt",
            None, // unzoned
        );
        set_active(&db, Bind::Worktree, "/repo/wt", Some("repo"), "a-secrets").unwrap();
        let r = compose(&cfg, &db, "/repo/wt", Some("repo"), None);
        assert!(get(&r.overrides, "A_KEY").is_none());
        assert_eq!(r.denied.len(), 1);
    }

    #[test]
    fn global_bundle_composes_for_zoned_worktree() {
        // A global (unzoned) bundle stays usable inside a zone.
        let (db, cfg) = zoned_setup(
            &[("shared", bundle_with_env(&[("EDITOR", "vim")]))],
            "/repo",
            "/repo/wt",
            Some("clientA"),
        );
        set_active(&db, Bind::Worktree, "/repo/wt", Some("repo"), "shared").unwrap();
        let r = compose(&cfg, &db, "/repo/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "EDITOR"), Some("vim"));
        assert!(r.denied.is_empty());
    }

    #[test]
    fn zone_deny_covers_extends_reachability() {
        // A visible bundle `extends` a foreign zone-owned one → the parent is
        // denied at fold time (extends expands the chain, deny catches it).
        let mut child = bundle_with_env(&[("CHILD", "1")]);
        child.extends = vec!["a-secrets".into()];
        let mut parent = zoned_bundle("clientA", "A_KEY", "secret");
        parent.dotfiles = Some(crate::config::DotfilesSpec {
            source: "/foreign-parent".into(),
            ..Default::default()
        });
        let (db, cfg) = zoned_setup(
            &[("a-secrets", parent), ("child", child)],
            "/repo",
            "/repo/wt",
            Some("clientB"),
        );
        set_active(&db, Bind::Worktree, "/repo/wt", Some("repo"), "child").unwrap();
        let r = compose(&cfg, &db, "/repo/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "CHILD"), Some("1"), "visible child folds");
        assert!(
            get(&r.overrides, "A_KEY").is_none(),
            "foreign parent denied"
        );
        assert!(r.denied.iter().any(|(n, _)| n == "a-secrets"));
        assert!(!r.authorized.iter().any(|n| n == "a-secrets"));
        assert!(dotfile_targets(&cfg, &r).is_empty());
    }

    // --- decoupled identities: per-tool credential resolution ----------------

    fn ident(
        git_config: &str,
        ssh_key: &str,
        gh_config: &str,
        gpg_home: &str,
    ) -> crate::config::IdentityConfig {
        use crate::config::{IdentityConfig, IdentityGh, IdentityGit, IdentityGpg};
        IdentityConfig {
            git: IdentityGit {
                config: git_config.into(),
                ssh_key: ssh_key.into(),
            },
            gh: IdentityGh {
                config: gh_config.into(),
            },
            gpg: IdentityGpg {
                home: gpg_home.into(),
            },
            signing: Default::default(),
            accounts: Default::default(),
        }
    }

    #[test]
    fn bundle_identity_folds_per_tool_credentials_and_leaks_no_token() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.identities.insert(
            "washu".into(),
            ident("/id/git", "/id/key", "/id/gh", "/id/gpg"),
        );
        cfg.bundle.insert(
            "w".into(),
            Bundle {
                identity: "washu".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Global, "/wt", None, "w").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&r.overrides, "GIT_CONFIG_GLOBAL"), Some("/id/git"));
        assert_eq!(get(&r.overrides, "GH_CONFIG_DIR"), Some("/id/gh"));
        assert_eq!(get(&r.overrides, "GNUPGHOME"), Some("/id/gpg"));
        assert_eq!(
            get(&r.overrides, "GIT_SSH_COMMAND"),
            Some("ssh -i /id/key -o IdentitiesOnly=yes")
        );
        // The firewall invariant: an identity never sets a forge token.
        assert_eq!(get(&r.overrides, "GH_TOKEN"), None);
        assert_eq!(get(&r.overrides, "GITHUB_TOKEN"), None);
    }

    #[test]
    fn bundle_explicit_config_dir_overrides_referenced_identity() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.identities
            .insert("washu".into(), ident("/id/git", "", "", ""));
        let mut b = Bundle {
            identity: "washu".into(),
            ..Default::default()
        };
        b.config_dirs
            .insert("GIT_CONFIG_GLOBAL".into(), "/explicit/git".into());
        cfg.bundle.insert("w".into(), b);
        set_active(&db, Bind::Global, "/wt", None, "w").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(
            get(&r.overrides, "GIT_CONFIG_GLOBAL"),
            Some("/explicit/git"),
            "the bundle's explicit config_dirs override its referenced identity"
        );
    }

    #[test]
    fn profile_base_identity_folds_with_bundle_mix_and_fallback() {
        // The headline "mix and match": the active profile resolves git+ssh from
        // its base identity, a worktree bundle supplies gh from a *different*
        // identity, and gpg (set by neither) is left to fall back.
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        // profile::name() is "default" in tests (no THEGN_PROFILE).
        cfg.profiles.insert(
            "default".into(),
            crate::config::ProfileConfig {
                identity: "base".into(),
                ..Default::default()
            },
        );
        cfg.identities
            .insert("base".into(), ident("/base/git", "/base/key", "", ""));
        cfg.identities
            .insert("gh-personal".into(), ident("", "", "/gh/personal", ""));
        cfg.bundle.insert(
            "b".into(),
            Bundle {
                identity: "gh-personal".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "b").unwrap();
        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        assert_eq!(
            get(&r.overrides, "GIT_CONFIG_GLOBAL"),
            Some("/base/git"),
            "git comes from the profile-base identity"
        );
        assert_eq!(
            get(&r.overrides, "GIT_SSH_COMMAND"),
            Some("ssh -i /base/key -o IdentitiesOnly=yes")
        );
        assert_eq!(
            get(&r.overrides, "GH_CONFIG_DIR"),
            Some("/gh/personal"),
            "gh comes from the bundle-referenced identity"
        );
        assert_eq!(
            get(&r.overrides, "GNUPGHOME"),
            None,
            "gpg set by neither ⇒ not overridden (falls back to profile-root)"
        );
    }

    #[test]
    fn bundle_identity_overrides_profile_base_per_tool() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "default".into(),
            crate::config::ProfileConfig {
                identity: "base".into(),
                ..Default::default()
            },
        );
        cfg.identities
            .insert("base".into(), ident("/base/git", "", "/base/gh", ""));
        // The worktree bundle's identity overrides git only; gh falls through to
        // the profile base.
        cfg.identities
            .insert("wt".into(), ident("/wt/git", "", "", ""));
        cfg.bundle.insert(
            "b".into(),
            Bundle {
                identity: "wt".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "b").unwrap();
        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "GIT_CONFIG_GLOBAL"), Some("/wt/git"));
        assert_eq!(get(&r.overrides, "GH_CONFIG_DIR"), Some("/base/gh"));
    }

    #[test]
    fn identity_accounts_fold_to_provider_home() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.accounts.push(Account {
            name: "washu".into(),
            provider: "claude".into(),
            dir: Some("/creds/claude-washu".into()),
        });
        let mut idc = crate::config::IdentityConfig::default();
        idc.accounts.insert("claude".into(), "washu".into());
        cfg.identities.insert("washu".into(), idc);
        cfg.bundle.insert(
            "b".into(),
            Bundle {
                identity: "washu".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Global, "/wt", None, "b").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(
            get(&r.overrides, "CLAUDE_CONFIG_DIR"),
            Some("/creds/claude-washu")
        );
    }

    #[test]
    fn identity_dir_that_exists_is_mounted_path_preserving() {
        let db = Db::open_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("tg-id-gh-{}", util::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_s = dir.to_string_lossy().into_owned();
        let mut cfg = Config::default();
        cfg.identities
            .insert("washu".into(), ident("", "", &dir_s, ""));
        cfg.bundle.insert(
            "b".into(),
            Bundle {
                identity: "washu".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Global, "/wt", None, "b").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert_eq!(get(&r.overrides, "GH_CONFIG_DIR"), Some(dir_s.as_str()));
        assert!(
            r.mounts.iter().any(|m| m.dest == dir_s && !m.ro),
            "an existing identity dir is mounted read-write path-preserving"
        );
        std::fs::remove_dir_all(&dir).ok(); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn directly_bound_identity_overrides_bundle_referenced_one() {
        // The identity switcher pins "switched" at the worktree; a bound bundle
        // references "bundle-id". The explicit switch wins.
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.identities
            .insert("bundle-id".into(), ident("/b/git", "", "", ""));
        cfg.identities
            .insert("switched".into(), ident("/s/git", "", "", ""));
        cfg.bundle.insert(
            "b".into(),
            Bundle {
                identity: "bundle-id".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Global, "/wt", Some("repo"), "b").unwrap();
        crate::identity::set_active(&db, Bind::Worktree, "/wt", Some("repo"), "switched").unwrap();
        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "GIT_CONFIG_GLOBAL"), Some("/s/git"));
    }

    #[test]
    fn bound_identity_worktree_beats_global() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.identities
            .insert("g".into(), ident("/g/git", "", "", ""));
        cfg.identities
            .insert("w".into(), ident("/w/git", "", "", ""));
        crate::identity::set_active(&db, Bind::Global, "/wt", Some("repo"), "g").unwrap();
        crate::identity::set_active(&db, Bind::Worktree, "/wt", Some("repo"), "w").unwrap();
        let r = compose(&cfg, &db, "/wt", Some("repo"), None);
        assert_eq!(get(&r.overrides, "GIT_CONFIG_GLOBAL"), Some("/w/git"));
    }

    #[test]
    fn unknown_identity_is_ignored_not_fatal() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        cfg.bundle.insert(
            "b".into(),
            Bundle {
                identity: "ghost".into(),
                ..Default::default()
            },
        );
        set_active(&db, Bind::Global, "/wt", None, "b").unwrap();
        let r = compose(&cfg, &db, "/wt", None, None);
        assert!(get(&r.overrides, "GIT_CONFIG_GLOBAL").is_none());
    }

    #[test]
    fn sweep_keeps_live_owner_residue_and_removes_dead_owner_residue() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let live = home.join(".thegn-stage-1a2b-0-0-.bashrc");
        let dead = home.join(".thegn-old-3c4d-0-0-.vimrc");
        let malformed = home.join(".thegn-stage-zz-junk");
        for path in [&live, &dead, &malformed] {
            std::fs::write(path, "residue").unwrap();
        }

        sweep_dotfile_residue(&home, &|pid| pid == 0x1a2b).unwrap();

        assert!(live.exists());
        assert!(!dead.exists());
        assert!(!malformed.exists());
    }

    #[test]
    fn materialize_reports_a_typed_error_for_a_non_directory_source() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source_file = root.path().join("source");
        std::fs::write(&source_file, "not a directory").unwrap();
        let spec = DotfilesSpec {
            source: source_file.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &root.path().join("home"), &|_| false),
            Err(DotfilesError::SourceNotDirectory { .. })
        ));
    }

    #[test]
    fn materialize_reports_publish_failure_instead_of_zero() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("entry"), "new file").unwrap();
        std::fs::create_dir_all(home.join("entry")).unwrap();
        std::fs::write(home.join("entry/old"), "preserve").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        assert!(matches!(
            materialize_dotfiles(&spec, &home, &|_| false),
            Err(DotfilesError::Publish(_))
        ));
        assert_eq!(
            std::fs::read_to_string(home.join("entry/old")).unwrap(),
            "preserve"
        );
        assert!(!home.join(".thegn-dotfiles.json").exists());
    }

    #[test]
    fn unchanged_source_is_ok_zero() {
        use crate::config::{DotfileMode, DotfilesSpec};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("bundle/home");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("entry"), "value").unwrap();
        let spec = DotfilesSpec {
            source: source.to_string_lossy().into_owned(),
            mode: DotfileMode::Template,
        };

        let first = materialize_dotfiles(&spec, &home, &|_| false).unwrap();
        assert!(first > 0);
        assert_eq!(materialize_dotfiles(&spec, &home, &|_| false).unwrap(), 0);
    }
}
