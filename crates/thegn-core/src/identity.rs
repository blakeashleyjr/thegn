//! Named, decoupled per-tool **identities** (roadmap H/AU) — the mix-and-match
//! credential primitive referenced by profiles and bundles.
//!
//! A profile (`[profiles.<p>] identity = "…"`) or a bundle (`[bundle.<n>]
//! identity = "…"`) names an identity; each tool it sets — git config, git SSH
//! key, `gh` config, GnuPG home, agent accounts — resolves **independently**, and
//! any tool it leaves unset falls through to a less specific scope and finally to
//! the profile-root default (which [`crate::profile::reroot`] pins into the
//! process env and the pane allowlist carries). The pure lookup + `~`-expanded
//! resolution live here; folding an identity into a pane's
//! [`crate::bundle::ResolvedEnv`] lives in [`crate::bundle`].

use crate::bundle::Bind;
use crate::config::{Config, IdentityConfig};
use crate::db::Db;
use crate::store::WorkspaceStore;
use crate::util;

/// Look up a named identity, or `None` if undefined (callers warn + skip).
pub fn resolve<'a>(cfg: &'a Config, name: &str) -> Option<&'a IdentityConfig> {
    cfg.identities.get(name)
}

// --- directly-bound identity (the identity switcher) -----------------------
//
// Separate from the `[profiles.<p>].identity` / `[bundle.<n>].identity` *config*
// references: the switcher pins an identity at a scope over the `ui_state` KV
// (worktree → workspace → global, most-specific wins), mirroring the bundle
// binding. `bundle::compose` folds these *after* the bundle chain, so an explicit
// switch wins over a bundle-referenced identity.

fn scope_global() -> String {
    "identity".to_string()
}
fn scope_ws(slug: &str) -> String {
    format!("identity:ws:{slug}")
}
fn scope_wt(worktree: &str) -> String {
    format!("identity:wt:{worktree}")
}

fn bound_global(db: &Db) -> Option<String> {
    db.get_ui_state(&scope_global(), "active").ok().flatten()
}
fn bound_ws(db: &Db, slug: &str) -> Option<String> {
    db.get_ui_state(&scope_ws(slug), "active").ok().flatten()
}
fn bound_wt(db: &Db, worktree: &str) -> Option<String> {
    db.get_ui_state(&scope_wt(worktree), "active")
        .ok()
        .flatten()
}

/// The single most-specific directly-bound identity (worktree → workspace →
/// global), for the switcher chip + display. `None` ⇒ none bound.
pub fn active_name(db: &Db, worktree: &str, slug: Option<&str>) -> Option<String> {
    bound_wt(db, worktree)
        .or_else(|| slug.and_then(|s| bound_ws(db, s)))
        .or_else(|| bound_global(db))
}

/// Directly-bound identities that apply to a scope, low→high (global, workspace,
/// worktree). `bundle::compose` folds these in order so the worktree binding
/// wins. Empty ⇒ none bound.
pub fn bound_in_order(db: &Db, worktree: &str, slug: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(g) = bound_global(db) {
        out.push(g);
    }
    if let Some(w) = slug.and_then(|s| bound_ws(db, s)) {
        out.push(w);
    }
    if let Some(t) = bound_wt(db, worktree) {
        out.push(t);
    }
    out
}

/// Bind `name` as the active identity at the given scope.
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

/// Clear the active-identity binding at the given scope.
pub fn clear_active(db: &Db, bind: Bind, worktree: &str, slug: Option<&str>) -> anyhow::Result<()> {
    let scope = match bind {
        Bind::Global => scope_global(),
        Bind::Workspace => scope_ws(slug.unwrap_or_default()),
        Bind::Worktree => scope_wt(worktree),
    };
    db.del_ui_state(&scope, "active")?;
    Ok(())
}

/// An identity's per-tool credential locations, `~` expanded. Each field is
/// `None` when the identity leaves that tool unset (mix-and-match).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// `GIT_CONFIG_GLOBAL` target.
    pub git_config: Option<String>,
    /// SSH private key feeding `GIT_SSH_COMMAND`.
    pub git_ssh_key: Option<String>,
    /// `GH_CONFIG_DIR` target.
    pub gh_config: Option<String>,
    /// `GNUPGHOME` target.
    pub gpg_home: Option<String>,
    /// Commit-signing binding: `(gpg.format, user.signingKey)`. `None` when the
    /// identity sets no signing key (falls through the scope chain / repo git
    /// config). The key is `~`-expanded only for the ssh format (a path);
    /// openpgp key ids are left verbatim.
    pub git_signing: Option<(String, String)>,
}

impl Resolved {
    /// The `GIT_SSH_COMMAND` value forcing this identity's key with
    /// `IdentitiesOnly=yes` (so ambient agent keys can't leak), or `None` when the
    /// identity sets no key.
    pub fn git_ssh_command(&self) -> Option<String> {
        self.git_ssh_key
            .as_ref()
            .map(|k| format!("ssh -i {k} -o IdentitiesOnly=yes"))
    }

    /// The `git -c …` overrides that bind this identity's signing key, if set:
    /// `["-c", "gpg.format=<fmt>", "-c", "user.signingKey=<key>"]`. Empty when
    /// the identity sets no signing key, so a caller can unconditionally splice
    /// the result into a git argv. The per-operation controls (the commit
    /// overlay's `^S` cycle, `[git] override_gpg`) still layer above this.
    pub fn git_signing_args(&self) -> Vec<String> {
        match &self.git_signing {
            Some((fmt, key)) => vec![
                "-c".into(),
                format!("gpg.format={fmt}"),
                "-c".into(),
                format!("user.signingKey={key}"),
            ],
            None => Vec::new(),
        }
    }
}

/// Resolve an [`IdentityConfig`] into `~`-expanded per-tool paths.
pub fn resolved(id: &IdentityConfig) -> Resolved {
    let opt = |s: &str| (!s.is_empty()).then(|| util::expand_tilde(s));
    let git_signing = (!id.signing.key.is_empty()).then(|| {
        let fmt = id.signing.format.as_str().to_string();
        // An ssh signing key is a path (expand `~`); an openpgp key id is not.
        let key = if matches!(id.signing.format, crate::config::SigningFormat::Ssh) {
            util::expand_tilde(&id.signing.key)
        } else {
            id.signing.key.clone()
        };
        (fmt, key)
    });
    Resolved {
        git_config: opt(&id.git.config),
        git_ssh_key: opt(&id.git.ssh_key),
        gh_config: opt(&id.gh.config),
        gpg_home: opt(&id.gpg.home),
        git_signing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IdentityConfig, IdentityGh, IdentityGit, IdentityGpg};

    #[test]
    fn resolve_finds_defined_and_misses_unknown() {
        let mut cfg = Config::default();
        cfg.identities
            .insert("washu".into(), IdentityConfig::default());
        assert!(resolve(&cfg, "washu").is_some());
        assert!(resolve(&cfg, "nope").is_none());
    }

    #[test]
    fn resolved_maps_set_tools_and_leaves_unset_none() {
        // A partial identity: git + gh set, gpg/ssh unset ⇒ those resolve None
        // (they fall through to a less specific scope / profile-root fallback).
        let id = IdentityConfig {
            git: IdentityGit {
                config: "/a/gitconfig".into(),
                ssh_key: String::new(),
            },
            gh: IdentityGh {
                config: "/a/gh".into(),
            },
            gpg: IdentityGpg::default(),
            signing: Default::default(),
            accounts: Default::default(),
        };
        let r = resolved(&id);
        assert_eq!(r.git_config.as_deref(), Some("/a/gitconfig"));
        assert_eq!(r.gh_config.as_deref(), Some("/a/gh"));
        assert_eq!(r.git_ssh_key, None);
        assert_eq!(r.gpg_home, None);
        assert_eq!(r.git_ssh_command(), None);
        // No signing set ⇒ no signing args (falls through to repo/global config).
        assert_eq!(r.git_signing, None);
        assert!(r.git_signing_args().is_empty());
    }

    #[test]
    fn signing_resolves_format_and_key_by_format() {
        use crate::config::{IdentitySigning, SigningFormat};
        // openpgp key id is left verbatim.
        let gpg = IdentityConfig {
            signing: IdentitySigning {
                format: SigningFormat::Openpgp,
                key: "ABCD1234".into(),
            },
            ..Default::default()
        };
        assert_eq!(
            resolved(&gpg).git_signing,
            Some(("openpgp".into(), "ABCD1234".into()))
        );
        assert_eq!(
            resolved(&gpg).git_signing_args(),
            vec!["-c", "gpg.format=openpgp", "-c", "user.signingKey=ABCD1234"]
        );
        // ssh key is a path ⇒ `~` expanded.
        let ssh = IdentityConfig {
            signing: IdentitySigning {
                format: SigningFormat::Ssh,
                key: "~/.ssh/id_sign.pub".into(),
            },
            ..Default::default()
        };
        let (fmt, key) = resolved(&ssh).git_signing.unwrap();
        assert_eq!(fmt, "ssh");
        assert!(
            !key.starts_with('~'),
            "ssh key path must be expanded: {key}"
        );
        assert!(key.ends_with("/.ssh/id_sign.pub"));
    }

    #[test]
    fn git_ssh_command_forces_identities_only() {
        let r = Resolved {
            git_ssh_key: Some("/keys/id_washu".into()),
            ..Default::default()
        };
        assert_eq!(
            r.git_ssh_command().as_deref(),
            Some("ssh -i /keys/id_washu -o IdentitiesOnly=yes")
        );
    }

    #[test]
    fn resolved_expands_leading_tilde() {
        let id = IdentityConfig {
            gpg: IdentityGpg {
                home: "~/.gnupg".into(),
            },
            ..Default::default()
        };
        let home = resolved(&id).gpg_home.unwrap();
        assert!(!home.starts_with('~'), "tilde must be expanded: {home}");
        assert!(home.ends_with("/.gnupg"));
    }

    #[test]
    fn binding_roundtrip_and_scope_precedence() {
        let db = crate::db::Db::open_memory().unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")), None);
        assert!(bound_in_order(&db, "/wt", Some("repo")).is_empty());

        set_active(&db, Bind::Global, "/wt", Some("repo"), "g").unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")).as_deref(), Some("g"));

        // A worktree binding is more specific than the global one.
        set_active(&db, Bind::Worktree, "/wt", Some("repo"), "w").unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")).as_deref(), Some("w"));
        // Fold order is low→high (global first, worktree last).
        assert_eq!(
            bound_in_order(&db, "/wt", Some("repo")),
            vec!["g".to_string(), "w".to_string()]
        );

        clear_active(&db, Bind::Worktree, "/wt", Some("repo")).unwrap();
        assert_eq!(active_name(&db, "/wt", Some("repo")).as_deref(), Some("g"));
    }
}
