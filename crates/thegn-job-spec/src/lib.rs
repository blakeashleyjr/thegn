//! Portable, provider-independent input to a Thegn Job. Parse at submission,
//! then resolve source, environment and effective policy at the authority.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use thegn_cas::Digest;

pub mod result;
pub use result::{JobResult, JobStatus};
pub mod selected;
pub use selected::{
    MAX_SELECTED_LOG_BYTES, RunnerDirectory, RunnerRefusal, SelectedCancel, SelectedCompletion,
    SelectedFailure, SelectedJobResult, SelectedRunner, selected_job_readback, submit_selected,
};
mod validation;
pub use validation::{byte_quantity, duration_ms};

type Env = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobSpec {
    pub api_version: Version,
    pub kind: Kind,
    pub metadata: Metadata,
    /// Explicit owner-selected runner alias. Used only by typed recipes; it is
    /// independent of the worktree's interactive placement.
    #[serde(default)]
    pub runner: Option<String>,
    /// A small, closed set of build/test operations with fixed argv. A spec
    /// that names a recipe carries no command steps and must be dispatched only
    /// through the typed selected-runner route; generic executors must reject
    /// it.
    #[serde(default)]
    pub recipe: Option<Recipe>,
    #[serde(default)]
    pub source: Source,
    pub environment: Environment,
    #[serde(default)]
    pub resources: Resources,
    pub isolation: Option<Isolation>,
    #[serde(default)]
    pub placement: Placement,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub caches: Vec<Cache>,
    #[serde(default)]
    pub env: Env,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default = "one")]
    pub shards: NonZeroU32,
    #[serde(default)]
    pub results: Parsers,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default = "default_timeout")]
    pub timeout: String,
}

/// Build/test commands admitted for explicit project-runner dispatch. These
/// values never contain command text or shell fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Recipe {
    CargoBuild,
    CargoTest,
}

impl Recipe {
    /// Fixed argument vector for the allowlisted operation.
    pub fn argv(self) -> &'static [&'static str] {
        match self {
            Self::CargoBuild => &["cargo", "build"],
            Self::CargoTest => &["cargo", "test"],
        }
    }
}

/// Structural JSON Schemas. Validate cross-field constraints with JobSpec::validate.
pub fn job_schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(JobSpec)
}
pub fn result_schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(JobResult)
}

fn one() -> NonZeroU32 {
    NonZeroU32::MIN
}
fn default_timeout() -> String {
    "60m".into()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub enum Version {
    #[serde(rename = "thegn.dev/jobs/v0")]
    V0,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub enum Kind {
    Job,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    #[schemars(length(min = 1))]
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Source {
    pub snapshot: Snapshot,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Snapshot {
    Auto(Auto),
    Pinned(Digest),
}
impl Default for Snapshot {
    fn default() -> Self {
        Self::Auto(Auto::Auto)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Auto {
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum Environment {
    Image { image: String },
    Nix { nix: NixEnv },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NixEnv {
    pub flake: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    pub cpu: Option<NonZeroU32>,
    pub memory: Option<String>,
    pub shm: Option<String>,
    pub disk: Option<String>,
    pub arch: Arch,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    #[default]
    Any,
    #[serde(rename = "x86_64")]
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    None,
    Process,
    Container,
    Gvisor,
    Microvm,
    DedicatedHost,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Placement {
    /// Absent means unrestricted within trusted policy. An explicit empty list
    /// admits no candidate.
    pub providers: Option<Vec<String>>,
    pub regions: Option<Vec<String>>,
    pub spot: Spot,
    pub affinity: Vec<Affinity>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Spot {
    #[default]
    Allowed,
    Preferred,
    Forbidden,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Affinity {
    Cache,
    Latency,
    Cost,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: Env,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum Cache {
    Keyed {
        key: String,
        paths: Vec<String>,
        #[serde(default, rename = "restoreKeys")]
        restore_keys: Vec<String>,
    },
    Tool {
        tool: CacheTool,
    },
}
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum CacheTool {
    Sccache,
    Nix,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub run: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub timeout: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Parsers {
    pub junit: Option<String>,
    pub cargo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Retry {
    pub on_preempt: RetryLimit,
    #[serde(default = "no_retries")]
    pub on_failure: RetryLimit,
    pub escalate: Option<Escalation>,
}
impl Default for Retry {
    fn default() -> Self {
        Self {
            on_preempt: RetryLimit::default(),
            on_failure: no_retries(),
            escalate: None,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum RetryLimit {
    Count { max: u32 },
    Never(Never),
}
impl Default for RetryLimit {
    fn default() -> Self {
        Self::Count { max: 2 }
    }
}
fn no_retries() -> RetryLimit {
    RetryLimit::Count { max: 0 }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Never {
    Never,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Escalation {
    OnDemand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    Oci,
    NixEnv,
    Shm,
    Services,
    CacheVolume,
    CacheTool(CacheTool),
    Secrets,
    Junit,
    CargoJson,
}

impl Feature {
    /// Capability identifier advertised in a provider offering or guest Hello.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Oci => "oci",
            Self::NixEnv => "nix-env",
            Self::Shm => "shm",
            Self::Services => "services",
            Self::CacheVolume => "cache-volume",
            Self::CacheTool(CacheTool::Sccache) => "cache-sccache",
            Self::CacheTool(CacheTool::Nix) => "cache-nix",
            Self::Secrets => "secrets",
            Self::Junit => "junit",
            Self::CargoJson => "cargo-json",
        }
    }
}
