//! `thegn ci <action>` — cross-provider CI/CD inspection (AV group).
//!
//! The non-interactive surface over [`thegn_svc::ci`]: run history, job/step
//! drilldown, logs (with jump-to-failure), and the Phase-B mutations
//! (rerun/trigger/cancel). Mirrors `cmd::pr`, but the provider methods are async
//! (HTTP/CLI), so each verb spins a current-thread tokio runtime and blocks on a
//! single future — no concurrency, just a bridge from the sync clap dispatch.
//!
//! `runs` also warms the `ci_runs_cache` the native host paints from, exactly as
//! `pr status` warms `pr_cache`.

use anyhow::{Result, bail};
use thegn_core::ci::{self, CiJob, CiRun, CiState, RerunScope};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::store::CacheStore;
use thegn_core::{msg, outln};
use thegn_svc::ci::{CiClient, provider_for};

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Recent runs (newest first); `--branch` to filter, `--limit` to cap.
    Runs {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Emit one JSON array instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// One run's jobs and steps.
    View {
        run_id: String,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// A job's cached log ("why did it fail") with a jump-to-failure marker.
    #[command(alias = "log")]
    Logs {
        /// The run id (needed by providers whose job ids aren't global).
        run_id: String,
        /// The job id.
        job_id: String,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Emit one JSON object instead of human-readable log text.
        #[arg(long)]
        json: bool,
    },
    /// Re-run a run (`--failed` for only the failed jobs).
    Rerun {
        run_id: String,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        failed: bool,
    },
    /// Trigger a workflow with `-i key=value` inputs (workflow_dispatch).
    Trigger {
        workflow: String,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(short = 'i', long = "input", value_name = "KEY=VALUE")]
        input: Vec<String>,
    },
    /// Cancel an in-flight run.
    Cancel {
        run_id: String,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Show which CI systems the worktree is configured for + the active provider.
    Detect {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Runs {
            target,
            branch,
            limit,
            json,
        } => runs(cfg, target.worktree, branch, limit, json),
        Action::View { run_id, target } => view(cfg, target.worktree, &run_id),
        Action::Logs {
            run_id,
            job_id,
            target,
            json,
        } => log(cfg, target.worktree, &run_id, &job_id, json),
        Action::Rerun {
            run_id,
            target,
            failed,
        } => rerun(cfg, target.worktree, &run_id, failed),
        Action::Trigger {
            workflow,
            target,
            input,
        } => trigger(cfg, target.worktree, &workflow, input),
        Action::Cancel { run_id, target } => cancel(cfg, target.worktree, &run_id),
        Action::Detect { target } => detect(cfg, target.worktree),
    }
}

/// Run a single provider future to completion on a throwaway current-thread
/// runtime (the verb is otherwise synchronous).
/// Resolve the worktree + its CI provider. An unresolved provider is a hard
/// error (returned `Err`), not a silent no-op: main() propagates it so the
/// process exits non-zero — mutation verbs (trigger/rerun/cancel) must never
/// report success to a script/CI when nothing was actually done.
fn client(cfg: &Config, worktree: Option<String>) -> Result<(GitLoc, CiClient)> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match provider_for(&loc, &cfg.ci) {
        Some(c) => Ok((loc, c)),
        None => bail!("no CI provider for this worktree (set [ci] provider, or check the remote)"),
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
    match ci::duration_secs(start, finish, thegn_core::util::now()) {
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
    json_out: bool,
) -> Result<()> {
    // `runs` is a READ verb: unlike the mutation verbs, listing when no CI
    // provider resolves is graceful degradation, not an error — emit an empty
    // JSON array (valid for scripts) / a friendly note and exit 0.
    let wt = resolve_worktree(worktree);
    // Host-path cache key — matches the panel's reader (never `loc.path()`).
    let cache_key = GitLoc::worktree_cache_key(&wt);
    let loc = GitLoc::for_worktree(&wt);
    let Some(client) = provider_for(&loc, &cfg.ci) else {
        if json_out {
            return super::emit_json(&Vec::<CiRun>::new());
        }
        outln!("no CI provider for this worktree (set [ci] provider, or check the remote)");
        return Ok(());
    };
    let limit = limit.unwrap_or(cfg.ci.max_runs);
    let branch_q = branch.as_deref();
    match client.runs(&loc, branch_q, limit) {
        Ok(runs) => {
            if json_out {
                // Still warm the cache the native panel reads — same fetch.
                if let Ok(db) = Db::open() {
                    let json = serde_json::to_string(&runs).unwrap_or_default();
                    let _ = db.put_ci_cache(&cache_key, branch_q.unwrap_or(""), &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                }
                return super::emit_json(&runs);
            }
            if runs.is_empty() {
                outln!("no CI runs found");
                return Ok(());
            }
            // Warm the cache the native panel reads.
            if let Ok(db) = Db::open() {
                let json = serde_json::to_string(&runs).unwrap_or_default();
                let _ = db.put_ci_cache(&cache_key, branch_q.unwrap_or(""), &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
        // A fetch failure degrades gracefully — the "never crashes, always a
        // readable state" contract. Under --json, stdout must stay valid JSON,
        // so emit an empty array and send the reason to stderr (exit 0); human
        // mode prints a note. The finding this fixes was a NON-JSON string on
        // stdout under --json, not a missing non-zero exit.
        Err(e) if json_out => {
            msg::warn(&format!("ci: {e}"));
            return super::emit_json(&Vec::<CiRun>::new());
        }
        Err(e) => outln!("ci: {e}"),
    }
    Ok(())
}

fn view(cfg: &Config, worktree: Option<String>, run_id: &str) -> Result<()> {
    let (loc, client) = client(cfg, worktree)?;
    match client.run_detail(&loc, run_id) {
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

fn log(
    cfg: &Config,
    worktree: Option<String>,
    run_id: &str,
    job_id: &str,
    json_out: bool,
) -> Result<()> {
    let wt = resolve_worktree(worktree);
    let key = GitLoc::worktree_cache_key(&wt);
    let cached = (cfg.ci.log_cache_runs > 0)
        .then(|| {
            Db::open()
                .ok()
                .and_then(|db| db.get_ci_log(&key, run_id, job_id).ok().flatten())
        })
        .flatten();
    let entry = if let Some(mut entry) = cached {
        // Cache rows have already been redacted and bounded. Re-apply the
        // configured policy so a later config change cannot widen a response.
        let (text, truncated) = thegn_core::ci_log::bounded_tail(
            &entry.text,
            cfg.ci
                .log_tail_lines
                .min(thegn_core::ci_log::HARD_MAX_LOG_LINES),
            cfg.ci
                .log_max_bytes
                .min(thegn_core::ci_log::HARD_MAX_LOG_BYTES),
        );
        entry.text = text;
        entry.truncated |= truncated;
        Some((entry, "cache"))
    } else {
        None
    };
    let entry = match entry {
        Some(entry) => entry,
        None => {
            let (loc, provider) = match client(cfg, Some(wt.to_string_lossy().into_owned())) {
                Ok(value) => value,
                Err(error) if json_out => {
                    msg::warn(&format!("ci: {error}"));
                    return super::emit_json(&serde_json::json!({ "error": error.to_string() }));
                }
                Err(error) => return Err(error),
            };
            let detail = provider.run_detail(&loc, run_id).ok();
            let (job_name, head_sha) = detail
                .as_ref()
                .and_then(|run| {
                    run.jobs
                        .iter()
                        .find(|job| job.id == job_id)
                        .map(|job| (job.name.clone(), run.sha.clone()))
                })
                .unwrap_or_default();
            let raw = match provider.logs(&loc, run_id, job_id) {
                Ok(raw) => raw,
                Err(error) if json_out => {
                    msg::warn(&format!("ci: {error}"));
                    return super::emit_json(&serde_json::json!({ "error": error.to_string() }));
                }
                Err(error) => return Err(anyhow::anyhow!("ci: {error}")),
            };
            let mut entry = thegn_core::ci_log::CiLogEntry::new(
                key.clone(),
                run_id,
                job_id,
                job_name,
                &raw.text,
                cfg.ci
                    .log_tail_lines
                    .min(thegn_core::ci_log::HARD_MAX_LOG_LINES),
                cfg.ci
                    .log_max_bytes
                    .min(thegn_core::ci_log::HARD_MAX_LOG_BYTES),
                thegn_core::util::now(),
            );
            entry.truncated |= raw.truncated;
            entry.head_sha = head_sha;
            if cfg.ci.log_cache_runs > 0
                && let Ok(db) = Db::open()
            {
                let _ = db.put_ci_log(&entry);
            }
            (entry, "provider")
        }
    };
    let (entry, source) = entry;
    if json_out {
        return super::emit_json(&serde_json::json!({
            "worktree": entry.worktree,
            "run_id": entry.run_id,
            "job_id": entry.job_id,
            "logs": [entry.clone()],
            "source": source,
            "fetched_at": entry.fetched_at,
            "truncated": entry.truncated,
            "redacted": entry.redacted,
        }));
    }
    if entry.truncated {
        outln!("… (showing last {} lines)", cfg.ci.log_tail_lines);
    }
    let log = thegn_core::ci::CiLog {
        text: entry.text.clone(),
        truncated: entry.truncated,
    };
    if let Some(n) = log.first_failure_line() {
        outln!(">> first failure at line {}", n + 1);
    }
    outln!("{}", entry.text);
    Ok(())
}

fn rerun(cfg: &Config, worktree: Option<String>, run_id: &str, failed: bool) -> Result<()> {
    let (loc, client) = client(cfg, worktree)?;
    if !client.caps().rerun {
        msg::die("this provider can't re-run runs");
    }
    if failed && !client.caps().rerun_failed {
        // Don't silently retry everything when the user asked for failed-only.
        msg::die("this provider can't re-run only failed jobs — drop --failed to retry all");
    }
    let scope = if failed {
        RerunScope::Failed
    } else {
        RerunScope::All
    };
    match client.rerun(&loc, run_id, scope) {
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
    let (loc, client) = client(cfg, worktree)?;
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
    match client.trigger(&loc, workflow, &inputs) {
        Ok(()) => msg::info(&format!("triggered {workflow}")),
        Err(e) => msg::die(&format!("ci trigger failed: {e}")),
    }
    Ok(())
}

fn cancel(cfg: &Config, worktree: Option<String>, run_id: &str) -> Result<()> {
    let (loc, client) = client(cfg, worktree)?;
    if !client.caps().cancel {
        msg::die("this provider can't cancel runs");
    }
    match client.cancel(&loc, run_id) {
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
    match thegn_svc::ci::resolve_system(&loc, &cfg.ci) {
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
