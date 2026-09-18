//! The typed, bounded core configuration admission boundary.
//!
//! This module accepts already-captured sources.  It deliberately owns no
//! filesystem, environment enumeration, database, or host-process side
//! effects.  The host will wire those capture seams in a later chunk; until
//! then the legacy loader remains available for existing callers, but this
//! boundary never calls its fallback-to-default path.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::config::{Config, EnvSource};
use crate::config_budget;
use crate::config_validate::{self, SemanticMode};
use crate::host_definition_snapshot::HostDefinitionsSnapshot;

pub const NORMALIZATION_VERSION: u32 = 1;

/// An already-captured source.  `identity` is hashed and never exposed in a
/// diagnostic; callers may pass an absolute path, a logical source name, or a
/// platform-specific identity token without making it a leak surface.
#[derive(Debug, Clone, Copy)]
pub struct SourceInput<'a> {
    pub identity: &'a str,
    pub explicit: bool,
    pub content: SourceContent<'a>,
}

#[derive(Debug, Clone, Copy)]
pub enum SourceContent<'a> {
    Absent,
    Bytes(&'a [u8]),
    Failure(SourceFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFailure {
    Unreadable,
    InvalidUtf8,
    TransientIo,
}

impl<'a> SourceInput<'a> {
    pub const fn absent(identity: &'a str, explicit: bool) -> Self {
        Self {
            identity,
            explicit,
            content: SourceContent::Absent,
        }
    }

    pub const fn bytes(identity: &'a str, explicit: bool, content: &'a [u8]) -> Self {
        Self {
            identity,
            explicit,
            content: SourceContent::Bytes(content),
        }
    }

    pub const fn failure(identity: &'a str, explicit: bool, failure: SourceFailure) -> Self {
        Self {
            identity,
            explicit,
            content: SourceContent::Failure(failure),
        }
    }
}

/// All trusted inputs are frozen before this function is called.  In
/// particular, `env` is wrapped and read once by the composition itself, so a
/// later ambient mutation cannot change a published candidate.
pub struct AdmissionInputs<'a> {
    pub defaults: Config,
    pub base: SourceInput<'a>,
    pub profile: Option<SourceInput<'a>>,
    pub env: &'a dyn EnvSource,
    pub overrides: &'a [String],
    pub hosts: &'a HostDefinitionsSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAdmissionError {
    AbsentDefaultPath,
    ExplicitPathMissing,
    Unreadable,
    InvalidUtf8,
    Oversized,
    ParseInvalid,
    SchemaInvalid,
    SemanticInvalid,
    ProfileInvalid,
    EnvironmentInvalid,
    CliInvalid,
    HostInvalid,
    TransientIo,
}

impl fmt::Display for ConfigAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AbsentDefaultPath => "implicit default config is absent",
            Self::ExplicitPathMissing => "explicit config path is missing",
            Self::Unreadable => "config source is unreadable",
            Self::InvalidUtf8 => "config source is not valid UTF-8",
            Self::Oversized => "config source or normalized candidate exceeds an admission limit",
            Self::ParseInvalid => "config source is not valid TOML",
            Self::SchemaInvalid => "config source violates the admitted schema",
            Self::SemanticInvalid => "config source violates an admitted semantic constraint",
            Self::ProfileInvalid => "selected profile cannot be admitted",
            Self::EnvironmentInvalid => "supplied environment override cannot be admitted",
            Self::CliInvalid => "supplied CLI override cannot be admitted",
            Self::HostInvalid => "captured host definitions cannot be admitted",
            Self::TransientIo => "config source failed with a transient I/O error",
        })
    }
}

impl std::error::Error for ConfigAdmissionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AdmissionDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
}

impl fmt::Debug for AdmissionDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionDiagnostic")
            .field("severity", &self.severity)
            .field("message", &self.message)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    pub identity_digest: [u8; 32],
    pub content_digest: [u8; 32],
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentities {
    pub base: SourceIdentity,
    pub profile: Option<SourceIdentity>,
    pub environment_digest: [u8; 32],
    pub overrides_digest: [u8; 32],
    pub host_digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRevision {
    pub generation: u64,
    pub digest: [u8; 32],
    pub sources: SourceIdentities,
    pub normalization_version: u32,
    pub host_schema: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionHealth {
    Healthy,
    FirstRunDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Defaults,
    Base,
    Profile,
    Environment,
    Cli,
    Hosts,
    Final,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LayerTraceEntry {
    pub layer: LayerKind,
    pub source: SourceIdentity,
    pub normalized_digest: [u8; 32],
    pub diagnostics: Vec<AdmissionDiagnostic>,
}

impl fmt::Debug for LayerTraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerTraceEntry")
            .field("layer", &self.layer)
            .field("source", &self.source)
            .field("normalized_digest", &self.normalized_digest)
            .field("diagnostic_count", &self.diagnostics.len())
            .finish()
    }
}

/// The provenance used by future explain/get consumers.  Entries and messages
/// are bounded and contain only redacted source identities, never raw paths or
/// values from the admitted configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct LayerTrace {
    entries: Vec<LayerTraceEntry>,
    diagnostics: Vec<AdmissionDiagnostic>,
}

impl fmt::Debug for LayerTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerTrace")
            .field("entries", &self.entries)
            .field("diagnostic_count", &self.diagnostics.len())
            .finish()
    }
}

impl LayerTrace {
    pub fn entries(&self) -> &[LayerTraceEntry] {
        &self.entries
    }

    pub fn diagnostics(&self) -> &[AdmissionDiagnostic] {
        &self.diagnostics
    }
}

pub struct AdmittedConfig {
    config: Config,
    trace: LayerTrace,
    revision: ConfigRevision,
    health: AdmissionHealth,
}

impl fmt::Debug for AdmittedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmittedConfig")
            .field("trace", &self.trace)
            .field("revision", &self.revision)
            .field("health", &self.health)
            .finish_non_exhaustive()
    }
}

impl AdmittedConfig {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn trace(&self) -> &LayerTrace {
        &self.trace
    }

    pub fn revision(&self) -> ConfigRevision {
        self.revision.clone()
    }

    pub fn health(&self) -> AdmissionHealth {
        self.health
    }

    fn with_generation(mut self, generation: u64) -> Self {
        self.revision.generation = generation;
        self
    }
}

#[derive(Default)]
pub struct AdmissionStore {
    current: Mutex<Option<Arc<AdmittedConfig>>>,
}

impl AdmissionStore {
    pub fn new(initial: AdmittedConfig) -> Self {
        Self {
            current: Mutex::new(Some(Arc::new(initial.with_generation(1)))),
        }
    }

    pub fn current(&self) -> Option<Arc<AdmittedConfig>> {
        self.current.lock().ok().and_then(|current| current.clone())
    }

    pub fn publish(
        &self,
        candidate: AdmittedConfig,
        expected_generation: u64,
    ) -> Result<Arc<AdmittedConfig>, StaleConfigError> {
        let mut current = self.current.lock().map_err(|_| StaleConfigError {
            expected_generation,
            observed_generation: None,
        })?;
        let observed = current.as_ref().map(|config| config.revision.generation);
        if observed != Some(expected_generation) {
            return Err(StaleConfigError {
                expected_generation,
                observed_generation: observed,
            });
        }
        let generation = expected_generation.saturating_add(1);
        let admitted = Arc::new(candidate.with_generation(generation));
        *current = Some(admitted.clone());
        Ok(admitted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleConfigError {
    pub expected_generation: u64,
    pub observed_generation: Option<u64>,
}

impl fmt::Display for StaleConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "configuration generation {} is stale (current {:?})",
            self.expected_generation, self.observed_generation
        )
    }
}

impl std::error::Error for StaleConfigError {}

pub struct RevisionGuard {
    revision: ConfigRevision,
}

impl RevisionGuard {
    pub fn revision(&self) -> &ConfigRevision {
        &self.revision
    }
}

pub fn require_current(
    expected: &ConfigRevision,
    current: &AdmissionStore,
) -> Result<RevisionGuard, StaleConfigError> {
    let observed = current.current();
    let Some(observed) = observed else {
        return Err(StaleConfigError {
            expected_generation: expected.generation,
            observed_generation: None,
        });
    };
    if observed.revision.generation != expected.generation
        || observed.revision.digest != expected.digest
    {
        return Err(StaleConfigError {
            expected_generation: expected.generation,
            observed_generation: Some(observed.revision.generation),
        });
    }
    Ok(RevisionGuard {
        revision: expected.clone(),
    })
}

struct ParsedLayer {
    identity: SourceIdentity,
    normalized_digest: [u8; 32],
    normalized: String,
    diagnostics: Vec<AdmissionDiagnostic>,
}

struct CapturedEnv<'a> {
    source: &'a dyn EnvSource,
    values: RefCell<BTreeMap<String, String>>,
}

impl<'a> CapturedEnv<'a> {
    fn new(source: &'a dyn EnvSource) -> Self {
        Self {
            source,
            values: RefCell::new(BTreeMap::new()),
        }
    }

    fn values(&self) -> BTreeMap<String, String> {
        self.values.borrow().clone()
    }
}

impl EnvSource for CapturedEnv<'_> {
    fn get(&self, key: &str) -> Option<String> {
        let value = self.source.get(key);
        if let Some(value) = &value {
            self.values
                .borrow_mut()
                .insert(key.to_string(), value.clone());
        }
        value
    }
}

/// Admit one complete candidate.  No legacy loader is called here, and all
/// failures return before an `AdmittedConfig` can be constructed.
pub fn admit(inputs: AdmissionInputs<'_>) -> Result<AdmittedConfig, ConfigAdmissionError> {
    let base_bytes = source_bytes(&inputs.base, false)?;
    let base = parse_layer(&inputs.base, base_bytes, LayerKind::Base)?;
    let profile = match inputs.profile {
        Some(profile) => {
            let bytes = source_bytes(&profile, true)
                .map_err(|error| map_profile_error(error))?
                .ok_or(ConfigAdmissionError::ProfileInvalid)?;
            parse_layer(&profile, Some(bytes), LayerKind::Profile).map_err(map_profile_error)?
        }
        None => None,
    };

    let combined_bytes = base_bytes
        .map_or(0, <[u8]>::len)
        .checked_add(profile.as_ref().map_or(0, |layer| layer.identity.bytes))
        .ok_or(ConfigAdmissionError::Oversized)?;
    if combined_bytes > config_budget::MAX_COMBINED_SOURCE_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }

    let mut cfg = inputs.defaults;
    let mut entries = Vec::new();
    let mut diagnostics = Vec::new();
    let default_source = synthetic_identity("defaults", 0);
    entries.push(LayerTraceEntry {
        layer: LayerKind::Defaults,
        source: default_source.clone(),
        normalized_digest: digest(b"defaults", &[b"config-admission-v1"]),
        diagnostics: Vec::new(),
    });
    let first_run_default = base.is_none();

    if let Some(base) = base {
        diagnostics.extend(base.diagnostics.clone());
        entries.push(trace_entry(LayerKind::Base, &base));
        apply_layer_overlay(&mut cfg, &base.normalized)?;
    }
    if let Some(profile) = profile {
        diagnostics.extend(profile.diagnostics.clone());
        entries.push(trace_entry(LayerKind::Profile, &profile));
        apply_layer_overlay(&mut cfg, &profile.normalized)
            .map_err(|_| ConfigAdmissionError::ProfileInvalid)?;
    }

    let captured_env = CapturedEnv::new(inputs.env);
    let (overlay, env_warnings) =
        crate::config_diagnostics::capture(|| crate::config::env_overlay(&captured_env));
    let env_values = captured_env.values();
    check_env_budget(&env_values)?;
    let env_errors = invalid_supplied_diagnostics(&env_warnings);
    if !env_errors.is_empty() {
        return Err(ConfigAdmissionError::EnvironmentInvalid);
    }
    let ((), duration_warnings) = crate::config_diagnostics::capture(|| {
        crate::config::apply_env_overlay_checked(&mut cfg, overlay)
    });
    let duration_errors = invalid_supplied_diagnostics(&duration_warnings);
    if !duration_errors.is_empty() {
        return Err(ConfigAdmissionError::EnvironmentInvalid);
    }
    let env_diagnostics = bounded_diagnostics(
        env_warnings
            .into_iter()
            .chain(duration_warnings)
            .map(|message| AdmissionDiagnostic {
                severity: DiagnosticSeverity::Warning,
                message: safe_message(&message),
            }),
    );
    diagnostics.extend(env_diagnostics.clone());
    let env_identity = synthetic_identity_from_pairs("environment", &env_values);
    entries.push(LayerTraceEntry {
        layer: LayerKind::Environment,
        source: env_identity.clone(),
        normalized_digest: env_identity.content_digest,
        diagnostics: env_diagnostics,
    });

    let override_identity = validate_and_apply_overrides(&mut cfg, inputs.overrides)?;
    entries.push(LayerTraceEntry {
        layer: LayerKind::Cli,
        source: override_identity.clone(),
        normalized_digest: override_identity.content_digest,
        diagnostics: Vec::new(),
    });

    // post_process is the existing runtime normalization step.  It is called
    // only after all trusted layers, and never as a recovery path.
    cfg.post_process();
    check_normalized_candidate(&cfg)?;
    // Validate the complete file/profile/env/CLI candidate before host rows are
    // merged, so a trusted-layer semantic failure cannot be mislabeled as a
    // host-source failure. The host composition repeats the same strict
    // checks after its bounded merge.
    check_final_config(&cfg)?;

    let composed = crate::host_config_checked::compose_host_definitions_checked(&cfg, inputs.hosts)
        .map_err(|_| ConfigAdmissionError::HostInvalid)?;
    cfg = composed.config().clone();
    check_final_config(&cfg)?;

    let host_identity = host_identity(inputs.hosts)?;
    entries.push(LayerTraceEntry {
        layer: LayerKind::Hosts,
        source: host_identity.clone(),
        normalized_digest: host_identity.content_digest,
        diagnostics: Vec::new(),
    });
    let final_bytes = bounded_serialized_config(&cfg)?;
    let final_digest = digest(b"thegn/config-admission/final/v1", &[&final_bytes]);
    let final_identity = SourceIdentity {
        identity_digest: digest(
            b"thegn/config-admission/final-identity/v1",
            &[&final_digest],
        ),
        content_digest: final_digest,
        bytes: final_bytes.len(),
    };
    entries.push(LayerTraceEntry {
        layer: LayerKind::Final,
        source: final_identity,
        normalized_digest: final_digest,
        diagnostics: Vec::new(),
    });

    let base_identity = entries
        .iter()
        .find(|entry| entry.layer == LayerKind::Base)
        .map(|entry| entry.source.clone())
        .unwrap_or_else(|| synthetic_identity("base-absent", 0));
    let profile_identity = entries
        .iter()
        .find(|entry| entry.layer == LayerKind::Profile)
        .map(|entry| entry.source.clone());
    let sources = SourceIdentities {
        base: base_identity,
        profile: profile_identity,
        environment_digest: env_identity.content_digest,
        overrides_digest: override_identity.content_digest,
        host_digest: host_identity.content_digest,
    };
    let profile_identity_digest = sources
        .profile
        .as_ref()
        .map_or([0; 32], |profile| profile.identity_digest);
    let profile_content_digest = sources
        .profile
        .as_ref()
        .map_or([0; 32], |profile| profile.content_digest);
    let revision_digest = digest(
        b"thegn/config-admission/revision/v1",
        &[
            &final_digest,
            &sources.base.identity_digest,
            &sources.base.content_digest,
            &profile_identity_digest,
            &profile_content_digest,
            &sources.environment_digest,
            &sources.overrides_digest,
            &sources.host_digest,
        ],
    );
    let trace = LayerTrace {
        entries,
        diagnostics: bounded_diagnostics(diagnostics.into_iter()),
    };
    Ok(AdmittedConfig {
        config: cfg,
        trace,
        revision: ConfigRevision {
            generation: 0,
            digest: revision_digest,
            sources,
            normalization_version: NORMALIZATION_VERSION,
            host_schema: inputs.hosts.observed_schema(),
        },
        health: if first_run_default {
            AdmissionHealth::FirstRunDefault
        } else {
            AdmissionHealth::Healthy
        },
    })
}

fn source_bytes<'a>(
    input: &SourceInput<'a>,
    profile: bool,
) -> Result<Option<&'a [u8]>, ConfigAdmissionError> {
    match input.content {
        SourceContent::Absent if input.explicit => Err(if profile {
            ConfigAdmissionError::ProfileInvalid
        } else {
            ConfigAdmissionError::ExplicitPathMissing
        }),
        SourceContent::Absent => Ok(None),
        SourceContent::Failure(SourceFailure::Unreadable) => Err(if profile {
            ConfigAdmissionError::ProfileInvalid
        } else {
            ConfigAdmissionError::Unreadable
        }),
        SourceContent::Failure(SourceFailure::InvalidUtf8) => Err(if profile {
            ConfigAdmissionError::ProfileInvalid
        } else {
            ConfigAdmissionError::InvalidUtf8
        }),
        SourceContent::Failure(SourceFailure::TransientIo) => {
            Err(ConfigAdmissionError::TransientIo)
        }
        SourceContent::Bytes(bytes) => {
            if bytes.len() > config_budget::MAX_SOURCE_BYTES {
                return Err(ConfigAdmissionError::Oversized);
            }
            std::str::from_utf8(bytes).map_err(|_| {
                if profile {
                    ConfigAdmissionError::ProfileInvalid
                } else {
                    ConfigAdmissionError::InvalidUtf8
                }
            })?;
            config_budget::scan(bytes).map_err(|_| ConfigAdmissionError::Oversized)?;
            Ok(Some(bytes))
        }
    }
}

fn parse_layer(
    input: &SourceInput<'_>,
    bytes: Option<&[u8]>,
    layer: LayerKind,
) -> Result<Option<ParsedLayer>, ConfigAdmissionError> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let body = std::str::from_utf8(bytes).map_err(|_| ConfigAdmissionError::InvalidUtf8)?;
    let normalized = crate::config_compat::normalize(body)
        .map_err(|_| layer_error(layer, ConfigAdmissionError::ParseInvalid))?;
    if normalized.body.len() > config_budget::MAX_NORMALIZED_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    let raw: toml::Value = normalized
        .body
        .parse()
        .map_err(|_| layer_error(layer, ConfigAdmissionError::ParseInvalid))?;
    let json = serde_json::to_value(raw)
        .map_err(|_| layer_error(layer, ConfigAdmissionError::ParseInvalid))?;
    let schema_errors = config_validate::validate_config_schema_value(&json);
    if !schema_errors.is_empty() {
        return Err(layer_error(layer, ConfigAdmissionError::SchemaInvalid));
    }
    // Deserialize only after the raw schema walk has rejected unknown keys,
    // invalid enums, and wrong shapes; serde's compatibility fallback is never
    // allowed to become an admitted value.
    let _: Config = toml::from_str(&normalized.body)
        .map_err(|_| layer_error(layer, ConfigAdmissionError::SchemaInvalid))?;
    let diagnostics = bounded_diagnostics(normalized.diagnostics.into_iter().map(|message| {
        AdmissionDiagnostic {
            severity: DiagnosticSeverity::Warning,
            message: safe_message(&message),
        }
    }));
    let identity = SourceIdentity {
        identity_digest: digest(
            b"thegn/config-admission/source-identity/v1",
            &[input.identity.as_bytes()],
        ),
        content_digest: digest(b"thegn/config-admission/source-bytes/v1", &[bytes]),
        bytes: bytes.len(),
    };
    Ok(Some(ParsedLayer {
        identity,
        normalized_digest: digest(
            b"thegn/config-admission/normalized/v1",
            &[normalized.body.as_bytes()],
        ),
        normalized: normalized.body,
        diagnostics,
    }))
}

fn apply_layer_overlay(cfg: &mut Config, normalized: &str) -> Result<(), ConfigAdmissionError> {
    let value: serde_json::Value =
        toml::from_str(normalized).map_err(|_| ConfigAdmissionError::ParseInvalid)?;
    crate::config::apply_json_overlay(cfg, value).map_err(|_| ConfigAdmissionError::SchemaInvalid)
}

fn layer_error(layer: LayerKind, error: ConfigAdmissionError) -> ConfigAdmissionError {
    if layer == LayerKind::Profile {
        ConfigAdmissionError::ProfileInvalid
    } else {
        error
    }
}

fn map_profile_error(error: ConfigAdmissionError) -> ConfigAdmissionError {
    match error {
        ConfigAdmissionError::ExplicitPathMissing
        | ConfigAdmissionError::Unreadable
        | ConfigAdmissionError::InvalidUtf8
        | ConfigAdmissionError::Oversized
        | ConfigAdmissionError::ParseInvalid
        | ConfigAdmissionError::SchemaInvalid
        | ConfigAdmissionError::SemanticInvalid
        | ConfigAdmissionError::TransientIo => ConfigAdmissionError::ProfileInvalid,
        other => other,
    }
}

fn check_env_budget(values: &BTreeMap<String, String>) -> Result<(), ConfigAdmissionError> {
    let mut aggregate = 0usize;
    for (key, value) in values {
        let entry = key
            .len()
            .checked_add(value.len())
            .ok_or(ConfigAdmissionError::Oversized)?;
        if value.len() > config_budget::MAX_ENV_VALUE_BYTES {
            return Err(ConfigAdmissionError::Oversized);
        }
        aggregate = aggregate
            .checked_add(entry)
            .ok_or(ConfigAdmissionError::Oversized)?;
    }
    config_budget::check_entries(
        values.len(),
        aggregate,
        values.values().map(String::len),
        config_budget::MAX_ENV_ENTRIES,
        config_budget::MAX_ENV_VALUE_BYTES,
        config_budget::MAX_ENV_BYTES,
    )
    .map_err(|_| ConfigAdmissionError::Oversized)
}

fn validate_and_apply_overrides(
    cfg: &mut Config,
    overrides: &[String],
) -> Result<SourceIdentity, ConfigAdmissionError> {
    if overrides.len() > config_budget::MAX_CLI_ENTRIES {
        return Err(ConfigAdmissionError::Oversized);
    }
    let mut total = 0usize;
    for override_value in overrides {
        if override_value.len() > config_budget::MAX_CLI_ENTRY_BYTES {
            return Err(ConfigAdmissionError::Oversized);
        }
        total = total
            .checked_add(override_value.len())
            .ok_or(ConfigAdmissionError::Oversized)?;
        let Some((key, value)) = override_value.split_once('=') else {
            return Err(ConfigAdmissionError::CliInvalid);
        };
        let (result, warnings) =
            crate::config_diagnostics::capture(|| Config::apply_override_str(cfg, key, value));
        // The legacy enum deserializers warn-and-default without returning an
        // error. Any warning emitted while applying a supplied override is
        // therefore a strict CLI rejection (there are no compatibility
        // warnings on this direct dotted-key path).
        if result.is_err() || !warnings.is_empty() {
            return Err(ConfigAdmissionError::CliInvalid);
        }
    }
    if total > config_budget::MAX_CLI_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    Ok(synthetic_identity_from_strings("cli", overrides))
}

fn check_normalized_candidate(cfg: &Config) -> Result<(), ConfigAdmissionError> {
    let bytes = bounded_serialized_config(cfg)?;
    if bytes.len() > config_budget::MAX_NORMALIZED_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    Ok(())
}

fn check_final_config(cfg: &Config) -> Result<(), ConfigAdmissionError> {
    let value = serde_json::to_value(cfg).map_err(|_| ConfigAdmissionError::SchemaInvalid)?;
    if !config_validate::validate_config_schema_value(&value).is_empty() {
        return Err(ConfigAdmissionError::SchemaInvalid);
    }
    if !crate::config_duration::errors_for_config(cfg).is_empty() {
        return Err(ConfigAdmissionError::SchemaInvalid);
    }
    if !config_validate::typed_semantic_errors(cfg, SemanticMode::StopOnError).is_empty() {
        return Err(ConfigAdmissionError::SemanticInvalid);
    }
    Ok(())
}

fn bounded_serialized_config(cfg: &Config) -> Result<Vec<u8>, ConfigAdmissionError> {
    let bytes = serde_json::to_vec(cfg).map_err(|_| ConfigAdmissionError::SchemaInvalid)?;
    if bytes.len() > config_budget::MAX_NORMALIZED_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    Ok(bytes)
}

fn host_identity(hosts: &HostDefinitionsSnapshot) -> Result<SourceIdentity, ConfigAdmissionError> {
    let bytes = serde_json::to_vec(&(hosts.observed_schema(), hosts.definitions()))
        .map_err(|_| ConfigAdmissionError::HostInvalid)?;
    Ok(SourceIdentity {
        identity_digest: digest(
            b"thegn/config-admission/host-identity/v1",
            &[b"state-hosts"],
        ),
        content_digest: digest(b"thegn/config-admission/host-bytes/v1", &[&bytes]),
        bytes: bytes.len(),
    })
}

fn trace_entry(layer: LayerKind, parsed: &ParsedLayer) -> LayerTraceEntry {
    LayerTraceEntry {
        layer,
        source: parsed.identity.clone(),
        normalized_digest: parsed.normalized_digest,
        diagnostics: parsed.diagnostics.clone(),
    }
}

fn synthetic_identity(label: &str, bytes: usize) -> SourceIdentity {
    SourceIdentity {
        identity_digest: digest(
            b"thegn/config-admission/synthetic-identity/v1",
            &[label.as_bytes()],
        ),
        content_digest: digest(
            b"thegn/config-admission/synthetic-content/v1",
            &[label.as_bytes()],
        ),
        bytes,
    }
}

fn synthetic_identity_from_pairs(label: &str, pairs: &BTreeMap<String, String>) -> SourceIdentity {
    let mut parts = Vec::new();
    for (key, value) in pairs {
        parts.push(key.as_bytes());
        parts.push(value.as_bytes());
    }
    let refs: Vec<&[u8]> = parts;
    SourceIdentity {
        identity_digest: digest(
            b"thegn/config-admission/pairs-identity/v1",
            &[label.as_bytes()],
        ),
        content_digest: digest(b"thegn/config-admission/pairs-content/v1", &refs),
        bytes: pairs
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum(),
    }
}

fn synthetic_identity_from_strings(label: &str, values: &[String]) -> SourceIdentity {
    let refs: Vec<&[u8]> = values.iter().map(String::as_bytes).collect();
    SourceIdentity {
        identity_digest: digest(
            b"thegn/config-admission/strings-identity/v1",
            &[label.as_bytes()],
        ),
        content_digest: digest(b"thegn/config-admission/strings-content/v1", &refs),
        bytes: values.iter().map(String::len).sum(),
    }
}

fn invalid_supplied_diagnostics(messages: &[String]) -> Vec<AdmissionDiagnostic> {
    messages
        .iter()
        .filter(|message| {
            message.contains("ignoring")
                || message.contains("override ignored")
                || message.contains("invalid supplied value")
        })
        .map(|message| AdmissionDiagnostic {
            severity: DiagnosticSeverity::Error,
            message: safe_message(message),
        })
        .collect()
}

fn safe_message(message: &str) -> String {
    let redacted = crate::log_redact::redact_text_line(message);
    redacted
        .chars()
        .take(config_budget::MAX_DIAGNOSTIC_BYTES)
        .collect()
}

fn bounded_diagnostics(
    diagnostics: impl Iterator<Item = AdmissionDiagnostic>,
) -> Vec<AdmissionDiagnostic> {
    diagnostics.take(config_budget::MAX_DIAGNOSTICS).collect()
}

fn digest(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain.len().to_le_bytes());
    hasher.update(domain);
    for part in parts {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

#[cfg(test)]
#[path = "config_admission_tests.rs"]
mod tests;
