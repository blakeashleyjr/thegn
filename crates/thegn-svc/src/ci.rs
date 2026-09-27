//! CI/CD provider seam (AV group). The cross-provider sibling of
//! [`crate::forge`]: a [`CiProvider`] trait normalizing run history / job-step
//! drilldown / logs / trigger·rerun·cancel onto thegn-core's
//! [`thegn_core::ci`] model, with per-provider impls that degrade
//! native→CLI→error just like the forge's `GithubNative`→`GithubCli` ladder
//! ("a gap is slower or unavailable, never broken").
//!
//! Phase A ships GitHub Actions (via the `gh` CLI, reusing the user's `gh`
//! auth) and GitLab CI (via `glab` + the GitLab API), routed as a
//! `Box<dyn CiProvider>` ([`CiClient`]).
//!
//! Every subprocess call is blocking; callers invoke these from a
//! `spawn_blocking` task (the host's hydration seam), exactly as the `gh`
//! backend is driven.

use thegn_core::ci::{
    CiCaps, CiError, CiJob, CiLog, CiRun, CiState, CiStep, CiSystem, CiWorkflow, RerunScope,
    classify_stderr,
};
use thegn_core::config::{CiConfig, CiProviderKind};
use thegn_core::remote::GitLoc;
use thegn_core::secretref::BareAs;

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Command, Stdio};

/// Provider-side bounds are deliberately finite even before the host applies
/// the user's cache policy.  This protects the blocking provider lane from a
/// job that emits an unbounded log (the configured, usually smaller, bounds
/// are applied again by the cache-facing read paths).
const PROVIDER_LOG_MAX_LINES: usize = 2_000;
const PROVIDER_LOG_MAX_BYTES: usize = 1024 * 1024;

/// A CI/CD backend for one provider. Read methods first; mutations are
/// capability-gated via [`Self::caps`] so a provider can decline what it can't do.
///
/// A blocking seam (provider-seams spec): every implementation is a
/// subprocess (`gh`, `glab`) or a REST call it drives synchronously, so
/// callers run it on a blocking thread and no runtime handle is needed.
/// Object-safe: `provider_for` hands back a `Box<dyn CiProvider>`.
pub trait CiProvider: thegn_core::seam::Probe + Send + Sync {
    /// Which CI system this is.
    fn system(&self) -> CiSystem;

    /// Recent runs (newest first), optionally filtered to `branch`.
    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError>;

    /// One run with its jobs (and steps, where the provider exposes them).
    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError>;

    /// A job's log text ("why did it fail"). `run_id` is needed by providers
    /// whose job ids aren't globally addressable (GitLab); GitHub ignores it.
    fn logs(&self, loc: &GitLoc, run_id: &str, job_id: &str) -> Result<CiLog, CiError>;

    /// Dispatchable workflow definitions (drives the trigger prompt).
    fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError>;

    /// Trigger a workflow with `inputs` (`workflow_dispatch`). Phase B.
    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError>;

    /// Re-run a run (all jobs or only the failed ones). Phase B.
    fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError>;

    /// Cancel an in-flight run. Phase B.
    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError>;

    fn caps(&self) -> CiCaps;
}

/// Provider run history plus rows rejected because their target identifiers
/// were malformed. Consumers preserve this count when presenting a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunList {
    pub runs: Vec<CiRun>,
    pub discarded_rows: usize,
}

/// A targeted run response plus the number of malformed jobs omitted from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunDetail {
    pub run: CiRun,
    pub discarded_jobs: usize,
}

// === provider selection ====================================================

/// Pick the concrete CI provider for a worktree from `[ci]` config, resolving
/// `"auto"` by sniffing the git remote then falling back to detected CI files.
/// `None` when CI is disabled, undetected, or the resolved system's kind is
/// `reserved` in `CiProviderKind` (Drone/Woodpecker/Jenkins/Argo) — the
/// caller shows a note. The reserved set is the config enum's, not a second
/// list here: `kind_coverage` below pins that every non-reserved kind
/// constructs a client.
pub fn provider_for(loc: &GitLoc, cfg: &CiConfig) -> Option<CiClient> {
    let system = resolve_system(loc, cfg)?;
    client_for_system_with_config(system, cfg)
}

/// The system → client factory (pure; `provider_for` adds remote/file sniffing).
pub fn client_for_system(system: CiSystem) -> Option<CiClient> {
    client_for_system_with_config(system, &CiConfig::default())
}

/// System → client factory carrying provider configuration and resolving any
/// configured token once, at construction. The value is injected into the
/// child environment (never argv) for every `glab` call.
fn client_for_system_with_config(system: CiSystem, cfg: &CiConfig) -> Option<CiClient> {
    match system {
        CiSystem::GithubActions => Some(Box::new(GithubCi)),
        CiSystem::GitlabCi => Some(Box::new(GitlabCi::from_config(cfg))),
        CiSystem::Drone | CiSystem::Woodpecker | CiSystem::Jenkins | CiSystem::Argo => None,
    }
}

/// The kind → system map for explicit (non-`auto`) selections.
pub fn system_for_kind(kind: CiProviderKind) -> Option<CiSystem> {
    match kind {
        CiProviderKind::None | CiProviderKind::Auto => None,
        CiProviderKind::Github => Some(CiSystem::GithubActions),
        CiProviderKind::Gitlab => Some(CiSystem::GitlabCi),
        CiProviderKind::Drone => Some(CiSystem::Drone),
        CiProviderKind::Woodpecker => Some(CiSystem::Woodpecker),
        CiProviderKind::Jenkins => Some(CiSystem::Jenkins),
        CiProviderKind::Argo => Some(CiSystem::Argo),
    }
}

/// Resolve the active [`CiSystem`] for a worktree (pure once the remote URL is
/// in hand). `provider == auto` sniffs the origin host, else honours the config.
pub fn resolve_system(loc: &GitLoc, cfg: &CiConfig) -> Option<CiSystem> {
    match cfg.provider {
        CiProviderKind::None => None,
        CiProviderKind::Auto => {
            if let Some(sys) = origin_url(loc).as_deref().and_then(system_from_remote_host) {
                return Some(sys);
            }
            // Fall back to detected CI-config files in the worktree.
            if !loc.is_remote()
                && let Some(cfg) =
                    thegn_core::ci::detect_ci_configs(std::path::Path::new(&loc.path())).first()
            {
                return Some(cfg.system);
            }
            None
        }
        explicit => system_for_kind(explicit),
    }
}

/// Map a git remote URL's host to a CI system (pure, tested).
///
/// Matching is the exact apex or a subdomain of it, and deliberately NOT a
/// `starts_with("github.")` prefix: `github.com.evil.test` begins with that
/// prefix and is an attacker-controlled host, while a genuine self-hosted
/// `github.mycorp.com` is structurally identical to it — no prefix rule can tell
/// them apart. Self-hosted instances therefore need explicit configuration
/// rather than a guess that also admits a lookalike.
pub fn system_from_remote_host(url: &str) -> Option<CiSystem> {
    let host = parse_gitlab_remote(url)?.host;
    let hostname = host.split(':').next()?;
    if hostname == "github.com" || hostname.ends_with(".github.com") {
        Some(CiSystem::GithubActions)
    } else if hostname == "gitlab.com" || hostname.ends_with(".gitlab.com") {
        Some(CiSystem::GitlabCi)
    } else {
        None
    }
}

fn origin_url(loc: &GitLoc) -> Option<String> {
    let out = loc
        .git_command(&["remote", "get-url", "origin"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The selected CI backend. Object-safe, so routing is a `Box<dyn>` and a
/// new provider is one `impl CiProvider` + one factory arm.
pub type CiClient = Box<dyn CiProvider>;

// === helpers ===============================================================

fn run_cli(cmd: &mut std::process::Command) -> Result<String, CiError> {
    let out = cmd.output().map_err(|e| CiError::Other(e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(classify_stderr(&String::from_utf8_lossy(&out.stderr)))
    }
}

/// Run a log-producing command while retaining only its newest complete lines.
///
/// `Command::output` is unsuitable for CI logs: it buffers the complete child
/// stdout before the caller can impose a cap.  Drain stdout in a fixed-size
/// reader instead and keep a bounded deque; stderr is drained concurrently so
/// a noisy failed provider cannot deadlock the child.  The status and error
/// classification remain identical to [`run_cli`].
fn run_bounded_log(cmd: &mut Command) -> Result<CiLog, CiError> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CiError::Other(e.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CiError::Other("provider stdout was not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CiError::Other("provider stderr was not piped".into()))?;

    let out_thread = std::thread::spawn(|| collect_log(stdout));
    let err_thread = std::thread::spawn(|| read_stderr(stderr));
    let status = child.wait().map_err(|e| CiError::Other(e.to_string()))?;
    let (text, truncated) = out_thread
        .join()
        .map_err(|_| CiError::Other("provider log reader panicked".into()))??;
    let stderr = err_thread
        .join()
        .map_err(|_| CiError::Other("provider stderr reader panicked".into()))?;
    if !status.success() {
        return Err(classify_stderr(&stderr));
    }
    Ok(CiLog { text, truncated })
}

/// Read at most a fixed number of bytes per chunk and discard old complete
/// lines as new output arrives.  An over-sized line is discarded rather than
/// allowing one pathological line to defeat the byte ceiling.
fn collect_log<R: Read>(mut reader: R) -> Result<(String, bool), CiError> {
    let mut bytes = [0u8; 8192];
    let mut line = Vec::new();
    let mut lines: VecDeque<String> = VecDeque::new();
    let mut kept_bytes = 0usize;
    let mut truncated = false;
    let mut discard_line = false;

    loop {
        let n = reader
            .read(&mut bytes)
            .map_err(|e| CiError::Other(e.to_string()))?;
        if n == 0 {
            if !line.is_empty() && !discard_line {
                push_log_line(&mut lines, &mut kept_bytes, &line, &mut truncated);
            }
            break;
        }
        for &byte in &bytes[..n] {
            if discard_line {
                if byte == b'\n' {
                    discard_line = false;
                    truncated = true;
                }
                continue;
            }
            line.push(byte);
            if line.len() > PROVIDER_LOG_MAX_BYTES {
                line.clear();
                discard_line = true;
                truncated = true;
            } else if byte == b'\n' {
                push_log_line(&mut lines, &mut kept_bytes, &line, &mut truncated);
                line.clear();
            }
        }
    }

    Ok((lines.into_iter().collect(), truncated))
}

fn push_log_line(
    lines: &mut VecDeque<String>,
    kept_bytes: &mut usize,
    line: &[u8],
    truncated: &mut bool,
) {
    let Ok(line) = std::str::from_utf8(line) else {
        *truncated = true;
        return;
    };
    let len = line.len();
    if len > PROVIDER_LOG_MAX_BYTES {
        *truncated = true;
        return;
    }
    lines.push_back(line.to_owned());
    *kept_bytes += len;
    while lines.len() > PROVIDER_LOG_MAX_LINES || *kept_bytes > PROVIDER_LOG_MAX_BYTES {
        if let Some(old) = lines.pop_front() {
            *kept_bytes -= old.len();
            *truncated = true;
        }
    }
}

fn read_stderr<R: Read>(mut reader: R) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = reader.read(&mut chunk) {
        if n == 0 {
            break;
        }
        if bytes.len() < 64 * 1024 {
            let take = n.min(64 * 1024 - bytes.len());
            bytes.extend_from_slice(&chunk[..take]);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn nonempty(s: &serde_json::Value, key: &str) -> Option<String> {
    s.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|v| !v.is_empty())
}

/// Parse provider JSON identifiers as unsigned JSON integers. Provider IDs do
/// not pass through a string grammar: serde's number representation rejects
/// fractions, exponents, negatives and values outside u64.
fn json_id(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_u64)
        .filter(|id| *id > 0)
        .map(|id| id.to_string())
}

/// Canonical textual IDs supplied by CLI/API callers. Keep this pure so every
/// ingress and provider boundary applies the same contract.
pub fn validate_ci_id(value: &str) -> Result<&str, CiError> {
    let valid = !value.is_empty()
        && value.len() <= 20
        && value.as_bytes()[0] != b'0'
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0);
    if valid {
        Ok(value)
    } else {
        Err(CiError::Other(format!("invalid CI identifier {value:?}")))
    }
}

/// Workflow names and repository-relative workflow paths accepted by `gh`.
/// Spaces are valid in names; path selectors additionally allow `/`. Leading
/// option syntax, controls, URL syntax and dot segments are never selectors.
pub fn validate_workflow_selector(value: &str) -> Result<&str, CiError> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.starts_with('-')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !value.chars().any(char::is_control)
        && !value.contains('\\');
    if valid {
        Ok(value)
    } else {
        Err(CiError::Other(format!(
            "invalid workflow selector {value:?}"
        )))
    }
}

/// Validate a Git branch ref using the forbidden forms from git-check-ref-format.
/// Query-reserved characters such as `&`, `=`, `%` and `#` remain valid data.
pub fn validate_branch_ref(value: &str) -> Result<&str, CiError> {
    let bytes = value.as_bytes();
    let bad = value.is_empty()
        || value == "@"
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock"))
        || bytes
            .iter()
            .any(|b| *b <= 0x20 || *b == 0x7f || b"~^:?*[\\".contains(b));
    if bad {
        Err(CiError::Other(format!("invalid branch ref {value:?}")))
    } else {
        Ok(value)
    }
}

fn github_workflow_argv(
    workflow: &str,
    inputs: &[(String, String)],
) -> Result<Vec<String>, CiError> {
    validate_workflow_selector(workflow)?;
    let mut args = vec!["workflow".into(), "run".into()];
    for (key, value) in inputs {
        args.push("-f".into());
        args.push(format!("{key}={value}"));
    }
    args.extend(["--".into(), workflow.into()]);
    Ok(args)
}

fn github_run_detail_argv(run_id: &str) -> Result<Vec<String>, CiError> {
    validate_ci_id(run_id)?;
    Ok(vec![
        "run".into(),
        "view".into(),
        "--json".into(),
        GH_DETAIL_FIELDS.into(),
        "--".into(),
        run_id.into(),
    ])
}

fn github_rerun_argv(run_id: &str, scope: RerunScope) -> Result<Vec<String>, CiError> {
    validate_ci_id(run_id)?;
    let mut args = vec!["run".into(), "rerun".into()];
    if scope == RerunScope::Failed {
        args.push("--failed".into());
    }
    args.extend(["--".into(), run_id.into()]);
    Ok(args)
}

fn github_cancel_argv(run_id: &str) -> Result<Vec<String>, CiError> {
    validate_ci_id(run_id)?;
    Ok(vec![
        "run".into(),
        "cancel".into(),
        "--".into(),
        run_id.into(),
    ])
}

fn gitlab_pipelines_endpoint(
    project_segment: &str,
    branch: Option<&str>,
    limit: usize,
) -> Result<String, CiError> {
    let mut endpoint = format!(
        "projects/{project_segment}/pipelines?per_page={}",
        limit.max(1)
    );
    if let Some(branch) = branch {
        validate_branch_ref(branch)?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("ref", branch)
            .finish();
        endpoint.push('&');
        endpoint.push_str(&query);
    }
    Ok(endpoint)
}

fn gitlab_run_endpoint(project: &str, run_id: &str, action: &str) -> Result<String, CiError> {
    validate_ci_id(run_id)?;
    if !matches!(action, "" | "jobs" | "retry" | "cancel") {
        return Err(CiError::Other(format!(
            "invalid GitLab pipeline operation {action:?}"
        )));
    }
    let suffix = if action.is_empty() {
        String::new()
    } else {
        format!("/{action}")
    };
    Ok(format!("projects/{project}/pipelines/{run_id}{suffix}"))
}

fn gitlab_job_trace_endpoint(project: &str, job_id: &str) -> Result<String, CiError> {
    validate_ci_id(job_id)?;
    Ok(format!("projects/{project}/jobs/{job_id}/trace"))
}

fn parse_provider_row<T>(
    rows: &[serde_json::Value],
    mut parse: impl FnMut(&serde_json::Value) -> Option<T>,
) -> (Vec<T>, usize) {
    let mut valid = Vec::with_capacity(rows.len());
    let mut discarded = 0;
    for row in rows {
        if let Some(value) = parse(row) {
            valid.push(value);
        } else {
            discarded += 1;
        }
    }
    (valid, discarded)
}

// === GitHub Actions (gh CLI) ==============================================

const GH_RUN_FIELDS: &str = "databaseId,name,displayTitle,headBranch,headSha,event,\
                             status,conclusion,number,createdAt,updatedAt,url,workflowName";
const GH_DETAIL_FIELDS: &str = "databaseId,name,displayTitle,headBranch,headSha,event,\
                                status,conclusion,number,createdAt,updatedAt,url,workflowName,jobs";

/// GitHub Actions via the `gh` CLI — reuses the user's existing `gh` auth
/// (keyring, enterprise hosts) instead of threading a token.
pub struct GithubCi;

impl thegn_core::seam::Probe for GithubCi {
    fn probe(&self) -> thegn_core::seam::ProbeReport {
        thegn_core::seam::ProbeReport::new(
            "ci",
            "github",
            crate::seam::registry::binary_availability("gh"),
        )
        .with_caps(&self.caps())
        .note("GitHub Actions via `gh` (reuses `gh auth`)")
    }
}

impl CiProvider for GithubCi {
    fn system(&self) -> CiSystem {
        CiSystem::GithubActions
    }
    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError> {
        let limit_s = limit.max(1).to_string();
        let mut args = vec!["run", "list", "--limit", &limit_s, "--json", GH_RUN_FIELDS];
        if let Some(b) = branch {
            validate_branch_ref(b)?;
            args.push("--branch");
            args.push(b);
        }
        let json = run_cli(&mut loc.gh_command(&args))?;
        let (runs, discarded_rows) = parse_gh_runs_with_discarded(&json);
        if discarded_rows > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows, "discarded malformed GitHub run rows");
        }
        Ok(CiRunList {
            runs,
            discarded_rows,
        })
    }

    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError> {
        let args = github_run_detail_argv(run_id)?;
        let argv: Vec<_> = args.iter().map(String::as_str).collect();
        let json = run_cli(&mut loc.gh_command(&argv))?;
        let value: serde_json::Value = serde_json::from_str(&json)
            .map_err(|error| CiError::Other(format!("invalid provider run response: {error}")))?;
        let observed = value
            .get("databaseId")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let run = parse_gh_run_detail(&json)
            .ok_or_else(|| CiError::Other(format!("invalid provider run identifier {observed}")))?;
        let discarded_jobs = gh_jobs_with_discarded(value.get("jobs")).1;
        if discarded_jobs > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = discarded_jobs, "discarded malformed GitHub job rows");
        }
        validate_ci_id(&run.id)?;
        if run.id != run_id {
            return Err(CiError::Other(format!(
                "provider returned run id {:?} for requested {run_id:?}",
                run.id
            )));
        }
        Ok(CiRunDetail {
            run,
            discarded_jobs,
        })
    }

    fn logs(&self, loc: &GitLoc, _run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        // `gh run view --job <id> --log` (job ids are globally addressable).
        validate_ci_id(_run_id)?;
        validate_ci_id(job_id)?;
        run_bounded_log(&mut loc.gh_command(&["run", "view", "--job", job_id, "--log"]))
    }

    fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        let json =
            run_cli(&mut loc.gh_command(&["workflow", "list", "--json", "id,name,path,state"]))?;
        Ok(parse_gh_workflows(&json))
    }

    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        let args = github_workflow_argv(workflow, inputs)?;
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(&mut loc.gh_command(&argv)).map(|_| ())
    }

    fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError> {
        let args = github_rerun_argv(run_id, scope)?;
        let argv: Vec<_> = args.iter().map(String::as_str).collect();
        run_cli(&mut loc.gh_command(&argv)).map(|_| ())
    }

    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        let args = github_cancel_argv(run_id)?;
        let argv: Vec<_> = args.iter().map(String::as_str).collect();
        run_cli(&mut loc.gh_command(&argv)).map(|_| ())
    }

    fn caps(&self) -> CiCaps {
        CiCaps {
            logs: true,
            steps: true,
            trigger: true,
            rerun: true,
            rerun_failed: true,
            cancel: true,
        }
    }
}

/// Parse `gh run list --json …` (an array) into runs.
pub fn parse_gh_runs(json: &str) -> Vec<CiRun> {
    parse_gh_runs_with_discarded(json).0
}

pub fn parse_gh_runs_with_discarded(json: &str) -> (Vec<CiRun>, usize) {
    let Some(rows) = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
    else {
        return (Vec::new(), 0);
    };
    parse_provider_row(&rows, gh_run_from_value)
}

/// Parse `gh run view <id> --json …` (a single object, with `jobs`).
pub fn parse_gh_run_detail(json: &str) -> Option<CiRun> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if !v.is_object() {
        return None;
    }
    gh_run_from_value(&v)
}

fn gh_run_from_value(v: &serde_json::Value) -> Option<CiRun> {
    let id = json_id(v, "databaseId")?;
    let status = nonempty(v, "status").unwrap_or_default();
    let conclusion = nonempty(v, "conclusion");
    let completed = status.eq_ignore_ascii_case("completed");
    Some(CiRun {
        id,
        name: nonempty(v, "workflowName")
            .or_else(|| nonempty(v, "name"))
            .unwrap_or_default(),
        title: nonempty(v, "displayTitle").unwrap_or_default(),
        event: nonempty(v, "event").unwrap_or_default(),
        branch: nonempty(v, "headBranch").unwrap_or_default(),
        sha: nonempty(v, "headSha").unwrap_or_default(),
        state: CiState::from_github(&status, conclusion.as_deref()),
        status_raw: status,
        conclusion_raw: conclusion,
        url: nonempty(v, "url").unwrap_or_default(),
        run_number: v.get("number").and_then(serde_json::Value::as_u64),
        started_at: nonempty(v, "createdAt"),
        finished_at: completed.then(|| nonempty(v, "updatedAt")).flatten(),
        jobs: gh_jobs_with_discarded(v.get("jobs")).0,
    })
}

fn gh_jobs_with_discarded(value: Option<&serde_json::Value>) -> (Vec<CiJob>, usize) {
    value
        .and_then(serde_json::Value::as_array)
        .map(|rows| parse_provider_row(rows, gh_job_from_value))
        .unwrap_or_default()
}

fn gh_job_from_value(v: &serde_json::Value) -> Option<CiJob> {
    let id = json_id(v, "databaseId")?;
    let status = nonempty(v, "status").unwrap_or_default();
    let conclusion = nonempty(v, "conclusion");
    Some(CiJob {
        id,
        name: nonempty(v, "name").unwrap_or_default(),
        state: CiState::from_github(&status, conclusion.as_deref()),
        url: nonempty(v, "url"),
        started_at: nonempty(v, "startedAt"),
        finished_at: nonempty(v, "completedAt"),
        steps: v
            .get("steps")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().map(gh_step_from_value).collect())
            .unwrap_or_default(),
    })
}

fn gh_step_from_value(v: &serde_json::Value) -> CiStep {
    let status = nonempty(v, "status").unwrap_or_default();
    let conclusion = nonempty(v, "conclusion");
    CiStep {
        name: nonempty(v, "name").unwrap_or_default(),
        number: v.get("number").and_then(serde_json::Value::as_u64),
        state: CiState::from_github(&status, conclusion.as_deref()),
        started_at: nonempty(v, "startedAt"),
        finished_at: nonempty(v, "completedAt"),
    }
}

/// Parse `gh workflow list --json id,name,path,state`.
pub fn parse_gh_workflows(json: &str) -> Vec<CiWorkflow> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    Some(CiWorkflow {
                        id: json_id(v, "id")?,
                        name: nonempty(v, "name").unwrap_or_default(),
                        path: nonempty(v, "path").unwrap_or_default(),
                        // `gh workflow list` doesn't expose the trigger set; treat
                        // active workflows as dispatchable (trigger degrades with a
                        // readable error if a given one isn't). Input prompting +
                        // accurate dispatchability come in Phase B.
                        dispatchable: nonempty(v, "state")
                            .map(|s| s.eq_ignore_ascii_case("active"))
                            .unwrap_or(true),
                        inputs: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// === GitLab CI (glab + GitLab API) ========================================

/// GitLab CI via `glab api`. A configured token is resolved by the injected
/// broker and supplied only through `GITLAB_TOKEN`; when absent, `glab` may use
/// its own credential store. GitLab has no per-job "steps", so
/// [`CiJob::steps`] stays empty.
#[derive(Default)]
pub struct GitlabCi {
    token: Option<String>,
    host: Option<String>,
}

impl thegn_core::seam::Probe for GitlabCi {
    fn probe(&self) -> thegn_core::seam::ProbeReport {
        thegn_core::seam::ProbeReport::new(
            "ci",
            "gitlab",
            crate::seam::registry::binary_availability("glab"),
        )
        .with_caps(&self.caps())
        .note("GitLab CI via `glab api`")
    }
}

impl GitlabCi {
    fn from_config(cfg: &CiConfig) -> Self {
        Self {
            token: crate::secret::resolve(&cfg.gitlab.token, BareAs::Literal, "ci:gitlab"),
            host: (!cfg.gitlab.host.trim().is_empty()).then(|| cfg.gitlab.host.trim().to_string()),
        }
    }

    /// Construct a `glab` command with credentials in the child environment,
    /// never in argv. Keeping this at one chokepoint prevents a new operation
    /// from accidentally falling back to an unaudited/config-ignored path.
    fn command(&self, loc: &GitLoc, remote: &GitlabRemote, args: &[&str]) -> Command {
        let mut cmd = loc.cli_command("glab", args);
        if let Some(token) = &self.token {
            cmd.env("GITLAB_TOKEN", token);
        }
        cmd.env("GITLAB_HOST", &remote.host);
        cmd
    }

    /// Resolve origin once for an operation. The resulting host and project
    /// are reused for both the credential-bearing command and its endpoint.
    fn remote(&self, loc: &GitLoc) -> Result<GitlabRemote, CiError> {
        let remote = snapshot_gitlab_remote(|| origin_url(loc))?;
        if let Some(configured_host) = &self.host {
            if normalize_gitlab_host(configured_host).as_deref() != Some(remote.host.as_str()) {
                return Err(CiError::Other(
                    "GitLab remote host does not match configured host".into(),
                ));
            }
        }
        Ok(remote)
    }

    fn project_seg(remote: &GitlabRemote) -> String {
        encode_project_segment(&remote.project)
    }
}

impl CiProvider for GitlabCi {
    fn system(&self) -> CiSystem {
        CiSystem::GitlabCi
    }
    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError> {
        let remote = self.remote(loc)?;
        let project_segment = Self::project_seg(&remote);
        let endpoint = gitlab_pipelines_endpoint(&project_segment, branch, limit)?;
        let json = run_cli(&mut self.command(loc, &remote, &["api", &endpoint]))?;
        let (runs, discarded) = parse_gitlab_pipelines_with_discarded(&json);
        if discarded > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = discarded, "discarded malformed GitLab pipeline rows");
        }
        Ok(CiRunList {
            runs,
            discarded_rows: discarded,
        })
    }

    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError> {
        validate_ci_id(run_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        // Pipeline header + its jobs (two calls; the jobs carry the states).
        let run_endpoint = gitlab_run_endpoint(&proj, run_id, "")?;
        let jobs_endpoint = gitlab_run_endpoint(&proj, run_id, "jobs")?;
        let pipe_json = run_cli(&mut self.command(loc, &remote, &["api", &run_endpoint]))?;
        let jobs_json = run_cli(&mut self.command(loc, &remote, &["api", &jobs_endpoint]))?;
        let value: serde_json::Value = serde_json::from_str(&pipe_json).map_err(|error| {
            CiError::Other(format!("invalid provider pipeline response: {error}"))
        })?;
        let observed = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let mut run = parse_gitlab_pipeline_detail(&pipe_json).ok_or_else(|| {
            CiError::Other(format!("invalid provider pipeline identifier {observed}"))
        })?;
        validate_ci_id(&run.id)?;
        if run.id != run_id {
            return Err(CiError::Other(format!(
                "provider returned run id {:?} for requested {run_id:?}",
                run.id
            )));
        }
        let (jobs, discarded_jobs) = parse_gitlab_jobs_with_discarded(&jobs_json);
        if discarded_jobs > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = discarded_jobs, "discarded malformed GitLab job rows");
        }
        run.jobs = jobs;
        Ok(CiRunDetail {
            run,
            discarded_jobs,
        })
    }

    fn logs(&self, loc: &GitLoc, _run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        validate_ci_id(_run_id)?;
        validate_ci_id(job_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        let endpoint = gitlab_job_trace_endpoint(&proj, job_id)?;
        run_bounded_log(&mut self.command(loc, &remote, &["api", &endpoint]))
    }

    fn workflows(&self, _loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        // GitLab has one pipeline definition (`.gitlab-ci.yml`), not a set of
        // dispatchable workflows; manual-trigger support is Phase B.
        Ok(Vec::new())
    }

    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        validate_workflow_selector(workflow)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        let mut args = vec!["api".to_string(), "-X".into(), "POST".into()];
        args.push(format!("projects/{proj}/pipeline"));
        for (k, v) in inputs {
            args.push("-f".into());
            args.push(format!("{k}={v}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(&mut self.command(loc, &remote, &argv)).map(|_| ())
    }

    fn rerun(&self, loc: &GitLoc, run_id: &str, _scope: RerunScope) -> Result<(), CiError> {
        validate_ci_id(run_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        // GitLab: `retry` re-runs failed jobs; a fresh full run isn't a single
        // call, so both scopes map to retry (it's the closest primitive).
        let endpoint = gitlab_run_endpoint(&proj, run_id, "retry")?;
        run_cli(&mut self.command(loc, &remote, &["api", "-X", "POST", &endpoint])).map(|_| ())
    }

    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        validate_ci_id(run_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        let endpoint = gitlab_run_endpoint(&proj, run_id, "cancel")?;
        run_cli(&mut self.command(loc, &remote, &["api", "-X", "POST", &endpoint])).map(|_| ())
    }

    fn caps(&self) -> CiCaps {
        CiCaps {
            logs: true,
            steps: false,
            trigger: true,
            rerun: true,
            // Pipeline `retry` has no failed-only scope — see `rerun` above.
            rerun_failed: false,
            cancel: true,
        }
    }
}

/// Extract a GitLab project path (`group/sub/repo`, keeping subgroups) from a
/// git remote URL. Pure, tested.
pub fn gitlab_project_path(url: &str) -> Option<String> {
    parse_gitlab_remote(url).map(|remote| remote.project)
}

#[derive(Debug)]
struct GitlabRemote {
    host: String,
    project: String,
}

fn parse_gitlab_remote_checked(raw: &str) -> Result<GitlabRemote, CiError> {
    parse_gitlab_remote(raw)
        .ok_or_else(|| CiError::Other("invalid GitLab remote authority or project path".into()))
}

fn snapshot_gitlab_remote(
    read_origin: impl FnOnce() -> Option<String>,
) -> Result<GitlabRemote, CiError> {
    let raw = read_origin().ok_or(CiError::NotConfigured)?;
    parse_gitlab_remote_checked(&raw)
}

fn normalize_gitlab_host(raw: &str) -> Option<String> {
    if raw.contains("://") {
        let url = url::Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let host = url.host_str()?;
        return Some(match url.port() {
            Some(port) => format!("{host}:{port}").to_ascii_lowercase(),
            None => host.to_ascii_lowercase(),
        });
    }
    let host = raw.trim().trim_matches('/').to_ascii_lowercase();
    (!host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':')))
    .then_some(host)
}

/// Accept only ordinary SSH/HTTP(S) git remotes with an unambiguous host and
/// plain project path. Encoded paths and URL decorations are rejected before
/// constructing an authenticated `glab api` request.
fn parse_gitlab_remote(raw: &str) -> Option<GitlabRemote> {
    let raw = raw.trim();
    let (host, path) = if raw.contains("://") {
        if !raw_url_path_is_unambiguous(raw) {
            return None;
        }
        let parsed = url::Url::parse(raw).ok()?;
        if !matches!(parsed.scheme(), "https" | "http" | "ssh")
            || parsed.host_str().is_none()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || (parsed.scheme() != "ssh" && !parsed.username().is_empty())
            || (parsed.scheme() == "ssh"
                && !parsed.username().is_empty()
                && parsed.username() != "git")
            || parsed.password().is_some()
        {
            return None;
        }
        let host = match parsed.port() {
            Some(port) => format!("{}:{port}", parsed.host_str()?),
            None => parsed.host_str()?.to_string(),
        };
        (
            host.to_ascii_lowercase(),
            parsed.path().trim_start_matches('/').to_string(),
        )
    } else {
        // SCP-like SSH: optional conventional username, a DNS/IPv4 host, and
        // a colon before the path. IPv6 must use a real ssh:// URL.
        let (authority, path) = raw.split_once(':')?;
        let (username, host) = authority
            .rsplit_once('@')
            .map_or((None, authority), |(username, host)| (Some(username), host));
        if username.is_some_and(|user| {
            user.is_empty()
                || !user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }) {
            return None;
        }
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return None;
        }
        (host.to_ascii_lowercase(), path.to_string())
    };
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let segments: Vec<_> = path.split('/').collect();
    if segments.len() < 2
        || segments.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || part.starts_with('.')
                || part
                    .bytes()
                    .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'.' | b'_' | b'-'))
        })
    {
        return None;
    }
    Some(GitlabRemote {
        host,
        project: segments.join("/"),
    })
}

/// Check path text before `url::Url` can normalize dot segments or decode its
/// interpretation of delimiters. Encoded path bytes are rejected entirely:
/// project components have a deliberately narrow literal grammar.
fn raw_url_path_is_unambiguous(raw: &str) -> bool {
    let Some((_, authority_and_path)) = raw.split_once("://") else {
        return false;
    };
    let authority_end = authority_and_path
        .find(|ch| matches!(ch, '/' | '?' | '#'))
        .unwrap_or(authority_and_path.len());
    let suffix = &authority_and_path[authority_end..];
    let path_end = suffix
        .find(|ch| matches!(ch, '?' | '#'))
        .unwrap_or(suffix.len());
    let path = &suffix[..path_end];
    if !path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return false;
    }
    path.split('/')
        .filter(|component| !component.is_empty())
        .all(|component| component != "." && component != "..")
}

fn encode_project_segment(project: &str) -> String {
    let mut encoded = String::new();
    for byte in project.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Parse `GET projects/:id/pipelines` (array) into runs.
pub fn parse_gitlab_pipelines(json: &str) -> Vec<CiRun> {
    parse_gitlab_pipelines_with_discarded(json).0
}

pub fn parse_gitlab_pipelines_with_discarded(json: &str) -> (Vec<CiRun>, usize) {
    let Some(rows) = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
    else {
        return (Vec::new(), 0);
    };
    parse_provider_row(&rows, gitlab_pipeline_from_value)
}

/// Parse `GET projects/:id/pipelines/:id` (single object) into a run header.
pub fn parse_gitlab_pipeline_detail(json: &str) -> Option<CiRun> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if !v.is_object() {
        return None;
    }
    gitlab_pipeline_from_value(&v)
}

fn gitlab_pipeline_from_value(v: &serde_json::Value) -> Option<CiRun> {
    let id = json_id(v, "id")?;
    let status = nonempty(v, "status").unwrap_or_default();
    let terminal = CiState::from_gitlab(&status).is_terminal();
    Some(CiRun {
        id: id.clone(),
        name: nonempty(v, "name").unwrap_or_else(|| format!("pipeline #{id}")),
        title: nonempty(v, "ref").unwrap_or_default(),
        event: nonempty(v, "source").unwrap_or_default(),
        branch: nonempty(v, "ref").unwrap_or_default(),
        sha: nonempty(v, "sha").unwrap_or_default(),
        state: CiState::from_gitlab(&status),
        status_raw: status,
        conclusion_raw: None,
        url: nonempty(v, "web_url").unwrap_or_default(),
        run_number: v.get("iid").and_then(serde_json::Value::as_u64),
        started_at: nonempty(v, "created_at"),
        finished_at: terminal.then(|| nonempty(v, "updated_at")).flatten(),
        jobs: Vec::new(),
    })
}

/// Parse `GET projects/:id/pipelines/:id/jobs` (array) into jobs.
pub fn parse_gitlab_jobs(json: &str) -> Vec<CiJob> {
    parse_gitlab_jobs_with_discarded(json).0
}

pub fn parse_gitlab_jobs_with_discarded(json: &str) -> (Vec<CiJob>, usize) {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            parse_provider_row(&arr, |v| {
                let id = json_id(v, "id")?;
                let status = nonempty(v, "status").unwrap_or_default();
                Some(CiJob {
                    id,
                    name: nonempty(v, "name").unwrap_or_default(),
                    state: CiState::from_gitlab(&status),
                    url: nonempty(v, "web_url"),
                    started_at: nonempty(v, "started_at"),
                    finished_at: nonempty(v, "finished_at"),
                    steps: Vec::new(),
                })
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_host_to_system() {
        assert_eq!(
            system_from_remote_host("git@github.com:o/r.git"),
            Some(CiSystem::GithubActions)
        );
        assert_eq!(
            system_from_remote_host("https://gitlab.com/g/s/r.git"),
            Some(CiSystem::GitlabCi)
        );
        assert_eq!(
            system_from_remote_host("https://example.test/github.com/g/r.git"),
            None
        );
        assert_eq!(
            system_from_remote_host("https://github.com.evil.test/g/r.git"),
            None
        );
        assert_eq!(system_from_remote_host("git@bitbucket.org:o/r.git"), None);
    }

    #[test]
    fn gitlab_path_parsing() {
        assert_eq!(
            gitlab_project_path("git@gitlab.com:group/sub/repo.git").as_deref(),
            Some("group/sub/repo")
        );
        assert_eq!(
            gitlab_project_path("https://gitlab.example.com/group/repo").as_deref(),
            Some("group/repo")
        );
        assert_eq!(
            gitlab_project_path("ssh://git@gitlab.example.com:2222/group/repo.git").as_deref(),
            Some("group/repo")
        );
        let remote = parse_gitlab_remote("https://gitlab.example.com:8443/group/sub/repo.git")
            .expect("parsed GitLab authority and project");
        assert_eq!(remote.host, "gitlab.example.com:8443");
        assert_eq!(remote.project, "group/sub/repo");
        // single-segment (no group) → None (GitLab projects always have a group)
        assert_eq!(gitlab_project_path("https://gitlab.com/repo.git"), None);
        for invalid in [
            "https://gitlab.com/group/repo?next=other",
            "https://gitlab.com/group/repo#fragment",
            "https://user@gitlab.com/group/repo",
            "https://user:token@gitlab.com/group/repo",
            "https://gitlab.com/group%2Frepo/sub",
            "https://gitlab.com/group\\repo",
            // URL parsing normalizes these dot segments unless the raw path is
            // checked before parsing; remotes must not silently retarget.
            "https://gitlab.com/../group/repo",
            "https://gitlab.com/%2e%2e/group/repo",
            "git@gitlab.com:group/../repo.git",
            "git@gitlab.com:group//repo.git",
            "git@gitlab.com:group/repo#fragment",
        ] {
            assert_eq!(gitlab_project_path(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn gitlab_remote_errors_never_echo_credentials_or_remote_text() {
        let secret_remote = "https://user:fixture-token@gitlab.example.test/group/repo";
        let error = parse_gitlab_remote_checked(secret_remote)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-token"));
        assert!(!error.contains("user"));
        assert_eq!(error, "invalid GitLab remote authority or project path");
    }

    #[test]
    fn gitlab_operation_takes_one_remote_snapshot_for_host_and_project() {
        let mut reads = 0;
        let remote = snapshot_gitlab_remote(|| {
            reads += 1;
            Some("https://gitlab.example.test/group/repo".into())
        })
        .unwrap();
        assert_eq!(reads, 1);

        let endpoint =
            gitlab_pipelines_endpoint(&GitlabCi::project_seg(&remote), None, 10).unwrap();
        let client = GitlabCi {
            token: Some("fixture-token".into()),
            host: None,
        };
        let loc = GitLoc::for_worktree(std::path::Path::new("."));
        let command = client.command(&loc, &remote, &["api", &endpoint]);
        assert_eq!(endpoint, "projects/group%2Frepo/pipelines?per_page=10");
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| key == "GITLAB_HOST")
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
                .as_deref(),
            Some("gitlab.example.test")
        );
    }

    #[test]
    fn ci_ids_have_one_bounded_canonical_spelling() {
        for valid in ["1", "9", "12345678901234567890"] {
            assert_eq!(validate_ci_id(valid), Ok(valid));
        }
        for invalid in [
            "",
            "0",
            "01",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1.0",
            "1e5",
            "1/2",
            "1..2",
            "1%2f3",
            "1#x",
            "1\n",
            "1\t",
            "--help",
            "18446744073709551616",
            "123456789012345678901",
        ] {
            let err = validate_ci_id(invalid).unwrap_err();
            // The message quotes the input with `{:?}`, which ESCAPES control
            // characters — deliberately, so a rejected id carrying a newline or a
            // tab cannot inject them into a log line or a terminal. So assert the
            // escaped spelling; asserting the raw bytes would demand the unsafe
            // behaviour for the `"1\n"` and `"1\t"` cases.
            assert!(
                err.to_string().contains(&format!("{invalid:?}")),
                "{invalid:?}: {err}"
            );
        }
    }

    #[test]
    fn provider_json_ids_are_numeric_and_bad_list_rows_are_counted() {
        let payload = r#"[
          {"databaseId":1,"status":"completed"},
          {"databaseId":"2","status":"completed"},
          {"databaseId":1.0,"status":"completed"},
          {"databaseId":-1,"status":"completed"},
          {"databaseId":18446744073709551616,"status":"completed"}
        ]"#;
        let (runs, discarded) = parse_gh_runs_with_discarded(payload);
        assert_eq!(
            runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
            ["1"]
        );
        assert_eq!(discarded, 4);

        let jobs = r#"[{"id":7},{"id":"8"},{"id":1e5}]"#;
        let (jobs, discarded) = parse_gitlab_jobs_with_discarded(jobs);
        assert_eq!(
            jobs.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
            ["7"]
        );
        assert_eq!(discarded, 2);
        let rows: serde_json::Value =
            serde_json::from_str(r#"[{"databaseId":7},{"databaseId":"8"},{"databaseId":1e5}]"#)
                .unwrap();
        let (parsed, discarded) = gh_jobs_with_discarded(Some(&rows));
        assert_eq!(parsed.len(), 1);
        assert_eq!(discarded, 2);
    }

    #[test]
    fn workflow_argv_and_gitlab_query_preserve_target_identity() {
        let inputs = vec![("environment".into(), "staging".into())];
        assert_eq!(
            github_workflow_argv("Release candidate (manual).yml", &inputs).unwrap(),
            [
                "workflow",
                "run",
                "-f",
                "environment=staging",
                "--",
                "Release candidate (manual).yml"
            ]
        );
        for invalid in ["--help", "../workflow.yml", "a//b", "bad\nname"] {
            assert!(validate_workflow_selector(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            gitlab_pipelines_endpoint("group%2Fsub%2Frepo", Some("feature&status=success#x%"), 30)
                .unwrap(),
            "projects/group%2Fsub%2Frepo/pipelines?per_page=30&ref=feature%26status%3Dsuccess%23x%25"
        );
        assert!(gitlab_pipelines_endpoint("group%2Frepo", Some("bad ref"), 30).is_err());
        // Match git-check-ref-format's rule that a ref component cannot start
        // with a dot. This guards validation independently of query encoding.
        for invalid in [".hidden", "feature/.hidden"] {
            assert!(validate_branch_ref(invalid).is_err(), "{invalid:?}");
        }
        for invalid in [
            "feature/..hidden",
            "feature/trailing./part",
            "feature/name.lock",
            "feature/@{upstream}",
            "feature/name\\part",
            "feature/name\u{7f}part",
            "@",
        ] {
            assert!(validate_branch_ref(invalid).is_err(), "{invalid:?}");
        }
        for valid in [
            "feature&status=success",
            "release#candidate",
            "percent%name",
        ] {
            assert_eq!(validate_branch_ref(valid), Ok(valid));
        }
        assert_eq!(
            github_run_detail_argv("123").unwrap(),
            ["run", "view", "--json", GH_DETAIL_FIELDS, "--", "123"]
        );
        assert_eq!(
            github_rerun_argv("123", RerunScope::Failed).unwrap(),
            ["run", "rerun", "--failed", "--", "123"]
        );
        assert_eq!(
            github_cancel_argv("123").unwrap(),
            ["run", "cancel", "--", "123"]
        );
    }

    #[test]
    fn rejected_identifiers_never_reach_request_construction() {
        let mut authenticated_requests = 0;
        for id in ["--help", "01", "1e5", "1#x", "1%2f2"] {
            if github_cancel_argv(id).is_ok() {
                authenticated_requests += 1;
            }
            assert!(github_cancel_argv(id).is_err(), "{id:?}");
            if gitlab_run_endpoint("g%2Fr", id, "retry").is_ok() {
                authenticated_requests += 1;
            }
            assert!(gitlab_run_endpoint("g%2Fr", id, "retry").is_err(), "{id:?}");
        }
        if gitlab_job_trace_endpoint("g%2Fr", "--help").is_ok() {
            authenticated_requests += 1;
        }
        assert!(gitlab_job_trace_endpoint("g%2Fr", "--help").is_err());
        if github_workflow_argv("--help", &[]).is_ok() {
            authenticated_requests += 1;
        }
        assert!(github_workflow_argv("--help", &[]).is_err());
        assert_eq!(authenticated_requests, 0);

        assert_eq!(
            gitlab_run_endpoint("g%2Fr", "123", "retry").unwrap(),
            "projects/g%2Fr/pipelines/123/retry"
        );
        assert_eq!(
            gitlab_job_trace_endpoint("g%2Fr", "456").unwrap(),
            "projects/g%2Fr/jobs/456/trace"
        );
    }

    #[test]
    fn gitlab_config_token_is_in_child_environment_never_argv() {
        let mut cfg = CiConfig::default();
        cfg.gitlab.token = "fixture-secret-token".into();
        cfg.gitlab.host = "gitlab.example.test".into();
        let client = GitlabCi::from_config(&cfg);
        let loc = GitLoc::for_worktree(std::path::Path::new("."));
        let remote = GitlabRemote {
            host: "gitlab.example.test".into(),
            project: "g/r".into(),
        };
        let command = client.command(&loc, &remote, &["api", "projects/g%2Fr/pipelines"]);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(!args.iter().any(|arg| arg.contains("fixture-secret-token")));
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env.get("GITLAB_TOKEN").and_then(Option::as_deref),
            Some("fixture-secret-token")
        );
        assert_eq!(
            env.get("GITLAB_HOST").and_then(Option::as_deref),
            Some("gitlab.example.test")
        );
    }

    #[test]
    fn parse_gh_run_list() {
        let json = r#"[
          {"databaseId":123,"name":"CI","workflowName":"CI","displayTitle":"fix: thing",
           "headBranch":"main","headSha":"abc123","event":"push","status":"completed",
           "conclusion":"failure","number":42,"createdAt":"2026-06-25T10:00:00Z",
           "updatedAt":"2026-06-25T10:05:00Z","url":"https://gh/run/123"},
          {"databaseId":124,"workflowName":"CI","displayTitle":"wip","headBranch":"main",
           "headSha":"def","event":"push","status":"in_progress","conclusion":"",
           "number":43,"createdAt":"2026-06-25T11:00:00Z","updatedAt":"2026-06-25T11:01:00Z",
           "url":"https://gh/run/124"}
        ]"#;
        let runs = parse_gh_runs(json);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "123");
        assert_eq!(runs[0].name, "CI");
        assert_eq!(runs[0].state, CiState::Fail);
        assert_eq!(runs[0].finished_at.as_deref(), Some("2026-06-25T10:05:00Z"));
        assert_eq!(runs[0].run_number, Some(42));
        // in-flight run: no finished_at, Running
        assert_eq!(runs[1].state, CiState::Running);
        assert_eq!(runs[1].finished_at, None);
        assert!(parse_gh_runs("not json").is_empty());
        assert!(parse_gh_runs("{}").is_empty());
    }

    #[test]
    fn parse_gh_detail_with_jobs_and_steps() {
        let json = r#"{
          "databaseId":123,"workflowName":"CI","displayTitle":"fix","headBranch":"main",
          "headSha":"abc","event":"push","status":"completed","conclusion":"failure",
          "number":42,"createdAt":"2026-06-25T10:00:00Z","updatedAt":"2026-06-25T10:05:00Z",
          "url":"https://gh/run/123",
          "jobs":[
            {"databaseId":1,"name":"build","status":"completed","conclusion":"success",
             "startedAt":"2026-06-25T10:00:10Z","completedAt":"2026-06-25T10:02:00Z",
             "url":"https://gh/job/1",
             "steps":[{"name":"Checkout","number":1,"status":"completed","conclusion":"success"}]},
            {"databaseId":2,"name":"test","status":"completed","conclusion":"failure",
             "startedAt":"2026-06-25T10:02:00Z","completedAt":"2026-06-25T10:05:00Z",
             "url":"https://gh/job/2","steps":[]}
          ]
        }"#;
        let run = parse_gh_run_detail(json).unwrap();
        assert_eq!(run.id, "123");
        assert_eq!(run.jobs.len(), 2);
        assert_eq!(run.jobs[0].name, "build");
        assert_eq!(run.jobs[0].state, CiState::Pass);
        assert_eq!(run.jobs[0].steps.len(), 1);
        assert_eq!(run.jobs[0].steps[0].state, CiState::Pass);
        assert_eq!(run.jobs[1].state, CiState::Fail);
        assert!(parse_gh_run_detail("[]").is_none());
    }

    #[test]
    fn parse_gh_workflow_list() {
        let json = r#"[
          {"id":1,"name":"CI","path":".github/workflows/ci.yml","state":"active"},
          {"id":2,"name":"Stale","path":".github/workflows/stale.yml","state":"disabled_manually"}
        ]"#;
        let wfs = parse_gh_workflows(json);
        assert_eq!(wfs.len(), 2);
        assert_eq!(wfs[0].id, "1");
        assert!(wfs[0].dispatchable);
        assert!(!wfs[1].dispatchable);
    }

    #[test]
    fn parse_gitlab_pipeline_list_and_jobs() {
        let pipelines = r#"[
          {"id":1001,"iid":7,"ref":"main","sha":"abc","status":"failed","source":"push",
           "web_url":"https://gl/p/1001","created_at":"2026-06-25T10:00:00Z",
           "updated_at":"2026-06-25T10:06:00Z"},
          {"id":1002,"iid":8,"ref":"main","sha":"def","status":"running","source":"push",
           "web_url":"https://gl/p/1002","created_at":"2026-06-25T11:00:00Z",
           "updated_at":"2026-06-25T11:01:00Z"}
        ]"#;
        let runs = parse_gitlab_pipelines(pipelines);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "1001");
        assert_eq!(runs[0].state, CiState::Fail);
        assert_eq!(runs[0].name, "pipeline #1001");
        assert_eq!(runs[0].run_number, Some(7));
        assert_eq!(runs[0].finished_at.as_deref(), Some("2026-06-25T10:06:00Z"));
        // running pipeline → not terminal → no finished_at
        assert_eq!(runs[1].state, CiState::Running);
        assert_eq!(runs[1].finished_at, None);

        let detail = r#"{"id":1001,"iid":7,"ref":"main","sha":"abc","status":"failed",
          "source":"push","web_url":"https://gl/p/1001","created_at":"2026-06-25T10:00:00Z",
          "updated_at":"2026-06-25T10:06:00Z"}"#;
        assert_eq!(parse_gitlab_pipeline_detail(detail).unwrap().id, "1001");

        let jobs = r#"[
          {"id":5001,"name":"build","stage":"build","status":"success",
           "started_at":"2026-06-25T10:00:10Z","finished_at":"2026-06-25T10:02:00Z",
           "web_url":"https://gl/j/5001"},
          {"id":5002,"name":"test","stage":"test","status":"failed",
           "started_at":"2026-06-25T10:02:00Z","finished_at":"2026-06-25T10:06:00Z",
           "web_url":"https://gl/j/5002"}
        ]"#;
        let jl = parse_gitlab_jobs(jobs);
        assert_eq!(jl.len(), 2);
        assert_eq!(jl[0].name, "build");
        assert_eq!(jl[0].state, CiState::Pass);
        assert!(jl[0].steps.is_empty());
        assert_eq!(jl[1].state, CiState::Fail);
        assert!(parse_gitlab_jobs("nope").is_empty());
    }

    #[test]
    fn caps_are_set() {
        assert!(GithubCi.caps().steps);
        assert!(!GitlabCi::default().caps().steps);
        assert!(GithubCi.caps().trigger);
        assert!(GitlabCi::default().caps().cancel);
        // GitLab's pipeline `retry` can't scope to failed jobs; offering the
        // distinction anyway would silently retry everything.
        assert!(GithubCi.caps().rerun_failed);
        assert!(!GitlabCi::default().caps().rerun_failed);
    }
}

#[cfg(test)]
mod kind_coverage_tests {
    use super::*;

    /// Every `CiProviderKind` value either builds a client or is `reserved`
    /// — the provider-seams rule, pinned for this seam. `auto`/`none` are
    /// selectors rather than providers and resolve per worktree, so they are
    /// exercised by `resolve_system` tests instead.
    #[test]
    fn every_ci_kind_is_implemented_or_reserved() {
        crate::seam::kind_coverage(|k: CiProviderKind| match k {
            CiProviderKind::Auto | CiProviderKind::None => Some(None),
            explicit => system_for_kind(explicit)
                .and_then(client_for_system)
                .map(Some),
        });
    }
}
