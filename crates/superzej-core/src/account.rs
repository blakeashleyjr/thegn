//! Client-side account switching for coding-agent CLIs (roadmap item 656).
//!
//! Codex and Claude Code each locate their *entire* credential home from a
//! single env var — Codex honors `CODEX_HOME` (default `~/.codex`) and Claude
//! Code honors `CLAUDE_CONFIG_DIR` (default `~/.claude`). superzej registers N
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
use crate::util;
use std::path::PathBuf;

/// A coding-agent CLI whose credentials live in a relocatable home directory.
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

/// The supported providers. Extend this table to add a new CLI.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "codex",
        home_env: "CODEX_HOME",
        default_dir: ".codex",
        login_argv: &["codex", "login"],
        auth_marker: "auth.json",
    },
    Provider {
        id: "claude",
        home_env: "CLAUDE_CONFIG_DIR",
        default_dir: ".claude",
        login_argv: &["claude"],
        auth_marker: ".credentials.json",
    },
];

/// Look up a provider by id.
pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Infer the provider from a command line's program basename
/// (`/usr/bin/codex --foo` → codex).
pub fn infer_provider(command: &str) -> Option<&'static Provider> {
    let prog = command.split_whitespace().next()?;
    let base = prog.rsplit('/').next().unwrap_or(prog);
    provider(base)
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

/// The managed credential-home dir for an account superzej owns
/// (`$XDG_STATE_HOME/superzej/accounts/<provider>/<slug>/`).
pub fn managed_dir(provider_id: &str, name: &str) -> PathBuf {
    util::xdg_state_home()
        .join("superzej")
        .join("accounts")
        .join(provider_id)
        .join(util::slugify(name))
}

/// One resolved account: where its creds live and whether it is logged in.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub name: String,
    pub provider: String,
    pub dir: PathBuf,
    /// `true` when superzej manages the dir (vs an adopted/config dir).
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
    if let Some(a) = cfg
        .accounts
        .iter()
        .find(|a| a.provider == provider_id && a.name == name)
    {
        return Some(match &a.dir {
            Some(d) => PathBuf::from(util::expand_tilde(d)),
            None => managed_dir(provider_id, name),
        });
    }
    db.account_dir(provider_id, name)
        .ok()
        .flatten()
        .map(PathBuf::from)
}

/// The full merged account list for a provider (config + DB-managed), for the
/// picker. Config entries take precedence on name collisions.
pub fn list(cfg: &Config, db: &Db, provider_id: &str) -> Vec<AccountInfo> {
    let mut out: Vec<AccountInfo> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for a in cfg.accounts.iter().filter(|a| a.provider == provider_id) {
        let dir = match &a.dir {
            Some(d) => PathBuf::from(util::expand_tilde(d)),
            None => managed_dir(provider_id, &a.name),
        };
        seen.insert(a.name.clone());
        out.push(AccountInfo {
            authed: marked_authed(provider_id, &dir),
            managed: a.dir.is_none(),
            name: a.name.clone(),
            provider: provider_id.to_string(),
            dir,
        });
    }
    for (name, dir, managed) in db.list_accounts(provider_id).unwrap_or_default() {
        if seen.contains(&name) {
            continue;
        }
        let dir = PathBuf::from(dir);
        out.push(AccountInfo {
            authed: marked_authed(provider_id, &dir),
            managed,
            name,
            provider: provider_id.to_string(),
            dir,
        });
    }
    out
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

/// The active account *name* for `provider`, by precedence. `None` ⇒ no
/// selection (the CLI uses its own default home).
pub fn active_name(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    slug: Option<&str>,
    provider_id: &str,
) -> Option<String> {
    if let Some(n) = db
        .get_ui_state(&scope_wt(provider_id, worktree), "active")
        .ok()
        .flatten()
    {
        return Some(n);
    }
    if let Some(slug) = slug {
        if let Some(n) = cfg
            .workspace
            .get(slug)
            .and_then(|w| w.accounts.get(provider_id))
        {
            return Some(n.clone());
        }
        if let Some(n) = db
            .get_ui_state(&scope_ws(provider_id, slug), "active")
            .ok()
            .flatten()
        {
            return Some(n);
        }
    }
    db.get_ui_state(&scope_global(provider_id), "active")
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
    let p = provider_for(cfg, choice)?;
    let name = active_name(cfg, db, worktree, slug, p.id)?;
    let dir = account_dir(cfg, db, p.id, &name)?;
    let _ = db.touch_account(p.id, &name, util::now());
    Some((p.home_env.to_string(), dir))
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
        assert_eq!(
            account_dir(&cfg, &db, "codex", "db-only"),
            Some(PathBuf::from("/var/creds"))
        );
    }

    #[test]
    fn active_name_precedence() {
        let db = Db::open_memory().unwrap();
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
    fn marked_authed_checks_marker_and_unknown_provider() {
        let tmp = std::env::temp_dir().join(format!("sz-acct-mark-{}", util::now()));
        std::fs::create_dir_all(&tmp).unwrap();
        // No marker yet → not authed.
        assert!(!marked_authed("claude", &tmp));
        // Drop the provider's marker → authed.
        std::fs::write(tmp.join(".credentials.json"), b"{}").unwrap();
        assert!(marked_authed("claude", &tmp));
        // Unknown provider has no marker concept → never authed.
        assert!(!marked_authed("nope", &tmp));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn list_merges_config_and_db_with_dedup_and_auth() {
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();

        // A config-adopted account whose dir contains the auth marker.
        let authed_dir = std::env::temp_dir().join(format!("sz-acct-list-{}", util::now()));
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

        std::fs::remove_dir_all(&authed_dir).ok();
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
    fn managed_dir_is_under_state_home() {
        let d = managed_dir("codex", "My Work!");
        assert!(
            d.starts_with(
                util::xdg_state_home()
                    .join("superzej")
                    .join("accounts")
                    .join("codex")
            )
        );
        // Name is slugified.
        assert!(d.ends_with("my-work"));
    }
}
