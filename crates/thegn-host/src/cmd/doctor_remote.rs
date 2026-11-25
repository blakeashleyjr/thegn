//! Doctor's "Remote live readiness" block (sprites + vercel envs).
//!
//! Offline: no provider call, names only, never values. `json` is a stable
//! shape that the live preflight reads (`remote_live`).

use std::collections::BTreeSet;

use thegn_core::config::{Config, EnvProviderKind};
use thegn_core::outln;
use thegn_core::store::{PoolStore, WorkspaceStore};
use thegn_svc::sync_journal::{self, SyncJournalEntry, SyncState};
use thegn_svc::vps::registry::VpsRecord;

/// A `creating` ledger entry older than this is stale.
const STALE_CREATING_SECS: i64 = 3600;
const SPRITES_REVIEW_COMMAND: &str = "thegn env reap --provider sprites --unrecorded";

fn clean(value: &str) -> String {
    crate::merge_sweep::safe_display(value)
}

fn clean_basename(value: &str) -> String {
    let trimmed = value.trim_end_matches(['/', '\\']);
    let basename = std::path::Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .or_else(|| trimmed.rsplit(['/', '\\']).next())
        .unwrap_or("");
    clean(basename)
}

fn pending_line(total: usize, failed: usize) -> String {
    format!(
        "pull-back pending: {total} ({failed} failed); closing or removing the worktree, or thegn env sync-retry, retries it and keeps the sandbox until it succeeds"
    )
}

fn remote_envs(cfg: &Config) -> Vec<(&String, EnvProviderKind)> {
    let mut v: Vec<_> = cfg
        .env
        .iter()
        .map(|(name, e)| (name, EnvProviderKind::of(e.provider.provider.trim())))
        .filter(|(_, kind)| matches!(kind, EnvProviderKind::Sprites | EnvProviderKind::Vercel))
        .collect();
    v.sort_by(|a, b| a.0.cmp(b.0));
    v
}

/// Whether the host controller id is readable. Read-only: unlike
/// `controller_id()` this never mints one.
fn controller_id_readable() -> bool {
    let new =
        thegn_svc::state_promote::state_root().join(thegn_svc::vps::controller::CONTROLLER_ID_FILE);
    let legacy = thegn_core::util::thegn_dir().join(thegn_svc::vps::controller::CONTROLLER_ID_FILE);
    [new, legacy].iter().any(|path| {
        std::fs::read_to_string(path)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    })
}

/// The env's sandbox policy: the global `[sandbox]` with the env overlay's
/// passthrough list and git policy applied.
fn env_sandbox(
    cfg: &Config,
    env: &thegn_core::config::EnvConfig,
) -> thegn_core::config::SandboxConfig {
    let mut sb = cfg.sandbox.clone();
    if let Some(list) = &env.sandbox.env_passthrough {
        sb.env_passthrough = list.clone();
    }
    if let Some(git) = &env.sandbox.git {
        sb.git = sb.git.merged(git);
    }
    sb
}

fn login_agents(cfg: &Config, env: &thegn_core::config::EnvConfig) -> Vec<String> {
    let overlay = env.sandbox.home.as_ref().and_then(|h| h.agents.clone());
    let agents = overlay.unwrap_or_else(|| cfg.sandbox.home.agents.clone());
    if !agents.is_empty() {
        return agents;
    }
    let picked = crate::agent::provisioned_agent_kinds(cfg);
    if picked.is_empty() {
        crate::agent::detect_host_agents()
    } else {
        picked
    }
}

fn sandbox_agents(cfg: &Config, env: &thegn_core::config::EnvConfig) -> Vec<String> {
    env.sandbox
        .home
        .as_ref()
        .and_then(|h| h.agents.clone())
        .unwrap_or_else(|| cfg.sandbox.home.agents.clone())
        .into_iter()
        .map(|agent| clean(&agent))
        .collect()
}

fn channel_warning(clamped: bool, rows_empty: bool) -> Option<String> {
    (clamped && rows_empty).then(|| {
        "dev-only provider envs (not Sprites or Vercel) are configured but this instance runs on the stable channel, which ignores them; use the dev instance (THEGN_CHANNEL=dev)".to_string()
    })
}

fn report_preamble(rows: &[serde_json::Value]) -> Vec<String> {
    let warnings = rows
        .iter()
        .filter_map(|row| {
            let warning = row["warning"].as_str()?;
            let error = row["sync_journal_error"].as_str().unwrap_or("");
            if error.is_empty() {
                Some(format!("  warning: {warning}"))
            } else {
                Some(format!("  warning: {warning}: {error}"))
            }
        })
        .collect::<Vec<_>>();
    if warnings.is_empty() && rows.is_empty() {
        vec!["  (no sprites or vercel envs configured)".to_string()]
    } else {
        warnings
    }
}

fn cap_display(value: u64) -> String {
    if value == 0 {
        "provider default".to_string()
    } else {
        value.to_string()
    }
}

fn ledger_names_for(records: &[VpsRecord], provider: &str) -> Vec<(String, String, i64)> {
    records
        .iter()
        .filter(|r| r.provider == provider)
        .map(|r| (r.name.clone(), r.state.clone(), r.created_at))
        .collect()
}

fn pool_counts(states: &[&str]) -> (usize, usize, usize) {
    let mut ready = 0;
    let mut provisioning = 0;
    let mut other = 0;
    for state in states {
        match *state {
            "ready" => ready += 1,
            "provisioning" => provisioning += 1,
            _ => other += 1,
        }
    }
    (ready, provisioning, other)
}

fn warm_pool_report_line(
    provider: &str,
    ready: usize,
    provisioning: usize,
    other: usize,
    size: usize,
) -> Option<String> {
    if provider == "vercel" {
        return Some("  warm pool: not used for vercel".to_string());
    }
    (ready != 0 || provisioning != 0 || other != 0 || size != 0).then(|| {
        format!(
            "  warm pool: {ready} ready, {provisioning} provisioning ([lifecycle.pool] size = {size}; per-project sizes: TUI +/- or thegn env pool-fill --size)"
        )
    })
}

/// Names of sandboxes bound to a worktree row of this env, or spares.
fn bound_names(cfg: &Config, db: &thegn_core::db::Db, env_name: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for row in db.worktrees().unwrap_or_default() {
        let effective = db
            .effective_env_checked(&row.worktree, &row.repo_root)
            .ok()
            .flatten();
        if effective.as_deref() != Some(env_name) {
            continue;
        }
        if let Ok(Some(name)) = db.worktree_provider_sandbox(&row.worktree) {
            out.insert(name);
        }
        if let Ok(Some(name)) =
            crate::provider_factory::provider_sandbox_name_checked(cfg, &row.worktree, env_name)
        {
            out.insert(name);
        }
    }
    out
}

pub(crate) fn json(cfg: &Config) -> serde_json::Value {
    let db = thegn_core::db::Db::open_read_only_current().ok().flatten();
    let (journal, journal_error) = match sync_journal::list_at(&sync_journal::dir()) {
        Ok(journal) => (journal, None),
        Err(error) => (Vec::new(), Some(clean(&error.to_string()))),
    };
    let mut rows = json_with(
        cfg,
        db.as_ref(),
        &thegn_svc::vps::registry::list(),
        thegn_core::util::now(),
        &journal,
    );
    let rows_empty = matches!(rows.as_array(), Some(rows) if rows.is_empty());
    if let Some(error) = journal_error
        && let Some(rows) = rows.as_array_mut()
    {
        for row in rows.iter_mut() {
            if row.get("env").is_some() {
                row["sync_journal_error"] = serde_json::Value::String(error.clone());
            }
        }
        rows.push(serde_json::json!({
            "warning": "sync journal unavailable",
            "sync_journal_error": error,
        }));
    }
    if let Some(warning) = channel_warning(
        crate::channel_state::clamped(thegn_core::channel::Feature::Providers),
        rows_empty,
    ) && let Some(rows) = rows.as_array_mut()
    {
        rows.push(serde_json::json!({
            "warning": warning,
            "sync_journal_error": "",
        }));
    }
    rows
}

pub(crate) fn json_with(
    cfg: &Config,
    db: Option<&thegn_core::db::Db>,
    records: &[VpsRecord],
    now: i64,
    journal: &[SyncJournalEntry],
) -> serde_json::Value {
    let controller = controller_id_readable();
    let rows = remote_envs(cfg)
        .into_iter()
        .map(|(name, kind)| {
            let env = &cfg.env[name];
            let pc = &env.provider;
            let provider = if kind == EnvProviderKind::Vercel { "vercel" } else { "sprites" };
            let ledger = ledger_names_for(records, provider);
            let count = |state: &str| ledger.iter().filter(|(_, s, _)| s == state).count();
            let pool_spares = match db {
                Some(db) => ledger
                    .iter()
                    .filter_map(|(n, _, _)| db.pool_spare_by_name(n).ok().flatten())
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };
            let pool_states = pool_spares
                .iter()
                .map(|spare| spare.state.as_str())
                .collect::<Vec<_>>();
            let (pool_ready, pool_provisioning, pool_other) = pool_counts(&pool_states);
            let stale = ledger
                .iter()
                .filter(|(_, s, at)| s == "creating" && now.saturating_sub(*at) > STALE_CREATING_SECS)
                .count();
            let unbound: Vec<String> = match db {
                Some(db) => {
                    let bound = bound_names(cfg, db, name);
                    ledger
                        .iter()
                        .map(|(n, _, _)| n)
                        .filter(|n| !bound.contains(*n))
                        .filter(|n| !matches!(db.pool_spare_by_name(n), Ok(Some(_))))
                        .map(|n| clean(n))
                        .collect()
                }
                None => Vec::new(),
            };
            let sandbox = env_sandbox(cfg, env);
            let placement = env.placement.as_str();
            let placement_warning = if env.placement
                != thegn_core::config::PlacementMode::Provider
            {
                format!(
                    "[env.{name}] has a provider table but placement = \"{placement}\": worktrees on it run on this machine, not in a sandbox; set placement = \"provider\""
                )
            } else {
                String::new()
            };
            let credential_free = kind.credential_free();
            let implicit =
                kind == EnvProviderKind::Sprites && !pc.model_credentials_is_explicit();
            let mode = if implicit {
                thegn_core::config::ModelCredentials::None
            } else {
                pc.model_credentials
            };
            let grant = thegn_core::provider_credentials::model_grant(
                kind,
                mode,
            );
            let forwarded: Vec<String> = if credential_free {
                Vec::new()
            } else {
                let forwarded = sandbox
                    .passthrough_env_remote()
                    .into_iter();
                let forwarded = if grant.env_keys {
                    forwarded.collect::<Vec<_>>()
                } else {
                    thegn_core::provider_credentials::strip_model_env(forwarded.collect())
                        .0
                };
                forwarded
                    .into_iter()
                    .map(|(k, _)| clean(&k))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            let login_upload = if credential_free {
                mode.as_str().to_string()
            } else if implicit {
                "unset".to_string()
            } else if kind == EnvProviderKind::Sprites && !grant.login_files {
                "none".to_string()
            } else {
                let agents = login_agents(cfg, env);
                if agents.is_empty() { "none".to_string() } else { clean(&agents.join(",")) }
            };
            let credentials_warning = if implicit {
                format!(
                    "credentials are not forwarded until you approve them on this project's first launch; set model_credentials = \"upload\" in [env.{name}.provider] to forward them, or \"none\" to never send them"
                )
            } else {
                String::new()
            };
            let sync_pending = journal
                .iter()
                .filter(|entry| entry.env_name == name.as_str())
                .map(|entry| {
                    let (state, reason) = match &entry.state {
                        SyncState::Uploaded => ("uploaded", String::new()),
                        SyncState::Pulling => ("pulling", String::new()),
                        SyncState::Failed { reason } => ("failed", clean(reason)),
                    };
                    serde_json::json!({
                        "worktree": clean_basename(&entry.worktree),
                        "state": state,
                        "reason": reason,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "env": clean(name),
                "provider": provider,
                "placement": placement,
                "placement_warning": placement_warning,
                "sandbox_agents": sandbox_agents(cfg, env),
                "max_instances": pc.max_instances,
                "max_lifetime_secs": pc.max_lifetime_secs,
                "token_resolves": crate::secret::resolve_present_cached(&crate::env_ui::effective_token_env(pc)),
                "controller_id": controller,
                "ledger": {
                    "ready": count("ready"),
                    "creating": count("creating"),
                    "stale_creating": stale,
                },
                "pool_ready": pool_ready,
                "pool_provisioning": pool_provisioning,
                "pool_other": pool_other,
                "pool_size_config": cfg.lifecycle.pool.size,
                "review": if kind == EnvProviderKind::Sprites {
                    SPRITES_REVIEW_COMMAND
                } else {
                    ""
                },
                "unbound": unbound,
                "forwarded_env": forwarded,
                "git_token": !credential_free && sandbox.git.admits_git_token(),
                "login_upload": login_upload,
                "credentials_warning": credentials_warning,
                "sync_pending": sync_pending,
                "sync_journal_error": "",
            })
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(rows)
}

pub(crate) fn report(cfg: &Config) {
    outln!("");
    outln!("Remote live readiness");
    let rows = json(cfg);
    let rows = rows.as_array().cloned().unwrap_or_default();
    let preamble = report_preamble(&rows);
    for line in &preamble {
        outln!("{line}");
    }
    if rows.is_empty() {
        return;
    }
    let yn = |v: &serde_json::Value| {
        if v.as_bool().unwrap_or(false) {
            "yes"
        } else {
            "NO"
        }
    };
    for row in rows {
        if row["warning"].is_string() {
            continue;
        }
        let s = |k: &str| row[k].as_str().unwrap_or("").to_string();
        outln!("  {:<12} {}", s("env"), s("provider"));
        outln!("               placement: {}", s("placement"));
        if !s("placement_warning").is_empty() {
            outln!("               warning: {}", s("placement_warning"));
        }
        outln!(
            "               token resolves: {} · controller id readable: {}",
            yn(&row["token_resolves"]),
            yn(&row["controller_id"])
        );
        let l = &row["ledger"];
        outln!(
            "               ledger: {} ready, {} creating ({} stale)",
            l["ready"],
            l["creating"],
            l["stale_creating"]
        );
        if let Some(line) = warm_pool_report_line(
            s("provider").as_str(),
            row["pool_ready"].as_u64().unwrap_or(0) as usize,
            row["pool_provisioning"].as_u64().unwrap_or(0) as usize,
            row["pool_other"].as_u64().unwrap_or(0) as usize,
            row["pool_size_config"].as_u64().unwrap_or(0) as usize,
        ) {
            outln!("{line}");
        }
        if !s("review").is_empty() {
            outln!(
                "               review: {} (sprites on the account this host did not record; read-only)",
                s("review")
            );
        }
        let pending = row["sync_pending"].as_array().cloned().unwrap_or_default();
        if !pending.is_empty() {
            let failed = pending
                .iter()
                .filter(|entry| entry["state"].as_str() == Some("failed"))
                .count();
            outln!("               {}", pending_line(pending.len(), failed));
            for entry in pending
                .iter()
                .filter(|entry| entry["state"].as_str() == Some("failed"))
            {
                outln!(
                    "               pull-back failed: {}: {}",
                    entry["worktree"].as_str().unwrap_or(""),
                    entry["reason"].as_str().unwrap_or("")
                );
            }
        }
        let names = |k: &str| -> Vec<String> {
            row[k]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let unbound = names("unbound");
        if !unbound.is_empty() {
            outln!(
                "               not bound to any worktree: {} (they may still bill; thegn env reap --provider {} lists them)",
                unbound.join(", "),
                s("provider")
            );
        }
        let forwarded = names("forwarded_env");
        if forwarded.is_empty() && s("provider") == "vercel" {
            outln!("               forwarded env: none (credential-free provider)");
        } else if forwarded.is_empty() {
            outln!("               forwarded env: none");
        } else {
            outln!("               forwarded env: {}", forwarded.join(", "));
        }
        outln!(
            "               git token forwarded: {} · login upload: {}",
            yn(&row["git_token"]),
            s("login_upload")
        );
        let agents = names("sandbox_agents");
        outln!(
            "               sandbox agents: {} (provisioning installs them; a missing CLI makes agent run exit 127)",
            if agents.is_empty() {
                "none".to_string()
            } else {
                agents.join(", ")
            }
        );
        outln!(
            "               caps: max_instances {} · max_lifetime_secs {}",
            cap_display(row["max_instances"].as_u64().unwrap_or(0)),
            cap_display(row["max_lifetime_secs"].as_u64().unwrap_or(0))
        );
        if !s("credentials_warning").is_empty() {
            outln!("               warning: {}", s("credentials_warning"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::testenv::{EnvGuard, IsolatedState};

    fn rec(name: &str, provider: &str, state: &str, created_at: i64) -> VpsRecord {
        VpsRecord {
            name: name.into(),
            provider: provider.into(),
            state: state.into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at,
            owner: String::new(),
            account: String::new(),
        }
    }

    fn cfg_with(provider: &str) -> Config {
        let mut cfg = Config::default();
        let mut env = thegn_core::config::EnvConfig {
            placement: thegn_core::config::PlacementMode::Provider,
            ..Default::default()
        };
        env.provider.provider = provider.into();
        cfg.env.insert("remote".into(), env);
        cfg
    }

    #[test]
    fn pool_counts_table() {
        type Case = (&'static [&'static str], (usize, usize, usize));
        let cases: &[Case] = &[
            (&[], (0, 0, 0)),
            (&["ready"], (1, 0, 0)),
            (&["provisioning", "provisioning"], (0, 2, 0)),
            (&["claiming", "claimed", "quarantined"], (0, 0, 3)),
            (&["ready", "provisioning", "claimed"], (1, 1, 1)),
        ];
        for (states, expected) in cases {
            assert_eq!(pool_counts(states), *expected);
        }
    }

    #[test]
    fn json_reports_ready_pool_spare_from_ledger_name() {
        let _state = IsolatedState::new();
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.insert_pool_spare("spare-1", "/repo", "remote").unwrap();
        db.set_pool_spare_ready("spare-1", None, "lock").unwrap();
        let mut cfg = cfg_with("sprites");
        cfg.lifecycle.pool.size = 2;
        let records = [rec("spare-1", "sprites", "ready", 0)];

        let out = json_with(&cfg, Some(&db), &records, 0, &[]);
        let row = &out.as_array().unwrap()[0];
        assert_eq!(row["pool_ready"], 1);
        assert_eq!(row["pool_provisioning"], 0);
        assert_eq!(row["pool_other"], 0);
        assert_eq!(row["pool_size_config"], 2);
    }

    #[test]
    fn warm_pool_report_line_for_vercel() {
        assert_eq!(
            warm_pool_report_line("vercel", 0, 0, 0, 0),
            Some("  warm pool: not used for vercel".into())
        );
    }

    #[test]
    fn rows_carry_placement_agents_and_caps() {
        let mut cfg = cfg_with("sprites");
        let env = cfg.env.get_mut("remote").unwrap();
        env.placement = thegn_core::config::PlacementMode::Local;
        env.sandbox.home = Some(thegn_core::config::HomeOverlay {
            agents: Some(vec!["codex".into(), "claude".into()]),
            ..Default::default()
        });
        env.provider.max_instances = 3;
        env.provider.max_lifetime_secs = 900;

        let rows = json_with(&cfg, None, &[], 0, &[]);
        let row = &rows.as_array().unwrap()[0];

        assert_eq!(row["placement"].as_str(), Some("local"));
        assert!(row["placement_warning"].is_string());
        assert_eq!(
            row["sandbox_agents"],
            serde_json::json!(["codex", "claude"])
        );
        assert_eq!(row["max_instances"].as_u64(), Some(3));
        assert_eq!(row["max_lifetime_secs"].as_u64(), Some(900));
    }

    #[test]
    fn placement_warning_only_off_provider() {
        let mut cfg = cfg_with("sprites");
        let rows = json_with(&cfg, None, &[], 0, &[]);
        let provider = &rows.as_array().unwrap()[0];
        assert_eq!(provider["placement_warning"], "");

        cfg.env.get_mut("remote").unwrap().placement = thegn_core::config::PlacementMode::Ssh;
        let rows = json_with(&cfg, None, &[], 0, &[]);
        let remote = &rows.as_array().unwrap()[0];
        assert_eq!(
            remote["placement_warning"],
            "[env.remote] has a provider table but placement = \"ssh\": worktrees on it run on this machine, not in a sandbox; set placement = \"provider\""
        );
    }

    #[test]
    fn channel_warning_table() {
        assert_eq!(channel_warning(false, false), None);
        assert_eq!(channel_warning(false, true), None);
        assert_eq!(channel_warning(true, false), None);
        assert_eq!(
            channel_warning(true, true),
            Some("dev-only provider envs (not Sprites or Vercel) are configured but this instance runs on the stable channel, which ignores them; use the dev instance (THEGN_CHANNEL=dev)".into())
        );
    }

    #[test]
    fn report_skips_the_empty_line_when_clamped() {
        let warning = channel_warning(true, true).unwrap();
        let rows = vec![serde_json::json!({
            "warning": warning,
            "sync_journal_error": "",
        })];
        let rendered = report_preamble(&rows).join("\n");

        assert!(rendered.contains("provider envs (not Sprites or Vercel) are configured"));
        assert!(!rendered.contains("no sprites or vercel envs configured"));
    }

    fn journal_entry(worktree: &str, env_name: &str, state: SyncState) -> SyncJournalEntry {
        SyncJournalEntry {
            version: sync_journal::JOURNAL_VERSION,
            worktree: worktree.into(),
            env_name: env_name.into(),
            provider: "sprites".into(),
            sandbox: "sandbox".into(),
            workdir: "/workspace".into(),
            baseline: std::collections::BTreeMap::new(),
            state,
            uploaded_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn unbound_is_ledger_minus_spares_and_shape_is_stable() {
        let _state = IsolatedState::new();
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.insert_pool_spare("spare-1", "/repo", "remote").unwrap();
        let records = vec![
            rec("spare-1", "sprites", "ready", 0),
            rec("orphan-1", "sprites", "ready", 0),
            rec("stale-1", "sprites", "creating", 0),
            rec("other", "vercel", "ready", 0),
        ];
        let out = json_with(&cfg_with("sprites"), Some(&db), &records, 10_000, &[]);
        let row = &out.as_array().unwrap()[0];
        let keys: BTreeSet<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "env",
                "provider",
                "placement",
                "placement_warning",
                "sandbox_agents",
                "max_instances",
                "max_lifetime_secs",
                "token_resolves",
                "controller_id",
                "ledger",
                "pool_ready",
                "pool_provisioning",
                "pool_other",
                "pool_size_config",
                "review",
                "unbound",
                "forwarded_env",
                "git_token",
                "login_upload",
                "credentials_warning",
                "sync_pending",
                "sync_journal_error"
            ])
        );
        let unbound: BTreeSet<&str> = row["unbound"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(unbound, BTreeSet::from(["orphan-1", "stale-1"]));
        assert_eq!(row["ledger"]["ready"], 2);
        assert_eq!(row["ledger"]["creating"], 1);
        assert_eq!(row["ledger"]["stale_creating"], 1);
    }

    #[test]
    fn review_hint_is_sprites_only() {
        let _state = IsolatedState::new();
        let mut cfg = cfg_with("sprites");
        let mut vercel = thegn_core::config::EnvConfig {
            placement: thegn_core::config::PlacementMode::Provider,
            ..Default::default()
        };
        vercel.provider.provider = "vercel".into();
        cfg.env.insert("vercel".into(), vercel);

        let out = json_with(&cfg, None, &[], 0, &[]);
        let rows = out.as_array().unwrap();
        let sprites = rows
            .iter()
            .find(|row| row["provider"] == "sprites")
            .unwrap();
        let vercel = rows.iter().find(|row| row["provider"] == "vercel").unwrap();

        assert_eq!(sprites["review"], SPRITES_REVIEW_COMMAND);
        assert_eq!(vercel["review"], "");
    }

    #[test]
    fn vercel_forwards_nothing_and_unset_sprites_waits_for_consent() {
        let temp = tempfile::tempdir().unwrap();
        let mut vars: Vec<(String, String)> = thegn_core::testenv::state_env_all(temp.path())
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.display().to_string()))
            .collect();
        vars.push((
            "THEGN_DIR".into(),
            temp.path().join(".thegn").display().to_string(),
        ));
        vars.push(("ANTHROPIC_API_KEY".into(), "sentinel".into()));
        let pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let _env = EnvGuard::set(&pairs);
        let mut cfg = cfg_with("vercel");
        cfg.sandbox.env_passthrough = vec!["ANTHROPIC_API_KEY".into()];
        let out = json_with(&cfg, None, &[], 0, &[]);
        assert_eq!(out[0]["forwarded_env"], serde_json::json!([]));
        assert_eq!(out[0]["git_token"], false);
        assert!(!out.to_string().contains("sentinel"));

        let mut sprites = cfg_with("sprites");
        sprites.sandbox.env_passthrough = vec!["ANTHROPIC_API_KEY".into()];
        sprites.sandbox.home.agents = vec!["claude".into()];
        let out = json_with(&sprites, None, &[], 0, &[]);
        assert_eq!(out[0]["forwarded_env"], serde_json::json!([]));
        assert_eq!(out[0]["login_upload"], "unset");
        assert!(
            out[0]["credentials_warning"]
                .as_str()
                .unwrap()
                .contains("model_credentials = \"upload\"")
        );
        assert!(
            out[0]["credentials_warning"]
                .as_str()
                .unwrap()
                .contains("first launch")
        );
        assert!(!out.to_string().contains("sentinel"));
    }

    #[test]
    fn sprites_upload_forwards_model_env_and_reports_upload() {
        let temp = tempfile::tempdir().unwrap();
        let mut vars: Vec<(String, String)> = thegn_core::testenv::state_env_all(temp.path())
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.display().to_string()))
            .collect();
        vars.push((
            "THEGN_DIR".into(),
            temp.path().join(".thegn").display().to_string(),
        ));
        vars.push(("ANTHROPIC_API_KEY".into(), "sentinel-b167".into()));
        let pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let _env = EnvGuard::set(&pairs);
        let mut cfg = cfg_with("sprites");
        cfg.sandbox.env_passthrough = vec!["ANTHROPIC_API_KEY".into()];
        cfg.sandbox.home.agents = vec!["claude".into()];
        let provider = &mut cfg.env.get_mut("remote").unwrap().provider;
        provider.model_credentials = thegn_core::config::ModelCredentials::Upload;
        provider.mark_model_credentials_explicit();
        let out = json_with(&cfg, None, &[], 0, &[]);
        assert!(
            out[0]["forwarded_env"]
                .to_string()
                .contains("ANTHROPIC_API_KEY")
        );
        assert_eq!(out[0]["login_upload"], "claude");
        assert!(!out.to_string().contains("sentinel-b167"));
    }

    #[test]
    fn sprites_none_withholds_model_env_and_has_no_default_warning() {
        let temp = tempfile::tempdir().unwrap();
        let mut vars: Vec<(String, String)> = thegn_core::testenv::state_env_all(temp.path())
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.display().to_string()))
            .collect();
        vars.push((
            "THEGN_DIR".into(),
            temp.path().join(".thegn").display().to_string(),
        ));
        vars.push(("ANTHROPIC_API_KEY".into(), "sentinel-b167".into()));
        let pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let _env = EnvGuard::set(&pairs);
        let mut cfg = cfg_with("sprites");
        cfg.sandbox.env_passthrough = vec!["ANTHROPIC_API_KEY".into()];
        let provider = &mut cfg.env.get_mut("remote").unwrap().provider;
        provider.model_credentials = thegn_core::config::ModelCredentials::None;
        provider.mark_model_credentials_explicit();
        let out = json_with(&cfg, None, &[], 0, &[]);
        assert_eq!(out[0]["forwarded_env"], serde_json::json!([]));
        assert_eq!(out[0]["login_upload"], "none");
        assert_eq!(out[0]["credentials_warning"], "");
        assert!(!out.to_string().contains("sentinel-b167"));
    }

    #[test]
    fn doctor_lists_failed_sync_entries_for_their_env() {
        let _state = IsolatedState::new();
        let cfg = cfg_with("sprites");
        let journal = [journal_entry(
            "/private/worktrees/remote-one",
            "remote",
            SyncState::Failed {
                reason: "network\nretry".into(),
            },
        )];

        let out = json_with(&cfg, None, &[], 0, &journal);

        assert_eq!(
            out[0]["sync_pending"],
            serde_json::json!([{
                "worktree": "remote-one",
                "state": "failed",
                "reason": "network�retry"
            }])
        );
    }

    #[test]
    fn doctor_pending_line_names_thegn_env_sync_retry() {
        assert!(pending_line(2, 1).contains("thegn env sync-retry"));
    }

    #[test]
    fn doctor_ignores_entries_for_other_envs() {
        let _state = IsolatedState::new();
        let mut cfg = cfg_with("sprites");
        let mut other = thegn_core::config::EnvConfig {
            placement: thegn_core::config::PlacementMode::Provider,
            ..Default::default()
        };
        other.provider.provider = "vercel".into();
        cfg.env.insert("other".into(), other);
        let journal = [journal_entry(
            "/private/worktrees/other",
            "other",
            SyncState::Uploaded,
        )];

        let out = json_with(&cfg, None, &[], 0, &journal);

        let rows = out.as_array().unwrap();
        let remote = rows.iter().find(|row| row["env"] == "remote").unwrap();
        assert_eq!(remote["sync_pending"], serde_json::json!([]));
        let other = rows.iter().find(|row| row["env"] == "other").unwrap();
        assert_eq!(other["sync_pending"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn doctor_rows_never_contain_paths_beyond_the_basename() {
        let _state = IsolatedState::new();
        let cfg = cfg_with("sprites");
        let journal = [journal_entry(
            "/private/attacker/worktrees/remote-one",
            "remote",
            SyncState::Pulling,
        )];

        let out = json_with(&cfg, None, &[], 0, &journal);

        assert_eq!(out[0]["sync_pending"][0]["worktree"], "remote-one");
        assert!(
            !out[0]["sync_pending"][0]["worktree"]
                .as_str()
                .unwrap()
                .contains('/')
        );
    }
}
