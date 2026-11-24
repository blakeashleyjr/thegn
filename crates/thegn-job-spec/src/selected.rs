//! Typed submission seam for an explicitly selected project runner.
//!
//! This route accepts only a pinned source identity and one closed recipe. It
//! deliberately does not inspect or intercept arbitrary process commands.

use thegn_cas::Digest;

use crate::{
    Diagnostic, JobResult, JobSpec, JobStatus, Recipe, Snapshot, TestSummary, Timings, Truncation,
};

/// Maximum retained output from a selected build/test recipe.
pub const MAX_SELECTED_LOG_BYTES: usize = 16 * 1024;

/// Outcome returned by a runner after it has executed the requested recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedJobResult {
    pub runner: String,
    pub recipe: Recipe,
    pub source: Digest,
    pub completion: SelectedCompletion,
    pub log_tail: String,
}

/// Terminal state reported by the selected runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedCompletion {
    Exited(i32),
    Cancelled,
    /// The runner returned without proving a terminal state.
    Indeterminate,
}

/// Typed refusal before any runner call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerRefusal {
    InvalidSpec(String),
    SourceMismatch { expected: Digest, observed: Digest },
    RunnerUnavailable(String),
    WrongRunner,
    WrongSource,
    WrongRecipe,
    Indeterminate,
}

impl std::fmt::Display for RunnerRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSpec(reason) => write!(f, "invalid selected job: {reason}"),
            Self::SourceMismatch { .. } => {
                f.write_str("captured source digest does not match the pinned identity")
            }
            Self::RunnerUnavailable(name) => write!(f, "selected runner {name:?} is unavailable"),
            Self::WrongRunner => f.write_str("runner result came from a different runner"),
            Self::WrongSource => f.write_str("runner result names a different source digest"),
            Self::WrongRecipe => f.write_str("runner result names a different recipe"),
            Self::Indeterminate => f.write_str("runner did not return a terminal exit code"),
        }
    }
}

/// A runner that accepts only a fully typed, source-pinned request.
pub trait SelectedRunner: Send + Sync {
    fn submit(
        &self,
        runner_alias: &str,
        recipe: Recipe,
        source: &Digest,
    ) -> Result<SelectedJobResult, RunnerRefusal>;
}

/// Owner-controlled registry. `resolve` must return only explicitly
/// preauthorized runner aliases; there is no placement-based fallback.
pub trait RunnerDirectory {
    fn resolve(&self, alias: &str) -> Option<&dyn SelectedRunner>;
}

/// Verify captured source identity, select exactly the configured runner, and
/// validate the runner's receipt before exposing a result.
pub fn submit_selected(
    spec: &JobSpec,
    captured_source: &Digest,
    directory: &dyn RunnerDirectory,
) -> Result<SelectedJobResult, RunnerRefusal> {
    spec.validate()
        .map_err(|error| RunnerRefusal::InvalidSpec(error.to_string()))?;
    let runner_alias = spec
        .runner
        .as_deref()
        .ok_or_else(|| RunnerRefusal::InvalidSpec("runner is not selected".into()))?;
    let recipe = spec
        .recipe
        .ok_or_else(|| RunnerRefusal::InvalidSpec("typed recipe is not selected".into()))?;
    let Snapshot::Pinned(expected) = &spec.source.snapshot else {
        return Err(RunnerRefusal::InvalidSpec("source is not pinned".into()));
    };
    if expected != captured_source {
        return Err(RunnerRefusal::SourceMismatch {
            expected: expected.clone(),
            observed: captured_source.clone(),
        });
    }
    let runner = directory
        .resolve(runner_alias)
        .ok_or_else(|| RunnerRefusal::RunnerUnavailable(runner_alias.to_string()))?;
    let result = runner.submit(runner_alias, recipe, expected)?;
    if result.runner != runner_alias {
        return Err(RunnerRefusal::WrongRunner);
    }
    if &result.source != expected {
        return Err(RunnerRefusal::WrongSource);
    }
    if result.recipe != recipe {
        return Err(RunnerRefusal::WrongRecipe);
    }
    if result.completion == SelectedCompletion::Indeterminate {
        return Err(RunnerRefusal::Indeterminate);
    }
    Ok(bound_log_tail(result))
}

/// Adapt a selected-runner receipt or refusal to the established user-facing
/// job result shape. This keeps selected jobs readable by the same consumers
/// as ordinary jobs without inventing provider placement or run identifiers.
pub fn selected_job_readback(
    job_id: impl Into<String>,
    outcome: Result<SelectedJobResult, RunnerRefusal>,
) -> JobResult {
    let (status, log_tail, diagnostic) = match outcome {
        Ok(result) => {
            let result = bound_log_tail(result);
            match result.completion {
                SelectedCompletion::Exited(0) => (JobStatus::Succeeded, result.log_tail, None),
                SelectedCompletion::Exited(code) => {
                    let reason = format!("selected runner exited with status {code}");
                    (
                        JobStatus::Failed {
                            reason: reason.clone(),
                        },
                        result.log_tail,
                        Some(reason),
                    )
                }
                SelectedCompletion::Cancelled => (
                    JobStatus::Cancelled,
                    result.log_tail,
                    Some("selected runner cancelled the job".into()),
                ),
                SelectedCompletion::Indeterminate => {
                    let reason = "selected runner did not prove a terminal result".to_owned();
                    (
                        JobStatus::Indeterminate {
                            reason: reason.clone(),
                        },
                        result.log_tail,
                        Some(reason),
                    )
                }
            }
        }
        Err(refusal @ RunnerRefusal::RunnerUnavailable(_)) => {
            let reason = refusal.to_string();
            (
                JobStatus::Rejected {
                    reason: reason.clone(),
                    missing_capabilities: vec![],
                },
                String::new(),
                Some(reason),
            )
        }
        Err(RunnerRefusal::Indeterminate) => {
            let reason = RunnerRefusal::Indeterminate.to_string();
            (
                JobStatus::Indeterminate {
                    reason: reason.clone(),
                },
                String::new(),
                Some(reason),
            )
        }
        Err(
            refusal @ (RunnerRefusal::WrongRunner
            | RunnerRefusal::WrongSource
            | RunnerRefusal::WrongRecipe),
        ) => {
            let reason = refusal.to_string();
            (
                JobStatus::Indeterminate {
                    reason: reason.clone(),
                },
                String::new(),
                Some(reason),
            )
        }
        Err(refusal) => {
            let reason = refusal.to_string();
            (
                JobStatus::Rejected {
                    reason: reason.clone(),
                    missing_capabilities: vec![],
                },
                String::new(),
                Some(reason),
            )
        }
    };
    let diagnostics = diagnostic
        .map(|message| {
            vec![Diagnostic {
                level: "error".into(),
                code: None,
                message,
                file: None,
                line: None,
            }]
        })
        .unwrap_or_default();
    let incomplete = matches!(
        &status,
        JobStatus::Cancelled | JobStatus::TimedOut | JobStatus::Indeterminate { .. }
    );
    JobResult {
        job_id: job_id.into(),
        status,
        shards: vec![],
        summary: TestSummary {
            incomplete,
            ..TestSummary::default()
        },
        failures: vec![],
        diagnostics,
        truncated: Truncation::default(),
        log_tail,
        logs: vec![],
        artifacts: vec![],
        timings: Timings::default(),
        cache: Default::default(),
        placement: vec![],
        cost: crate::result::Cost {
            currency: "USD".into(),
            compute_micros: None,
            storage_micros: None,
            egress_micros: None,
            estimate_source: None,
        },
    }
}

fn bound_log_tail(mut result: SelectedJobResult) -> SelectedJobResult {
    if result.log_tail.len() > MAX_SELECTED_LOG_BYTES {
        let mut start = result.log_tail.len() - MAX_SELECTED_LOG_BYTES;
        while !result.log_tail.is_char_boundary(start) {
            start += 1;
        }
        result.log_tail.drain(..start);
    }
    result
}
