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

fn clean(value: &str) -> String {
    crate::merge_sweep::safe_display(value)
}

fn clean_basename(value: &str) -> String {
    let trimmed = value.trim_end_matches(|c| c == '/' || c == '\\');
    let basename = std::path::Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .or_else(|| trimmed.rsplit(|c| c == '/' || c == '\\').next())
        .unwrap_or("");
    clean(basename)
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

fn ledger_names_for(records: &[VpsRecord], provider: &str) -> Vec<(String, String, i64)> {
    records
        .iter()
        .filter(|r| r.provider == provider)
        .map(|r| (r.name.clone(), r.state.clone(), r.created_at))
        .collect()
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
    if let Some(error) = journal_error {
        if let Some(rows) = rows.as_array_mut() {
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
            let credential_free = kind.credential_free();
            let mode = if kind == EnvProviderKind::Sprites
                && !pc.model_credentials_is_explicit()
            {
                thegn_core::config::ModelCredentials::Upload
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
            } else if kind == EnvProviderKind::Sprites && !grant.login_files {
                "none".to_string()
            } else {
                let agents = login_agents(cfg, env);
                if agents.is_empty() { "none".to_string() } else { clean(&agents.join(",")) }
            };
            let credentials_warning = if kind == EnvProviderKind::Sprites
                && !pc.model_credentials_is_explicit()
            {
                format!(
                    "credentials are forwarded by default on this env; set model_credentials = \"none\" in [env.{name}.provider] to turn them off"
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
                "token_resolves": crate::secret::resolve_present_cached(&crate::env_ui::effective_token_env(pc)),
                "controller_id": controller,
                "ledger": {
                    "ready": count("ready"),
                    "creating": count("creating"),
                    "stale_creating": stale,
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
    if rows.is_empty() {
        outln!("  (no sprites or vercel envs configured)");
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
        if let Some(warning) = row["warning"].as_str() {
            let error = row["sync_journal_error"].as_str().unwrap_or("");
            if error.is_empty() {
                outln!("  warning: {warning}");
            } else {
                outln!("  warning: {warning}: {error}");
            }
            continue;
        }
        let s = |k: &str| row[k].as_str().unwrap_or("").to_string();
        outln!("  {:<12} {}", s("env"), s("provider"));
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
        let pending = row["sync_pending"].as_array().cloned().unwrap_or_default();
        if !pending.is_empty() {
            let failed = pending
                .iter()
                .filter(|entry| entry["state"].as_str() == Some("failed"))
                .count();
            outln!(
                "               pull-back pending: {} ({} failed); closing or removing the worktree retries it and keeps the sandbox until it succeeds",
                pending.len(),
                failed
            );
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
                "token_resolves",
                "controller_id",
                "ledger",
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
    fn vercel_forwards_nothing_and_no_value_reaches_the_json() {
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
        assert!(
            out[0]["forwarded_env"]
                .to_string()
                .contains("ANTHROPIC_API_KEY")
        );
        assert_eq!(out[0]["login_upload"], "claude");
        assert!(
            out[0]["credentials_warning"]
                .as_str()
                .unwrap()
                .contains("model_credentials = \"none\"")
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
    fn doctor_ignores_entries_for_other_envs() {
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

        for row in out.as_array().unwrap() {
            assert_eq!(row["sync_pending"], serde_json::json!([]));
        }
    }

    #[test]
    fn doctor_rows_never_contain_paths_beyond_the_basename() {
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
