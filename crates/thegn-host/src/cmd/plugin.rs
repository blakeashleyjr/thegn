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
use thegn_svc::plugin::LoadedPlugin;
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

fn inspection_lines(plugin: &LoadedPlugin) -> Vec<String> {
    let id = plugin.spec.manifest.id.as_str();
    if !plugin.spec.enabled {
        return vec![format!("plugin {id}: disabled (not negotiated)")];
    }
    let negotiated = match negotiate(&plugin.spec) {
        Ok(negotiated) => negotiated,
        Err(error) => {
            return vec![format!(
                "plugin {id}: api {} rejected by host {}: {error}",
                plugin.spec.manifest.api,
                thegn_core::plugin_api::API_VERSION
            )];
        }
    };
    let mut granted = negotiated
        .granted
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut missing = negotiated
        .denied
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    granted.sort();
    missing.sort();
    let scopes = plugin.spec.scope_set().to_csv();
    let mut lines = vec![format!(
        "plugin {id}: api {} negotiated with host {}; host-call scopes [{}]",
        negotiated.api,
        thegn_core::plugin_api::API_VERSION,
        scopes
    )];
    lines.push(format!(
        "  capabilities: granted [{}]; missing [{}]",
        granted.join(","),
        missing.join(",")
    ));
    for contribution in &negotiated.accepted_contributions {
        lines.push(format!(
            "  contribution {}: accepted {}",
            contribution.id.as_str(),
            contribution.extension_point.wire_name()
        ));
    }
    for rejected in &negotiated.rejected_contributions {
        lines.push(format!(
            "  contribution {}: rejected {}",
            rejected.contribution.id.as_str(),
            rejected.reason
        ));
    }
    lines
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
            let loaded = discover(cfg, &dir);
            for plugin in &loaded {
                for line in inspection_lines(plugin) {
                    outln!("{line}");
                }
            }
            let problems = check_specs(cfg, &dir);
            if problems.is_empty() {
                outln!("plugins: ok ({} discovered)", loaded.len());
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
    use thegn_core::plugin_api::{
        API_VERSION, CadenceHint, Contribution, ContributionId, ExtensionPoint, PluginId,
        PluginManifest, PluginMode, PluginSpec,
    };

    #[test]
    fn config_dir_is_the_config_files_parent() {
        assert_eq!(
            config_dir(&PathBuf::from("/home/u/.config/thegn/config.toml")),
            PathBuf::from("/home/u/.config/thegn")
        );
    }

    fn inspected_plugin(point: ExtensionPoint, capabilities: &[&str]) -> LoadedPlugin {
        LoadedPlugin {
            spec: PluginSpec {
                manifest: PluginManifest {
                    id: PluginId::new("inspect"),
                    name: "Inspect".into(),
                    version: "1.0.0".into(),
                    api: API_VERSION,
                    capabilities: capabilities
                        .iter()
                        .map(|cap| thegn_core::plugin_api::Capability::parse(cap).unwrap())
                        .collect(),
                    contributions: vec![Contribution {
                        id: ContributionId::new("inspect.row"),
                        extension_point: point,
                        label: "Inspect".into(),
                        surface: None,
                        cadence: CadenceHint::OnDemand,
                        metadata: Default::default(),
                        caps: serde_json::Value::Null,
                        chord: None,
                    }],
                },
                command: vec!["true".into()],
                cwd: String::new(),
                env: Default::default(),
                timeout_secs: 5,
                scopes: vec![thegn_core::control::Scope::Exec],
                mode: PluginMode::Resident,
                enabled: true,
            },
            dir: None,
        }
    }

    #[test]
    fn check_inspection_reports_api_scopes_grants_and_acceptance() {
        let plugin = inspected_plugin(ExtensionPoint::PaletteAction, &["surface:palette"]);
        let text = inspection_lines(&plugin).join("\n");
        assert!(
            text.contains("api 0.3.0 negotiated with host 0.3.0"),
            "{text}"
        );
        assert!(text.contains("host-call scopes [exec]"), "{text}");
        assert!(text.contains("granted [surface:palette]"), "{text}");
        assert!(text.contains("accepted PaletteAction"), "{text}");
    }

    #[test]
    fn check_inspection_reports_stable_reserved_reason() {
        let plugin = inspected_plugin(ExtensionPoint::PanelSection, &["surface:panel"]);
        let text = inspection_lines(&plugin).join("\n");
        assert!(
            text.contains("rejected unsupported extension point PanelSection: reserved by THE-108"),
            "{text}"
        );
    }
}
