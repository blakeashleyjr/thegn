//! Typed submission seam for an explicitly selected project runner.
//!
//! This route accepts only a pinned source identity and one closed recipe. It
//! deliberately does not inspect or intercept arbitrary process commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thegn_cas::Digest;

use crate::result::{Diagnostic, JobResult, JobStatus, TestSummary, Timings, Truncation};
use crate::{JobSpec, Recipe, Snapshot};

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
    /// Earlier output was discarded: by the runner, or by the route when it
    /// bounded the tail. A consumer must not treat the tail as complete.
    pub truncated: bool,
}

/// Terminal state reported by the selected runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedCompletion {
    Exited(i32),
    Cancelled,
    /// The runner stopped the recipe at the submitted deadline.
    TimedOut,
    /// The runner observed its process terminated by a signal.
    Signalled(i32),
    /// The runner returned without proving a terminal state.
    Indeterminate,
}

/// Why a selected submission was refused or left unproven. The route raises
/// these before it calls a runner; a runner also uses them to say what it did
/// not prove, and the route then treats that as [`SelectedFailure::Unproven`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerRefusal {
    InvalidSpec(String),
    SourceMismatch {
        expected: Digest,
        observed: Digest,
    },
    RunnerUnavailable(String),
    /// The runner proves nothing was dispatched (no recipe exec was sent): an
    /// unresolved binding, a snapshot or claim failure, a provider rejection.
    /// The route reads it back as [`SelectedFailure::Refused`], never unproven.
    NotStarted(String),
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
            Self::NotStarted(reason) => write!(f, "{reason}"),
            Self::WrongRunner => f.write_str("runner result came from a different runner"),
            Self::WrongSource => f.write_str("runner result names a different source digest"),
            Self::WrongRecipe => f.write_str("runner result names a different recipe"),
            Self::Indeterminate => f.write_str("runner did not return a terminal exit code"),
        }
    }
}

/// Why a selected submission produced no accepted terminal receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedFailure {
    /// The route refused before it called the runner, so nothing ran.
    Refused(RunnerRefusal),
    /// The route called the runner but recovered no trustworthy terminal
    /// receipt. The recipe may have run, so this is never a plain rejection.
    Unproven(RunnerRefusal),
}

impl std::fmt::Display for SelectedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) | Self::Unproven(refusal) => write!(f, "{refusal}"),
        }
    }
}

type CancelHook = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    hooks: Mutex<Vec<CancelHook>>,
}

/// A user cancel handle for one selected submission. Clones share the state;
/// [`cancel`](Self::cancel) is idempotent and may be called before, during or
/// after the submission. It is std-only so this crate stays free of an async
/// runtime: a runner bridges it to its own wake-up with
/// [`on_cancel`](Self::on_cancel).
#[derive(Clone, Default)]
pub struct SelectedCancel {
    state: Arc<CancelState>,
}

impl std::fmt::Debug for SelectedCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedCancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl SelectedCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation and run every registered hook once.
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::SeqCst);
        let hooks = {
            let mut guard = self
                .state
                .hooks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *guard)
        };
        for hook in hooks {
            hook();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }

    /// Run `hook` when the token is cancelled; immediately when it already is.
    pub fn on_cancel(&self, hook: impl Fn() + Send + Sync + 'static) {
        let mut guard = self
            .state
            .hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_cancelled() {
            drop(guard);
            hook();
        } else {
            guard.push(Box::new(hook));
        }
    }
}

/// A runner that accepts only a fully typed, source-pinned request. `submit`
/// may already have started work when it returns `Err`, so every error it
/// returns is read back as unproven, except [`RunnerRefusal::NotStarted`],
/// which asserts nothing was dispatched. A cancel before or
/// during dispatch must come back as `Ok` with [`SelectedCompletion::Cancelled`].
pub trait SelectedRunner: Send + Sync {
    fn submit(
        &self,
        runner_alias: &str,
        recipe: Recipe,
        source: &Digest,
        deadline: Duration,
        cancel: &SelectedCancel,
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
    cancel: &SelectedCancel,
) -> Result<SelectedJobResult, SelectedFailure> {
    spec.validate()
        .map_err(|error| SelectedFailure::Refused(RunnerRefusal::InvalidSpec(error.to_string())))?;
    let deadline = Duration::from_millis(crate::duration_ms(&spec.timeout).map_err(|error| {
        SelectedFailure::Refused(RunnerRefusal::InvalidSpec(format!("timeout: {error}")))
    })?);
    let runner_alias = spec.runner.as_deref().ok_or_else(|| {
        SelectedFailure::Refused(RunnerRefusal::InvalidSpec("runner is not selected".into()))
    })?;
    let recipe = spec.recipe.ok_or_else(|| {
        SelectedFailure::Refused(RunnerRefusal::InvalidSpec(
            "typed recipe is not selected".into(),
        ))
    })?;
    let Snapshot::Pinned(expected) = &spec.source.snapshot else {
        return Err(SelectedFailure::Refused(RunnerRefusal::InvalidSpec(
            "source is not pinned".into(),
        )));
    };
    if expected != captured_source {
        return Err(SelectedFailure::Refused(RunnerRefusal::SourceMismatch {
            expected: expected.clone(),
            observed: captured_source.clone(),
        }));
    }
    if cancel.is_cancelled() {
        // Nothing was sent: a cancel before dispatch is a cancelled receipt.
        return Ok(SelectedJobResult {
            runner: runner_alias.to_string(),
            recipe,
            source: expected.clone(),
            completion: SelectedCompletion::Cancelled,
            log_tail: String::new(),
            truncated: false,
        });
    }
    let runner = directory.resolve(runner_alias).ok_or_else(|| {
        SelectedFailure::Refused(RunnerRefusal::RunnerUnavailable(runner_alias.to_string()))
    })?;
    // The runner call itself may have started work, so nothing it returns is a
    // pre-submission refusal. A lost or mismatched receipt stays indeterminate.
    let result = runner
        .submit(runner_alias, recipe, expected, deadline, cancel)
        .map_err(|refusal| match refusal {
            // The runner proved nothing was sent, so this is a plain refusal.
            RunnerRefusal::NotStarted(_) => SelectedFailure::Refused(refusal),
            other => SelectedFailure::Unproven(other),
        })?;
    if result.runner != runner_alias {
        return Err(SelectedFailure::Unproven(RunnerRefusal::WrongRunner));
    }
    if &result.source != expected {
        return Err(SelectedFailure::Unproven(RunnerRefusal::WrongSource));
    }
    if result.recipe != recipe {
        return Err(SelectedFailure::Unproven(RunnerRefusal::WrongRecipe));
    }
    if result.completion == SelectedCompletion::Indeterminate {
        return Err(SelectedFailure::Unproven(RunnerRefusal::Indeterminate));
    }
    Ok(bound_log_tail(result))
}

/// Adapt a selected-runner receipt or failure to the established user-facing
/// job result shape. This keeps selected jobs readable by the same consumers
/// as ordinary jobs without inventing provider placement or run identifiers.
pub fn selected_job_readback(
    job_id: impl Into<String>,
    outcome: Result<SelectedJobResult, SelectedFailure>,
) -> JobResult {
    let (status, receipt, diagnostic) = match outcome {
        Ok(result) => {
            let (status, diagnostic) = match result.completion {
                SelectedCompletion::Exited(0) => (JobStatus::Succeeded, None),
                SelectedCompletion::Exited(code) => {
                    let reason = format!("selected runner exited with status {code}");
                    (
                        JobStatus::Failed {
                            reason: reason.clone(),
                        },
                        Some(reason),
                    )
                }
                SelectedCompletion::Cancelled => (
                    JobStatus::Cancelled,
                    Some("selected runner cancelled the job".into()),
                ),
                SelectedCompletion::TimedOut => (
                    JobStatus::TimedOut,
                    Some("selected runner reached the submitted deadline".into()),
                ),
                SelectedCompletion::Signalled(signal) => {
                    let reason = format!("selected runner was terminated by signal {signal}");
                    (
                        JobStatus::Failed {
                            reason: reason.clone(),
                        },
                        Some(reason),
                    )
                }
                SelectedCompletion::Indeterminate => {
                    let reason = "selected runner did not prove a terminal result".to_owned();
                    (
                        JobStatus::Indeterminate {
                            reason: reason.clone(),
                        },
                        Some(reason),
                    )
                }
            };
            (status, Some(result), diagnostic)
        }
        Err(SelectedFailure::Refused(refusal)) => {
            let reason = refusal.to_string();
            (
                JobStatus::Rejected {
                    reason: reason.clone(),
                    missing_capabilities: vec![],
                },
                None,
                Some(reason),
            )
        }
        Err(SelectedFailure::Unproven(refusal)) => {
            let reason = refusal.to_string();
            (
                JobStatus::Indeterminate {
                    reason: reason.clone(),
                },
                None,
                Some(reason),
            )
        }
    };
    let (log_tail, truncated_log) = match receipt {
        Some(result) => {
            let result = bound_log_tail(result);
            (result.log_tail, result.truncated)
        }
        None => (String::new(), false),
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
        truncated: Truncation {
            log_tail: truncated_log,
            ..Truncation::default()
        },
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
        result.truncated = true;
    }
    result
}
