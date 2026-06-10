//! `superzej config <action>` — inspect and edit the effective configuration.
//!
//! `show`/`get` reflect the fully-layered config (defaults < file < env <
//! flags), so they're the truth-of-record for "what is superzej actually
//! using". `get` prints a bare value (no decoration) for scripts and the WASM
//! plugins, mirroring the `superzej theme` contract.

use crate::cli::ConfigAction;
use crate::config::{self, Config};
use crate::{msg, util};
use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

/// The committed example, seeded on first `config edit`.
const EXAMPLE: &str = include_str!("../../../../config/config.toml.example");

pub fn run(cfg: &Config, action: ConfigAction, path: PathBuf) -> Result<()> {
    match action {
        ConfigAction::Path => crate::outln!("{}", path.display()),
        ConfigAction::Show { json } | ConfigAction::List { json } => show(cfg, json)?,
        ConfigAction::Get { key, json } => get(cfg, &key, json)?,
        ConfigAction::Edit => edit(&path)?,
        ConfigAction::Validate => validate(&path)?,
        ConfigAction::Schema => {
            let schema = schemars::schema_for!(Config);
            crate::outln!("{}", serde_json::to_string_pretty(&schema).unwrap());
        }
    }
    Ok(())
}

fn show(cfg: &Config, json: bool) -> Result<()> {
    if json {
        crate::outln!("{}", serde_json::to_string_pretty(cfg)?);
    } else {
        // Effective config as TOML — round-trippable, copy-pasteable.
        crate::out!("{}", toml::to_string_pretty(cfg)?);
    }
    Ok(())
}

fn get(cfg: &Config, key: &str, json: bool) -> Result<()> {
    match cfg.get_dotted(key) {
        Some(v) => {
            if json {
                crate::outln!("{}", serde_json::to_string(&v)?);
            } else {
                crate::outln!("{v}");
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
            // No file is a valid state (all defaults).
            crate::outln!("no config file at {} — using defaults (ok)", path.display());
            return Ok(());
        }
    };
    let errs = config::validate_str(&body);
    if errs.is_empty() {
        crate::outln!("{} ok", path.display());
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
    use crate::config::{Config, LogLevel, Picker};

    #[test]
    fn show_outputs_toml_by_default() {
        let cfg = Config::default();
        // Just verify it doesn't panic - full output testing would require capturing stdout
        let result = show(&cfg, false);
        assert!(result.is_ok());
    }

    #[test]
    fn show_outputs_json_when_flag_set() {
        let cfg = Config::default();
        let result = show(&cfg, true);
        assert!(result.is_ok());
    }

    #[test]
    fn get_returns_known_key() {
        let cfg = Config::default();
        // Test getting a known key - "picker" should exist
        let result = get(&cfg, "picker", false);
        assert!(result.is_ok());
    }

    #[test]
    fn get_returns_unknown_key_error() {
        let cfg = Config::default();
        // Test getting an unknown key
        let result = get(&cfg, "nonexistent.key", false);
        assert!(result.is_err());
    }

    #[test]
    fn get_returns_json_when_flag_set() {
        let cfg = Config::default();
        let result = get(&cfg, "picker", true);
        assert!(result.is_ok());
    }
}
