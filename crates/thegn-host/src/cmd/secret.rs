//! `thegn secret <action>` — the credential broker CLI (THE-66).
//!
//! Operator-surface, admin-scoped custody: store/remove/list/migrate/audit
//! secrets and rotate managed SSH keys. There is deliberately **no** value-read
//! verb — the broker resolves for components, not for callers. `set` reads the
//! secret from **stdin** (never argv), and no command ever prints a value.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use thegn_core::config::Config;
use thegn_core::secret_scan;
use thegn_core::secretref::{BareAs, SecretRef};
use thegn_core::{config_write, msg, outln};

use crate::secret;

/// `thegn secret` subcommands.
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Store a secret in the broker (keyring, else a 0600 file). The value is
    /// read from STDIN — never the command line — and never echoed. Prints the
    /// SecretRef to paste into config.
    Set {
        /// The account/name to store under (e.g. `fly-dev`, `work-linear`).
        name: String,
    },
    /// Remove a stored secret (both keyring and file backends; never errors on a
    /// missing entry).
    Rm {
        /// The account/name that was stored.
        name: String,
    },
    /// List configured secret refs and their backends — names + presence only,
    /// never values.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Move plaintext literal secrets out of config into the store, rewriting
    /// each field to the returned ref (comment-preserving). `--dry-run` reports
    /// what would move without touching anything.
    Migrate {
        #[arg(long)]
        dry_run: bool,
    },
    /// Summarize configured refs with their backend and last resolution outcome
    /// (presence only, never a value).
    Audit {
        #[arg(long)]
        json: bool,
    },
    /// Managed SSH key custody.
    Ssh {
        #[command(subcommand)]
        action: SshAction,
    },
}

/// `thegn secret ssh` subcommands.
#[derive(clap::Subcommand, Clone)]
pub enum SshAction {
    /// Rotate a managed SSH key across its scope's live instances.
    Rotate {
        /// Restrict to one recorded account label (`secret audit` lists them).
        #[arg(long)]
        account: Option<String>,
        /// Report the plan without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(cfg: &Config, action: Action, config_path: PathBuf) -> Result<()> {
    match action {
        Action::Set { name } => set(&name),
        Action::Rm { name } => {
            let report = secret::forget_with_report(&name);
            let keyring = match &report.keyring {
                secret::ForgetKeyringOutcome::Removed => "removed".to_string(),
                secret::ForgetKeyringOutcome::Failed(error) => format!("failed: {error}"),
                secret::ForgetKeyringOutcome::OutcomeUnknown => "outcome unknown".to_string(),
            };
            let file = match &report.file {
                secret::ForgetFileOutcome::Removed => "removed".to_string(),
                secret::ForgetFileOutcome::Absent => "absent".to_string(),
                secret::ForgetFileOutcome::Failed(error) => format!("failed: {error}"),
            };
            outln!("secret {name}: keyring {keyring}; file {file}");
            match report.keyring {
                secret::ForgetKeyringOutcome::OutcomeUnknown => {
                    bail!(
                        "keyring removal outcome unknown for `{name}`; retry once the keyring responds"
                    )
                }
                secret::ForgetKeyringOutcome::Failed(error) => {
                    bail!("keyring removal failed for `{name}`: {error}")
                }
                secret::ForgetKeyringOutcome::Removed => report.ensure_file_removed(&name),
            }
        }
        Action::List { json } => list(cfg, json),
        Action::Migrate { dry_run } => migrate(cfg, &config_path, dry_run),
        Action::Audit { json } => audit(cfg, json),
        Action::Ssh { action } => match action {
            SshAction::Rotate { account, dry_run } => ssh_rotate(cfg, account.as_deref(), dry_run),
        },
    }
}

/// Read a secret from stdin (never argv) and store it, printing the ref.
fn set(name: &str) -> Result<()> {
    let mut val = String::new();
    std::io::stdin()
        .read_to_string(&mut val)
        .map_err(|e| anyhow::anyhow!("read secret from stdin: {e}"))?;
    let val = val.trim();
    if val.is_empty() {
        bail!(
            "no secret on stdin — pipe the value in (e.g. `printf %s \"$TOKEN\" | thegn secret set {name}`)"
        );
    }
    let r = secret::store(name, val)?;
    // Print the REF, never the value.
    outln!("{r}");
    msg::info(&format!(
        "stored secret `{name}` — paste `{r}` into the config field"
    ));
    Ok(())
}

/// List configured refs: path, backend, presence. Never a value.
fn list(cfg: &Config, json: bool) -> Result<()> {
    let refs = secret_scan::secret_refs(cfg);
    if json {
        let rows: Vec<_> = refs
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "backend": f.reference.backend_kind(),
                    "consumer": f.consumer,
                    "name": f.reference.audit_name(),
                    "present": secret::present(&f.reference),
                })
            })
            .collect();
        // The `--json` convention: one compact document via `emit_json`.
        return crate::cmd::emit_json(&rows);
    }
    if refs.is_empty() {
        outln!("no secret refs configured");
        return Ok(());
    }
    for f in &refs {
        let present = if secret::present(&f.reference) {
            "resolves"
        } else {
            "missing"
        };
        outln!(
            "{:<48} {:<8} {:<10} {}",
            f.path,
            f.reference.backend_kind(),
            present,
            f.reference.audit_name()
        );
    }
    Ok(())
}

/// Value-free audit view: configured refs plus the exact managed SSH key
/// fingerprint authorized on each recorded live instance.
fn audit(cfg: &Config, json: bool) -> Result<()> {
    let refs = secret_scan::secret_refs(cfg);
    let managed =
        thegn_core::managed_ssh::list().context("read complete managed SSH custody inventory")?;
    if json {
        let secret_rows: Vec<_> = refs
            .iter()
            .map(|field| {
                serde_json::json!({
                    "path": field.path,
                    "backend": field.reference.backend_kind(),
                    "consumer": field.consumer,
                    "name": field.reference.audit_name(),
                    "present": secret::present(&field.reference),
                })
            })
            .collect();
        let key_rows: Vec<_> = managed
            .iter()
            .map(|record| {
                serde_json::json!({
                    "provider": &record.provider,
                    "account": &record.account,
                    "instance": &record.instance,
                    "key": record.key_path.file_name().and_then(|name| name.to_str()),
                    "fingerprint": &record.key_fingerprint,
                    "fingerprint_verified": thegn_core::managed_ssh::fingerprint_is_verified(&record.key_fingerprint),
                    "authorized_at": record.authorized_at,
                    "rotation_recovery": record.rotation_recovery.as_ref().map(|recovery| serde_json::json!({
                        "key": recovery.key_path.file_name().and_then(|name| name.to_str()),
                        "fingerprint": &recovery.key_fingerprint,
                        "fingerprint_verified": thegn_core::managed_ssh::fingerprint_is_verified(&recovery.key_fingerprint),
                        "authorized": recovery.authorized,
                        "verified": recovery.verified,
                        "retiring_started": recovery.retiring_started,
                        "retiring_completed": recovery.retiring_completed,
                    })),
                })
            })
            .collect();
        return crate::cmd::emit_json(&serde_json::json!({
            "secret_refs": secret_rows,
            "managed_ssh": key_rows,
        }));
    }
    list(cfg, false)?;
    if managed.is_empty() {
        outln!("managed ssh: no live authorization records");
    } else {
        outln!("managed ssh authorizations:");
        for record in managed {
            let fingerprint =
                thegn_core::managed_ssh::fingerprint_audit_label(&record.key_fingerprint);
            let recovery = record
                .rotation_recovery
                .as_ref()
                .map(|state| {
                    format!(
                        " recovery={}",
                        thegn_core::managed_ssh::fingerprint_audit_label(&state.key_fingerprint)
                    )
                })
                .unwrap_or_default();
            outln!(
                "  {:<12} {:<24} {:<24} {}{}",
                record.provider,
                record.account,
                record.instance,
                fingerprint,
                recovery,
            );
        }
    }
    Ok(())
}

/// Migrate plaintext literals into the store and rewrite the config fields.
fn migrate(cfg: &Config, config_path: &std::path::Path, dry_run: bool) -> Result<()> {
    let mut moved = 0usize;
    let mut embedded_mpd_host_remains = false;

    // Issue-tracker account tokens (bare = literal).
    for acct in &cfg.issues.issue_accounts {
        let r = SecretRef::parse(&acct.token, BareAs::Literal);
        if !(r.is_literal() && r.is_configured()) {
            continue;
        }
        let path = format!("issues.issue_accounts[{}].token", acct.name);
        if dry_run {
            outln!("would migrate {path} (plaintext -> 0600 file)");
            moved += 1;
            continue;
        }
        let value = r.expose_literal().unwrap_or_default();
        let account = format!("issue-{}", acct.name);
        // Issue tokens now resolve `keyring:` too (THE-72: the svc resolver in
        // `thegn_svc::issue::secret`, installed from `main.rs`/`run.rs`), so
        // `store` would also work here. Migration keeps writing a 0600 file:
        // it is the form that resolves on every box including a headless one
        // with no Secret Service, and changing what `migrate` emits is a
        // separate, opt-in decision — not a side effect of the resolver landing.
        let new_ref = secret::store_file(&account, value)?;
        config_write::set_issue_account_token(config_path, &acct.name, &new_ref)?;
        outln!("migrated {path} -> {new_ref}");
        moved += 1;
    }

    // GitLab CI token (bare = literal).
    {
        let r = SecretRef::parse(&cfg.ci.gitlab.token, BareAs::Literal);
        if r.is_literal() && r.is_configured() {
            if dry_run {
                outln!("would migrate ci.gitlab.token (plaintext -> 0600 file)");
                moved += 1;
            } else {
                let value = r.expose_literal().unwrap_or_default();
                // Same as issue tokens: resolved via expand_env_ref today.
                let new_ref = secret::store_file("ci-gitlab", value)?;
                config_write::set_key(config_path, "ci.gitlab.token", &new_ref)?;
                outln!("migrated ci.gitlab.token -> {new_ref}");
                moved += 1;
            }
        }
    }

    moved += migrate_calendar_tokens(cfg, config_path, dry_run, &mut |name: &str, value: &str| {
        secret::store_file(name, value)
    })?;

    // MPD config password (bare = historical literal). The stable config key
    // remains unchanged so existing daemon credentials continue to work.
    if let Some(password) = cfg
        .media
        .mpd
        .password
        .as_ref()
        .filter(|password| password.is_literal() && password.secret_ref().is_configured())
        && let Some(value) = password.expose_literal()
    {
        if dry_run {
            outln!("would migrate media.mpd.password (plaintext -> 0600 file)");
            moved += 1;
        } else {
            let new_ref = secret::store_file("media-mpd", value)?;
            config_write::set_key(config_path, "media.mpd.password", &new_ref)?;
            outln!("migrated media.mpd.password -> {new_ref}");
            moved += 1;
        }
    }

    if let Ok(host) = std::env::var("MPD_HOST")
        && let Some(reference) = secret_scan::mpd_host_secret_ref(&host)
        && let Some(value) = reference.expose_literal()
    {
        let socket_uses_host_env = cfg.media.mpd.socket.trim().is_empty()
            || cfg.media.mpd.socket.trim() == "127.0.0.1:6600";
        if socket_uses_host_env && cfg.media.mpd.password.is_none() {
            embedded_mpd_host_remains = true;
            if dry_run {
                outln!(
                    "would migrate MPD_HOST embedded credential to media.mpd.password (plaintext -> 0600 file)"
                );
                moved += 1;
            } else {
                let new_ref = secret::store_file("media-mpd", value)?;
                config_write::set_key(config_path, "media.mpd.password", &new_ref)?;
                outln!("migrated MPD_HOST credential to media.mpd.password -> {new_ref}");
                moved += 1;
            }
        } else {
            embedded_mpd_host_remains = true;
        }
    }

    if embedded_mpd_host_remains {
        outln!(
            "MPD_HOST still contains an embedded plaintext password; remove the password@ prefix and keep only the host"
        );
    }

    if moved == 0 {
        if !embedded_mpd_host_remains {
            outln!("no plaintext secrets to migrate — config is clean");
        }
    } else if dry_run {
        msg::info(&format!(
            "{moved} plaintext secret(s) would move; re-run without --dry-run to apply"
        ));
    } else {
        msg::info(&format!("migrated {moved} secret(s) into the store"));
    }
    Ok(())
}

/// Migrate literal tokens for remote calendar accounts. The account must be
/// present in this file before anything is stored, because another config
/// layer may own the effective account definition.
fn migrate_calendar_tokens(
    cfg: &Config,
    config_path: &std::path::Path,
    dry_run: bool,
    store: &mut dyn FnMut(&str, &str) -> Result<String>,
) -> Result<usize> {
    let mut moved = 0;
    for acct in &cfg.calendar.accounts {
        if !matches!(
            acct.provider,
            thegn_core::config::CalendarProviderKind::IcsUrl
                | thegn_core::config::CalendarProviderKind::CalDav
        ) {
            continue;
        }
        let reference = SecretRef::parse(&acct.token, BareAs::Literal);
        if !(reference.is_literal() && reference.is_configured()) {
            continue;
        }
        let path = format!("calendar.accounts[{}].token", acct.name);
        if !config_write::calendar_account_in_file(config_path, &acct.name)? {
            outln!(
                "skipped {path}: the account is not defined in {}; move its token by hand",
                config_path.display()
            );
            continue;
        }
        if config_write::calendar_account_token_in_file(config_path, &acct.name)?.as_deref()
            != Some(acct.token.as_str())
        {
            outln!(
                "skipped {path}: the token in {} is not the effective one; move it by hand",
                config_path.display()
            );
            continue;
        }
        if dry_run {
            outln!("would migrate {path} (plaintext -> 0600 file)");
            moved += 1;
            continue;
        }
        let new_ref = store(
            &format!("calendar-{}", acct.name),
            reference.expose_literal().unwrap_or_default(),
        )?;
        config_write::set_calendar_account_token(config_path, &acct.name, &new_ref)
            .with_context(|| format!("stored {new_ref} but could not rewrite {path}"))?;
        outln!("migrated {path} -> {new_ref}");
        moved += 1;
    }
    Ok(moved)
}

/// Rotate recorded managed SSH authorizations transactionally. Values and
/// private-key material never enter output or argv.
fn ssh_rotate(cfg: &Config, account: Option<&str>, dry_run: bool) -> Result<()> {
    let scope = cfg.credentials.ssh.managed_key_scope;
    outln!("managed_key_scope = {scope}");
    let summary = crate::managed_ssh_rotation::rotate(cfg, account, dry_run)?;
    if summary.dry_run {
        msg::info(&format!(
            "dry run: {} key scope(s), {} live authorization(s)",
            summary.scopes, summary.instances
        ));
    } else {
        msg::info(&format!(
            "rotated {} key scope(s) across {} live authorization(s)",
            summary.scopes, summary.instances
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::migrate_calendar_tokens;
    use thegn_core::config::Config;

    #[test]
    fn calendar_migrate_moves_a_literal_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = "[[calendar.accounts]]\nname = \"work\"\nprovider = \"ics_url\"\nurl = \"https://example.test/work.ics\"\ntoken = \"pasted\"\n";
        std::fs::write(&path, source).unwrap();
        let cfg: Config = toml::from_str(source).unwrap();
        let mut calls = Vec::new();
        let moved = migrate_calendar_tokens(&cfg, &path, false, &mut |name, value| {
            calls.push((name.to_string(), value.to_string()));
            Ok(format!("file:/fake/{name}"))
        })
        .unwrap();
        assert_eq!(moved, 1);
        assert_eq!(calls, [("calendar-work".to_string(), "pasted".to_string())]);
        let body = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(
            parsed["calendar"]["accounts"][0]["token"].as_str(),
            Some("file:/fake/calendar-work")
        );

        let updated: Config = toml::from_str(&body).unwrap();
        let second = migrate_calendar_tokens(&updated, &path, false, &mut |name, value| {
            calls.push((name.to_string(), value.to_string()));
            Ok(format!("file:/fake/{name}"))
        })
        .unwrap();
        assert_eq!(second, 0);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn calendar_migrate_dry_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source =
            "[[calendar.accounts]]\nname = \"work\"\nprovider = \"ics_url\"\ntoken = \"pasted\"\n";
        std::fs::write(&path, source).unwrap();
        let cfg: Config = toml::from_str(source).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut calls = Vec::new();
        let moved = migrate_calendar_tokens(&cfg, &path, true, &mut |name, value| {
            calls.push((name.to_string(), value.to_string()));
            Ok(format!("file:/fake/{name}"))
        })
        .unwrap();
        assert_eq!(moved, 1);
        assert!(calls.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn calendar_migrate_skips_an_account_outside_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = "[calendar]\n";
        std::fs::write(&path, source).unwrap();
        let cfg: Config = toml::from_str(
            "[[calendar.accounts]]\nname = \"work\"\nprovider = \"ics_url\"\ntoken = \"pasted\"\n",
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut calls = Vec::new();
        let moved = migrate_calendar_tokens(&cfg, &path, false, &mut |name, value| {
            calls.push((name.to_string(), value.to_string()));
            Ok(format!("file:/fake/{name}"))
        })
        .unwrap();
        assert_eq!(moved, 0);
        assert!(calls.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn calendar_migrate_skips_a_token_defined_in_another_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = "[[calendar.accounts]]\nname = \"work\"\nprovider = \"ics_url\"\ntoken = \"keyring:x\"\n";
        std::fs::write(&path, source).unwrap();
        let cfg: Config = toml::from_str(
            "[[calendar.accounts]]\nname = \"work\"\nprovider = \"ics_url\"\ntoken = \"pasted\"\n",
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut calls = Vec::new();
        let moved = migrate_calendar_tokens(&cfg, &path, false, &mut |name, value| {
            calls.push((name.to_string(), value.to_string()));
            Ok(format!("file:/fake/{name}"))
        })
        .unwrap();
        assert_eq!(moved, 0);
        assert!(calls.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
