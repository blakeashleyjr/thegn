//! GitHub backend seam. The native impl (Phase 4) uses octocrab with a single
//! GraphQL round trip for PR state + checks + reviews, deserialized into
//! superzej-core's existing `PrPanel`/`PrStatus`/`CheckRun` model. The `Cli`
//! fallback wraps core's `gh`-subprocess code. Token sourcing (Phase 4):
//! `GH_TOKEN`/`GITHUB_TOKEN` env → `gh auth token` → config field.

use serde_json::Value;
use superzej_core::github::{
    self, CheckRun, CreateOpts, GhError, MergeMethod, PanelState, PrPanel, PrStatus,
};
use superzej_core::remote::GitLoc;

/// Async because the native impl is reqwest/octocrab; the CLI fallback wraps its
/// blocking subprocess on `spawn_blocking`.
#[allow(async_fn_in_trait)]
pub trait GhBackend: Send + Sync {
    async fn pr_status(&self, loc: &GitLoc) -> Result<PrPanel, GhError>;
    async fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, GhError>;
    async fn merge_pr(
        &self,
        loc: &GitLoc,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), GhError>;
    async fn approve(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), GhError>;
    async fn rerun_failed(&self, loc: &GitLoc) -> Result<u32, GhError>;
}

/// The permanent fallback: every op via the `gh` CLI (through superzej-core's
/// existing, tested `github` module). The octocrab native impl (Phase 4)
/// composes over this for ops it doesn't cover.
pub struct CliGh;

impl GhBackend for CliGh {
    async fn pr_status(&self, loc: &GitLoc) -> Result<PrPanel, GhError> {
        Ok(github::pr_status(loc))
    }
    async fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, GhError> {
        github::create_pr(loc, opts)
    }
    async fn merge_pr(
        &self,
        loc: &GitLoc,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), GhError> {
        github::merge_pr(loc, method, delete_branch, auto)
    }
    async fn approve(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), GhError> {
        github::approve_pr(loc, body)
    }
    async fn rerun_failed(&self, loc: &GitLoc) -> Result<u32, GhError> {
        github::rerun_failed_checks(loc)
    }
}

/// Source a GitHub token for the octocrab native impl. Precedence:
/// `GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token` (reuses the user's existing
/// `gh` login: keyring, refresh, enterprise hosts — we drop `gh` from the hot
/// path, not as a dependency). Returns `None` if no token is available.
pub fn resolve_token() -> Option<String> {
    token_from(
        |k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()),
        gh_auth_token,
    )
}

/// Pure precedence logic, injectable for testing.
fn token_from(
    env: impl Fn(&str) -> Option<String>,
    gh_cli: impl Fn() -> Option<String>,
) -> Option<String> {
    env("GH_TOKEN")
        .or_else(|| env("GITHUB_TOKEN"))
        .or_else(gh_cli)
        .map(|t| t.trim().to_string())
}

/// The single GraphQL query that replaces the CLI's separate `gh pr view` +
/// `gh run list`: PR state + checks + reviews in one round trip.
pub const PR_QUERY: &str = r#"
query($owner:String!,$repo:String!,$head:String!){
  repository(owner:$owner,name:$repo){
    pullRequests(headRefName:$head, first:1, states:[OPEN,MERGED,CLOSED]){
      nodes{
        number title state url isDraft headRefName baseRefName
        mergeable mergeStateStatus reviewDecision
        commits(last:1){ nodes{ commit{ statusCheckRollup{
          contexts(first:100){ nodes{
            __typename
            ... on CheckRun   { name status conclusion detailsUrl }
            ... on StatusContext { context state targetUrl }
          }}}}}}
      }
    }
  }
}"#;

/// One `statusCheckRollup.contexts` node → a `CheckRun` (handles both the
/// `CheckRun` and `StatusContext` shapes via `__typename`).
fn check_from_ctx(ctx: &Value) -> CheckRun {
    let s = |k: &str| ctx.get(k).and_then(Value::as_str).map(str::to_string);
    match ctx.get("__typename").and_then(Value::as_str) {
        Some("StatusContext") => CheckRun {
            name: s("context").unwrap_or_default(),
            status: String::new(),
            conclusion: None,
            state: s("state"),
            workflow_name: None,
            details_url: s("targetUrl"),
        },
        _ => CheckRun {
            name: s("name").unwrap_or_default(),
            status: s("status").unwrap_or_default(),
            conclusion: s("conclusion"),
            state: None,
            workflow_name: None,
            details_url: s("detailsUrl"),
        },
    }
}

/// Parse a GraphQL response (the whole `{data,errors}` body, or just `data`)
/// into a `PrPanel`. Pure — the network call is elsewhere — so the mapping that
/// must match the CLI path is unit-tested against a fixture.
pub fn parse_graphql_pr(resp: &Value, worktree: &str, branch: &str, now: i64) -> PrPanel {
    let data = resp.get("data").unwrap_or(resp);
    let nodes = data
        .pointer("/repository/pullRequests/nodes")
        .and_then(Value::as_array);

    let state = match nodes.and_then(|n| n.first()) {
        None => PanelState::NoPr,
        Some(node) => {
            let s = |k: &str| {
                node.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let rollup = node
                .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().map(check_from_ctx).collect::<Vec<_>>())
                .unwrap_or_default();
            let mut pr = PrStatus {
                number: node.get("number").and_then(Value::as_u64).unwrap_or(0),
                title: s("title"),
                state: s("state"),
                url: s("url"),
                is_draft: node
                    .get("isDraft")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                head_ref_name: s("headRefName"),
                base_ref_name: s("baseRefName"),
                mergeable: s("mergeable"),
                merge_state_status: s("mergeStateStatus"),
                review_decision: node
                    .get("reviewDecision")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status_check_rollup: rollup,
                checks: Default::default(),
            };
            pr.recompute_checks();
            PanelState::Pr(Box::new(pr))
        }
    };
    PrPanel {
        state,
        worktree: worktree.to_string(),
        branch: branch.to_string(),
        fetched_at: now,
    }
}

/// Parse `owner/repo` from a git remote URL (ssh or https, with/without `.git`).
pub fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    // git@github.com:owner/repo(.git)  |  ssh://git@github.com/owner/repo
    // https://github.com/owner/repo(.git)
    let path = if let Some(rest) = url
        .split_once(':')
        .map(|(_, r)| r)
        .filter(|_| url.contains('@') && !url.contains("://"))
    {
        rest.to_string()
    } else if let Some(idx) = url.find("://") {
        let after = &url[idx + 3..];
        after.split_once('/').map(|(_, r)| r.to_string())?
    } else {
        return None;
    };
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((
        owner.to_string(),
        repo.split('/').next().unwrap_or(repo).to_string(),
    ))
}

/// The native GitHub backend: octocrab GraphQL for `pr_status` (one round trip)
/// on local locs with a resolvable token; everything else (writes, remote, or
/// any failure) delegates to the `gh`-CLI fallback. Mirrors the gix/CliGit split.
pub struct GhNative {
    fallback: CliGh,
}

impl Default for GhNative {
    fn default() -> Self {
        Self { fallback: CliGh }
    }
}

impl GhNative {
    pub fn new() -> Self {
        Self::default()
    }

    fn owner_repo(&self, loc: &GitLoc) -> Option<(String, String)> {
        let out = loc
            .git_command(&["remote", "get-url", "origin"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
            .and_then(|u| parse_owner_repo(&u))
    }
}

impl GhBackend for GhNative {
    async fn pr_status(&self, loc: &GitLoc) -> Result<PrPanel, GhError> {
        // Native path only for local locs with a token + resolvable origin.
        if loc.is_remote() {
            return self.fallback.pr_status(loc).await;
        }
        let (Some(token), Some((owner, repo))) = (resolve_token(), self.owner_repo(loc)) else {
            return self.fallback.pr_status(loc).await;
        };
        let branch = github::pr_status(loc).branch; // cheap rev-parse via core
        let client = match octocrab::OctocrabBuilder::new()
            .personal_token(token)
            .build()
        {
            Ok(c) => c,
            Err(_) => return self.fallback.pr_status(loc).await,
        };
        let body = serde_json::json!({
            "query": PR_QUERY,
            "variables": { "owner": owner, "repo": repo, "head": branch },
        });
        match client.graphql::<Value>(&body).await {
            Ok(resp) if resp.get("errors").is_none() => Ok(parse_graphql_pr(
                &resp,
                &loc.path(),
                &branch,
                superzej_core::util::now(),
            )),
            // GraphQL errors or transport failure → CLI fallback (keeps working).
            _ => self.fallback.pr_status(loc).await,
        }
    }

    async fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, GhError> {
        self.fallback.create_pr(loc, opts).await
    }
    async fn merge_pr(
        &self,
        loc: &GitLoc,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), GhError> {
        self.fallback
            .merge_pr(loc, method, delete_branch, auto)
            .await
    }
    async fn approve(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), GhError> {
        self.fallback.approve(loc, body).await
    }
    async fn rerun_failed(&self, loc: &GitLoc) -> Result<u32, GhError> {
        self.fallback.rerun_failed(loc).await
    }
}

fn gh_auth_token() -> Option<String> {
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!tok.is_empty()).then_some(tok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_precedence_prefers_gh_token_then_github_token_then_cli() {
        // GH_TOKEN wins.
        let env = |k: &str| match k {
            "GH_TOKEN" => Some("a".to_string()),
            "GITHUB_TOKEN" => Some("b".to_string()),
            _ => None,
        };
        assert_eq!(token_from(env, || Some("c".into())).as_deref(), Some("a"));

        // Falls back to GITHUB_TOKEN.
        let env = |k: &str| (k == "GITHUB_TOKEN").then(|| "b".to_string());
        assert_eq!(token_from(env, || Some("c".into())).as_deref(), Some("b"));

        // Falls back to the gh CLI.
        let env = |_: &str| None;
        assert_eq!(token_from(env, || Some("c".into())).as_deref(), Some("c"));

        // Nothing available.
        assert_eq!(token_from(|_| None, || None), None);
    }

    #[test]
    fn token_is_trimmed() {
        assert_eq!(
            token_from(|k| (k == "GH_TOKEN").then(|| "  x\n".to_string()), || None).as_deref(),
            Some("x")
        );
    }

    #[test]
    fn owner_repo_parses_ssh_and_https_forms() {
        assert_eq!(
            parse_owner_repo("git@github.com:blake/superzej.git"),
            Some(("blake".into(), "superzej".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/blake/superzej"),
            Some(("blake".into(), "superzej".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/blake/superzej.git"),
            Some(("blake".into(), "superzej".into()))
        );
        assert_eq!(
            parse_owner_repo("ssh://git@github.com/org/repo.git"),
            Some(("org".into(), "repo".into()))
        );
        assert_eq!(parse_owner_repo("not a url"), None);
    }

    #[test]
    fn graphql_no_pr_node_maps_to_no_pr() {
        let resp = serde_json::json!({
            "data": { "repository": { "pullRequests": { "nodes": [] } } }
        });
        let panel = parse_graphql_pr(&resp, "/wt", "feat", 1);
        assert!(matches!(panel.state, PanelState::NoPr));
        assert_eq!(panel.branch, "feat");
    }

    #[test]
    fn graphql_pr_maps_fields_and_rolls_up_checks() {
        // Mirrors GitHub's GraphQL shape: one CheckRun (success) + one failing
        // StatusContext + one pending CheckRun.
        let resp = serde_json::json!({
          "data": { "repository": { "pullRequests": { "nodes": [{
            "number": 42, "title": "Add native host", "state": "OPEN",
            "url": "https://github.com/x/y/pull/42", "isDraft": false,
            "headRefName": "feat", "baseRefName": "main",
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": {
              "contexts": { "nodes": [
                {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"u1"},
                {"__typename":"StatusContext","context":"ci/legacy","state":"FAILURE","targetUrl":"u2"},
                {"__typename":"CheckRun","name":"test","status":"IN_PROGRESS","conclusion":null,"detailsUrl":"u3"}
              ]}
            }}}]}
          }]}}}
        });
        let panel = parse_graphql_pr(&resp, "/wt", "feat", 7);
        match panel.state {
            PanelState::Pr(pr) => {
                assert_eq!(pr.number, 42);
                assert_eq!(pr.title, "Add native host");
                assert_eq!(pr.state, "OPEN");
                assert_eq!(pr.base_ref_name, "main");
                assert_eq!(pr.review_decision.as_deref(), Some("APPROVED"));
                assert_eq!(pr.status_check_rollup.len(), 3);
                // Rollup: 1 pass (CheckRun SUCCESS), 1 fail (StatusContext FAILURE),
                // 1 pending (CheckRun no conclusion). Must match the CLI summary.
                assert_eq!(pr.checks.total, 3);
                assert_eq!(pr.checks.passed, 1);
                assert_eq!(pr.checks.failed, 1);
                assert_eq!(pr.checks.pending, 1);
            }
            other => panic!("expected Pr, got {other:?}"),
        }
        assert_eq!(panel.fetched_at, 7);
    }
}
