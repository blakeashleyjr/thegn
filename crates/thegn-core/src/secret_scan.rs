//! Enumerate every configured secret reference in a [`Config`] (THE-66).
//!
//! One place that knows *which* config fields name a secret and *what a bare
//! string means* for each (the per-field [`BareAs`] marker) — so the CLI
//! (`secret list` / `secret audit` / `secret migrate`), `config validate`'s
//! plaintext-secret warning, and `thegn doctor`'s per-ref presence rows all
//! read the same list instead of re-deriving it.
//!
//! Pure: it builds typed [`SecretRef`]s; it resolves nothing (that is the
//! broker's job, host-side).

use crate::config::Config;
use crate::secretref::{BareAs, SecretRef};

/// One configured secret field: where it is, its parsed ref, and a stable
/// consumer tag for the audit trail.
#[derive(Debug, Clone)]
pub struct SecretFieldRef {
    /// Dotted config path, e.g. `env.fly.provider.api_key_env`.
    pub path: String,
    /// The parsed reference (with this field's bare-string meaning applied).
    pub reference: SecretRef,
    /// The audit consumer tag (`provider:fly`, `issues:linear`, `ci:gitlab`).
    pub consumer: String,
}

/// Every configured secret field across the config, in a stable order.
///
/// Covers the fields the credential-broker change unifies: provider tokens
/// (bare ⇒ env-name), issue-tracker account tokens (bare ⇒ literal — the field
/// family this change gives keyring support), and the GitLab CI token (bare ⇒
/// literal). Extend here as more consumers migrate onto the broker (VPN keys,
/// snapshot store creds, MCP upstream env).
pub fn secret_refs(cfg: &Config) -> Vec<SecretFieldRef> {
    let mut out = Vec::new();

    // Provider API tokens — historic bare-as-env-name semantics.
    for (name, env) in &cfg.env {
        let p = &env.provider;
        if p.provider.trim().is_empty() || p.api_key_env.trim().is_empty() {
            continue;
        }
        out.push(SecretFieldRef {
            path: format!("env.{name}.provider.api_key_env"),
            reference: SecretRef::parse(&p.api_key_env, BareAs::EnvName),
            consumer: format!("provider:{}", p.provider.trim()),
        });
    }

    // Issue-tracker account tokens — historic bare-as-literal semantics (a
    // pasted key was silently accepted as plaintext). Typed now, and any scheme
    // (incl. `keyring:`) works uniformly.
    for acct in &cfg.issues.issue_accounts {
        if acct.token.trim().is_empty() {
            continue;
        }
        out.push(SecretFieldRef {
            path: format!("issues.issue_accounts[{}].token", acct.name),
            reference: SecretRef::parse(&acct.token, BareAs::Literal),
            consumer: format!("issues:{}", acct.provider.as_str()),
        });
    }

    // GitLab CI token — historic bare-as-literal semantics.
    let gl = cfg.ci.gitlab.token.trim();
    if !gl.is_empty() {
        out.push(SecretFieldRef {
            path: "ci.gitlab.token".to_string(),
            reference: SecretRef::parse(gl, BareAs::Literal),
            consumer: "ci:gitlab".to_string(),
        });
    }

    out
}

/// The configured refs that are deprecated inline literals (plaintext in
/// config) — what `config validate` warns about and `secret migrate` moves.
pub fn literal_refs(cfg: &Config) -> Vec<SecretFieldRef> {
    secret_refs(cfg)
        .into_iter()
        .filter(|s| s.reference.is_literal())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn scans_provider_issue_and_ci_tokens_with_right_bare_semantics() {
        let toml = r#"
[env.fly]
[env.fly.provider]
provider = "fly"
api_key_env = "FLY_API_TOKEN"

[[issues.issue_accounts]]
name = "work-linear"
provider = "linear"
token = "lin_plaintext_secret"

[[issues.issue_accounts]]
name = "kr"
provider = "linear"
token = "keyring:work-linear"

[ci.gitlab]
token = "env:GITLAB_TOKEN"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let refs = secret_refs(&cfg);
        // Provider bare string is an env-name ref.
        let fly = refs.iter().find(|r| r.path.contains("fly")).unwrap();
        assert_eq!(fly.reference.backend_kind(), "env");
        assert_eq!(fly.consumer, "provider:fly");
        // A pasted issue token is a literal (the thing we warn about).
        let lin = refs
            .iter()
            .find(|r| r.path.contains("work-linear"))
            .unwrap();
        assert!(lin.reference.is_literal());
        assert_eq!(lin.consumer, "issues:linear");
        // A keyring ref on an issue token is a keyring ref (the new capability).
        let kr = refs.iter().find(|r| r.path.contains("[kr]")).unwrap();
        assert_eq!(kr.reference.backend_kind(), "keyring");
        // CI token env ref.
        let ci = refs.iter().find(|r| r.path == "ci.gitlab.token").unwrap();
        assert_eq!(ci.reference.backend_kind(), "env");

        // Only the pasted literal is flagged for migration.
        let lits = literal_refs(&cfg);
        assert_eq!(lits.len(), 1);
        assert!(lits[0].path.contains("work-linear"));
    }

    #[test]
    fn default_config_has_no_plaintext_literal_refs() {
        // A fresh default config carries some refs (e.g. the CI token defaults
        // to `env:GITLAB_TOKEN`), but NONE is a plaintext literal — the thing
        // `config validate` warns about and `secret migrate` moves.
        let cfg = Config::default();
        assert!(
            literal_refs(&cfg).is_empty(),
            "default config must not paste a plaintext secret: {:?}",
            literal_refs(&cfg)
                .iter()
                .map(|r| &r.path)
                .collect::<Vec<_>>()
        );
        // Any default refs resolve through env, not inline literals.
        for r in secret_refs(&cfg) {
            assert_ne!(r.reference.backend_kind(), "literal", "{}", r.path);
        }
    }
}
