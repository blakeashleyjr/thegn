//! `thegn plugin` — inspect the plugin set without launching the compositor.
//!
//! - `list` — every discovered plugin (`[[plugins]]` config entries plus
//!   `<config_dir>/plugins/*/plugin.toml`), with mode, enabled state and
//!   negotiation status. Reads the same loader the running UI uses, so what
//!   this prints is what the compositor will start.
//! - `check` — full validation (api compatibility, command presence,
//!   contribution acceptance, id clashes, unparseable manifests); exits
//!   non-zero when any *enabled* plugin fails, so it fits a hook.
//!
//! There is no `restart` verb: crashed resident plugins restart themselves
//! with backoff, and deliberate restarts ride config hot-reload.

use anyhow::Result;
use clap::Subcommand;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_svc::plugin::{check_specs, discover, negotiate};

#[derive(Subcommand, Clone)]
pub enum Action {
    /// List discovered plugins with mode, enabled state and negotiation status.
    List {
        /// Emit machine-readable JSON instead of the text table.
        #[arg(long)]
        json: bool,
    },
    /// Validate every enabled plugin; exits non-zero on any problem.
    Check,
}

/// The directory plugin directories live under (`plugin.toml` per subdir):
/// the config file's own directory.
fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
}

pub fn run(cfg: &Config, action: Action, config_path: &Path) -> Result<()> {
    let dir = config_dir(config_path);
    match action {
        Action::List { json } => {
            let loaded = discover(cfg, &dir);
            if json {
                let rows: Vec<serde_json::Value> = loaded
                    .iter()
                    .map(|p| {
                        let status = match negotiate(&p.spec) {
                            Ok(n) if n.unsupported_contributions.is_empty() => "ok".to_string(),
                            Ok(n) => format!(
                                "partial ({} unsupported contribution(s))",
                                n.unsupported_contributions.len()
                            ),
                            Err(e) => e,
                        };
                        serde_json::json!({
                            "id": p.spec.manifest.id.as_str(),
                            "name": p.spec.manifest.name,
                            "version": p.spec.manifest.version,
                            "mode": p.spec.mode,
                            "enabled": p.spec.enabled,
                            "source": p.dir.as_ref().map(|d| d.display().to_string()),
                            "status": status,
                        })
                    })
                    .collect();
                super::emit_json(&rows)?;
                return Ok(());
            }
            if loaded.is_empty() {
                outln!(
                    "no plugins configured (add [[plugins]] or {}/plugins/<dir>/plugin.toml)",
                    dir.display()
                );
                return Ok(());
            }
            for p in &loaded {
                let status = match negotiate(&p.spec) {
                    Ok(n) if n.unsupported_contributions.is_empty() => "ok".to_string(),
                    Ok(n) => format!(
                        "partial ({} unsupported contribution(s))",
                        n.unsupported_contributions.len()
                    ),
                    Err(e) => e,
                };
                let source = p
                    .dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "[[plugins]]".into());
                outln!(
                    "{:<20} {:<10} {:<9} {:<30} {}",
                    p.spec.manifest.id.as_str(),
                    format!("{:?}", p.spec.mode).to_lowercase(),
                    if p.spec.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    status,
                    source
                );
            }
            Ok(())
        }
        Action::Check => {
            let problems = check_specs(cfg, &dir);
            if problems.is_empty() {
                outln!("plugins: ok ({} discovered)", discover(cfg, &dir).len());
                return Ok(());
            }
            for p in &problems {
                outln!("{p}");
            }
            anyhow::bail!("{} plugin problem(s)", problems.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_is_the_config_files_parent() {
        assert_eq!(
            config_dir(&PathBuf::from("/home/u/.config/thegn/config.toml")),
            PathBuf::from("/home/u/.config/thegn")
        );
    }
}
