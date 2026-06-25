//! `superzej config <action>` — inspect/edit the effective (layered) config.

use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;
use superzej_core::config::{self, Config};
use superzej_core::{msg, outln, util};

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
    /// Strictly validate the config file; non-zero exit on any problem.
    Validate,
    /// Print the JSON schema for editor autocomplete and validation.
    Schema,
}

pub fn run(cfg: &Config, action: Action, path: PathBuf) -> Result<()> {
    match action {
        Action::Path => outln!("{}", path.display()),
        Action::Show { json } => show(cfg, json)?,
        Action::Get { key, json } => get(cfg, &key, json)?,
        Action::Edit => edit(&path)?,
        Action::Validate => validate(&path)?,
        Action::Schema => {
            let schema = schemars::schema_for!(Config);
            outln!("{}", serde_json::to_string_pretty(&schema).unwrap());
        }
    }
    Ok(())
}

fn show(cfg: &Config, json: bool) -> Result<()> {
    if json {
        outln!("{}", serde_json::to_string_pretty(cfg)?);
    } else {
        superzej_core::out!("{}", toml::to_string_pretty(cfg)?);
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
