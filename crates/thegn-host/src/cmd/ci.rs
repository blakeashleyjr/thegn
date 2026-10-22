//! `thegn ci <action>` — cross-provider CI/CD inspection (AV group).
//!
//! The non-interactive surface over [`thegn_svc::ci`]: run history, job/step
//! drilldown, logs (with jump-to-failure), and the Phase-B mutations
//! (rerun/trigger/cancel). Mirrors `cmd::pr`, but the provider methods are async
//! (HTTP/CLI), so each verb spins a current-thread tokio runtime and blocks on a
//! single future — no concurrency, just a bridge from the sync clap dispatch.
//!
//! `runs --branch <current branch>` also warms the `ci_runs_cache` the native
//! host paints from; other branches and the all-branch listing never overwrite it.

use anyhow::{Result, bail};
use thegn_core::ci::{self, CiJob, CiRun, CiState, RerunScope};
use thegn_core::config::Config;
use thegn_core::config_ci::MAX_CI_RUNS;
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
    /// Re-run a run: every job by default, `--failed` for only the failed jobs, `--failed-or-canceled` for a retry of failed and canceled jobs (GitLab).
    Rerun {
        run_id: String,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        failed: bool,
        #[arg(long, conflicts_with = "failed")]
        failed_or_canceled: bool,
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
            failed_or_canceled,
        } => rerun(cfg, target.worktree, &run_id, failed, failed_or_canceled),
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

fn validate_id_at_cli(value: &str) -> Result<()> {
    thegn_svc::ci::validate_ci_id(value)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.to_string()))
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

fn shared_cache_branch<'a>(requested: Option<&'a str>, current: Option<&str>) -> Option<&'a str> {
    requested.filter(|branch| current == Some(*branch))
}

fn runs(
    cfg: &Config,
    worktree: Option<String>,
    branch: Option<String>,
    limit: Option<usize>,
    json_out: bool,
) -> Result<()> {
    if let Some(branch) = branch.as_deref() {
        thegn_svc::ci::validate_branch_ref(branch)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    if let Some(limit) = limit
        && (limit == 0 || limit > MAX_CI_RUNS)
    {
        bail!("--limit must be between 1 and {MAX_CI_RUNS}");
    }
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
    let limit = limit.unwrap_or(cfg.ci.max_runs.min(MAX_CI_RUNS));
    let branch_q = branch.as_deref();
    let current_branch = if branch_q.is_some() {
        crate::worktree_snapshot::current_branch(&wt)
    } else {
        None
    };
    let cache_branch = shared_cache_branch(branch_q, current_branch.as_deref());
    match client.runs(&loc, branch_q, limit) {
        Ok(result) => {
            let runs = result.runs;
            if result.discarded_rows > 0 {
                msg::warn(&format!(
                    "CI provider omitted {} malformed run row(s)",
                    result.discarded_rows
                ));
            }
            if json_out {
                if result.discarded_rows > 0 {
                    return super::emit_json(&serde_json::json!({
                        "error": "incomplete CI run list",
                        "discarded_rows": result.discarded_rows
                    }));
                }
                // Still warm the cache the native panel reads — same fetch.
                if let Some(cache_branch) = cache_branch
                    && let Ok(db) = Db::open()
                {
                    let json = serde_json::to_string(&thegn_core::ci::CiRunCache {
                        runs: runs.clone(),
                        discarded_rows: result.discarded_rows,
                    })
                    .unwrap_or_default();
                    let _ = db.put_ci_cache(&cache_key, cache_branch, &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                }
                return super::emit_json(&runs);
            }
            if runs.is_empty() {
                outln!("no CI runs found");
                return Ok(());
            }
            // Warm the cache the native panel reads.
            if result.discarded_rows == 0
                && let Some(cache_branch) = cache_branch
                && let Ok(db) = Db::open()
            {
                let json = serde_json::to_string(&thegn_core::ci::CiRunCache {
                    runs: runs.clone(),
                    discarded_rows: result.discarded_rows,
                })
                .unwrap_or_default();
                let _ = db.put_ci_cache(&cache_key, cache_branch, &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }
            for r in &runs {
                outln!(
                    "{} {:<22} {:<10} {:<8} {:>7}  {}  {}",
                    glyph(r.state),
                    truncate(&thegn_svc::lsp::sanitize_for_terminal(&r.name), 22),
                    truncate(&thegn_svc::lsp::sanitize_for_terminal(&r.branch), 10),
                    truncate(&thegn_svc::lsp::sanitize_for_terminal(&r.event), 8),
                    dur(r.started_at.as_deref(), r.finished_at.as_deref()),
                    thegn_svc::lsp::sanitize_for_terminal(&r.id),
                    truncate(&thegn_svc::lsp::sanitize_for_terminal(&r.title), 40),
                );
            }
        }
        // A fetch failure degrades gracefully — the "never crashes, always a
        // readable state" contract. Under --json, stdout must stay valid JSON,
        // so emit an empty array and send the reason to stderr (exit 0); human
        // mode prints a note. The finding this fixes was a NON-JSON string on
        // stdout under --json, not a missing non-zero exit.
        Err(e) if json_out => {
            msg::warn(&format!(
                "ci: {}",
                thegn_svc::lsp::sanitize_for_terminal(&e.to_string())
            ));
            return super::emit_json(&serde_json::json!({ "error": e.to_string() }));
        }
        Err(e) => outln!(
            "ci: {}",
            thegn_svc::lsp::sanitize_for_terminal(&e.to_string())
        ),
    }
    Ok(())
}

fn detail_warnings(detail: &thegn_svc::ci::CiRunDetail) -> Vec<String> {
    let mut warnings = Vec::new();
    if detail.discarded_jobs > 0 {
        warnings.push(format!(
            "CI provider omitted {} malformed job row(s)",
            detail.discarded_jobs
        ));
    }
    if let Some(incomplete) = detail.incomplete {
        warnings.push(format!(
            "CI job list is incomplete: {}; showing the first {} job(s)",
            incomplete.reason(),
            detail.run.jobs.len()
        ));
    }
    warnings
}

fn view(cfg: &Config, worktree: Option<String>, run_id: &str) -> Result<()> {
    validate_id_at_cli(run_id)?;
    let (loc, client) = client(cfg, worktree)?;
    match client.run_detail(&loc, run_id) {
        Ok(detail) => {
            for warning in detail_warnings(&detail) {
                msg::warn(&warning);
            }
            print_run_detail(&detail.run);
        }
        Err(e) => outln!(
            "ci: {}",
            thegn_svc::lsp::sanitize_for_terminal(&e.to_string())
        ),
    }
    Ok(())
}

fn print_run_detail(run: &CiRun) {
    outln!(
        "{} {} #{}  [{}]  {}",
        glyph(run.state),
        thegn_svc::lsp::sanitize_for_terminal(&run.name),
        run.run_number.unwrap_or(0),
        thegn_svc::lsp::sanitize_for_terminal(&run.status_raw),
        dur(run.started_at.as_deref(), run.finished_at.as_deref())
    );
    if !run.title.is_empty() {
        outln!("  {}", thegn_svc::lsp::sanitize_for_terminal(&run.title));
    }
    if !run.url.is_empty() {
        outln!("  {}", thegn_svc::lsp::sanitize_for_terminal(&run.url));
    }
    for j in &run.jobs {
        print_job(j);
    }
}

fn print_job(j: &CiJob) {
    outln!(
        "  {} {:<24} {:>7}  {}",
        glyph(j.state),
        truncate(&thegn_svc::lsp::sanitize_for_terminal(&j.name), 24),
        dur(j.started_at.as_deref(), j.finished_at.as_deref()),
        thegn_svc::lsp::sanitize_for_terminal(&j.id),
    );
    for s in &j.steps {
        outln!(
            "      {} {}",
            glyph(s.state),
            thegn_svc::lsp::sanitize_for_terminal(&s.name)
        );
    }
}

fn log(
    cfg: &Config,
    worktree: Option<String>,
    run_id: &str,
    job_id: &str,
    json_out: bool,
) -> Result<()> {
    validate_id_at_cli(run_id)?;
    validate_id_at_cli(job_id)?;
    let wt = resolve_worktree(worktree);
    let key = GitLoc::worktree_cache_key(&wt);
    let cached = (cfg.ci.log_cache_runs > 0)
        .then(|| {
            Db::open()
                .ok()
                .and_then(|db| db.get_ci_log(&key, run_id, job_id).ok().flatten())
        })
        .flatten()
        .filter(|entry| {
            entry.run_id == run_id
                && entry.job_id == job_id
                && thegn_svc::ci::validate_ci_id(&entry.run_id).is_ok()
                && thegn_svc::ci::validate_ci_id(&entry.job_id).is_ok()
                && thegn_core::ci_log::cacheable_log_entry(entry)
        });
    let entry = if let Some(mut entry) = cached {
        // Cache rows have already been redacted and bounded. Re-apply the
        // configured policy so a later config change cannot widen a response.
        let redacted = thegn_core::ci_log::redact(&entry.text);
        let (text, truncated) = thegn_core::ci_log::bounded_tail(
            &redacted,
            cfg.ci
                .log_tail_lines
                .min(thegn_core::ci_log::HARD_MAX_LOG_LINES),
            cfg.ci
                .log_max_bytes
                .min(thegn_core::ci_log::HARD_MAX_LOG_BYTES),
        );
        entry.text = text;
        entry.redacted = true;
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
                    msg::warn(&format!(
                        "ci: {}",
                        thegn_svc::lsp::sanitize_for_terminal(&error.to_string())
                    ));
                    return super::emit_json(&serde_json::json!({ "error": error.to_string() }));
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(thegn_svc::lsp::sanitize_for_terminal(
                        &error.to_string()
                    )));
                }
            };
            let detail = provider.run_detail(&loc, run_id).ok();
            if let Some(detail) = &detail {
                for warning in detail_warnings(detail) {
                    msg::warn(&warning);
                }
            }
            let (job_name, head_sha) = detail
                .as_ref()
                .and_then(|detail| {
                    detail
                        .run
                        .jobs
                        .iter()
                        .find(|job| job.id == job_id)
                        .map(|job| (job.name.clone(), detail.run.sha.clone()))
                })
                .unwrap_or_default();
            let raw = match provider.logs(&loc, run_id, job_id) {
                Ok(raw) => raw,
                Err(error) if json_out => {
                    msg::warn(&format!(
                        "ci: {}",
                        thegn_svc::lsp::sanitize_for_terminal(&error.to_string())
                    ));
                    return super::emit_json(&serde_json::json!({ "error": error.to_string() }));
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "ci: {}",
                        thegn_svc::lsp::sanitize_for_terminal(&error.to_string())
                    ));
                }
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
                && thegn_core::ci_log::cacheable_log_entry(&entry)
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
    outln!(
        "{}",
        render_log_output(&entry.text, entry.truncated, cfg.ci.log_tail_lines)
    );
    Ok(())
}

fn rerun_scope(
    caps: &ci::CiCaps,
    failed: bool,
    failed_or_canceled: bool,
) -> Result<RerunScope, &'static str> {
    let scope = if failed_or_canceled {
        RerunScope::FailedOrCanceled
    } else if failed {
        RerunScope::Failed
    } else {
        RerunScope::All
    };
    if caps.supports(scope) {
        return Ok(scope);
    }
    Err(match scope {
        RerunScope::All if caps.supports(RerunScope::FailedOrCanceled) => {
            "this provider can't re-run every job — its retry re-runs failed and canceled jobs: use --failed-or-canceled"
        }
        RerunScope::Failed if caps.supports(RerunScope::FailedOrCanceled) => {
            "this provider can't re-run only failed jobs — its retry includes canceled jobs: use --failed-or-canceled"
        }
        RerunScope::FailedOrCanceled => {
            "this provider can't retry failed and canceled jobs together — use --failed or omit the flag for a full rerun"
        }
        _ => "this provider can't re-run runs",
    })
}

fn rerun(
    cfg: &Config,
    worktree: Option<String>,
    run_id: &str,
    failed: bool,
    failed_or_canceled: bool,
) -> Result<()> {
    validate_id_at_cli(run_id)?;
    let (loc, client) = client(cfg, worktree)?;
    let scope = match rerun_scope(&client.caps(), failed, failed_or_canceled) {
        Ok(scope) => scope,
        Err(message) => msg::die(message),
    };
    match client.rerun(&loc, run_id, scope) {
        Ok(()) => msg::info(match scope {
            RerunScope::All => "re-running",
            RerunScope::Failed => "re-running failed jobs",
            RerunScope::FailedOrCanceled => "retrying failed and canceled jobs",
        }),
        Err(e) => msg::die(&format!(
            "ci rerun failed: {}",
            thegn_svc::lsp::sanitize_for_terminal(&e.to_string())
        )),
    }
    Ok(())
}

fn trigger(
    cfg: &Config,
    worktree: Option<String>,
    workflow: &str,
    input: Vec<String>,
) -> Result<()> {
    thegn_svc::ci::validate_workflow_selector(workflow)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let inputs = thegn_svc::ci::parse_trigger_inputs(&input)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let (loc, client) = client(cfg, worktree)?;
    if !client.caps().trigger {
        msg::die("this provider can't trigger workflows");
    }
    match client.trigger(&loc, workflow, &inputs) {
        Ok(()) => msg::info(&format!("triggered {workflow}")),
        Err(e) => msg::die(&format!(
            "ci trigger failed: {}",
            thegn_svc::lsp::sanitize_for_terminal(&e.to_string())
        )),
    }
    Ok(())
}

fn cancel(cfg: &Config, worktree: Option<String>, run_id: &str) -> Result<()> {
    validate_id_at_cli(run_id)?;
    let (loc, client) = client(cfg, worktree)?;
    if !client.caps().cancel {
        msg::die("this provider can't cancel runs");
    }
    match client.cancel(&loc, run_id) {
        Ok(()) => msg::info("cancelled"),
        Err(e) => msg::die(&format!(
            "ci cancel failed: {}",
            thegn_svc::lsp::sanitize_for_terminal(&e.to_string())
        )),
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
        for line in detect_lines(&detected) {
            outln!("{line}");
        }
    }
    match thegn_svc::ci::resolve_system(&loc, &cfg.ci) {
        Some(sys) => outln!("active provider: {}", sys.label()),
        None => outln!("active provider: none"),
    }
    Ok(())
}

fn detect_lines(detected: &[ci::CiConfig]) -> Vec<String> {
    detected
        .iter()
        .map(|c| {
            thegn_svc::lsp::sanitize_for_terminal(&format!(
                "  {:<16} {}",
                c.system.label(),
                c.files.join(", ")
            ))
        })
        .collect()
}

fn render_log_output(text: &str, truncated: bool, tail_lines: usize) -> String {
    let sanitized = thegn_svc::lsp::sanitize_for_terminal(text);
    let mut output = String::new();
    if truncated {
        output.push_str(&format!("… (showing last {tail_lines} lines)\n"));
    }
    let log = thegn_core::ci::CiLog {
        text: text.to_string(),
        truncated,
    };
    if let Some(line) = log.first_failure_line() {
        output.push_str(&format!(">> first failure at line {}\n", line + 1));
    }
    output.push_str(&sanitized);
    output
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_warms_the_shared_cache_only_for_the_current_branch() {
        assert_eq!(shared_cache_branch(Some("b"), Some("a")), None);
        assert_eq!(shared_cache_branch(Some("a"), Some("a")), Some("a"));
        assert_eq!(shared_cache_branch(None, Some("a")), None);
        assert_eq!(shared_cache_branch(Some("a"), None), None);
    }

    #[test]
    fn detected_repository_names_are_inert_in_human_output() {
        let detected = [ci::CiConfig {
            system: thegn_core::ci::CiSystem::GithubActions,
            files: vec![
                ".github/workflows/\u{1b}]52;c;SGVsbG8=\u{7}".into(),
                "\u{1b}[2Jworkflow.yml\u{009d}title".into(),
            ],
        }];
        let output = detect_lines(&detected).join("\n");
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{009d}'));
        assert!(output.contains("workflow.yml"));
    }

    #[test]
    fn human_log_output_bytes_make_terminal_controls_inert() {
        let hostile = concat!(
            "ordinary failure\n",
            "\u{1b}]52;c;SGVsbG8=\u{7}\n",
            "\u{1b}[2J\n",
            "\u{1b}_APC payload\u{1b}\\\n",
            "\u{1b}P DCS payload\u{1b}\\\n",
            "\u{009d}C1 OSC\u{009c}\n",
            "split control starts\u{1b}\n[2J remains text\n",
        );
        let bytes = render_log_output(hostile, true, 20).into_bytes();
        assert!(!bytes.contains(&0x1b));
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(
            !rendered
                .chars()
                .any(|c| ('\u{0080}'..='\u{009f}').contains(&c))
        );
        assert!(rendered.contains("ordinary failure\n"));
        assert!(rendered.contains("split control starts\n[2J remains text"));
    }

    #[test]
    fn cli_rejects_bad_ci_targets_before_cache_or_provider_access() {
        let config = Config::default();
        let log_result = log(
            &config,
            Some("/path/that/need-not-exist".into()),
            "--help",
            "1",
            true,
        );
        assert!(log_result.unwrap_err().to_string().contains("--help"));
        let bad_job = log(
            &config,
            Some("/path/that/need-not-exist".into()),
            "1",
            "1e5",
            true,
        );
        assert!(bad_job.unwrap_err().to_string().contains("1e5"));

        let bad_ref = runs(
            &config,
            Some("/path/that/need-not-exist".into()),
            Some("bad ref".into()),
            None,
            true,
        );
        assert!(bad_ref.unwrap_err().to_string().contains("bad ref"));

        let rerun_result = rerun(&config, None, "01", false, false);
        assert!(rerun_result.unwrap_err().to_string().contains("01"));

        let cancel_result = cancel(&config, None, "--help");
        assert!(cancel_result.unwrap_err().to_string().contains("--help"));

        let trigger_result = trigger(&config, None, "--help", Vec::new());
        assert!(trigger_result.unwrap_err().to_string().contains("--help"));
    }

    #[test]
    fn detail_warnings_name_an_incomplete_job_list() {
        let incomplete = thegn_svc::ci::CiRunDetail {
            run: CiRun {
                jobs: vec![CiJob::default(), CiJob::default()],
                ..CiRun::default()
            },
            discarded_jobs: 0,
            incomplete: Some(thegn_svc::ci::CiDetailIncomplete::PageBudget { pages: 50 }),
        };
        let warnings = detail_warnings(&incomplete);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("incomplete"));
        assert!(warnings[0].contains("2 job(s)"));

        let malformed = thegn_svc::ci::CiRunDetail {
            run: CiRun::default(),
            discarded_jobs: 1,
            incomplete: None,
        };
        let warnings = detail_warnings(&malformed);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("malformed job row"));

        let clean = thegn_svc::ci::CiRunDetail {
            run: CiRun::default(),
            discarded_jobs: 0,
            incomplete: None,
        };
        assert!(detail_warnings(&clean).is_empty());
    }

    #[test]
    fn rerun_scope_cli_refuses_unsupported_scopes_with_guidance() {
        let gitlab = ci::CiCaps {
            rerun: true,
            rerun_failed_or_canceled: true,
            ..ci::CiCaps::default()
        };
        let refused_all = rerun_scope(&gitlab, false, false);
        assert!(matches!(refused_all, Err(message) if message.contains("--failed-or-canceled")));
        assert_eq!(
            rerun_scope(&gitlab, false, true),
            Ok(RerunScope::FailedOrCanceled)
        );

        let github = ci::CiCaps {
            rerun: true,
            rerun_all: true,
            rerun_failed: true,
            ..ci::CiCaps::default()
        };
        assert_eq!(rerun_scope(&github, false, false), Ok(RerunScope::All));
        assert_eq!(rerun_scope(&github, true, false), Ok(RerunScope::Failed));
    }

    #[test]
    fn trigger_inputs_cli_refuses_malformed_entries_before_provider_access() {
        let result = trigger(
            &Config::default(),
            Some("/path/that/need-not-exist".into()),
            "ci.yml",
            vec!["novalue".into()],
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid workflow input")
        );
    }
}
