//! The `[[forges]]` config family — named git forges (GitHub, GitHub
//! Enterprise, Forgejo, Gitea). Kept in a sibling module (rather than the
//! god-file `config.rs`) per the file-size ratchet; `config.rs` re-exports
//! everything here.
//!
//! **Config surface only for now.** The fetch layer (`thegn-svc`, `gh` CLI)
//! implements GitHub / GitHub Enterprise; Forgejo/Gitea are configurable but
//! error gracefully until their clients land (see `crate::forge::detect_forge`).

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};

config_enum! {
    /// Which forge software a `[[forges]]` entry runs. `github`/`ghe` drive the
    /// `gh` CLI today; `forgejo`/`gitea` are surface-only until their clients land.
    pub enum ForgeKind : "forge" {
        Github  = "github",
        Ghe     = "ghe" | "github-enterprise",
        Forgejo = "forgejo",
        Gitea   = "gitea",
    } default = Github;
}

/// A `[[forges]]` entry — one named forge login. Multiple entries let a user
/// track issues/PRs across github.com, a GitHub Enterprise instance, and a
/// self-hosted Forgejo at once. Mirrors the `[[issue_accounts]]` /
/// `[[accounts]]` named-account shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ForgeConfig {
    /// Stable id for this forge, e.g. `"work-ghe"`. Referenced by
    /// `[[issue_accounts]].forge`.
    pub name: String,
    /// Which forge software this entry runs.
    pub kind: ForgeKind,
    /// API host, e.g. `"github.com"` or `"git.example.com"`. `""` = the kind's
    /// default (`github.com` for GitHub).
    pub host: String,
    /// API token. Use a secret ref or `"env:VAR"`. Empty ⇒ the CLI's own auth
    /// (`gh` for GitHub).
    pub token: String,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        ForgeConfig {
            name: String::new(),
            kind: ForgeKind::Github,
            host: String::new(),
            token: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_kind_parses_canon_and_alias() {
        assert_eq!(
            ForgeKind::from_str_validated("github"),
            Ok(ForgeKind::Github)
        );
        assert_eq!(ForgeKind::from_str_validated("GHE"), Ok(ForgeKind::Ghe));
        assert_eq!(
            ForgeKind::from_str_validated("github-enterprise"),
            Ok(ForgeKind::Ghe)
        );
        assert_eq!(
            ForgeKind::from_str_validated("forgejo"),
            Ok(ForgeKind::Forgejo)
        );
        assert_eq!(ForgeKind::from_str_validated("gitea"), Ok(ForgeKind::Gitea));
        assert!(ForgeKind::from_str_validated("bitbucket").is_err());
    }

    #[test]
    fn forge_kind_round_trips_as_str_and_default() {
        assert_eq!(ForgeKind::Github.as_str(), "github");
        assert_eq!(ForgeKind::Ghe.as_str(), "ghe");
        assert_eq!(ForgeKind::default(), ForgeKind::Github);
        assert_eq!(ForgeKind::Forgejo.to_string(), "forgejo");
    }

    #[test]
    fn forge_config_defaults_to_github() {
        let f = ForgeConfig::default();
        assert!(f.name.is_empty());
        assert_eq!(f.kind, ForgeKind::Github);
        assert!(f.host.is_empty());
        assert!(f.token.is_empty());
    }
}
