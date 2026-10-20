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
use serde::Serialize;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::outln;
#[cfg(test)]
use thegn_svc::plugin::LoadedPlugin;
use thegn_svc::plugin::loader::{AdmittedPlugin, PluginDecision};

const LIVE_HEALTH_UNAVAILABLE: &str = "no attached compositor";

/// Stable plugin lifecycle vocabulary. The offline CLI can authoritatively
/// identify only `DisabledByConfig`; the remaining live states are reserved
/// for the compositor health publication follow-up.
///
/// The reserved variants are therefore constructed only by the vocabulary test
/// until that follow-up lands, which is what the `dead_code` exemption records.
/// Deleting them would make the follow-up redesign the published contract
/// instead of filling it in.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "reserved live states; see the doc comment")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PluginState {
    DisabledByConfig,
    Starting,
    Healthy,
    Degraded,
    CrashDisabled,
    Stopped,
    Unknown,
}

impl PluginState {
    /// Every variant, so the vocabulary test can assert one stable name per
    /// state in both output forms. Only the test needs it today.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "exercised by the vocabulary test only")
    )]
    const ALL: [Self; 7] = [
        Self::DisabledByConfig,
        Self::Starting,
        Self::Healthy,
        Self::Degraded,
        Self::CrashDisabled,
        Self::Stopped,
        Self::Unknown,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::DisabledByConfig => "disabled-by-config",
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::CrashDisabled => "crash-disabled",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

/// The health projection used by both `plugin list` output modes. `None`
/// means that no authoritative compositor/supervisor snapshot was available;
/// it must never be rendered as healthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PluginHealth {
    state: PluginState,
    live_health: &'static str,
    live_health_reason: &'static str,
    last_failure: Option<String>,
    restart_count: Option<u32>,
    backoff_ms: Option<u64>,
}

impl PluginHealth {
    fn from_config(enabled: bool) -> Self {
        Self::from_metadata(
            if enabled {
                PluginState::Unknown
            } else {
                PluginState::DisabledByConfig
            },
            None,
            None,
            None,
        )
    }

    fn from_metadata(
        state: PluginState,
        last_failure: Option<&str>,
        restart_count: Option<u32>,
        backoff_ms: Option<u64>,
    ) -> Self {
        Self {
            state,
            live_health: "unavailable",
            live_health_reason: LIVE_HEALTH_UNAVAILABLE,
            last_failure: last_failure.map(safe_failure_text),
            restart_count,
            backoff_ms,
        }
    }
}

/// Redact secret-shaped diagnostics, then reuse the canonical bounded display
/// projection. The latter escapes all terminal controls and caps the result.
fn safe_failure_text(text: &str) -> String {
    let redacted = thegn_core::log_redact::redact_text_line(text);
    thegn_core::identity::display_label(redacted.as_bytes()).to_string()
}

#[derive(Debug, Serialize)]
struct PluginListRow {
    id: String,
    name: String,
    version: String,
    mode: thegn_core::plugin_api::PluginMode,
    enabled: bool,
    source: Option<String>,
    trust: &'static str,
    gated_by: Vec<String>,
    status: String,
    source_digest: String,
    #[serde(flatten)]
    health: PluginHealth,
}

fn negotiation_status(decision: &PluginDecision) -> String {
    match decision {
        PluginDecision::Admitted(n) if n.unsupported_contributions.is_empty() => "ok".to_string(),
        PluginDecision::Admitted(n) => format!(
            "partial ({} unsupported contribution(s))",
            n.unsupported_contributions.len()
        ),
        PluginDecision::Invalid(problems) => problems.first().map_or_else(
            || "refused: invalid plugin specification".to_string(),
            |problem| format!("refused: {}", problem.problem),
        ),
        PluginDecision::RefusedUntrusted(problem) => problem.problem.clone(),
        PluginDecision::Disabled => "disabled (not negotiated)".to_string(),
    }
}

fn list_row(admitted: &AdmittedPlugin) -> PluginListRow {
    let plugin = &admitted.plugin;
    let native_trust = plugin.spec.native_trust();
    let trust = native_trust.status_token();
    let status = negotiation_status(&admitted.decision);
    let status = if plugin.trusted_native_ignored {
        format!("{status}; trusted_native in plugin.toml ignored")
    } else {
        status
    };
    PluginListRow {
        id: plugin.spec.manifest.id.as_str().to_string(),
        name: plugin.spec.manifest.name.clone(),
        version: plugin.spec.manifest.version.clone(),
        mode: plugin.spec.mode,
        enabled: plugin.spec.enabled,
        source: plugin.dir.as_ref().map(|dir| dir.display().to_string()),
        trust,
        gated_by: plugin.spec.gated_by(),
        status,
        source_digest: admitted.source_digest.clone(),
        health: PluginHealth::from_config(plugin.spec.enabled),
    }
}

fn human_health(health: &PluginHealth) -> String {
    let failure = health.last_failure.as_deref().unwrap_or("unavailable");
    let restart_count = health
        .restart_count
        .map_or_else(|| "unavailable".to_string(), |count| count.to_string());
    let backoff_ms = health
        .backoff_ms
        .map_or_else(|| "unavailable".to_string(), |millis| millis.to_string());
    format!(
        "state={} live health unavailable: {}; last-failure={} restart-count={} backoff-ms={}",
        health.state.as_str(),
        health.live_health_reason,
        failure,
        restart_count,
        backoff_ms
    )
}

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

fn inspection_lines(admitted: &AdmittedPlugin) -> Vec<String> {
    let plugin = &admitted.plugin;
    let id = plugin.spec.manifest.id.as_str();
    let gated_by = plugin.spec.gated_by();
    let verdict = match plugin.spec.native_trust() {
        thegn_core::plugin_api::NativeTrust::Ungated => "ungated".to_string(),
        thegn_core::plugin_api::NativeTrust::Trusted => "trusted-native".to_string(),
        thegn_core::plugin_api::NativeTrust::Refused { .. } => "refused-untrusted".to_string(),
    };
    let mut native_line = format!(
        "  native: trusted_native={}; gated by [{}]; {verdict}",
        plugin.spec.trusted_native,
        gated_by.join(", ")
    );
    if plugin.trusted_native_ignored {
        native_line.push_str("; trusted_native in plugin.toml ignored");
    }
    let (negotiated, refusal_line) = match &admitted.decision {
        PluginDecision::Disabled => {
            return vec![
                format!("plugin {id}: disabled (not negotiated)"),
                native_line,
            ];
        }
        PluginDecision::RefusedUntrusted(problem) => {
            match thegn_svc::plugin::loader::negotiate(&plugin.spec) {
                Ok(negotiated) => (negotiated, Some(problem.problem.as_str())),
                Err(_) => {
                    return vec![format!("plugin {id}: {}", problem.problem), native_line];
                }
            }
        }
        PluginDecision::Invalid(problems) => {
            let mut lines = problems
                .iter()
                .map(|problem| format!("plugin {id}: refused: {}", problem.problem))
                .collect::<Vec<_>>();
            lines.push(native_line);
            return lines;
        }
        PluginDecision::Admitted(negotiated) => (negotiated, None),
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
    lines.push(native_line);
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
    if let Some(refusal) = refusal_line {
        lines.insert(0, format!("plugin {id}: {refusal}"));
    }
    lines
}

pub fn run(cfg: &Config, action: Action, config_path: &Path) -> Result<()> {
    let dir = config_dir(config_path);
    match action {
        Action::List { json } => {
            let admitted = thegn_svc::plugin::loader::admit(cfg, &dir);
            if json {
                let rows: Vec<PluginListRow> = admitted.plugins.iter().map(list_row).collect();
                super::emit_json(&rows)?;
                return Ok(());
            }
            if admitted.plugins.is_empty() {
                outln!(
                    "no plugins configured (add [[plugins]] or {}/plugins/<dir>/plugin.toml)",
                    dir.display()
                );
                for problem in &admitted.set_problems {
                    outln!("set: {problem}");
                }
                return Ok(());
            }
            for p in &admitted.plugins {
                let row = list_row(p);
                let source = p
                    .plugin
                    .dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "[[plugins]]".into());
                outln!(
                    "{:<20} {:<10} {:<18} {:<22} {:<30} gated by [{}] {} | {}",
                    row.id,
                    format!("{:?}", row.mode).to_lowercase(),
                    row.health.state.as_str(),
                    row.trust,
                    row.status,
                    row.gated_by.join(", "),
                    source,
                    human_health(&row.health),
                );
            }
            for problem in &admitted.set_problems {
                outln!("set: {problem}");
            }
            Ok(())
        }
        Action::Check => {
            let admitted = thegn_svc::plugin::loader::admit(cfg, &dir);
            for plugin in &admitted.plugins {
                for line in inspection_lines(plugin) {
                    outln!("{line}");
                }
            }
            let problems = admitted.problems();
            if problems.is_empty() {
                outln!(
                    "plugins: ok ({} discovered, set {})",
                    admitted.plugins.len(),
                    &admitted.digest[..12]
                );
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
        API_VERSION, ApiVersion, CadenceHint, Contribution, ContributionId, ExtensionPoint,
        PluginId, PluginManifest, PluginMode, PluginSpec,
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
                trusted_native: false,
            },
            dir: None,
            trusted_native_ignored: false,
        }
    }

    fn admitted_plugin(plugin: LoadedPlugin) -> AdmittedPlugin {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            plugins: vec![plugin.spec.clone()],
            ..Default::default()
        };
        let mut admitted = thegn_svc::plugin::loader::admit(&cfg, tmp.path());
        let mut admitted_plugin = admitted.plugins.remove(0);
        admitted_plugin.plugin = plugin;
        admitted_plugin.source_digest = "test-source-digest".into();
        admitted_plugin
    }

    #[test]
    fn check_inspection_reports_api_scopes_grants_and_acceptance() {
        let plugin = inspected_plugin(ExtensionPoint::PaletteAction, &["surface:palette"]);
        let text = inspection_lines(&admitted_plugin(plugin)).join("\n");
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
        let text = inspection_lines(&admitted_plugin(plugin)).join("\n");
        assert!(
            text.contains("rejected unsupported extension point PanelSection: reserved by THE-108"),
            "{text}"
        );
    }

    #[test]
    fn list_row_labels_trust_states_and_json_fields() {
        let mut trusted = inspected_plugin(ExtensionPoint::PaletteAction, &["network:example.com"]);
        trusted.spec.scopes.clear();
        trusted.spec.trusted_native = true;
        let mut refused = inspected_plugin(ExtensionPoint::PaletteAction, &["network:example.com"]);
        refused.spec.scopes.clear();
        let mut ungated = inspected_plugin(ExtensionPoint::PaletteAction, &["surface:palette"]);
        ungated.spec.scopes.clear();

        let rows = [
            list_row(&admitted_plugin(trusted)),
            list_row(&admitted_plugin(refused)),
            list_row(&admitted_plugin(ungated)),
        ];
        assert_eq!(rows[0].trust, "trusted-native");
        assert_eq!(rows[1].trust, "refused-untrusted");
        assert!(
            rows[1]
                .status
                .contains("refused: declares network:example.com")
        );
        assert_eq!(rows[2].trust, "ungated");
        let json = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(json["trust"], "refused-untrusted");
        assert_eq!(json["gated_by"], serde_json::json!(["network:example.com"]));
    }

    #[test]
    fn refused_directory_list_status_explains_how_to_grant_trust() {
        let mut plugin = inspected_plugin(ExtensionPoint::PaletteAction, &["network:example.com"]);
        plugin.spec.scopes.clear();
        plugin.dir = Some(std::path::PathBuf::from("/tmp/plugins/untrusted"));

        let row = list_row(&admitted_plugin(plugin));

        assert!(
            row.status
                .contains("move this plugin directory out of plugins/")
        );
        assert!(row.status.contains("absolute command and cwd paths"));
        assert!(row.status.contains("trusted_native = true"));
    }

    #[test]
    fn state_vocabulary_has_one_stable_name_in_human_and_json_forms() {
        for state in PluginState::ALL {
            let health = PluginHealth::from_metadata(state, None, None, None);
            let human = human_health(&health);
            let json = serde_json::to_value(&health).unwrap();
            assert!(
                human.contains(&format!("state={}", state.as_str())),
                "{human}"
            );
            assert_eq!(json["state"], state.as_str());
        }
    }

    #[test]
    fn offline_list_never_claims_enabled_plugin_is_healthy() {
        let plugin = inspected_plugin(ExtensionPoint::PaletteAction, &["surface:palette"]);
        let row = list_row(&admitted_plugin(plugin));
        assert_eq!(row.health.state, PluginState::Unknown);
        assert_eq!(row.health.live_health, "unavailable");
        assert_eq!(row.health.live_health_reason, LIVE_HEALTH_UNAVAILABLE);
        let human = human_health(&row.health);
        assert!(human.contains("state=unknown"));
        assert!(human.contains("live health unavailable: no attached compositor"));
        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["state"], "unknown");
        assert_eq!(json["live_health"], "unavailable");
        assert!(json["last_failure"].is_null());
        assert!(json["restart_count"].is_null());
        assert!(json["backoff_ms"].is_null());
        assert_ne!(json["state"], "healthy");
    }

    #[test]
    fn json_list_row_keeps_config_and_health_fields_in_one_stable_projection() {
        let mut plugin = inspected_plugin(ExtensionPoint::PaletteAction, &["surface:palette"]);
        plugin.spec.trusted_native = true;
        let json = serde_json::to_value(list_row(&admitted_plugin(plugin))).unwrap();
        assert_eq!(json["id"], "inspect");
        assert_eq!(json["name"], "Inspect");
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["mode"], "resident");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["status"], "ok");
        assert_eq!(json["state"], "unknown");
        assert_eq!(json["live_health"], "unavailable");
        assert_eq!(json["live_health_reason"], LIVE_HEALTH_UNAVAILABLE);
        assert_eq!(json["source_digest"], "test-source-digest");
    }

    #[test]
    fn config_disabled_state_is_distinct_and_exit_semantics_stay_inspection_only() {
        let health = PluginHealth::from_config(false);
        assert_eq!(health.state, PluginState::DisabledByConfig);
        assert_eq!(health.state.as_str(), "disabled-by-config");
        assert!(human_health(&health).contains("state=disabled-by-config"));
        assert_eq!(
            serde_json::to_value(&health).unwrap()["state"],
            "disabled-by-config"
        );
    }

    #[test]
    fn failure_metadata_is_redacted_bounded_and_control_free() {
        let hostile = "failure --token secret\u{1b}[31m\n".repeat(100);
        let health = PluginHealth::from_metadata(
            PluginState::Degraded,
            Some(&hostile),
            Some(2),
            Some(5_000),
        );
        let failure = health.last_failure.as_deref().unwrap();
        assert!(failure.len() <= thegn_core::identity::MAX_DISPLAY_LABEL_BYTES);
        assert!(!failure.chars().any(char::is_control));
        assert!(failure.contains(thegn_core::log_redact::REDACTED));
        assert_eq!(health.restart_count, Some(2));
        assert_eq!(health.backoff_ms, Some(5_000));
    }

    #[test]
    fn list_and_check_agree_on_a_negotiate_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut plugin = inspected_plugin(ExtensionPoint::PaletteAction, &["surface:palette"]);
        plugin.spec.manifest.api = ApiVersion::new(9, 0, 0);
        plugin.spec.scopes.clear();
        let cfg = Config {
            plugins: vec![plugin],
            ..Default::default()
        };

        let admitted = thegn_svc::plugin::loader::admit(&cfg, tmp.path());
        let listed = list_row(&admitted.plugins[0]).status;
        let checked = admitted.problems();

        assert!(listed.starts_with("refused: incompatible api"), "{listed}");
        assert!(
            checked.iter().any(|problem| {
                problem.plugin == "inspect" && listed == format!("refused: {}", problem.problem)
            }),
            "{checked:?}"
        );
        assert!(
            inspection_lines(&admitted.plugins[0])
                .iter()
                .any(|line| line == &format!("plugin inspect: {listed}"))
        );
    }
}
