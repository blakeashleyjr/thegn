//! `thegn config <action>` — inspect/edit the effective (layered) config.

use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;
use thegn_core::config::{self, Config};
use thegn_core::{msg, outln, util};

/// The committed example, seeded on first `config edit`.
const EXAMPLE: &str = include_str!("../../../../config/config.toml.example");

/// Config subcommands, mirroring the legacy `ConfigAction`.
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Print the path to the config file.
    Path,
    /// Print the effective merged config (defaults < file < env < flags).
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Print a single value by dotted key (bare value; for scripts).
    Get {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Open the config file in $EDITOR (seeds from the example if missing).
    Edit,
    /// Set one dotted key (`config set sandbox.backend docker`) in the config
    /// file, preserving comments/formatting. The write counterpart to `get`.
    Set { key: String, value: String },
    /// Strictly validate the config file; non-zero exit on any problem.
    Validate,
    /// Print the JSON schema for editor autocomplete and validation.
    Schema,
    /// Explain how a key resolves: effective value, which layer set it, and (for
    /// `sandbox.*` with `--repo`) the trust clamp trace (denials + pending).
    Explain {
        key: String,
        /// Also show the repo `.thegn.*` clamp trace for this repo path.
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action, path: PathBuf) -> Result<()> {
    match action {
        Action::Path => outln!("{}", path.display()),
        Action::Show { json } => show(cfg, json)?,
        Action::Get { key, json } => get(cfg, &key, json)?,
        Action::Edit => edit(&path)?,
        Action::Set { key, value } => {
            thegn_core::config_write::set_key(&path, &key, &value)?;
            outln!("set {key} = {value:?} in {}", path.display());
        }
        Action::Validate => validate(&path)?,
        Action::Schema => {
            let schema = schemars::schema_for!(Config);
            outln!("{}", serde_json::to_string_pretty(&schema).unwrap());
        }
        Action::Explain { key, repo, json } => explain(cfg, &key, repo, json, path)?,
    }
    Ok(())
}

fn explain(cfg: &Config, key: &str, repo: Option<String>, json: bool, path: PathBuf) -> Result<()> {
    use thegn_core::config::ProcessEnv;
    use thegn_core::config_resolve;
    let origin = config_resolve::explain(&ProcessEnv, &[], Some(path), key);
    if json {
        let mut obj = serde_json::json!({
            "key": origin.key,
            "value": origin.value,
            "origin": origin.origin.as_str(),
        });
        if let Some(repo) = &repo {
            let (events, pending) = repo_clamp(cfg, repo, key);
            obj["clamped"] = serde_json::json!(events);
            obj["pending"] = serde_json::json!(pending);
        }
        outln!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }
    outln!("{} = {}", origin.key, origin.value);
    outln!("  set by: {}", origin.origin.as_str());
    for (layer, val) in &origin.trace {
        outln!("    {}: {val}", layer.as_str());
    }
    if let Some(repo) = &repo {
        let (events, pending) = repo_clamp(cfg, repo, key);
        if !events.is_empty() || !pending.is_empty() {
            outln!("  repo `.thegn.*` clamp ({repo}):");
            for line in events {
                outln!("    {line}");
            }
            for p in pending {
                outln!("    pending: {p}");
            }
        }
    }
    Ok(())
}

/// Repo-overlay clamp events + pending summaries filtered to a key prefix, using
/// the persisted trust approvals.
fn repo_clamp(cfg: &Config, repo: &str, key: &str) -> (Vec<String>, Vec<String>) {
    use thegn_core::config_resolve::{Approvals, summarize_events};
    use thegn_core::db::Db;
    use thegn_core::store::RepoTrustStore;
    let root = thegn_core::repo::main_worktree(std::path::Path::new(repo))
        .unwrap_or_else(|| PathBuf::from(repo));
    let approvals = Db::open()
        .ok()
        .and_then(|db| db.repo_trust_approved(&root.to_string_lossy()).ok())
        .map(Approvals::from_canonical)
        .unwrap_or_else(Approvals::deny_all);
    let resolved = cfg.repo_sandbox_resolved(&root, &approvals);
    let events = summarize_events(&resolved.events)
        .into_iter()
        .filter(|l| l.contains(key) || key == "sandbox")
        .collect();
    let pending = resolved
        .pending
        .into_iter()
        .filter(|p| p.key.contains(key) || key == "sandbox")
        .map(|p| format!("{}: {}", p.key, p.summary))
        .collect();
    (events, pending)
}

fn show(cfg: &Config, json: bool) -> Result<()> {
    if json {
        outln!("{}", serde_json::to_string_pretty(cfg)?);
    } else {
        thegn_core::out!("{}", toml::to_string_pretty(cfg)?);
    }
    Ok(())
}

fn get(cfg: &Config, key: &str, json: bool) -> Result<()> {
    match cfg.get_dotted(key) {
        Some(v) => {
            if json {
                outln!("{}", serde_json::to_string(&v)?);
            } else {
                outln!("{v}");
            }
            Ok(())
        }
        None => anyhow::bail!("unknown config key: {key}"),
    }
}

fn edit(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, EXAMPLE)?;
        msg::info(&format!("seeded {} from the example", path.display()));
    }
    let editor = util::editor();
    // CLI path: `thegn config edit` hands the terminal to $EDITOR, no event loop.
    #[expect(clippy::disallowed_methods)]
    let status = Command::new(util::shell())
        .arg("-lc")
        .arg(format!(
            "{editor} {}",
            util::sh_quote(&path.to_string_lossy())
        ))
        .status()?;
    if !status.success() {
        anyhow::bail!("editor exited with status {status}");
    }
    Ok(())
}

fn validate(path: &PathBuf) -> Result<()> {
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            outln!("no config file at {} — using defaults (ok)", path.display());
            return Ok(());
        }
    };
    let errs = config::validate_str(&body);
    if errs.is_empty() {
        outln!("{} ok", path.display());
        Ok(())
    } else {
        for e in &errs {
            msg::error(e);
        }
        anyhow::bail!("{} problem(s) in {}", errs.len(), path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_outputs_toml_and_json_without_panicking() {
        let cfg = Config::default();
        assert!(show(&cfg, false).is_ok());
        assert!(show(&cfg, true).is_ok());
    }

    #[test]
    fn get_known_and_unknown_keys() {
        let cfg = Config::default();
        assert!(get(&cfg, "picker", false).is_ok());
        assert!(get(&cfg, "picker", true).is_ok());
        assert!(get(&cfg, "nonexistent.key", false).is_err());
    }
}
