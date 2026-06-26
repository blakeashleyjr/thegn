//! `superzej ci <action>` — cross-provider CI/CD inspection (AV group).
//!
//! The non-interactive surface over [`superzej_svc::ci`]: run history, job/step
//! drilldown, logs (with jump-to-failure), and the Phase-B mutations
//! (rerun/trigger/cancel). Mirrors `cmd::pr`, but the provider methods are async
//! (HTTP/CLI), so each verb spins a current-thread tokio runtime and blocks on a
//! single future — no concurrency, just a bridge from the sync clap dispatch.
//!
//! `runs` also warms the `ci_runs_cache` the native host paints from, exactly as
//! `pr status` warms `pr_cache`.

use anyhow::Result;
use superzej_core::ci::{self, CiJob, CiRun, CiState, RerunScope};
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::remote::GitLoc;
use superzej_core::{msg, outln};
use superzej_svc::ci::{CiClient, provider_for};

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Recent runs (newest first); `--branch` to filter, `--limit` to cap.
    Runs {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// One run's jobs and steps.
    View {
        run_id: String,
        #[arg(long)]
        worktree: Option<String>,
    },
    /// A job's log ("why did it fail") with a jump-to-failure marker.
    Log {
        /// The run id (needed by providers whose job ids aren't global).
        run_id: String,
        /// The job id.
        job_id: String,
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Re-run a run (`--failed` for only the failed jobs).
    Rerun {
        run_id: String,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        failed: bool,
    },
    /// Trigger a workflow with `-i key=value` inputs (workflow_dispatch).
    Trigger {
        workflow: String,
        #[arg(long)]
        worktree: Option<String>,
        #[arg(short = 'i', long = "input", value_name = "KEY=VALUE")]
        input: Vec<String>,
    },
    /// Cancel an in-flight run.
    Cancel {
        run_id: String,
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Show which CI systems the worktree is configured for + the active provider.
    Detect {
        #[arg(long)]
        worktree: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Runs {
            worktree,
            branch,
            limit,
        } => runs(cfg, worktree, branch, limit),
        Action::View { run_id, worktree } => view(cfg, worktree, &run_id),
        Action::Log {
            run_id,
            job_id,
            worktree,
        } => log(cfg, worktree, &run_id, &job_id),
        Action::Rerun {
            run_id,
            worktree,
            failed,
        } => rerun(cfg, worktree, &run_id, failed),
        Action::Trigger {
            workflow,
            worktree,
            input,
        } => trigger(cfg, worktree, &workflow, input),
        Action::Cancel { run_id, worktree } => cancel(cfg, worktree, &run_id),
        Action::Detect { worktree } => detect(cfg, worktree),
    }
}

/// Run a single provider future to completion on a throwaway current-thread
/// runtime (the verb is otherwise synchronous).
fn block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(fut)
}

/// Resolve the worktree + its CI provider, or print a readable note and return
/// `None` (CI disabled / undetected / provider not yet implemented).
fn client(cfg: &Config, worktree: Option<String>) -> Option<(GitLoc, CiClient)> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match provider_for(&loc, &cfg.ci) {
        Some(c) => Some((loc, c)),
        None => {
            outln!("no CI provider for this worktree (set [ci] provider, or check the remote)");
            None
        }
    }
}

fn glyph(s: CiState) -> &'static str {
    match s {
        CiState::Pass => "✓",
        CiState::Fail => "✗",
        CiState::Running => "●",
        CiState::Pending => "○",
        CiState::Cancelled => "⊘",
        CiState::Skipped => "–",
    }
}

fn dur(start: Option<&str>, finish: Option<&str>) -> String {
    match ci::duration_secs(start, finish, superzej_core::util::now()) {
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        Some(s) => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        None => "—".into(),
    }
}

fn runs(
    cfg: &Config,
    worktree: Option<String>,
    branch: Option<String>,
    limit: Option<usize>,
) -> Result<()> {
    let Some((loc, client)) = client(cfg, worktree) else {
        return Ok(());
    };
    let limit = limit.unwrap_or(cfg.ci.max_runs);
    let branch_q = branch.as_deref();
    match block(client.runs(&loc, branch_q, limit)) {
        Ok(runs) => {
            if runs.is_empty() {
                outln!("no CI runs found");
                return Ok(());
            }
            // Warm the cache the native panel reads.
            if let Ok(db) = Db::open() {
                let json = serde_json::to_string(&runs).unwrap_or_default();
                let _ = db.put_ci_cache(&loc.path(), branch_q.unwrap_or(""), &json);
            }
            for r in &runs {
                outln!(
                    "{} {:<22} {:<10} {:<8} {:>7}  {}  {}",
                    glyph(r.state),
                    truncate(&r.name, 22),
                    truncate(&r.branch, 10),
                    truncate(&r.event, 8),
                    dur(r.started_at.as_deref(), r.finished_at.as_deref()),
                    r.id,
                    truncate(&r.title, 40),
                );
            }
        }
        // Read verbs degrade gracefully (exit 0 with a note) — the "never
        // crashes, always a readable state" contract the panel relies on.
        Err(e) => outln!("ci: {e}"),
    }
    Ok(())
}

fn view(cfg: &Config, worktree: Option<String>, run_id: &str) -> Result<()> {
    let Some((loc, client)) = client(cfg, worktree) else {
        return Ok(());
    };
    match block(client.run_detail(&loc, run_id)) {
        Ok(run) => print_run_detail(&run),
        Err(e) => outln!("ci: {e}"),
    }
    Ok(())
}

fn print_run_detail(run: &CiRun) {
    outln!(
        "{} {} #{}  [{}]  {}",
        glyph(run.state),
        run.name,
        run.run_number.unwrap_or(0),
        run.status_raw,
        dur(run.started_at.as_deref(), run.finished_at.as_deref())
    );
    if !run.title.is_empty() {
        outln!("  {}", run.title);
    }
    if !run.url.is_empty() {
        outln!("  {}", run.url);
    }
    for j in &run.jobs {
        print_job(j);
    }
}

fn print_job(j: &CiJob) {
    outln!(
        "  {} {:<24} {:>7}  {}",
        glyph(j.state),
        truncate(&j.name, 24),
        dur(j.started_at.as_deref(), j.finished_at.as_deref()),
        j.id,
    );
    for s in &j.steps {
        outln!("      {} {}", glyph(s.state), s.name);
    }
}

fn log(cfg: &Config, worktree: Option<String>, run_id: &str, job_id: &str) -> Result<()> {
    let Some((loc, client)) = client(cfg, worktree) else {
        return Ok(());
    };
    match block(client.logs(&loc, run_id, job_id)) {
        Ok(mut log) => {
            // Apply the configured tail cap.
            let cap = cfg.ci.log_tail_lines;
            let lines: Vec<&str> = log.text.lines().collect();
            if cap > 0 && lines.len() > cap {
                log.text = lines[lines.len() - cap..].join("\n");
                log.truncated = true;
            }
            if log.truncated {
                outln!("… (showing last {} lines)", cfg.ci.log_tail_lines);
            }
            if let Some(n) = log.first_failure_line() {
                outln!(">> first failure at line {}", n + 1);
            }
            outln!("{}", log.text);
        }
        Err(e) => outln!("ci: {e}"),
    }
    Ok(())
}

fn rerun(cfg: &Config, worktree: Option<String>, run_id: &str, failed: bool) -> Result<()> {
    let Some((loc, client)) = client(cfg, worktree) else {
        return Ok(());
    };
    if !client.caps().rerun {
        msg::die("this provider can't re-run runs");
    }
    let scope = if failed {
        RerunScope::Failed
    } else {
        RerunScope::All
    };
    match block(client.rerun(&loc, run_id, scope)) {
        Ok(()) => msg::info(if failed {
            "re-running failed jobs"
        } else {
            "re-running"
        }),
        Err(e) => msg::die(&format!("ci rerun failed: {e}")),
    }
    Ok(())
}

fn trigger(
    cfg: &Config,
    worktree: Option<String>,
    workflow: &str,
    input: Vec<String>,
) -> Result<()> {
    let Some((loc, client)) = client(cfg, worktree) else {
        return Ok(());
    };
    if !client.caps().trigger {
        msg::die("this provider can't trigger workflows");
    }
    let inputs: Vec<(String, String)> = input
        .iter()
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    match block(client.trigger(&loc, workflow, &inputs)) {
        Ok(()) => msg::info(&format!("triggered {workflow}")),
        Err(e) => msg::die(&format!("ci trigger failed: {e}")),
    }
    Ok(())
}

fn cancel(cfg: &Config, worktree: Option<String>, run_id: &str) -> Result<()> {
    let Some((loc, client)) = client(cfg, worktree) else {
        return Ok(());
    };
    if !client.caps().cancel {
        msg::die("this provider can't cancel runs");
    }
    match block(client.cancel(&loc, run_id)) {
        Ok(()) => msg::info("cancelled"),
        Err(e) => msg::die(&format!("ci cancel failed: {e}")),
    }
    Ok(())
}

fn detect(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let detected = ci::detect_ci_configs(std::path::Path::new(&loc.path()));
    if detected.is_empty() {
        outln!("no CI config files detected in this worktree");
    } else {
        outln!("detected CI configs:");
        for c in &detected {
            outln!("  {:<16} {}", c.system.label(), c.files.join(", "));
        }
    }
    match superzej_svc::ci::resolve_system(&loc, &cfg.ci) {
        Some(sys) => outln!("active provider: {}", sys.label()),
        None => outln!("active provider: none"),
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
