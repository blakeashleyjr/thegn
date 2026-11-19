//! Terminal results emitted by the authority. Consumers may ignore additive
//! result fields. Fields with unavailable measurements carry None.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thegn_cas::Digest;

use crate::Isolation;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub job_id: String,
    pub status: JobStatus,
    pub shards: Vec<ShardResult>,
    pub summary: TestSummary,
    pub failures: Vec<Failure>,
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: Truncation,
    pub log_tail: String,
    pub logs: Vec<LogRef>,
    pub artifacts: Vec<Artifact>,
    pub timings: Timings,
    pub cache: BTreeMap<String, CacheStats>,
    pub placement: Vec<PlacementRecord>,
    pub cost: Cost,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
pub enum JobStatus {
    Succeeded,
    Failed {
        reason: String,
    },
    Cancelled,
    TimedOut,
    Rejected {
        reason: String,
        #[serde(rename = "missingCapabilities")]
        missing_capabilities: Vec<String>,
    },
    Indeterminate {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShardResult {
    pub index: NonZeroU32,
    pub attempts: Vec<RunAttempt>,
    pub exit: Option<Exit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunAttempt {
    pub run_id: String,
    pub worker_id: String,
    pub epoch: NonZeroU64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Outcome {
    Exited(Exit),
    Preempted,
    LeaseExpired,
    Fenced,
    Cancelled,
    TimedOut,
    InfrastructureFailure(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestSummary {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub flaky: u64,
    pub duration_ms: Option<u64>,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub test_id: String,
    pub suite: Option<String>,
    pub file: Option<String>,
    pub line: Option<NonZeroU32>,
    pub message: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<NonZeroU32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Truncation {
    pub failures: bool,
    pub diagnostics: bool,
    pub log_tail: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogRef {
    pub run_id: String,
    pub digest: Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub shard: NonZeroU32,
    pub run_id: String,
    pub path: String,
    pub digest: Digest,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Timings {
    pub wall_ms: Option<u64>,
    pub queued_ms: Option<u64>,
    pub attempts: BTreeMap<String, AttemptTimings>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttemptTimings {
    pub placement_ms: Option<u64>,
    pub wake_ms: Option<u64>,
    pub sync_ms: Option<u64>,
    pub cache_restore_ms: Option<u64>,
    pub steps: Vec<StepTiming>,
    pub cache_save_ms: Option<u64>,
    pub upload_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepTiming {
    pub name: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlacementRecord {
    pub run_id: String,
    pub provider: String,
    pub pool: Option<String>,
    pub region: Option<String>,
    pub shape: String,
    pub isolation: Isolation,
    pub spot: bool,
}

/// Monetary amounts are integer millionths of the named currency.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub currency: String,
    pub compute_micros: Option<u64>,
    pub storage_micros: Option<u64>,
    pub egress_micros: Option<u64>,
    pub estimate_source: Option<String>,
}
