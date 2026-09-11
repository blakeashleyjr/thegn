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
            secret::forget(&name);
            outln!("removed secret {name} (keyring + file, best-effort)");
            Ok(())
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
                    "authorized_at": record.authorized_at,
                    "rotation_recovery": record.rotation_recovery.as_ref().map(|recovery| serde_json::json!({
                        "key": recovery.key_path.file_name().and_then(|name| name.to_str()),
                        "fingerprint": &recovery.key_fingerprint,
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
            let recovery = record
                .rotation_recovery
                .as_ref()
                .map(|state| format!(" recovery={}", state.key_fingerprint))
                .unwrap_or_default();
            outln!(
                "  {:<12} {:<24} {:<24} {}{}",
                record.provider,
                record.account,
                record.instance,
                record.key_fingerprint,
                recovery,
            );
        }
    }
    Ok(())
}

/// Migrate plaintext literals into the store and rewrite the config fields.
fn migrate(cfg: &Config, config_path: &std::path::Path, dry_run: bool) -> Result<()> {
    let mut moved = 0usize;

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

    if moved == 0 {
        outln!("no plaintext secrets to migrate — config is clean");
    } else if dry_run {
        msg::info(&format!(
            "{moved} plaintext secret(s) would move; re-run without --dry-run to apply"
        ));
    } else {
        msg::info(&format!("migrated {moved} secret(s) into the store"));
    }
    Ok(())
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
