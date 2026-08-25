//! `thegn secret <action>` — the credential broker CLI (THE-66).
//!
//! Operator-surface, admin-scoped custody: store/remove/list/migrate/audit
//! secrets and rotate managed SSH keys. There is deliberately **no** value-read
//! verb — the broker resolves for components, not for callers. `set` reads the
//! secret from **stdin** (never argv), and no command ever prints a value.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Result, bail};
use thegn_core::config::{Config, ManagedKeyScope};
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
        /// Restrict to one provider account (per-account scope).
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
        Action::Audit { json } => list(cfg, json), // audit == list + presence today
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
        // Issue tokens resolve via expand_env_ref today (env:/file:, not
        // keyring:), so migrate to a 0600 file to keep them resolvable — no
        // silent breakage. (Keyring for these lands with the svc resolver.)
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

/// Rotate a managed SSH key. The pure key-naming + scope decision is wired; the
/// live-fleet re-authorization step (authorize new key on every instance in
/// scope, verify, de-authorize old, retire) rides the provider exec transports
/// and is reported here as the plan — see the design's partial-failure rules.
fn ssh_rotate(cfg: &Config, account: Option<&str>, _dry_run: bool) -> Result<()> {
    let scope = cfg.credentials.ssh.managed_key_scope;
    outln!("managed_key_scope = {scope}");
    match scope {
        ManagedKeyScope::Shared => {
            outln!(
                "shared scope: one key ({}) authorizes every managed remote.",
                scope.managed_key_basename("", "")
            );
            msg::warn(
                "rotating a shared key re-authorizes EVERY managed instance at once. \
                 Consider `managed_key_scope = \"per-account\"` first for isolated rotation.",
            );
        }
        ManagedKeyScope::PerAccount => {
            let acct = account.unwrap_or("<account>");
            outln!(
                "per-account scope: key basename for this account = {}",
                scope.managed_key_basename("<provider>", acct)
            );
        }
    }
    // Emit an audit breadcrumb (value-free) for the rotation intent.
    tracing::info!(
        target: "thegn::secret::audit",
        consumer = "secret.ssh.rotate",
        account = account.unwrap_or("*"),
        scope = %scope,
        "ssh managed-key rotation requested",
    );
    msg::info(
        "plan: generate replacement -> authorize on every live instance in scope -> \
         verify a connect with the new key -> de-authorize the old -> retire it. \
         Partial failure leaves BOTH keys authorized (never bricks a fleet). The \
         live per-instance authorization step is performed via the provider \
         transports; this build reports the plan and scope.",
    );
    Ok(())
}
