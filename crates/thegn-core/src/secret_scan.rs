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
use crate::config::VpnConfig;
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
/// Covers provider, issue, CI, VPN, snapshot, and MCP upstream fields. The
/// field-specific bare-string meaning is explicit at every insertion.
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

    scan_vpn(&mut out, "sandbox.vpn", &cfg.sandbox.vpn);
    for (name, env) in &cfg.env {
        if let Some(vpn) = &env.sandbox.vpn {
            scan_vpn(&mut out, &format!("env.{name}.sandbox.vpn"), vpn);
        }
    }
    for (name, profile) in &cfg.profiles {
        if let Some(vpn) = &profile.sandbox.vpn {
            scan_vpn(&mut out, &format!("profiles.{name}.sandbox.vpn"), vpn);
        }
    }

    if cfg.lifecycle.snapshot.backend == crate::config_env_tables::SnapshotBackend::S3 {
        for (field, value) in [
            ("access_key", &cfg.lifecycle.snapshot.access_key),
            ("secret_key", &cfg.lifecycle.snapshot.secret_key),
        ] {
            if !value.trim().is_empty() {
                out.push(SecretFieldRef {
                    path: format!("lifecycle.snapshot.{field}"),
                    reference: SecretRef::parse(value, BareAs::Literal),
                    consumer: format!("snapshot:s3:{}", field.replace('_', "-")),
                });
            }
        }
    }

    for (server, config) in &cfg.mcp_servers {
        for (name, value) in &config.env {
            if value.trim().is_empty() {
                continue;
            }
            out.push(SecretFieldRef {
                path: format!("mcp_servers.{server}.env.{name}"),
                reference: SecretRef::parse(value, BareAs::Literal),
                consumer: format!("mcp:{server}:{name}"),
            });
        }
    }

    out
}

fn scan_vpn(out: &mut Vec<SecretFieldRef>, prefix: &str, vpn: &VpnConfig) {
    if !vpn.is_enabled() {
        return;
    }
    let mut push = |suffix: &str, value: &str, consumer: &str| {
        if !value.trim().is_empty() {
            out.push(SecretFieldRef {
                path: format!("{prefix}.{suffix}"),
                reference: SecretRef::parse(value, BareAs::Literal),
                consumer: format!("vpn:{consumer}"),
            });
        }
    };
    push(
        "tailscale.auth_key",
        &vpn.tailscale.auth_key,
        "tailscale auth_key",
    );
    push(
        "wireguard.config",
        &vpn.wireguard.config,
        "wireguard config",
    );
    push(
        "openvpn.auth_user_pass",
        &vpn.openvpn.auth_user_pass,
        "openvpn auth_user_pass",
    );
    push(
        "netbird.setup_key",
        &vpn.netbird.setup_key,
        "netbird setup_key",
    );
    for (name, value) in &vpn.custom.env {
        push(
            &format!("custom.env.{name}"),
            value,
            &format!("custom env {name}"),
        );
    }
}

/// The configured refs that are deprecated inline literals (plaintext in
/// config) — what `config validate` warns about and `secret migrate` moves.
pub fn literal_refs(cfg: &Config) -> Vec<SecretFieldRef> {
    secret_refs(cfg)
        .into_iter()
        .filter(|s| {
            s.reference.is_literal()
                && (s.path.starts_with("issues.issue_accounts[") || s.path == "ci.gitlab.token")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EnvConfig, ProfileConfig, VpnConfig, VpnProviderKind};
    use crate::config_issues::IssueAccount;

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

    #[test]
    fn scans_vpn_snapshot_and_mcp_refs_with_consumer_tags() {
        let cfg: Config = toml::from_str(
            r#"
[sandbox.vpn]
provider = "tailscale"
[sandbox.vpn.tailscale]
auth_key = "keyring:tailnet-dev"

[lifecycle.snapshot]
backend = "s3"
bucket = "snapshots"
access_key = "env:AWS_ACCESS_KEY_ID"
secret_key = "file:/run/secrets/aws-secret"

[mcp_servers.linear]
command = ["linear-mcp"]
[mcp_servers.linear.env]
LINEAR_API_KEY = "keyring:linear-mcp"
"#,
        )
        .unwrap();
        let refs = secret_refs(&cfg);
        for (path, consumer) in [
            ("sandbox.vpn.tailscale.auth_key", "vpn:tailscale auth_key"),
            ("lifecycle.snapshot.access_key", "snapshot:s3:access-key"),
            ("lifecycle.snapshot.secret_key", "snapshot:s3:secret-key"),
            (
                "mcp_servers.linear.env.LINEAR_API_KEY",
                "mcp:linear:LINEAR_API_KEY",
            ),
        ] {
            assert_eq!(
                refs.iter()
                    .find(|field| field.path == path)
                    .map(|field| field.consumer.as_str()),
                Some(consumer),
                "missing {path}"
            );
        }
    }

    #[test]
    fn scans_env_and_profile_vpn_overlays_including_custom_secrets() {
        let mut cfg = Config::default();

        let mut env_vpn = VpnConfig {
            provider: crate::config::VpnProviderKind::Wireguard,
            ..VpnConfig::default()
        };
        env_vpn.wireguard.config = "file:/run/secrets/wg.conf".into();
        env_vpn
            .custom
            .env
            .insert("CORP_TOKEN".into(), "keyring:corp-vpn".into());
        let mut env = crate::config::EnvConfig::default();
        env.sandbox.vpn = Some(env_vpn);
        cfg.env.insert("remote".into(), env);

        let mut profile_vpn = VpnConfig {
            provider: crate::config::VpnProviderKind::Openvpn,
            ..VpnConfig::default()
        };
        profile_vpn.openvpn.auth_user_pass = "env:OPENVPN_AUTH".into();
        profile_vpn.netbird.setup_key = "keyring:netbird-dev".into();
        let mut profile = crate::config::ProfileConfig::default();
        profile.sandbox.vpn = Some(profile_vpn);
        cfg.profiles.insert("work".into(), profile);

        // A disabled overlay may retain template values, but none are active
        // secret consumers until the provider is selected.
        let mut disabled = crate::config::ProfileConfig::default();
        let mut disabled_vpn = VpnConfig::default();
        disabled_vpn.openvpn.auth_user_pass = "must-not-appear".into();
        disabled.sandbox.vpn = Some(disabled_vpn);
        cfg.profiles.insert("disabled".into(), disabled);

        let refs = secret_refs(&cfg);
        for path in [
            "env.remote.sandbox.vpn.wireguard.config",
            "env.remote.sandbox.vpn.custom.env.CORP_TOKEN",
            "profiles.work.sandbox.vpn.openvpn.auth_user_pass",
            "profiles.work.sandbox.vpn.netbird.setup_key",
        ] {
            assert!(
                refs.iter().any(|field| field.path == path),
                "missing {path}"
            );
        }
        assert!(
            refs.iter()
                .all(|field| !field.path.starts_with("profiles.disabled."))
        );
    }

    #[test]
    fn auto_migration_boundary_is_only_legacy_issue_and_gitlab_tokens() {
        let cfg: Config = toml::from_str(
            r#"
[[issues.issue_accounts]]
name = "work"
provider = "linear"
token = "literal-issue-token"

[ci.gitlab]
token = "literal-ci-token"

[sandbox.vpn]
provider = "tailscale"
[sandbox.vpn.tailscale]
auth_key = "literal-vpn-token"

[lifecycle.snapshot]
backend = "s3"
bucket = "snapshots"
access_key = "literal-snapshot-key"

[mcp_servers.linear]
command = ["linear-mcp"]
[mcp_servers.linear.env]
LINEAR_API_KEY = "literal-mcp-token"
"#,
        )
        .unwrap();
        assert_eq!(
            literal_refs(&cfg)
                .into_iter()
                .map(|field| field.path)
                .collect::<Vec<_>>(),
            vec![
                "issues.issue_accounts[work].token".to_string(),
                "ci.gitlab.token".to_string(),
            ]
        );
    }

    #[test]
    fn scans_every_enabled_vpn_overlay_and_skips_empty_secret_fields() {
        fn vpn(provider: VpnProviderKind, prefix: &str) -> VpnConfig {
            let mut vpn = VpnConfig {
                provider,
                ..VpnConfig::default()
            };
            vpn.tailscale.auth_key = format!("keyring:{prefix}-tailscale");
            vpn.wireguard.config = format!("file:/{prefix}/wg.conf");
            vpn.openvpn.auth_user_pass = format!("env:{prefix}_OPENVPN");
            vpn.netbird.setup_key = format!("keyring:{prefix}-netbird");
            vpn.custom
                .env
                .insert("TOKEN".into(), format!("keyring:{prefix}-custom"));
            vpn.custom.env.insert("EMPTY".into(), "   ".into());
            vpn
        }

        let mut cfg = Config::default();
        cfg.sandbox.vpn = vpn(VpnProviderKind::Tailscale, "global");

        let mut env = EnvConfig::default();
        env.sandbox.vpn = Some(vpn(VpnProviderKind::Wireguard, "env"));
        cfg.env.insert("remote".into(), env);

        let mut profile = ProfileConfig::default();
        profile.sandbox.vpn = Some(vpn(VpnProviderKind::Custom, "profile"));
        cfg.profiles.insert("work".into(), profile);

        // Exercise the explicit empty-value skips for account and MCP fields.
        cfg.issues.issue_accounts.push(IssueAccount {
            name: "empty".into(),
            token: "   ".into(),
            ..IssueAccount::default()
        });
        cfg.mcp_servers
            .entry("empty".into())
            .or_default()
            .env
            .insert("EMPTY".into(), " ".into());

        let refs = secret_refs(&cfg);
        let paths = refs
            .iter()
            .map(|field| field.path.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for prefix in [
            "sandbox.vpn",
            "env.remote.sandbox.vpn",
            "profiles.work.sandbox.vpn",
        ] {
            for suffix in [
                "tailscale.auth_key",
                "wireguard.config",
                "openvpn.auth_user_pass",
                "netbird.setup_key",
                "custom.env.TOKEN",
            ] {
                assert!(paths.contains(format!("{prefix}.{suffix}").as_str()));
            }
            assert!(!paths.contains(format!("{prefix}.custom.env.EMPTY").as_str()));
        }
        assert!(!paths.contains("issues.issue_accounts[empty].token"));
        assert!(!paths.contains("mcp_servers.empty.env.EMPTY"));
    }

    #[test]
    fn disabled_vpn_and_non_s3_snapshot_secrets_are_not_consumers() {
        let mut cfg = Config::default();
        cfg.sandbox.vpn.tailscale.auth_key = "literal-disabled-vpn-key".into();
        cfg.lifecycle.snapshot.access_key = "literal-local-snapshot-key".into();
        cfg.lifecycle.snapshot.secret_key = "literal-local-snapshot-secret".into();

        let paths = secret_refs(&cfg)
            .into_iter()
            .map(|field| field.path)
            .collect::<Vec<_>>();
        assert!(!paths.iter().any(|path| path.starts_with("sandbox.vpn.")));
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with("lifecycle.snapshot."))
        );
    }
}
