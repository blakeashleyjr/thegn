//! Effective settings for the Info screen, each with the layer that set it.

use crate::config::{Config, ProcessEnv, SandboxConfig};
use crate::config_resolve::Approvals;
use crate::info_model::{Field, Source, clean, config_value_text, origin_of};
use crate::store::{RepoTrustStore, WorkspaceStore};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const ENV_NAME: &str = "env.name";
const ENV_PLACEMENT: &str = "env.placement";
const ENV_PROVIDER: &str = "env.provider";
const ENV_DATA: &str = "env.data";

fn json_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn explain_source(
    config_path: Option<&PathBuf>,
    key: &str,
    loaded: Value,
) -> Result<Source, String> {
    let origin = crate::config_resolve::explain(&ProcessEnv, &[], config_path.cloned(), key)?;
    Ok(if origin.value != loaded {
        Source::LoadedDiffers
    } else {
        Source::from_trust(origin.origin)
    })
}

fn sandbox_value(sandbox: &SandboxConfig, key: &str) -> Value {
    match key {
        "enabled" => json_value(&sandbox.enabled),
        "backend" => json_value(&sandbox.backend),
        "profile" => json_value(&sandbox.profile),
        "network" => json_value(&sandbox.network),
        _ => Value::Null,
    }
}

fn sandbox_field(
    cfg: &Config,
    config_path: Option<&PathBuf>,
    environment: &crate::env::Environment,
    key: &str,
    label: &str,
    repo_sandbox: &SandboxConfig,
    repo_source: &str,
    profile_selection: Option<crate::config::SandboxProfile>,
) -> Result<Field, String> {
    let loaded = sandbox_value(&cfg.sandbox, key);
    let base = explain_source(config_path, &format!("sandbox.{key}"), loaded.clone())?;

    let mut profile_sandbox = cfg.sandbox.clone();
    if let Some(profile) = cfg.active_profile() {
        profile.sandbox.clone().apply(&mut profile_sandbox);
    }

    let repo_value = sandbox_value(repo_sandbox, key);
    let environment_value = sandbox_value(&environment.sandbox, key);
    let stages = vec![
        (base, loaded),
        (
            Source::ProfileTable(cfg.profile.clone()),
            sandbox_value(&profile_sandbox, key),
        ),
        (Source::Repo(repo_source.to_string()), repo_value.clone()),
        (
            Source::Env(environment.name.clone()),
            environment_value.clone(),
        ),
    ];

    let mut source = origin_of(&stages).unwrap_or(Source::Builtin);
    if key == "backend"
        && cfg
            .env
            .get(&environment.name)
            .is_none_or(|env| env.sandbox.backend.is_none())
        && environment_value != repo_value
    {
        source = Source::Placement;
    }
    if key == "profile"
        && profile_selection.is_some_and(|profile| json_value(&profile) == environment_value)
    {
        source = Source::WorktreeDb;
    }

    Ok(Field::value(
        &format!("sandbox.{key}"),
        label,
        config_value_text(&format!("sandbox.{key}"), environment_value),
        source,
    ))
}

fn value_text(value: Value) -> String {
    match value {
        Value::String(value) => value,
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn provider_text(env: &crate::config::EnvConfig) -> String {
    let mut value = json!({
        "provider": env.provider.provider.clone(),
        "exec": json_value(&env.provider.exec),
        "connect": json_value(&env.provider.connect),
        "transport": json_value(&env.provider.transport),
        "template": env.provider.template.clone(),
        "failover": json_value(&env.failover),
    });
    crate::redact::redact_json(&mut value);
    [
        "provider",
        "exec",
        "connect",
        "transport",
        "template",
        "failover",
    ]
    .into_iter()
    .filter_map(|key| value.get(key).cloned().map(|value| (key, value)))
    .map(|(key, value)| format!("{key}={}", value_text(value)))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Effective settings for `worktree` (the repo root for a workspace target),
/// each with its layer. This reads the state DB and config files but spawns
/// nothing. The second return value contains explanatory notes that do not
/// belong in the settings table.
pub fn settings(
    cfg: &Config,
    config_path: Option<PathBuf>,
    db: &crate::db::Db,
    repo_root: &Path,
    worktree: &Path,
    environment: &crate::env::Environment,
) -> (Vec<Field>, Vec<String>) {
    let mut fields = Vec::new();
    let mut notes = Vec::new();
    let root = repo_root.to_string_lossy().into_owned();
    let wt = worktree.to_string_lossy().into_owned();
    let repo_source = crate::config_repo::discover_repo_overlay(repo_root)
        .selected()
        .map(|candidate| candidate.path.display().to_string())
        .unwrap_or_default();

    let approvals = match db.repo_trust_approved(&root) {
        Ok(approved) => Approvals::from_canonical(approved),
        Err(error) => {
            notes.push(format!(
                "could not read repo trust approvals: {error}; repo sandbox requests were denied"
            ));
            Approvals::deny_all()
        }
    };
    let repo_sandbox = cfg.repo_sandbox_resolved(repo_root, &approvals).sandbox;
    let profile_selection = match db.effective_env_selection(&wt, &root) {
        Ok(selection) => selection.profile,
        Err(error) => {
            notes.push(format!(
                "could not read worktree sandbox selection: {error}"
            ));
            None
        }
    };

    let base_path = config_path.as_ref();
    let worktree_env = match db.worktree_env(&wt) {
        Ok(value) => value,
        Err(error) => {
            notes.push(format!(
                "could not read worktree environment selection: {error}"
            ));
            None
        }
    };
    let workspace_env = match db.workspace_env(&root) {
        Ok(value) => value,
        Err(error) => {
            notes.push(format!(
                "could not read workspace environment selection: {error}"
            ));
            None
        }
    };
    let repo_env = cfg.repo_env_name(repo_root);
    let repo_env_won = !repo_env.is_empty()
        && worktree_env.as_deref() != Some(environment.name.as_str())
        && workspace_env.as_deref() != Some(environment.name.as_str())
        && repo_env == environment.name;
    let env_name_source = if worktree_env.as_deref() == Some(environment.name.as_str()) {
        Source::WorktreeDb
    } else if workspace_env.as_deref() == Some(environment.name.as_str()) {
        Source::WorkspaceDb
    } else if repo_env_won {
        Source::Repo(repo_source.clone())
    } else if worktree == repo_root && !cfg.sandbox.main_env.is_empty() {
        explain_source(
            base_path,
            "sandbox.main_env",
            json_value(&cfg.sandbox.main_env),
        )
        .unwrap_or(Source::Builtin)
    } else if !cfg.sandbox.default_env.is_empty() || environment.name == "default" {
        explain_source(
            base_path,
            "sandbox.default_env",
            json_value(&cfg.sandbox.default_env),
        )
        .unwrap_or(Source::Builtin)
    } else {
        Source::Builtin
    };
    let env_name = if environment.unresolved_selection {
        format!(
            "{} (no [env.{}]; runs locally)",
            environment.name, environment.name
        )
    } else {
        environment.name.clone()
    };
    let env_name_field = Field::value(ENV_NAME, "Environment", env_name, env_name_source);
    fields.push(if environment.unresolved_selection {
        env_name_field.warned()
    } else {
        env_name_field
    });

    fields.push(Field::value(
        ENV_PLACEMENT,
        "Placement",
        environment.placement.label(),
        Source::Env(environment.name.clone()),
    ));
    if let Some(env) = cfg.env.get(&environment.name)
        && !env.provider.provider.trim().is_empty()
    {
        fields.push(Field::value(
            ENV_PROVIDER,
            "Provider",
            provider_text(env),
            Source::Env(environment.name.clone()),
        ));
    }
    fields.push(Field::value(
        ENV_DATA,
        "Data",
        config_value_text("env.data", json_value(&environment.data)),
        Source::Env(environment.name.clone()),
    ));

    for (key, label) in [
        ("enabled", "Sandbox enabled"),
        ("backend", "Sandbox backend"),
        ("profile", "Sandbox profile"),
        ("network", "Sandbox network"),
    ] {
        match sandbox_field(
            cfg,
            base_path,
            environment,
            key,
            label,
            &repo_sandbox,
            &repo_source,
            profile_selection,
        ) {
            Ok(field) => fields.push(field),
            Err(error) => fields.push(Field::unknown(&format!("sandbox.{key}"), label, error)),
        }
    }

    for (key, label) in [
        ("default_env", "Default environment"),
        ("main_env", "Main environment"),
    ] {
        let loaded = json_value(match key {
            "default_env" => &cfg.sandbox.default_env,
            _ => &cfg.sandbox.main_env,
        });
        let value = match key {
            "default_env" => cfg.sandbox.default_env.clone(),
            _ => cfg.sandbox.main_env.clone(),
        };
        match explain_source(base_path, &format!("sandbox.{key}"), loaded) {
            Ok(source) => fields.push(if value.is_empty() {
                Field::unset(&format!("sandbox.{key}"), label, source)
            } else {
                Field::value(&format!("sandbox.{key}"), label, value, source)
            }),
            Err(error) => fields.push(Field::unknown(&format!("sandbox.{key}"), label, error)),
        }
    }
    if worktree == repo_root && !cfg.sandbox.main_env.is_empty() {
        notes.push(
            "new worktrees resolve default_env; main_env applies to the root checkout".into(),
        );
    }
    if !repo_env.is_empty() && !repo_env_won {
        notes.push(format!(
            "repo env `{repo_env}` was not applied (it would weaken the sandbox; see `thegn config explain sandbox --repo`)"
        ));
    }

    let agent_value = cfg.default_agent_name().map(str::to_string);
    if let Some(agent) = agent_value {
        match explain_source(base_path, "agents", json_value(&cfg.agents)) {
            Ok(source) => fields.push(Field::value(
                "agents.default",
                "Default agent",
                agent,
                source,
            )),
            Err(error) => fields.push(Field::unknown("agents.default", "Default agent", error)),
        }
    } else {
        fields.push(Field::value(
            "agents.default",
            "Default agent",
            "shell only",
            Source::Builtin,
        ));
    }

    let gate = cfg.repo_merge_queue(repo_root).gate_command;
    match cfg.workspace_overlay(repo_root) {
        crate::workspace_overlay::WorkspaceOverlay::Refused(refusal) => fields.push(
            Field::unknown(
                "merge_queue.gate_command",
                "Merge gate",
                format!("[workspace] block refused: {refusal}"),
            )
            .warned(),
        ),
        crate::workspace_overlay::WorkspaceOverlay::Selected { key, overlay }
            if overlay.merge_queue.gate_command.is_some() =>
        {
            fields.push(if gate.is_empty() {
                Field::unset(
                    "merge_queue.gate_command",
                    "Merge gate",
                    Source::Workspace(key.to_string()),
                )
            } else {
                Field::value(
                    "merge_queue.gate_command",
                    "Merge gate",
                    clean(&crate::log_redact::redact_text_line(&gate), 120),
                    Source::Workspace(key.to_string()),
                )
            })
        }
        _ => match explain_source(
            base_path,
            "merge_queue.gate_command",
            json_value(&cfg.merge_queue.gate_command),
        ) {
            Ok(source) => {
                let value = clean(&crate::log_redact::redact_text_line(&gate), 120);
                fields.push(if value.is_empty() {
                    Field::unset("merge_queue.gate_command", "Merge gate", source)
                } else {
                    Field::value("merge_queue.gate_command", "Merge gate", value, source)
                });
            }
            Err(error) => fields.push(Field::unknown(
                "merge_queue.gate_command",
                "Merge gate",
                error,
            )),
        },
    }

    match explain_source(base_path, "worktrees_dir", json_value(&cfg.worktrees_dir)) {
        Ok(source) => fields.push(Field::value(
            "worktrees_dir",
            "Worktrees directory",
            cfg.worktrees_dir.clone(),
            source,
        )),
        Err(error) => fields.push(Field::unknown(
            "worktrees_dir",
            "Worktrees directory",
            error,
        )),
    }

    let issues = cfg.repo_issues(Some(repo_root));
    let issue_value = if !issues.providers.is_empty() {
        issues
            .providers
            .iter()
            .map(|provider| provider.as_str())
            .collect::<Vec<_>>()
            .join(",")
    } else {
        issues.provider.as_str().to_string()
    };
    let issue_source = if json_value(&issues) != json_value(&cfg.issues) {
        Source::Repo(repo_source)
    } else {
        match explain_source(
            base_path,
            "issues.provider",
            json_value(&cfg.issues.provider),
        ) {
            Ok(source) => source,
            Err(error) => {
                fields.push(Field::unknown("issues.provider", "Issues provider", error));
                return (fields, notes);
            }
        }
    };
    fields.push(Field::value(
        "issues.provider",
        "Issues provider",
        issue_value,
        issue_source,
    ));

    (fields, notes)
}

#[cfg(test)]
mod info_resolve_tests;
