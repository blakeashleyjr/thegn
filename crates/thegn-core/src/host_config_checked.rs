//! Additive host-source composition (THE-602), not launch authority.
//!
//! The caller supplies its already-layered Config and authorized store. No
//! opener, migration, config load/install, normalization or SecretRef resolution
//! occurs here. First schema initialization may construct environment-derived
//! serde defaults for metadata; it never installs those defaults into this input.
//! Limits bound admitted data and repeated validation work, not total allocation,
//! SQLite/filesystem execution time, or a launch snapshot's freshness.

use std::{fmt, io};

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::config_validate::{self, SemanticMode};
use crate::host_definition_snapshot::{HostDefinitionReadError, HostDefinitionsSnapshot};
use crate::store::HostStore;

pub const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 65_536;
pub const MAX_CONTAINER: usize = 1_024;
pub const MAX_KEY_BYTES: usize = 256;
pub const MAX_STRING_BYTES: usize = 64 * 1024;
pub const MAX_PIPELINE_STAGES: usize = 64;
pub const MAX_PROFILES: usize = 64;
pub const MAX_AUTOMATION_RULES: usize = 256;
pub const MAX_EFFECTIVE_RULE_VISITS: usize = 4_096;
pub const MAX_AUTOMATION_WORK_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCompositionError {
    Source(HostDefinitionReadError),
    Bounds,
    InvalidFinalConfig,
}

impl fmt::Display for HostCompositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "host source refused: {error}"),
            Self::Bounds => f.write_str("host composition exceeds checked data or work limits"),
            Self::InvalidFinalConfig => f.write_str("composed host configuration is invalid"),
        }
    }
}

impl std::error::Error for HostCompositionError {}

/// Final configuration validity only. No launch, provenance or freshness token.
pub struct HostComposedConfig {
    config: Config,
}

impl fmt::Debug for HostComposedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostComposedConfig([redacted])")
    }
}

impl HostComposedConfig {
    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// Capture every persisted definition before composing, including shadowed rows.
/// The store owns connection/opening authority; no failure becomes an empty map.
pub fn capture_and_compose_hosts_checked(
    config: &Config,
    store: &dyn HostStore,
) -> Result<HostComposedConfig, HostCompositionError> {
    let hosts = store
        .host_defs_checked()
        .map_err(HostCompositionError::Source)?;
    compose_host_definitions_checked(config, &hosts)
}

/// Compose an already captured strict source with the caller's layered input.
/// Existing undefined-host fallback and disabled-section semantics are retained.
/// This cannot recover raw information lost before the supplied Config existed.
pub fn compose_host_definitions_checked(
    config: &Config,
    hosts: &HostDefinitionsSnapshot,
) -> Result<HostComposedConfig, HostCompositionError> {
    // Admit before merge as well as afterward: no knowingly oversized input is
    // expanded. Neither pass clones effective automation configurations.
    admit(config)?;
    #[cfg(test)]
    config_validate::semantic_observation::record("config_clone");
    let mut config = config.clone();
    crate::host_config::merge_host_defs(&mut config, hosts.definitions());
    let value = admit(&config)?;
    if !config_validate::validate_config_schema_value(&value).is_empty()
        || !config_validate::typed_semantic_errors(&config, SemanticMode::StopOnError).is_empty()
    {
        return Err(HostCompositionError::InvalidFinalConfig);
    }
    Ok(HostComposedConfig { config })
}

fn admit(config: &Config) -> Result<Value, HostCompositionError> {
    if config.pipeline.stages.len() > MAX_PIPELINE_STAGES || config.profiles.len() > MAX_PROFILES {
        return Err(HostCompositionError::Bounds);
    }
    preflight_recursive_values(config)?;
    let (sidecar_bytes, mut nodes) = check_skipped_build(config.sandbox.build.as_ref())?;
    let value = bounded_value_with_limit(config, MAX_CONFIG_BYTES - sidecar_bytes)?;
    check_structure(&value, 0, &mut nodes)?;
    check_automation_work(config, MAX_EFFECTIVE_RULE_VISITS, MAX_AUTOMATION_WORK_BYTES)?;
    Ok(value)
}

/// Config-reachable recursive values: model provider defaults and plugin
/// contribution caps. PluginManifest is flattened into PluginSpec, so BOTH
/// values begin at JSON depth5. Bound borrowed trees before serializer or Clone
/// recursion. New dynamic/serde-skipped Config fields require this inventory to
/// be reviewed; wire protocol payloads are not embedded in Config.
fn preflight_recursive_values(config: &Config) -> Result<(), HostCompositionError> {
    if config.model_proxy.providers.len() > MAX_CONTAINER || config.plugins.len() > MAX_CONTAINER {
        return Err(HostCompositionError::Bounds);
    }
    let mut nodes = 0;
    for provider in &config.model_proxy.providers {
        if provider.defaults.len() > MAX_CONTAINER {
            return Err(HostCompositionError::Bounds);
        }
        for (key, value) in &provider.defaults {
            if key.len() > MAX_KEY_BYTES {
                return Err(HostCompositionError::Bounds);
            }
            check_structure(value, 5, &mut nodes)?;
        }
    }
    for plugin in &config.plugins {
        if plugin.manifest.contributions.len() > MAX_CONTAINER {
            return Err(HostCompositionError::Bounds);
        }
        for contribution in &plugin.manifest.contributions {
            // Null is omitted from JSON but still costs traversal here. Charge
            // it too, so empty caps cannot bypass the shared preflight budget.
            check_structure(&contribution.caps, 5, &mut nodes)?;
        }
    }
    Ok(())
}

/// SandboxBuild is serde-skipped but cloned. Account for its borrowed strings,
/// map and serialization in the SAME byte/node allowance; preserve it verbatim.
/// The other Config-reachable skipped field is accounts_restricted, a bool.
fn check_skipped_build(
    build: Option<&crate::sandbox_build::SandboxBuild>,
) -> Result<(usize, usize), HostCompositionError> {
    let Some(build) = build else {
        return Ok((0, 0));
    };
    // Exhaustive destructuring makes any newly added skipped build field an
    // explicit compile-time inventory review rather than an uncharged clone.
    let crate::sandbox_build::SandboxBuild {
        dockerfile,
        context,
        args,
        target,
    } = build;
    if args.len() > MAX_CONTAINER
        || [dockerfile, context]
            .into_iter()
            .any(|value| value.len() > MAX_STRING_BYTES)
        || target
            .as_ref()
            .is_some_and(|value| value.len() > MAX_STRING_BYTES)
        || args
            .iter()
            .any(|(key, value)| key.len() > MAX_KEY_BYTES || value.len() > MAX_STRING_BYTES)
    {
        return Err(HostCompositionError::Bounds);
    }
    // Standalone serialized sidecar: object + dockerfile/context/args/target,
    // then one value node per argument. Strings/maps are nonrecursive here.
    Ok((serialized_len(build, MAX_CONFIG_BYTES)?, 5 + args.len()))
}

fn check_structure(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), HostCompositionError> {
    if depth > MAX_DEPTH || *nodes >= MAX_NODES {
        return Err(HostCompositionError::Bounds);
    }
    *nodes += 1;
    match value {
        Value::String(value) if value.len() > MAX_STRING_BYTES => {
            return Err(HostCompositionError::Bounds);
        }
        Value::Array(values) => {
            if values.len() > MAX_CONTAINER {
                return Err(HostCompositionError::Bounds);
            }
            for value in values {
                check_structure(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            if values.len() > MAX_CONTAINER {
                return Err(HostCompositionError::Bounds);
            }
            for (key, value) in values {
                if key.len() > MAX_KEY_BYTES {
                    return Err(HostCompositionError::Bounds);
                }
                check_structure(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn charge(total: &mut usize, amount: usize, limit: usize) -> Result<(), HostCompositionError> {
    *total = total
        .checked_add(amount)
        .filter(|next| *next <= limit)
        .ok_or(HostCompositionError::Bounds)?;
    Ok(())
}

fn check_automation_work(
    config: &Config,
    visit_limit: usize,
    byte_limit: usize,
) -> Result<(), HostCompositionError> {
    let base = &config.automations;
    if base.rules.len() > MAX_AUTOMATION_RULES {
        return Err(HostCompositionError::Bounds);
    }
    let base_bytes = serialized_len(base, byte_limit)?;
    let mut bytes = 0;
    let mut visits = 0;
    charge(&mut bytes, base_bytes, byte_limit)?;
    charge(&mut visits, base.rules.len(), visit_limit)?;
    for profile in config.profiles.values() {
        let overlay = &profile.automations;
        if overlay.is_empty() {
            continue;
        }
        let rules = overlay.rules.as_ref().map_or(base.rules.len(), Vec::len);
        if rules > MAX_AUTOMATION_RULES {
            return Err(HostCompositionError::Bounds);
        }
        charge(&mut visits, rules, visit_limit)?;
        // The existing validator clones the base BEFORE applying a replacing
        // overlay. Charge that clone even when the effective rules are empty.
        charge(&mut bytes, base_bytes, byte_limit)?;
        charge(&mut bytes, serialized_len(overlay, byte_limit)?, byte_limit)?;
    }
    Ok(())
}

/// Serialization admission, with optional retained bytes. Counting profile
/// expansion never materializes another effective configuration or byte buffer.
struct JsonOutput {
    limit: usize,
    written: usize,
    bytes: Option<Vec<u8>>,
}

impl io::Write for JsonOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len())
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| io::Error::other("checked configuration byte limit"))?;
        if let Some(output) = &mut self.bytes {
            output.extend_from_slice(bytes);
        }
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_len(value: &impl Serialize, limit: usize) -> Result<usize, HostCompositionError> {
    let mut output = JsonOutput {
        limit,
        written: 0,
        bytes: None,
    };
    serde_json::to_writer(&mut output, value).map_err(|_| HostCompositionError::Bounds)?;
    Ok(output.written)
}

fn bounded_value_with_limit(
    value: &impl Serialize,
    limit: usize,
) -> Result<Value, HostCompositionError> {
    let mut output = JsonOutput {
        limit,
        written: 0,
        bytes: Some(Vec::new()),
    };
    serde_json::to_writer(&mut output, value).map_err(|_| HostCompositionError::Bounds)?;
    serde_json::from_slice(output.bytes.as_deref().expect("retained JSON output"))
        .map_err(|_| HostCompositionError::Bounds)
}

#[cfg(test)]
#[path = "host_config_checked_tests.rs"]
mod tests;
