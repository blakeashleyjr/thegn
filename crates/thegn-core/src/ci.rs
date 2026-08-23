//! Provider-agnostic CI/CD model — the substrate for the cross-provider CI
//! inspection layer (AV group). Mirrors how [`crate::forge::model`] keeps the
//! pure `CheckRun`/`Bucket`/`summarize` data + classifiers in core while the
//! native transport lives in `thegn-svc`: this module holds the normalized
//! run→job→step→log model, the provider-vocabulary→[`CiState`] mappers, repo
//! CI-config detection, and the log failure-scanner — all pure and testable.
//! The async `CiProvider` trait (and the per-provider GitHub/GitLab/… impls)
//! live in `thegn-svc`, which has the tokio/HTTP deps this crate forbids.

use serde::{Deserialize, Serialize};
use std::path::Path;

// --- normalized lifecycle state -------------------------------------------

/// A run/job/step's lifecycle state, normalized across providers. Each provider
/// has its own vocabulary (GitHub: `status` + `conclusion`; GitLab: a single
/// `status`); the `from_*` constructors fold those onto this common axis so the
/// UI renders one set of glyphs/colours regardless of source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    /// Queued / waiting / created — not started yet.
    #[default]
    Pending,
    /// Actively executing.
    Running,
    /// Finished successfully.
    Pass,
    /// Finished unsuccessfully (failure / error / timed-out).
    Fail,
    /// Aborted by a user or the system.
    Cancelled,
    /// Did not run (skipped / neutral / manual-not-triggered).
    Skipped,
}

impl CiState {
    /// Has the unit reached a terminal state (no further transitions expected)?
    pub fn is_terminal(self) -> bool {
        !matches!(self, CiState::Pending | CiState::Running)
    }

    /// Does this state count as a failure for rollups / "needs attention"?
    pub fn is_failure(self) -> bool {
        matches!(self, CiState::Fail)
    }

    /// Map a GitHub Actions `(status, conclusion)` pair. While in-flight the
    /// conclusion is absent; once `completed` the conclusion decides the outcome.
    pub fn from_github(status: &str, conclusion: Option<&str>) -> CiState {
        match status.to_ascii_lowercase().as_str() {
            "queued" | "waiting" | "requested" | "pending" => return CiState::Pending,
            "in_progress" => return CiState::Running,
            _ => {} // "completed" (or unknown) falls through to the conclusion
        }
        match conclusion.unwrap_or("").to_ascii_lowercase().as_str() {
            "success" => CiState::Pass,
            "skipped" | "neutral" => CiState::Skipped,
            "cancelled" | "canceled" => CiState::Cancelled,
            "" => CiState::Pending, // completed-but-no-conclusion: treat as not-done
            _ => CiState::Fail,     // failure, timed_out, action_required, stale, …
        }
    }

    /// Map a GitLab pipeline/job `status` (one field carries everything).
    pub fn from_gitlab(status: &str) -> CiState {
        match status.to_ascii_lowercase().as_str() {
            "success" => CiState::Pass,
            "failed" => CiState::Fail,
            "running" => CiState::Running,
            "canceled" | "cancelled" => CiState::Cancelled,
            "skipped" | "manual" => CiState::Skipped,
            // created, pending, preparing, scheduled, waiting_for_resource
            _ => CiState::Pending,
        }
    }
}

// --- the run → job → step model -------------------------------------------

/// A single CI run (GitHub workflow-run / GitLab pipeline / Drone build / …).
/// All ids are stringly typed because providers disagree (u64 vs slug). Every
/// extension field is `#[serde(default)]` so older `ci_runs_cache` rows keep
/// deserializing after the model grows (same discipline as [`crate::github`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRun {
    pub id: String,
    /// Workflow / pipeline name.
    pub name: String,
    /// Human title (commit subject or PR title), best-effort.
    #[serde(default)]
    pub title: String,
    /// Triggering event: push / pull_request / workflow_dispatch / schedule / …
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub sha: String,
    pub state: CiState,
    /// The provider's raw status/conclusion strings, kept for display + debugging.
    #[serde(default)]
    pub status_raw: String,
    #[serde(default)]
    pub conclusion_raw: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub run_number: Option<u64>,
    /// RFC3339 timestamps.
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    /// Jobs, populated on `run_detail` (empty in a history listing).
    #[serde(default)]
    pub jobs: Vec<CiJob>,
}

impl CiRun {
    /// Seconds the run took (finished) or has been running (started only),
    /// measured against `now` epoch seconds. Mirrors [`crate::forge::model::CheckRun::duration_secs`].
    pub fn duration_secs(&self, now: i64) -> Option<i64> {
        duration_secs(self.started_at.as_deref(), self.finished_at.as_deref(), now)
    }

    /// Stable identity of the *workflow* this run is an instance of, the
    /// grouping key for [`latest_per_workflow`]. Normally the workflow /
    /// pipeline name; unnamed GitLab pipelines carry a synthesized unique
    /// `pipeline #<id>` display name, which would defeat grouping, so those
    /// fold onto one shared key (a branch's unnamed pipelines are all
    /// instances of the same pipeline config).
    pub fn workflow_key(&self) -> &str {
        match self.name.strip_prefix("pipeline #") {
            Some(rest) if rest == self.id => "",
            _ => &self.name,
        }
    }
}

/// One job within a run (GitHub job / GitLab job / Drone step-group).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJob {
    pub id: String,
    pub name: String,
    pub state: CiState,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub steps: Vec<CiStep>,
}

impl CiJob {
    pub fn duration_secs(&self, now: i64) -> Option<i64> {
        duration_secs(self.started_at.as_deref(), self.finished_at.as_deref(), now)
    }
}

/// One step within a job (GitHub step). Not every provider exposes steps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiStep {
    pub name: String,
    #[serde(default)]
    pub number: Option<u64>,
    pub state: CiState,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// Rollup of run/job states, for the panel `Section::Ci` header.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiSummary {
    pub passed: u32,
    pub failed: u32,
    pub running: u32,
    pub pending: u32,
    pub other: u32,
    pub total: u32,
}

/// Summarize a set of states (jobs of a run, or a page of runs).
pub fn summarize<'a>(states: impl IntoIterator<Item = &'a CiState>) -> CiSummary {
    let mut s = CiSummary::default();
    for st in states {
        s.total += 1;
        match st {
            CiState::Pass => s.passed += 1,
            CiState::Fail => s.failed += 1,
            CiState::Running => s.running += 1,
            CiState::Pending => s.pending += 1,
            CiState::Cancelled | CiState::Skipped => s.other += 1,
        }
    }
    s
}

/// The most recent run per workflow — the pipeline's *current* state, GitHub's
/// own branch-status semantics. Assumes `runs` is newest-first, which every
/// provider listing (and therefore the `ci_runs_cache`) guarantees — the panel
/// already renders `runs[0]` as "latest". Grouping is by
/// [`CiRun::workflow_key`]; input order is preserved.
///
/// Fan-out tie-break: a workflow can emit several sibling runs from one
/// trigger (Dependabot spawns one run per ecosystem), all created in the same
/// second — "newest" is then arbitrary API order, and it flipped between
/// refreshes, sometimes hiding a red sibling behind a green one. Among runs of
/// a workflow whose `started_at` equals the newest run's, a **failing** sibling
/// wins as the representative: showing red while a sibling is green is a far
/// smaller error than showing green while a sibling is red.
pub fn latest_per_workflow(runs: &[CiRun]) -> Vec<&CiRun> {
    let mut out: Vec<&CiRun> = Vec::new();
    let mut idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in runs {
        match idx.get(r.workflow_key()) {
            None => {
                idx.insert(r.workflow_key(), out.len());
                out.push(r);
            }
            Some(&i) => {
                let cur = out[i];
                if !cur.state.is_failure()
                    && r.state.is_failure()
                    && cur.started_at.is_some()
                    && cur.started_at == r.started_at
                {
                    out[i] = r;
                }
            }
        }
    }
    out
}

/// Rollup of the *current* pipeline state — one entry per workflow, judged by
/// its most recent run. This is what the statusbar badge and section summary
/// count: a workflow that failed 16 times but passes now contributes one
/// `passed`, not 16 `failed` (history stays visible in the run list rows).
pub fn current_summary(runs: &[CiRun]) -> CiSummary {
    summarize(latest_per_workflow(runs).into_iter().map(|r| &r.state))
}

/// Parse one of [`CiRun`]'s RFC3339 timestamps to unix seconds. `None` on junk
/// (providers do occasionally hand back an empty or malformed stamp).
pub fn epoch_secs(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}

/// Shared duration helper: seconds between `start` and `finish` (or `now` if
/// still running). `None` without a parseable start. Clamped to ≥ 0.
pub fn duration_secs(start: Option<&str>, finish: Option<&str>, now: i64) -> Option<i64> {
    let start = start.and_then(epoch_secs)?;
    let end = finish.and_then(epoch_secs).unwrap_or(now);
    Some((end - start).max(0))
}

// --- logs ("why did it fail") ---------------------------------------------

/// A job/step's fetched log text plus whether it was tail-truncated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLog {
    pub text: String,
    /// True when only the tail was fetched (provider/config `log_tail` cap).
    #[serde(default)]
    pub truncated: bool,
}

impl CiLog {
    /// Zero-based index of the first line that looks like a failure marker, for
    /// the view's jump-to-failure. Recognizes GitHub Actions `##[error]`,
    /// generic `error:`/`error ` prefixes, non-zero exit-code lines, and common
    /// test-runner failure words. `None` if nothing matches (caller stays put).
    pub fn first_failure_line(&self) -> Option<usize> {
        self.text.lines().position(line_is_failure)
    }
}

/// Does a single log line look like a failure marker? Pure, so it's unit-tested
/// directly. Case-insensitive; deliberately conservative to avoid false jumps.
pub fn line_is_failure(line: &str) -> bool {
    let l = line.trim_start();
    if l.starts_with("##[error]") {
        return true;
    }
    let lower = l.to_ascii_lowercase();
    lower.starts_with("error:")
        || lower.starts_with("error ")
        || lower.starts_with("fatal:")
        || lower.starts_with("failed:")
        || nonzero_exit_after(&lower, "exit code")
        || nonzero_exit_after(&lower, "exited with code")
        || nonzero_exit_after(&lower, "process completed with exit code")
        || lower.contains("test failed")
        || lower.contains("build failed")
        || lower.contains("failures:")
        || lower.contains("panicked at")
}

/// True when `phrase` appears in `haystack` immediately followed by a **nonzero**
/// integer code — so benign success lines (`exited with code 0`, `Process
/// completed with exit code 0`, common in docker-compose / GitHub Actions logs)
/// don't count as failures, and `exit code 1` no longer matches `exit code 10`
/// only by accident. A phrase with no parseable trailing integer is not a match
/// (conservative: avoids a false jump on ambiguous text).
fn nonzero_exit_after(haystack: &str, phrase: &str) -> bool {
    haystack.match_indices(phrase).any(|(i, _)| {
        let rest = haystack[i + phrase.len()..].trim_start();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u64>().is_ok_and(|code| code != 0)
    })
}

// --- dispatchable workflows (trigger, Phase B) ----------------------------

/// A dispatchable workflow definition (GitHub `workflow_dispatch`, GitLab
/// pipeline schedule/trigger). Drives the trigger prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWorkflow {
    pub id: String,
    pub name: String,
    /// Repo-relative source path, when known (`.github/workflows/ci.yml`).
    #[serde(default)]
    pub path: String,
    /// Whether the workflow can be triggered manually.
    #[serde(default)]
    pub dispatchable: bool,
    #[serde(default)]
    pub inputs: Vec<WorkflowInput>,
}

/// One declared input of a dispatchable workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    /// string | boolean | choice | environment | number
    #[serde(default)]
    pub input_type: String,
    /// Allowed values, for `choice`.
    #[serde(default)]
    pub options: Vec<String>,
}

/// Scope of a re-run request (Phase B mutation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerunScope {
    /// Re-run every job in the run.
    All,
    /// Re-run only the failed jobs.
    Failed,
}

/// Which trait operations a provider actually supports — so a provider can
/// decline mutations it can't perform and the UI hides the corresponding keys.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiCaps {
    pub logs: bool,
    pub steps: bool,
    pub trigger: bool,
    pub rerun: bool,
    /// Can re-run *only the failed jobs* ([`RerunScope::Failed`]). GitHub can
    /// (`gh run rerun --failed`); GitLab's pipeline `retry` has no scope, so
    /// offering "rerun failed" there would silently retry everything.
    #[serde(default)]
    pub rerun_failed: bool,
    pub cancel: bool,
}

// --- provider errors (mirrors github::GhError) ----------------------------

/// Distinguishable CI-provider failure modes, mapped to readable panel states
/// so the UI never crashes (same contract as [`crate::forge::ForgeError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiError {
    /// The provider's CLI/binary isn't installed.
    NotInstalled,
    /// No token / not logged in.
    NotAuthenticated,
    /// No server URL / provider not configured for this worktree.
    NotConfigured,
    /// No runs / pipeline / build found.
    NotFound,
    /// API rate limited.
    RateLimited,
    /// Transient network failure (DNS/TLS/connect/timeout) — the provider is
    /// unreachable, distinct from a config/auth issue. Feeds the connectivity
    /// holder + lets the UI show "CI unreachable" rather than a hard error.
    Offline,
    Other(String),
}

impl CiError {
    pub fn message(&self) -> String {
        match self {
            CiError::NotInstalled => "CI provider CLI not installed".into(),
            CiError::NotAuthenticated => "CI provider not authenticated".into(),
            CiError::NotConfigured => "no CI provider configured for this worktree".into(),
            CiError::NotFound => "no CI runs found".into(),
            CiError::RateLimited => "CI provider API rate limited".into(),
            CiError::Offline => "CI provider unreachable".into(),
            CiError::Other(m) => m.clone(),
        }
    }

    /// Whether this is a transient network error (mirrors
    /// [`crate::seam::SeamError::is_transient`]).
    pub fn is_transient(&self) -> bool {
        matches!(self, CiError::Offline)
    }
}

impl std::fmt::Display for CiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Classify a tool's lowercased stderr into a [`CiError`] (parallels
/// [`crate::github`]'s private `classify`). Pure + tested.
pub fn classify_stderr(stderr: &str) -> CiError {
    let s = stderr.to_ascii_lowercase();
    if s.contains("command not found") || s.contains("no such file") || s.contains("not found: ") {
        CiError::NotInstalled
    } else if s.contains("401")
        || s.contains("unauthorized")
        || s.contains("authentication")
        || s.contains("not logged")
        || s.contains("token")
    {
        CiError::NotAuthenticated
    } else if s.contains("rate limit") || s.contains("api rate") {
        CiError::RateLimited
    } else if s.contains("error connecting to")
        || s.contains("could not resolve host")
        || s.contains("no such host")
        || s.contains("connection refused")
        || s.contains("network is unreachable")
        || s.contains("i/o timeout")
        || s.contains("check your internet connection")
        || s.contains("tls handshake")
    {
        // Transient network failure — distinct from Other so the UI can show
        // "CI unreachable" and the connectivity holder can flip offline.
        CiError::Offline
    } else if s.contains("404") || s.contains("not found") {
        CiError::NotFound
    } else {
        CiError::Other(stderr.trim().to_string())
    }
}

// --- repo CI-config detection ("how is the repo looking") -----------------

/// A CI system a repo can be configured for. The provider *kind*; the concrete
/// server endpoint/token come from `[ci]` config in [`crate::config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiSystem {
    GithubActions,
    GitlabCi,
    Drone,
    Woodpecker,
    Jenkins,
    Argo,
}

impl CiSystem {
    pub fn label(self) -> &'static str {
        match self {
            CiSystem::GithubActions => "GitHub Actions",
            CiSystem::GitlabCi => "GitLab CI",
            CiSystem::Drone => "Drone",
            CiSystem::Woodpecker => "Woodpecker",
            CiSystem::Jenkins => "Jenkins",
            CiSystem::Argo => "Argo",
        }
    }
}

/// One detected CI configuration: the system + the repo-relative file(s) that
/// evidence it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiConfig {
    pub system: CiSystem,
    pub files: Vec<String>,
}

/// Classify a list of repo-relative paths (forward-slash separated) into the CI
/// systems they configure. Pure — the FS walk lives in [`detect_ci_configs`],
/// so this gets full unit coverage (the `token_from`/`resolve_token` split).
pub fn classify_ci_files(paths: &[String]) -> Vec<CiConfig> {
    use CiSystem::*;
    // Preserve a stable, meaningful order regardless of input ordering.
    let order = [GithubActions, GitlabCi, Drone, Woodpecker, Jenkins, Argo];
    let mut hits: Vec<(CiSystem, Vec<String>)> = order.iter().map(|s| (*s, Vec::new())).collect();

    let push = |hits: &mut Vec<(CiSystem, Vec<String>)>, sys: CiSystem, p: &str| {
        if let Some(entry) = hits.iter_mut().find(|(s, _)| *s == sys) {
            entry.1.push(p.to_string());
        }
    };

    for raw in paths {
        let p = raw.trim_start_matches("./");
        let lower = p.to_ascii_lowercase();
        let is_yaml = lower.ends_with(".yml") || lower.ends_with(".yaml");
        if (p.starts_with(".github/workflows/")) && is_yaml {
            push(&mut hits, GithubActions, p);
        } else if lower == ".gitlab-ci.yml" {
            push(&mut hits, GitlabCi, p);
        } else if lower == ".drone.yml" || lower == ".drone.yaml" {
            push(&mut hits, Drone, p);
        } else if lower == ".woodpecker.yml"
            || lower == ".woodpecker.yaml"
            || (p.starts_with(".woodpecker/") && is_yaml)
        {
            push(&mut hits, Woodpecker, p);
        } else if p == "Jenkinsfile" || lower.ends_with("/jenkinsfile") || lower == "jenkinsfile" {
            push(&mut hits, Jenkins, p);
        } else if p.starts_with(".argo/") && is_yaml {
            push(&mut hits, Argo, p);
        }
    }

    hits.into_iter()
        .filter(|(_, files)| !files.is_empty())
        .map(|(system, files)| CiConfig { system, files })
        .collect()
}

/// Detect which CI systems a worktree is configured for, by well-known files.
/// Best-effort filesystem read rooted at `repo_root`; a missing repo or
/// unreadable dir simply yields fewer hits — never errors. The pure
/// classification is [`classify_ci_files`].
pub fn detect_ci_configs(repo_root: &Path) -> Vec<CiConfig> {
    let mut paths = Vec::new();
    let mut add_if = |rel: &str| {
        if repo_root.join(rel).exists() {
            paths.push(rel.to_string());
        }
    };
    add_if(".gitlab-ci.yml");
    add_if(".drone.yml");
    add_if(".drone.yaml");
    add_if(".woodpecker.yml");
    add_if(".woodpecker.yaml");
    add_if("Jenkinsfile");

    // Directory globs: .github/workflows, .woodpecker, .argo.
    for dir in [".github/workflows", ".woodpecker", ".argo"] {
        if let Ok(entries) = std::fs::read_dir(repo_root.join(dir)) {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    paths.push(format!("{dir}/{name}"));
                }
            }
        }
    }
    classify_ci_files(&paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_state_mapping() {
        assert_eq!(CiState::from_github("queued", None), CiState::Pending);
        assert_eq!(CiState::from_github("in_progress", None), CiState::Running);
        assert_eq!(
            CiState::from_github("completed", Some("success")),
            CiState::Pass
        );
        assert_eq!(
            CiState::from_github("completed", Some("failure")),
            CiState::Fail
        );
        assert_eq!(
            CiState::from_github("completed", Some("timed_out")),
            CiState::Fail
        );
        assert_eq!(
            CiState::from_github("completed", Some("cancelled")),
            CiState::Cancelled
        );
        assert_eq!(
            CiState::from_github("completed", Some("skipped")),
            CiState::Skipped
        );
        assert_eq!(
            CiState::from_github("completed", Some("neutral")),
            CiState::Skipped
        );
        // completed but no conclusion yet → not done
        assert_eq!(CiState::from_github("completed", None), CiState::Pending);
        // case-insensitivity
        assert_eq!(CiState::from_github("IN_PROGRESS", None), CiState::Running);
    }

    #[test]
    fn gitlab_state_mapping() {
        assert_eq!(CiState::from_gitlab("success"), CiState::Pass);
        assert_eq!(CiState::from_gitlab("failed"), CiState::Fail);
        assert_eq!(CiState::from_gitlab("running"), CiState::Running);
        assert_eq!(CiState::from_gitlab("canceled"), CiState::Cancelled);
        assert_eq!(CiState::from_gitlab("manual"), CiState::Skipped);
        assert_eq!(CiState::from_gitlab("created"), CiState::Pending);
        assert_eq!(CiState::from_gitlab("anything-else"), CiState::Pending);
    }

    #[test]
    fn state_predicates() {
        assert!(!CiState::Pending.is_terminal());
        assert!(!CiState::Running.is_terminal());
        assert!(CiState::Pass.is_terminal());
        assert!(CiState::Fail.is_terminal());
        assert!(CiState::Cancelled.is_terminal());
        assert!(CiState::Skipped.is_terminal());
        assert!(CiState::Fail.is_failure());
        assert!(!CiState::Cancelled.is_failure());
        assert_eq!(CiState::default(), CiState::Pending);
    }

    #[test]
    fn summary_counts() {
        let states = [
            CiState::Pass,
            CiState::Pass,
            CiState::Fail,
            CiState::Running,
            CiState::Pending,
            CiState::Cancelled,
            CiState::Skipped,
        ];
        let s = summarize(states.iter());
        assert_eq!(s.total, 7);
        assert_eq!(s.passed, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.running, 1);
        assert_eq!(s.pending, 1);
        assert_eq!(s.other, 2);
        assert_eq!(summarize([].iter()), CiSummary::default());
    }

    fn run_named(id: &str, name: &str, state: CiState) -> CiRun {
        CiRun {
            id: id.into(),
            name: name.into(),
            state,
            ..Default::default()
        }
    }

    #[test]
    fn latest_per_workflow_dedupes_history() {
        // Newest-first, as providers list them.
        let runs = vec![
            run_named("100", "ci", CiState::Pass),
            run_named("99", "docs", CiState::Running),
            run_named("98", "ci", CiState::Fail),
            run_named("97", "ci", CiState::Fail),
            run_named("96", "docs", CiState::Fail),
        ];
        let latest = latest_per_workflow(&runs);
        let ids: Vec<&str> = latest.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["100", "99"]); // one per workflow, order preserved
        assert!(latest_per_workflow(&[]).is_empty());
    }

    #[test]
    fn fanout_tie_prefers_the_failing_sibling() {
        // Dependabot shape: three sibling runs of one workflow, all created in
        // the same second, one failing — the failing one must represent the
        // workflow regardless of the API's arbitrary tie order.
        let at = Some("2026-08-20T23:53:45Z".to_string());
        let mk = |id: &str, state: CiState| CiRun {
            id: id.into(),
            name: "Dependabot Updates".into(),
            state,
            started_at: at.clone(),
            ..Default::default()
        };
        let runs = vec![
            mk("3", CiState::Pass),
            mk("2", CiState::Fail),
            mk("1", CiState::Pass),
        ];
        let latest = latest_per_workflow(&runs);
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].id, "2");
        let cur = current_summary(&runs);
        assert_eq!((cur.failed, cur.passed, cur.total), (1, 0, 1));

        // A failing run in a DIFFERENT (older) second is history, not a tie —
        // the newest run still represents the workflow.
        let mut runs = vec![mk("5", CiState::Pass)];
        runs.push(CiRun {
            started_at: Some("2026-08-20T20:00:00Z".into()),
            ..mk("4", CiState::Fail)
        });
        assert_eq!(latest_per_workflow(&runs)[0].id, "5");

        // Runs without timestamps never tie (no false representative swap).
        let untimed = vec![
            CiRun {
                started_at: None,
                ..mk("7", CiState::Pass)
            },
            CiRun {
                started_at: None,
                ..mk("6", CiState::Fail)
            },
        ];
        assert_eq!(latest_per_workflow(&untimed)[0].id, "7");
    }

    #[test]
    fn current_summary_reflects_now_not_history() {
        // The "16 failed" regression: a wall of old failures, but every
        // workflow's most recent run is green → the rollup is quiet.
        let mut runs = vec![
            run_named("100", "ci", CiState::Pass),
            run_named("99", "docs", CiState::Pass),
        ];
        for i in 0..16 {
            runs.push(run_named(&format!("{}", 98 - i), "ci", CiState::Fail));
        }
        assert_eq!(summarize(runs.iter().map(|r| &r.state)).failed, 16);
        let cur = current_summary(&runs);
        assert_eq!(cur.failed, 0);
        assert_eq!(cur.passed, 2);
        assert_eq!(cur.total, 2);

        // …and a currently-failing workflow still counts exactly once.
        runs.insert(0, run_named("101", "lint", CiState::Fail));
        let cur = current_summary(&runs);
        assert_eq!(cur.failed, 1);
        assert_eq!(cur.total, 3);
    }

    #[test]
    fn workflow_key_folds_synthesized_gitlab_names() {
        // Unnamed GitLab pipelines get a unique "pipeline #<id>" display name;
        // they all fold onto one key so only the newest pipeline counts.
        let runs = vec![
            run_named("12", "pipeline #12", CiState::Pass),
            run_named("11", "pipeline #11", CiState::Fail),
            run_named("10", "pipeline #10", CiState::Fail),
        ];
        assert_eq!(runs[0].workflow_key(), "");
        let cur = current_summary(&runs);
        assert_eq!(cur.total, 1);
        assert_eq!(cur.failed, 0);
        assert_eq!(cur.passed, 1);

        // A *named* pipeline keeps its own identity, even if it looks similar.
        let named = run_named("9", "pipeline #8", CiState::Fail); // id mismatch
        assert_eq!(named.workflow_key(), "pipeline #8");
        let real = run_named("7", "deploy", CiState::Fail);
        assert_eq!(real.workflow_key(), "deploy");
    }

    #[test]
    fn epoch_secs_parses_rfc3339_and_rejects_junk() {
        // Feeds the attention model's `since` for a failing CI run, so a bad
        // stamp must degrade to "no honest time" rather than a bogus one.
        assert_eq!(epoch_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_secs("2026-06-25T10:00:00Z"), Some(1_782_381_600));
        // Offsets normalize to UTC.
        assert_eq!(
            epoch_secs("2026-06-25T12:00:00+02:00"),
            epoch_secs("2026-06-25T10:00:00Z")
        );
        for junk in ["", "nonsense", "2026-06-25", "2026-13-99T99:99:99Z"] {
            assert_eq!(epoch_secs(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn durations() {
        let start = "2026-06-25T10:00:00Z";
        let finish = "2026-06-25T10:01:30Z";
        assert_eq!(duration_secs(Some(start), Some(finish), 0), Some(90));
        // still running: measure against `now`
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-25T10:00:10Z")
            .unwrap()
            .timestamp();
        assert_eq!(duration_secs(Some(start), None, now), Some(10));
        // clamps negatives to zero
        assert_eq!(duration_secs(Some(finish), Some(start), 0), Some(0));
        // no start → None
        assert_eq!(duration_secs(None, Some(finish), 0), None);
        // unparseable → None
        assert_eq!(duration_secs(Some("nonsense"), None, 0), None);

        let run = CiRun {
            started_at: Some(start.into()),
            finished_at: Some(finish.into()),
            ..Default::default()
        };
        assert_eq!(run.duration_secs(0), Some(90));
        let job = CiJob {
            started_at: Some(start.into()),
            finished_at: Some(finish.into()),
            ..Default::default()
        };
        assert_eq!(job.duration_secs(0), Some(90));
    }

    #[test]
    fn log_failure_scan() {
        let log = CiLog {
            text: "Compiling foo\nRunning tests\n##[error]Process completed with exit code 1\ndone"
                .into(),
            truncated: false,
        };
        assert_eq!(log.first_failure_line(), Some(2));

        let clean = CiLog {
            text: "all good\nfinished".into(),
            truncated: false,
        };
        assert_eq!(clean.first_failure_line(), None);
        assert_eq!(CiLog::default().first_failure_line(), None);

        assert!(line_is_failure("##[error]Something broke"));
        assert!(line_is_failure("  error: cannot find symbol"));
        assert!(line_is_failure("fatal: not a git repository"));
        assert!(line_is_failure("thread 'main' panicked at src/lib.rs:1"));
        assert!(line_is_failure("Test failed: assertion"));
        assert!(line_is_failure("npm exited with code 7"));
        assert!(!line_is_failure("no errors here"));
        assert!(!line_is_failure("warning: deprecated"));
        assert!(!line_is_failure(""));
    }

    /// Success lines that echo an exit *phrase* with a zero code (routine in
    /// docker-compose / GitHub-Actions logs) must NOT count as failures, and the
    /// nonzero-code paths still do. Regression for the audit finding where
    /// "exited with code 0" / "process completed with exit code 0" jumped
    /// jump-to-failure to a benign line.
    #[test]
    fn exit_code_zero_is_not_a_failure() {
        // Zero-code success lines: not failures.
        assert!(!line_is_failure("web_1 exited with code 0"));
        assert!(!line_is_failure("Process completed with exit code 0"));
        assert!(!line_is_failure("container exited with code 0"));
        assert!(!line_is_failure("job finished, exit code 0"));

        // Nonzero codes still trip it, in every phrasing.
        assert!(line_is_failure("db_1 exited with code 137"));
        assert!(line_is_failure("Process completed with exit code 2"));
        assert!(line_is_failure("step failed with exit code 1"));

        // "exit code 1" no longer matches "exit code 10" only by accident: both
        // are nonzero and both correctly count.
        assert!(line_is_failure("exit code 10"));

        // A phrase with no parseable trailing code is conservatively ignored.
        assert!(!line_is_failure("see exit code documentation"));

        // first_failure_line skips the benign code-0 line and lands on the real
        // failure further down.
        let log = CiLog {
            text: "web_1 exited with code 0\nstill running\ndb_1 exited with code 1\ndone".into(),
            truncated: false,
        };
        assert_eq!(log.first_failure_line(), Some(2));
    }

    #[test]
    fn error_classification_and_messages() {
        assert_eq!(
            classify_stderr("command not found: glab"),
            CiError::NotInstalled
        );
        assert_eq!(
            classify_stderr("HTTP 401 Unauthorized"),
            CiError::NotAuthenticated
        );
        assert_eq!(
            classify_stderr("invalid token provided"),
            CiError::NotAuthenticated
        );
        assert_eq!(
            classify_stderr("API rate limit exceeded"),
            CiError::RateLimited
        );
        assert_eq!(classify_stderr("404 project not found"), CiError::NotFound);
        // Transient network failures classify as Offline (before the 404 branch).
        for s in [
            "error connecting to api.github.com",
            "could not resolve host: gitlab.com",
            "connection refused",
            "network is unreachable",
            "dial tcp: i/o timeout",
            "tls handshake timeout",
        ] {
            assert_eq!(classify_stderr(s), CiError::Offline, "{s:?}");
        }
        assert!(CiError::Offline.is_transient());
        assert!(!CiError::NotAuthenticated.is_transient());
        match classify_stderr("boom") {
            CiError::Other(m) => assert_eq!(m, "boom"),
            other => panic!("unexpected {other:?}"),
        }
        // every message renders
        for e in [
            CiError::NotInstalled,
            CiError::NotAuthenticated,
            CiError::NotConfigured,
            CiError::NotFound,
            CiError::RateLimited,
            CiError::Offline,
            CiError::Other("x".into()),
        ] {
            assert!(!e.message().is_empty());
            assert_eq!(e.to_string(), e.message());
        }
    }

    #[test]
    fn ci_file_classification() {
        let paths: Vec<String> = [
            ".github/workflows/ci.yml",
            ".github/workflows/release.yaml",
            ".github/workflows/README.md", // ignored (not yaml)
            ".gitlab-ci.yml",
            ".drone.yml",
            ".woodpecker.yml",
            ".woodpecker/lint.yml",
            "Jenkinsfile",
            ".argo/build.yaml",
            "src/main.rs", // ignored
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let cfgs = classify_ci_files(&paths);
        let systems: Vec<CiSystem> = cfgs.iter().map(|c| c.system).collect();
        assert_eq!(
            systems,
            vec![
                CiSystem::GithubActions,
                CiSystem::GitlabCi,
                CiSystem::Drone,
                CiSystem::Woodpecker,
                CiSystem::Jenkins,
                CiSystem::Argo,
            ]
        );
        let gha = cfgs
            .iter()
            .find(|c| c.system == CiSystem::GithubActions)
            .unwrap();
        assert_eq!(gha.files.len(), 2); // .md excluded
        let wp = cfgs
            .iter()
            .find(|c| c.system == CiSystem::Woodpecker)
            .unwrap();
        assert_eq!(wp.files.len(), 2); // root + dir
        assert!(classify_ci_files(&[]).is_empty());
        assert!(classify_ci_files(&["src/main.rs".to_string()]).is_empty());

        // labels are non-empty for every system
        for sys in systems {
            assert!(!sys.label().is_empty());
        }
    }

    #[test]
    fn detect_ci_configs_walks_the_worktree() {
        let root = std::env::temp_dir().join(format!("sz-ci-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Missing repo → empty, never errors.
        assert!(detect_ci_configs(&root).is_empty());

        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::write(root.join(".github/workflows/ci.yml"), "on: push").unwrap();
        std::fs::write(root.join(".github/workflows/notes.md"), "x").unwrap(); // ignored
        std::fs::write(root.join(".gitlab-ci.yml"), "stages: []").unwrap();
        std::fs::write(root.join("Jenkinsfile"), "pipeline {}").unwrap();
        std::fs::create_dir_all(root.join(".woodpecker")).unwrap();
        std::fs::write(root.join(".woodpecker/lint.yml"), "steps: {}").unwrap();

        let cfgs = detect_ci_configs(&root);
        let systems: std::collections::HashSet<CiSystem> = cfgs.iter().map(|c| c.system).collect();
        assert!(systems.contains(&CiSystem::GithubActions));
        assert!(systems.contains(&CiSystem::GitlabCi));
        assert!(systems.contains(&CiSystem::Jenkins));
        assert!(systems.contains(&CiSystem::Woodpecker));
        // The non-yaml workflow file is excluded.
        let gha = cfgs
            .iter()
            .find(|c| c.system == CiSystem::GithubActions)
            .unwrap();
        assert_eq!(gha.files.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ci_state_serde_roundtrip() {
        let json = serde_json::to_string(&CiState::Pass).unwrap();
        assert_eq!(json, "\"pass\"");
        let back: CiState = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(back, CiState::Running);
        // a full run round-trips (cache contract)
        let run = CiRun {
            id: "42".into(),
            name: "CI".into(),
            state: CiState::Fail,
            jobs: vec![CiJob {
                id: "j1".into(),
                name: "build".into(),
                state: CiState::Fail,
                steps: vec![CiStep {
                    name: "compile".into(),
                    state: CiState::Fail,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = serde_json::to_string(&run).unwrap();
        let back: CiRun = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "42");
        assert_eq!(back.jobs[0].steps[0].state, CiState::Fail);
    }
}
