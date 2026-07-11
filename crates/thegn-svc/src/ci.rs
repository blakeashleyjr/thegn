//! CI/CD provider seam (AV group). The cross-provider sibling of [`crate::gh`]:
//! a [`CiProvider`] trait normalizing run history / job-step drilldown / logs /
//! trigger·rerun·cancel onto thegn-core's [`thegn_core::ci`] model, with
//! per-provider impls that degrade native→CLI→error just like `GhNative`→`CliGh`
//! ("a gap is slower or unavailable, never broken").
//!
//! Phase A ships GitHub Actions (via the `gh` CLI, reusing the user's `gh`
//! auth) and GitLab CI (via `glab` + the GitLab API). Both are exercised
//! through static dispatch ([`CiClient`]), so the object-unsafe `async fn` in
//! the trait is never made into a `dyn` — the same pattern `GhNative` uses.
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

/// A CI/CD backend for one provider. Read methods first; mutations are
/// capability-gated via [`Self::caps`] so a provider can decline what it can't do.
#[allow(async_fn_in_trait)]
pub trait CiProvider: Send + Sync {
    /// Recent runs (newest first), optionally filtered to `branch`.
    async fn runs(
        &self,
        loc: &GitLoc,
        branch: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CiRun>, CiError>;

    /// One run with its jobs (and steps, where the provider exposes them).
    async fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRun, CiError>;

    /// A job's log text ("why did it fail"). `run_id` is needed by providers
    /// whose job ids aren't globally addressable (GitLab); GitHub ignores it.
    async fn logs(&self, loc: &GitLoc, run_id: &str, job_id: &str) -> Result<CiLog, CiError>;

    /// Dispatchable workflow definitions (drives the trigger prompt).
    async fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError>;

    /// Trigger a workflow with `inputs` (`workflow_dispatch`). Phase B.
    async fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError>;

    /// Re-run a run (all jobs or only the failed ones). Phase B.
    async fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError>;

    /// Cancel an in-flight run. Phase B.
    async fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError>;

    fn caps(&self) -> CiCaps;
}

// === provider selection ====================================================

/// Pick the concrete CI provider for a worktree from `[ci]` config, resolving
/// `"auto"` by sniffing the git remote then falling back to detected CI files.
/// `None` when CI is disabled, undetected, or the resolved system isn't
/// implemented yet (Phase A = GitHub + GitLab only) — the caller shows a note.
pub fn provider_for(loc: &GitLoc, cfg: &CiConfig) -> Option<CiClient> {
    let system = resolve_system(loc, cfg)?;
    match system {
        CiSystem::GithubActions => Some(CiClient::Github(GithubCi)),
        CiSystem::GitlabCi => Some(CiClient::Gitlab(GitlabCi)),
        // Drone/Woodpecker/Jenkins/Argo land in Phases D/E.
        _ => None,
    }
}

/// Resolve the active [`CiSystem`] for a worktree (pure once the remote URL is
/// in hand). `provider == auto` sniffs the origin host, else honours the config.
pub fn resolve_system(loc: &GitLoc, cfg: &CiConfig) -> Option<CiSystem> {
    match cfg.provider {
        CiProviderKind::None => None,
        CiProviderKind::Github => Some(CiSystem::GithubActions),
        CiProviderKind::Gitlab => Some(CiSystem::GitlabCi),
        CiProviderKind::Drone => Some(CiSystem::Drone),
        CiProviderKind::Woodpecker => Some(CiSystem::Woodpecker),
        CiProviderKind::Jenkins => Some(CiSystem::Jenkins),
        CiProviderKind::Argo => Some(CiSystem::Argo),
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
    }
}

/// Map a git remote URL's host to a CI system (pure, tested).
pub fn system_from_remote_host(url: &str) -> Option<CiSystem> {
    let l = url.to_ascii_lowercase();
    if l.contains("github.") {
        Some(CiSystem::GithubActions)
    } else if l.contains("gitlab.") || l.contains("/gitlab") {
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

/// Static-dispatch wrapper over the concrete providers, so the host never needs
/// a `dyn CiProvider` (which `async fn` in the trait would forbid). Delegates
/// every method to the inner provider.
pub enum CiClient {
    Github(GithubCi),
    Gitlab(GitlabCi),
}

impl CiClient {
    pub fn system(&self) -> CiSystem {
        match self {
            CiClient::Github(_) => CiSystem::GithubActions,
            CiClient::Gitlab(_) => CiSystem::GitlabCi,
        }
    }
    pub fn caps(&self) -> CiCaps {
        match self {
            CiClient::Github(p) => p.caps(),
            CiClient::Gitlab(p) => p.caps(),
        }
    }
    pub async fn runs(
        &self,
        loc: &GitLoc,
        branch: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CiRun>, CiError> {
        match self {
            CiClient::Github(p) => p.runs(loc, branch, limit).await,
            CiClient::Gitlab(p) => p.runs(loc, branch, limit).await,
        }
    }
    pub async fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRun, CiError> {
        match self {
            CiClient::Github(p) => p.run_detail(loc, run_id).await,
            CiClient::Gitlab(p) => p.run_detail(loc, run_id).await,
        }
    }
    pub async fn logs(&self, loc: &GitLoc, run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        match self {
            CiClient::Github(p) => p.logs(loc, run_id, job_id).await,
            CiClient::Gitlab(p) => p.logs(loc, run_id, job_id).await,
        }
    }
    pub async fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        match self {
            CiClient::Github(p) => p.workflows(loc).await,
            CiClient::Gitlab(p) => p.workflows(loc).await,
        }
    }
    pub async fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        match self {
            CiClient::Github(p) => p.trigger(loc, workflow, inputs).await,
            CiClient::Gitlab(p) => p.trigger(loc, workflow, inputs).await,
        }
    }
    pub async fn rerun(
        &self,
        loc: &GitLoc,
        run_id: &str,
        scope: RerunScope,
    ) -> Result<(), CiError> {
        match self {
            CiClient::Github(p) => p.rerun(loc, run_id, scope).await,
            CiClient::Gitlab(p) => p.rerun(loc, run_id, scope).await,
        }
    }
    pub async fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        match self {
            CiClient::Github(p) => p.cancel(loc, run_id).await,
            CiClient::Gitlab(p) => p.cancel(loc, run_id).await,
        }
    }
}

// === helpers ===============================================================

fn run_cli(cmd: &mut std::process::Command) -> Result<String, CiError> {
    let out = cmd.output().map_err(|e| CiError::Other(e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(classify_stderr(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn nonempty(s: &serde_json::Value, key: &str) -> Option<String> {
    s.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|v| !v.is_empty())
}

/// Stringify a JSON id that may be a number or a string.
fn id_str(v: &serde_json::Value, key: &str) -> String {
    match v.get(key) {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

// === GitHub Actions (gh CLI) ==============================================

const GH_RUN_FIELDS: &str = "databaseId,name,displayTitle,headBranch,headSha,event,\
                             status,conclusion,number,createdAt,updatedAt,url,workflowName";
const GH_DETAIL_FIELDS: &str = "databaseId,name,displayTitle,headBranch,headSha,event,\
                                status,conclusion,number,createdAt,updatedAt,url,workflowName,jobs";

/// GitHub Actions via the `gh` CLI — reuses the user's existing `gh` auth
/// (keyring, enterprise hosts) instead of threading a token.
pub struct GithubCi;

impl CiProvider for GithubCi {
    async fn runs(
        &self,
        loc: &GitLoc,
        branch: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CiRun>, CiError> {
        let limit_s = limit.max(1).to_string();
        let mut args = vec!["run", "list", "--limit", &limit_s, "--json", GH_RUN_FIELDS];
        if let Some(b) = branch {
            args.push("--branch");
            args.push(b);
        }
        let json = run_cli(&mut loc.gh_command(&args))?;
        Ok(parse_gh_runs(&json))
    }

    async fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRun, CiError> {
        let json =
            run_cli(&mut loc.gh_command(&["run", "view", run_id, "--json", GH_DETAIL_FIELDS]))?;
        parse_gh_run_detail(&json).ok_or(CiError::NotFound)
    }

    async fn logs(&self, loc: &GitLoc, _run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        // `gh run view --job <id> --log` (job ids are globally addressable).
        let text = run_cli(&mut loc.gh_command(&["run", "view", "--job", job_id, "--log"]))?;
        Ok(CiLog {
            text,
            truncated: false,
        })
    }

    async fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        let json =
            run_cli(&mut loc.gh_command(&["workflow", "list", "--json", "id,name,path,state"]))?;
        Ok(parse_gh_workflows(&json))
    }

    async fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        let mut args: Vec<String> = vec!["workflow".into(), "run".into(), workflow.into()];
        for (k, v) in inputs {
            args.push("-f".into());
            args.push(format!("{k}={v}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(&mut loc.gh_command(&argv)).map(|_| ())
    }

    async fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError> {
        let mut args = vec!["run", "rerun", run_id];
        if scope == RerunScope::Failed {
            args.push("--failed");
        }
        run_cli(&mut loc.gh_command(&args)).map(|_| ())
    }

    async fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        run_cli(&mut loc.gh_command(&["run", "cancel", run_id])).map(|_| ())
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
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| arr.iter().map(gh_run_from_value).collect())
        .unwrap_or_default()
}

/// Parse `gh run view <id> --json …` (a single object, with `jobs`).
pub fn parse_gh_run_detail(json: &str) -> Option<CiRun> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(gh_run_from_value(&v))
}

fn gh_run_from_value(v: &serde_json::Value) -> CiRun {
    let status = nonempty(v, "status").unwrap_or_default();
    let conclusion = nonempty(v, "conclusion");
    let completed = status.eq_ignore_ascii_case("completed");
    CiRun {
        id: id_str(v, "databaseId"),
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
        jobs: v
            .get("jobs")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().map(gh_job_from_value).collect())
            .unwrap_or_default(),
    }
}

fn gh_job_from_value(v: &serde_json::Value) -> CiJob {
    let status = nonempty(v, "status").unwrap_or_default();
    let conclusion = nonempty(v, "conclusion");
    CiJob {
        id: id_str(v, "databaseId"),
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
    }
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
                .map(|v| CiWorkflow {
                    id: id_str(v, "id"),
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
                .collect()
        })
        .unwrap_or_default()
}

// === GitLab CI (glab + GitLab API) ========================================

/// GitLab CI via `glab api` (reuses `glab`'s configured auth). Pipelines→jobs;
/// GitLab has no per-job "steps", so [`CiJob::steps`] stays empty.
pub struct GitlabCi;

impl GitlabCi {
    /// URL-encode the project path (`group/sub/repo` → `group%2Fsub%2Frepo`) for
    /// the `projects/:id` API segment. Only `/` needs encoding in project paths.
    fn project_seg(loc: &GitLoc) -> Option<String> {
        let url = origin_url(loc)?;
        let path = gitlab_project_path(&url)?;
        Some(path.replace('/', "%2F"))
    }
}

impl CiProvider for GitlabCi {
    async fn runs(
        &self,
        loc: &GitLoc,
        branch: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CiRun>, CiError> {
        let proj = Self::project_seg(loc).ok_or(CiError::NotConfigured)?;
        let mut endpoint = format!("projects/{proj}/pipelines?per_page={}", limit.max(1));
        if let Some(b) = branch {
            endpoint.push_str(&format!("&ref={b}"));
        }
        let json = run_cli(&mut loc.cli_command("glab", &["api", &endpoint]))?;
        Ok(parse_gitlab_pipelines(&json))
    }

    async fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRun, CiError> {
        let proj = Self::project_seg(loc).ok_or(CiError::NotConfigured)?;
        // Pipeline header + its jobs (two calls; the jobs carry the states).
        let pipe_json = run_cli(&mut loc.cli_command(
            "glab",
            &["api", &format!("projects/{proj}/pipelines/{run_id}")],
        ))?;
        let jobs_json = run_cli(&mut loc.cli_command(
            "glab",
            &["api", &format!("projects/{proj}/pipelines/{run_id}/jobs")],
        ))?;
        let mut run = parse_gitlab_pipeline_detail(&pipe_json).ok_or(CiError::NotFound)?;
        run.jobs = parse_gitlab_jobs(&jobs_json);
        Ok(run)
    }

    async fn logs(&self, loc: &GitLoc, _run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        let proj = Self::project_seg(loc).ok_or(CiError::NotConfigured)?;
        let text = run_cli(&mut loc.cli_command(
            "glab",
            &["api", &format!("projects/{proj}/jobs/{job_id}/trace")],
        ))?;
        Ok(CiLog {
            text,
            truncated: false,
        })
    }

    async fn workflows(&self, _loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        // GitLab has one pipeline definition (`.gitlab-ci.yml`), not a set of
        // dispatchable workflows; manual-trigger support is Phase B.
        Ok(Vec::new())
    }

    async fn trigger(
        &self,
        loc: &GitLoc,
        _workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        let proj = Self::project_seg(loc).ok_or(CiError::NotConfigured)?;
        let mut args = vec!["api".to_string(), "-X".into(), "POST".into()];
        args.push(format!("projects/{proj}/pipeline"));
        for (k, v) in inputs {
            args.push("-f".into());
            args.push(format!("{k}={v}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(&mut loc.cli_command("glab", &argv)).map(|_| ())
    }

    async fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError> {
        let proj = Self::project_seg(loc).ok_or(CiError::NotConfigured)?;
        // GitLab: `retry` re-runs failed jobs; a fresh full run isn't a single
        // call, so both scopes map to retry (it's the closest primitive).
        let _ = scope;
        let endpoint = format!("projects/{proj}/pipelines/{run_id}/retry");
        run_cli(&mut loc.cli_command("glab", &["api", "-X", "POST", &endpoint])).map(|_| ())
    }

    async fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        let proj = Self::project_seg(loc).ok_or(CiError::NotConfigured)?;
        let endpoint = format!("projects/{proj}/pipelines/{run_id}/cancel");
        run_cli(&mut loc.cli_command("glab", &["api", "-X", "POST", &endpoint])).map(|_| ())
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
    let url = url.trim();
    let path = if url.contains('@') && !url.contains("://") {
        // git@gitlab.com:group/sub/repo.git
        url.split_once(':').map(|(_, r)| r.to_string())?
    } else {
        let idx = url.find("://")?;
        let after = &url[idx + 3..];
        after.split_once('/').map(|(_, r)| r.to_string())?
    };
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let path = path.trim_matches('/');
    (!path.is_empty() && path.contains('/')).then(|| path.to_string())
}

/// Parse `GET projects/:id/pipelines` (array) into runs.
pub fn parse_gitlab_pipelines(json: &str) -> Vec<CiRun> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| arr.iter().map(gitlab_pipeline_from_value).collect())
        .unwrap_or_default()
}

/// Parse `GET projects/:id/pipelines/:id` (single object) into a run header.
pub fn parse_gitlab_pipeline_detail(json: &str) -> Option<CiRun> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.is_object().then(|| gitlab_pipeline_from_value(&v))
}

fn gitlab_pipeline_from_value(v: &serde_json::Value) -> CiRun {
    let status = nonempty(v, "status").unwrap_or_default();
    let terminal = CiState::from_gitlab(&status).is_terminal();
    let id = id_str(v, "id");
    CiRun {
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
    }
}

/// Parse `GET projects/:id/pipelines/:id/jobs` (array) into jobs.
pub fn parse_gitlab_jobs(json: &str) -> Vec<CiJob> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .map(|v| {
                    let status = nonempty(v, "status").unwrap_or_default();
                    CiJob {
                        id: id_str(v, "id"),
                        name: nonempty(v, "name").unwrap_or_default(),
                        state: CiState::from_gitlab(&status),
                        url: nonempty(v, "web_url"),
                        started_at: nonempty(v, "started_at"),
                        finished_at: nonempty(v, "finished_at"),
                        steps: Vec::new(),
                    }
                })
                .collect()
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
        // single-segment (no group) → None (GitLab projects always have a group)
        assert_eq!(gitlab_project_path("https://gitlab.com/repo.git"), None);
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
        assert!(!GitlabCi.caps().steps);
        assert!(GithubCi.caps().trigger);
        assert!(GitlabCi.caps().cancel);
        // GitLab's pipeline `retry` can't scope to failed jobs; offering the
        // distinction anyway would silently retry everything.
        assert!(GithubCi.caps().rerun_failed);
        assert!(!GitlabCi.caps().rerun_failed);
    }
}
