//! The typed, bounded core configuration admission boundary.
//!
//! This module accepts already-captured sources.  It deliberately owns no
//! filesystem, environment enumeration, database, or host-process side
//! effects.  The host will wire those capture seams in a later chunk; until
//! then the legacy loader remains available for existing callers, but this
//! boundary never calls its fallback-to-default path.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::io;

use sha2::{Digest, Sha256};

use crate::config::{Config, EnvSource, PathExpansionContext};
use crate::config_budget;
use crate::config_validate::{self, SemanticMode};
use crate::host_definition_snapshot::HostDefinitionsSnapshot;

pub const NORMALIZATION_VERSION: u32 = 1;

/// An already-captured source.  `identity` is hashed and never exposed in a
/// diagnostic; callers may pass an absolute path, a logical source name, or a
/// platform-specific identity token without making it a leak surface.
#[derive(Clone, Copy)]
pub struct SourceInput<'a> {
    pub identity: &'a str,
    pub explicit: bool,
    pub content: SourceContent<'a>,
}

#[derive(Clone, Copy)]
pub enum SourceContent<'a> {
    Absent,
    Bytes(&'a [u8]),
    Failure(SourceFailure),
}

impl fmt::Debug for SourceInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceInput")
            .field("identity", &"<redacted>")
            .field("explicit", &self.explicit)
            .field("content", &self.content)
            .finish()
    }
}

impl fmt::Debug for SourceContent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("Absent"),
            Self::Bytes(bytes) => f
                .debug_tuple("Bytes")
                .field(&format_args!("<{} bytes>", bytes.len()))
                .finish(),
            Self::Failure(failure) => f.debug_tuple("Failure").field(failure).finish(),
        }
    }
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
    pub paths: &'a PathExpansionContext,
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

#[derive(Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    pub identity_digest: [u8; 32],
    pub bytes: usize,
    content_digest: [u8; 32],
}

impl fmt::Debug for SourceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceIdentity")
            .field("identity_digest", &"<redacted>")
            .field("content_digest", &"<redacted>")
            .field("bytes", &self.bytes)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SourceIdentities {
    base: SourceIdentity,
    profile: Option<SourceIdentity>,
    environment_digest: [u8; 32],
    overrides_digest: [u8; 32],
    host_digest: [u8; 32],
    path_context_digest: [u8; 32],
}

impl fmt::Debug for SourceIdentities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceIdentities")
            .field("base", &self.base)
            .field("profile", &self.profile)
            .field("environment_digest", &"<redacted>")
            .field("overrides_digest", &"<redacted>")
            .field("host_digest", &"<redacted>")
            .field("path_context_digest", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConfigRevision {
    pub generation: u64,
    pub normalization_version: u32,
    pub host_schema: i64,
    digest: [u8; 32],
    sources: SourceIdentities,
}

impl fmt::Debug for ConfigRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigRevision")
            .field("generation", &self.generation)
            .field("digest", &"<redacted>")
            .field("sources", &self.sources)
            .field("normalization_version", &self.normalization_version)
            .field("host_schema", &self.host_schema)
            .finish()
    }
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
    normalized_digest: [u8; 32],
    pub diagnostics: Vec<AdmissionDiagnostic>,
}

impl fmt::Debug for LayerTraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerTraceEntry")
            .field("layer", &self.layer)
            .field("source", &self.source)
            .field("normalized_digest", &"<redacted>")
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
}

struct ParsedLayer {
    identity: SourceIdentity,
    normalized_digest: [u8; 32],
    normalized: String,
    diagnostics: Vec<AdmissionDiagnostic>,
}

struct CapturedEnv<'a> {
    source: &'a dyn EnvSource,
    values: RefCell<BTreeMap<String, Option<String>>>,
    bytes: Cell<usize>,
    budget_error: Cell<bool>,
}

impl<'a> CapturedEnv<'a> {
    fn new(source: &'a dyn EnvSource) -> Self {
        Self {
            source,
            values: RefCell::new(BTreeMap::new()),
            bytes: Cell::new(0),
            budget_error: Cell::new(false),
        }
    }

    fn values(&self) -> BTreeMap<String, String> {
        self.values
            .borrow()
            .iter()
            .filter_map(|(key, value)| value.clone().map(|value| (key.clone(), value)))
            .collect()
    }

    fn budget_error(&self) -> bool {
        self.budget_error.get()
    }
}

impl EnvSource for CapturedEnv<'_> {
    fn get(&self, key: &str) -> Option<String> {
        if let Some(value) = self.values.borrow().get(key) {
            return value.clone();
        }
        let value = self.source.get(key);
        let valid = value.as_ref().is_none_or(|value| {
            let entry = key.len().checked_add(value.len());
            let Some(entry) = entry else {
                return false;
            };
            value.len() <= config_budget::MAX_ENV_VALUE_BYTES
                && self.values.borrow().len() < config_budget::MAX_ENV_ENTRIES
                && self
                    .bytes
                    .get()
                    .checked_add(entry)
                    .is_some_and(|total| total <= config_budget::MAX_ENV_BYTES)
        });
        if !valid {
            self.budget_error.set(true);
            self.values.borrow_mut().insert(key.to_string(), None);
            return None;
        }
        if let Some(value) = &value {
            self.bytes.set(self.bytes.get() + key.len() + value.len());
        }
        self.values
            .borrow_mut()
            .insert(key.to_string(), value.clone());
        value
    }
}

/// Admit one complete candidate.  No legacy loader is called here, and all
/// failures return before an `AdmittedConfig` can be constructed.
pub fn admit(inputs: AdmissionInputs<'_>) -> Result<AdmittedConfig, ConfigAdmissionError> {
    let path_context_digest = validate_admission_context(&inputs)?;
    validate_source_identity(&inputs.base)?;
    if let Some(profile) = inputs.profile {
        validate_source_identity(&profile)?;
    }
    crate::host_config_checked::check_config_bounds(&inputs.defaults)
        .map_err(|_| ConfigAdmissionError::Oversized)?;
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
        check_config_bounds(&cfg)?;
        apply_layer_overlay(&mut cfg, &base.normalized)?;
        check_config_bounds(&cfg)?;
    }
    if let Some(profile) = profile {
        diagnostics.extend(profile.diagnostics.clone());
        entries.push(trace_entry(LayerKind::Profile, &profile));
        check_config_bounds(&cfg)?;
        apply_layer_overlay(&mut cfg, &profile.normalized)
            .map_err(|_| ConfigAdmissionError::ProfileInvalid)?;
        check_config_bounds(&cfg).map_err(|_| ConfigAdmissionError::ProfileInvalid)?;
    }

    let captured_env = CapturedEnv::new(inputs.env);
    let (overlay, env_warnings, env_input_errors) =
        crate::config_diagnostics::capture_quiet(|| crate::config::env_overlay(&captured_env));
    if captured_env.budget_error() {
        return Err(ConfigAdmissionError::Oversized);
    }
    let env_values = captured_env.values();
    check_env_budget(&env_values)?;
    if !env_input_errors.is_empty() {
        return Err(ConfigAdmissionError::EnvironmentInvalid);
    }
    let ((), duration_warnings, duration_input_errors) =
        crate::config_diagnostics::capture_quiet(|| {
            crate::config::apply_env_overlay_checked(&mut cfg, overlay)
        });
    if !duration_input_errors.is_empty() {
        return Err(ConfigAdmissionError::EnvironmentInvalid);
    }
    check_config_bounds(&cfg)?;
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
    // Validate the raw composed candidate before normalization can clamp,
    // drop, or replace a supplied value.  Pure post-processing is then safe
    // to use for derived defaults and path expansion.
    check_pre_process_candidate(&cfg)?;
    check_final_config(&cfg)?;
    cfg.post_process_pure(inputs.paths);
    check_normalized_candidate(&cfg)?;
    // Validate the complete file/profile/env/CLI candidate before host rows are
    // merged, so a trusted-layer semantic failure cannot be mislabeled as a
    // host-source failure. The host composition repeats the same strict
    // checks after its bounded merge.
    check_final_config(&cfg)?;

    let composed = crate::host_config_checked::compose_host_definitions_checked(&cfg, inputs.hosts)
        .map_err(|_| ConfigAdmissionError::HostInvalid)?;
    cfg = composed.config().clone();
    check_pre_process_candidate(&cfg)?;
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
        path_context_digest,
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
            &path_context_digest,
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

fn validate_source_identity(input: &SourceInput<'_>) -> Result<(), ConfigAdmissionError> {
    if input.identity.len() > config_budget::MAX_CONTEXT_BYTES {
        Err(ConfigAdmissionError::Oversized)
    } else {
        Ok(())
    }
}

fn validate_admission_context(
    inputs: &AdmissionInputs<'_>,
) -> Result<[u8; 32], ConfigAdmissionError> {
    let home = inputs.paths.home().to_string_lossy();
    if home.len() > config_budget::MAX_CONTEXT_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    Ok(digest(
        b"thegn/config-admission/path-context/v1",
        &[home.as_bytes()],
    ))
}

fn check_config_bounds(cfg: &Config) -> Result<(), ConfigAdmissionError> {
    crate::host_config_checked::check_config_bounds(cfg)
        .map_err(|_| ConfigAdmissionError::Oversized)
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
    let normalized = crate::config_compat::normalize_admission(body).map_err(|error| {
        layer_error(
            layer,
            if matches!(error, crate::config_compat::NormalizeError::Budget(_)) {
                ConfigAdmissionError::Oversized
            } else {
                ConfigAdmissionError::ParseInvalid
            },
        )
    })?;
    if normalized.body.len() > config_budget::MAX_NORMALIZED_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    let raw: toml::Value = normalized
        .body
        .parse()
        .map_err(|_| layer_error(layer, ConfigAdmissionError::ParseInvalid))?;
    config_budget::check_toml_value(&raw)
        .map_err(|_| layer_error(layer, ConfigAdmissionError::Oversized))?;
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
        if key.is_empty()
            || key.len() > config_budget::MAX_CONTEXT_BYTES
            || key.split('.').count() > config_budget::MAX_DEPTH
        {
            return Err(ConfigAdmissionError::Oversized);
        }
        let trimmed = value.trim();
        let bracketed = (trimmed.starts_with('[') && trimmed.ends_with(']'))
            || (trimmed.starts_with('{') && trimmed.ends_with('}'));
        if bracketed {
            let mut source = String::with_capacity(trimmed.len() + 4);
            source.push_str("v = ");
            source.push_str(trimmed);
            config_budget::scan(source.as_bytes()).map_err(|_| ConfigAdmissionError::Oversized)?;
            let parsed: toml::Value = source
                .parse()
                .map_err(|_| ConfigAdmissionError::CliInvalid)?;
            config_budget::check_toml_value(&parsed)
                .map_err(|_| ConfigAdmissionError::Oversized)?;
        }
    }
    if total > config_budget::MAX_CLI_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }

    // Every structural/aggregate budget is now proven before the first
    // serde-json conversion or mutation of the candidate.
    for override_value in overrides {
        let (key, value) = override_value
            .split_once('=')
            .ok_or(ConfigAdmissionError::CliInvalid)?;
        validate_cli_override_shape(cfg, key, value)?;
        check_config_bounds(cfg)?;
        let (result, _warnings, _) = crate::config_diagnostics::capture_quiet(|| {
            Config::apply_override_str(cfg, key, value)
        });
        if result.is_err() {
            return Err(ConfigAdmissionError::CliInvalid);
        }
        check_config_bounds(cfg)?;
    }
    Ok(synthetic_identity_from_strings("cli", overrides))
}

/// Validate a raw CLI assignment against the same generated schema used for
/// file admission before serde gets a chance to warn-and-default an enum or
/// discard an unknown leaf.  The legacy coercion function is shared so quoted
/// strings, arrays, inline tables, booleans, numbers, and custom map keys keep
/// their established meanings.
fn validate_cli_override_shape(
    cfg: &Config,
    key: &str,
    raw_value: &str,
) -> Result<(), ConfigAdmissionError> {
    let key = crate::config_compat::canonical_key(key);
    let mut tree = bounded_json_value(cfg)?;
    let value = if key == "apps.tab_order" {
        serde_json::Value::Array(
            raw_value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| serde_json::Value::String(value.to_string()))
                .collect(),
        )
    } else {
        Config::coerce_override_value(raw_value)
    };
    let parts: Vec<&str> = key.split('.').collect();
    let mut current = &mut tree;
    for (index, part) in parts.iter().enumerate() {
        if index == parts.len() - 1 {
            if !current.is_object() {
                return Err(ConfigAdmissionError::CliInvalid);
            }
            current[*part] = value.clone();
        } else {
            current = current
                .get_mut(*part)
                .ok_or(ConfigAdmissionError::CliInvalid)?;
            if !current.is_object() {
                return Err(ConfigAdmissionError::CliInvalid);
            }
        }
    }
    if config_validate::validate_config_schema_value(&tree).is_empty() {
        Ok(())
    } else {
        Err(ConfigAdmissionError::CliInvalid)
    }
}

fn check_normalized_candidate(cfg: &Config) -> Result<(), ConfigAdmissionError> {
    check_config_bounds(cfg)?;
    let bytes = bounded_serialized_config(cfg)?;
    if bytes.len() > config_budget::MAX_NORMALIZED_BYTES {
        return Err(ConfigAdmissionError::Oversized);
    }
    Ok(())
}

fn check_final_config(cfg: &Config) -> Result<(), ConfigAdmissionError> {
    check_config_bounds(cfg)?;
    let value = bounded_json_value(cfg)?;
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

fn check_pre_process_candidate(cfg: &Config) -> Result<(), ConfigAdmissionError> {
    let invalid = cfg.metrics.interval_secs < 1.0
        || cfg.metrics.timeout_ms < 100
        || cfg.metrics.timeout_ms > 30_000
        || cfg.metrics.max_body_bytes == 0
        || cfg.metrics.targets.iter().any(|target| {
            (target.kind == crate::config::MetricsTargetKind::Command
                && target.command_argv().is_none())
                || (target.kind == crate::config::MetricsTargetKind::Prometheus
                    && target.url.trim().is_empty())
        })
        || cfg.preview.fetch_timeout_ms < crate::config_preview::MIN_FETCH_TIMEOUT_MS
        || cfg.preview.fetch_timeout_ms > crate::config_preview::MAX_FETCH_TIMEOUT_MS
        || cfg.preview.max_body_bytes == 0
        || cfg.preview.max_body_bytes > crate::config_preview::MAX_PREVIEW_BODY_BYTES
        || cfg.preview.ports.contains(&0)
        || cfg.clipboard.max_image_bytes == 0
        || cfg.clipboard.keep_hours == 0
        || cfg.clipboard.remote_dir.trim().is_empty()
        || crate::config::validate_strftime(&cfg.bars.date_format).is_err()
        || crate::config::validate_strftime(&cfg.bars.clock_format).is_err();
    if invalid {
        Err(ConfigAdmissionError::SemanticInvalid)
    } else {
        Ok(())
    }
}

fn bounded_serialized_config(cfg: &Config) -> Result<Vec<u8>, ConfigAdmissionError> {
    bounded_json_bytes(cfg)
}

fn host_identity(hosts: &HostDefinitionsSnapshot) -> Result<SourceIdentity, ConfigAdmissionError> {
    let bytes = bounded_json_bytes(&(hosts.observed_schema(), hosts.definitions()))
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

fn bounded_json_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ConfigAdmissionError> {
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let remaining = self.limit.saturating_sub(self.bytes.len());
            if bytes.len() > remaining {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "serialization budget",
                ));
            }
            let needed = self.bytes.len() + bytes.len();
            if needed > self.bytes.capacity() {
                // `try_reserve_exact` takes additional capacity, not the
                // desired final capacity.  Grow geometrically, but clamp the
                // requested final capacity to the logical limit so a write
                // can never retain more than the admitted budget.
                let mut capacity = self.bytes.capacity().max(1);
                while capacity < needed {
                    capacity = capacity.saturating_mul(2).min(self.limit);
                    if capacity < needed && capacity == self.limit {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "serialization budget",
                        ));
                    }
                }
                let additional = capacity - self.bytes.len();
                self.bytes
                    .try_reserve_exact(additional)
                    .map_err(|_| io::Error::other("serialization allocation"))?;
                if self.bytes.capacity() > self.limit {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "serialization capacity budget",
                    ));
                }
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut output = Limited {
        bytes: Vec::with_capacity(config_budget::MAX_NORMALIZED_BYTES.min(4096)),
        limit: config_budget::MAX_NORMALIZED_BYTES,
    };
    serde_json::to_writer(&mut output, value).map_err(|error| {
        if error.is_io() {
            ConfigAdmissionError::Oversized
        } else {
            ConfigAdmissionError::SchemaInvalid
        }
    })?;
    Ok(output.bytes)
}

fn bounded_json_value<T: serde::Serialize>(
    value: &T,
) -> Result<serde_json::Value, ConfigAdmissionError> {
    let bytes = bounded_json_bytes(value)?;
    serde_json::from_slice(&bytes).map_err(|_| ConfigAdmissionError::SchemaInvalid)
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

fn safe_message(message: &str) -> String {
    let redacted = crate::log_redact::redact_text_line(message);
    let limit = config_budget::MAX_DIAGNOSTIC_BYTES;
    if redacted.len() <= limit {
        return redacted;
    }
    let mut end = limit;
    while end > 0 && !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].to_string()
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
