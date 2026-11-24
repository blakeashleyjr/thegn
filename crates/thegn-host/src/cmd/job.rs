//! `thegn job run` — run a typed build/test recipe on the project's explicitly
//! selected runner (`[jobs] runner`, or `--runner <env>`).
//!
//! The recipe is one of a closed set (`cargo-build`, `cargo-test`); there is no
//! free-form command. The current worktree is snapshotted, its REAPI digest is
//! pinned into the spec, and the route (`submit_selected`) refuses any runner
//! that does not return a receipt for exactly that source and recipe. Ctrl-C
//! cancels the job. The process exits with the job's exit code.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_svc::selected_job::{
    self, Recipe, SelectedCancel, SelectedCompletion, SelectedFailure, SelectedJobResult,
};

use crate::selected_runner::{self, AttemptSummary};

/// Default wall clock for a recipe, in seconds (the executors clamp it to their
/// own limit).
const DEFAULT_DEADLINE_SECS: u64 = 1800;
/// The longest `--deadline` accepted (24 hours).
const MAX_DEADLINE_SECS: u64 = 86_400;
const INTERRUPT_POLL: Duration = Duration::from_millis(100);

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Run a build or test recipe on the project's selected Sprites or Vercel
    /// runner, then print its receipt and exit with the job's exit code.
    Run(RunArgs),
}

/// The closed set of recipes `job run` can submit.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeArg {
    /// `cargo build` in the pinned snapshot.
    CargoBuild,
    /// `cargo test` in the pinned snapshot.
    CargoTest,
}

impl RecipeArg {
    fn recipe(self) -> Recipe {
        match self {
            Self::CargoBuild => Recipe::CargoBuild,
            Self::CargoTest => Recipe::CargoTest,
        }
    }
}

#[derive(clap::Args, Clone, Debug)]
pub struct RunArgs {
    /// The env (`[env.<name>]`, provider sprites or vercel) that runs the job.
    /// Defaults to `[jobs] runner`.
    #[arg(long, value_name = "ENV")]
    pub runner: Option<String>,
    /// Longest wall clock for the recipe, in seconds. The runner clamps it to
    /// its own limit.
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = DEFAULT_DEADLINE_SECS,
        value_parser = clap::value_parser!(u64).range(1..=MAX_DEADLINE_SECS)
    )]
    pub deadline: u64,
    /// What to run.
    #[arg(value_enum)]
    pub recipe: RecipeArg,
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Run(args) => run_recipe(cfg, &args),
    }
}

/// Refuse to start a selected job from inside one: the launchers set the marker
/// in the recipe's environment, so a recipe that shells back into `thegn job
/// run` cannot fan out.
fn refuse_recursion(inside: bool) -> Result<()> {
    if inside {
        bail!(
            "refusing to run a selected job from inside one ({} is set): a recipe cannot \
             start another remote job",
            selected_job::SELECTED_JOB_ENV
        );
    }
    Ok(())
}

fn run_recipe(cfg: &Config, args: &RunArgs) -> Result<()> {
    refuse_recursion(selected_job::inside_selected_job())?;
    let env_name = selected_runner::select_env(cfg, args.runner.as_deref())?;
    let worktree = super::resolve_worktree(None);
    let repo_root = thegn_core::repo::main_worktree(&worktree).ok_or_else(|| {
        anyhow::Error::new(super::NotFound(format!(
            "not a git repository: {}",
            worktree.display()
        )))
    })?;
    let project = selected_runner::project_key(&repo_root).ok_or_else(|| {
        anyhow!(
            "cannot derive a project name from {}; it needs an ASCII letter or digit",
            repo_root.display()
        )
    })?;
    // The runtime that drives the executor's provider calls. The work thread
    // below blocks on it through `submit`; this thread only watches for Ctrl-C.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("thegn-job-rt")
        .build()
        .map_err(|error| anyhow!("cannot start the job runtime: {error}"))?;
    let runner = Arc::new(selected_runner::build(
        cfg,
        env_name,
        &project,
        &worktree,
        rt.handle().clone(),
    )?);
    let alias = runner.alias().to_owned();
    let recipe = args.recipe.recipe();
    let deadline = Duration::from_secs(args.deadline);
    thegn_core::msg::info(&format!(
        "running {} on {alias} (env {env_name}); Ctrl-C cancels",
        selected_job::recipe_name(recipe)
    ));

    let guard = crate::platform::InterruptGuard::install();
    let cancel = SelectedCancel::new();
    let work = {
        let runner = Arc::clone(&runner);
        let cancel = cancel.clone();
        let alias = alias.clone();
        let worktree = worktree.clone();
        thegn_core::thread_name::spawn_named("thegn-job-run", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            selected_job::run_selected(
                runner.selected(),
                &alias,
                recipe,
                &worktree,
                deadline,
                &cancel,
            )
        })
    };
    let mut interrupted = false;
    while !work.is_finished() {
        if !interrupted && guard.fired() {
            interrupted = true;
            cancel.cancel();
            thegn_core::msg::warn(if runner.cancels_mid_run() {
                "interrupt received: cancelling the remote job"
            } else {
                "interrupt received: this runner cannot stop a running job yet; waiting for \
                 it to end or reach its deadline"
            });
        }
        std::thread::sleep(INTERRUPT_POLL);
    }
    let submitted = match work.join() {
        Ok(submitted) => submitted,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    drop(guard);
    // Local failure before anything was sent (snapshot or spec).
    let outcome = submitted?;
    let summary = outcome
        .as_ref()
        .ok()
        .and_then(|result| runner.latest_attempt(result));
    let result = match outcome {
        Ok(result) => result,
        Err(SelectedFailure::Refused(refusal)) => {
            bail!("job refused before dispatch, nothing ran: {refusal}")
        }
        Err(SelectedFailure::Unproven(refusal)) => bail!(
            "job outcome unproven: {refusal}. The recipe may have run; check the runner's \
             ledger before retrying"
        ),
    };
    outln!(
        "{}",
        render_receipt(env_name, &alias, args.deadline, &result, summary)
    );
    rt.shutdown_timeout(Duration::from_secs(2));
    let code = exit_code(result.completion);
    if code != super::EXIT_OK {
        std::process::exit(code);
    }
    Ok(())
}

/// The process exit code for a terminal state: the recipe's own code when it
/// exited non-zero, `130` for a cancel, `124` for a deadline (as `timeout(1)`),
/// `128 + signal` for a signal, and the generic failure code when no terminal
/// state was proven or the code is out of range.
pub(crate) fn exit_code(completion: SelectedCompletion) -> i32 {
    match completion {
        SelectedCompletion::Exited(0) => super::EXIT_OK,
        SelectedCompletion::Exited(code) if (1..=255).contains(&code) => code,
        SelectedCompletion::Exited(_) | SelectedCompletion::Indeterminate => super::EXIT_ERROR,
        SelectedCompletion::Cancelled => 130,
        SelectedCompletion::TimedOut => 124,
        SelectedCompletion::Signalled(signal) => signal.saturating_add(128).clamp(1, 255),
    }
}

fn completion_text(completion: SelectedCompletion, deadline_secs: u64) -> String {
    match completion {
        SelectedCompletion::Exited(code) => format!("exited {code}"),
        SelectedCompletion::Cancelled => "cancelled".to_owned(),
        SelectedCompletion::TimedOut => format!("timed out (deadline {deadline_secs}s)"),
        SelectedCompletion::Signalled(signal) => format!("killed by signal {signal}"),
        SelectedCompletion::Indeterminate => "indeterminate".to_owned(),
    }
}

fn micros_usd(micros: u64) -> String {
    format!("${:.6}", micros as f64 / 1_000_000.0)
}

fn cost_text(summary: Option<AttemptSummary>) -> String {
    match summary {
        None => "unknown (no ledger row)".to_owned(),
        Some(AttemptSummary {
            spent_micros: Some(spent),
            ..
        }) => format!("~{} (estimate)", micros_usd(spent)),
        Some(AttemptSummary {
            reserved_micros, ..
        }) => format!(
            "not reported (up to {} reserved)",
            micros_usd(reserved_micros)
        ),
    }
}

/// The human receipt. The log tail is remote output, so control characters are
/// made inert before it reaches the terminal.
fn render_receipt(
    env_name: &str,
    alias: &str,
    deadline_secs: u64,
    result: &SelectedJobResult,
    summary: Option<AttemptSummary>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let hash = result.source.hash();
    // Writing to a String cannot fail.
    writeln!(
        out,
        "job: {} on {alias} (env {env_name})",
        selected_job::recipe_name(result.recipe)
    )
    .expect("write to String");
    writeln!(out, "source: {}", &hash[..hash.len().min(12)]).expect("write to String");
    if let Some(summary) = summary {
        writeln!(out, "attempt: {}", summary.attempt).expect("write to String");
    }
    writeln!(
        out,
        "completion: {}",
        completion_text(result.completion, deadline_secs)
    )
    .expect("write to String");
    let duration = summary.and_then(|summary| summary.duration_ms).map_or_else(
        || "unknown".to_owned(),
        |ms| format!("{:.1}s", ms as f64 / 1000.0),
    );
    writeln!(out, "duration: {duration}").expect("write to String");
    writeln!(out, "cost: {}", cost_text(summary)).expect("write to String");
    if result.log_tail.is_empty() {
        write!(out, "log: (empty)").expect("write to String");
    } else {
        if result.truncated {
            writeln!(out, "log (earlier output was dropped; last part only):")
                .expect("write to String");
        } else {
            writeln!(out, "log:").expect("write to String");
        }
        write!(
            out,
            "{}",
            thegn_core::ci_log::inert_controls(result.log_tail.trim_end())
        )
        .expect("write to String");
    }
    out
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;
