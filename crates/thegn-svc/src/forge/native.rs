//! The native GitHub layer: octocrab GraphQL for the two hot-path reads
//! (`pr_status`, `pr_list`) on local worktrees with a resolvable token.
//!
//! Everything else answers `Unsupported`, and any "this layer can't" condition
//! (remote location, no token, open circuit breaker, client build failure,
//! GraphQL-level errors) answers `NotConfigured` — so the `Ladder` falls
//! through to [`GithubCli`](thegn_core::github::GithubCli). Transport
//! failures and timeouts are `Offline` (final: retrying on `gh` would just
//! repeat the failure), and feed the process-wide circuit breaker +
//! connectivity holder.
//!
//! The seam is sync: each call builds a current-thread runtime and
//! `block_on`s the octocrab request, exactly as the host used to do at its
//! one call site — so callers stay on blocking threads and never need a
//! runtime handle.

use serde_json::Value;
use thegn_core::forge::model::*;
use thegn_core::forge::{Forge, ForgeCaps, ForgeError, PrRef, RepoRef};
use thegn_core::remote::GitLoc;
use thegn_core::seam::{Availability, Probe, ProbeReport};

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

/// All open PRs' headers in one round trip — the per-branch badge feed.
pub const PR_LIST_QUERY: &str = r#"
query($owner:String!,$repo:String!){
  repository(owner:$owner,name:$repo){
    pullRequests(first:100, states:[OPEN]){
      nodes{ number headRefName state url isDraft }
    }
  }
}"#;

/// Parse a `PR_LIST_QUERY` response into headers. Pure, fixture-tested.
pub fn parse_graphql_pr_list(resp: &Value) -> Vec<PrHeader> {
    let data = resp.get("data").unwrap_or(resp);
    data.pointer("/repository/pullRequests/nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| serde_json::from_value(n.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The single GraphQL query that replaces the CLI's separate `gh pr view` +
/// `gh run list`: PR state + checks + reviews in one round trip.
pub const PR_QUERY: &str = r#"
query($owner:String!,$repo:String!,$head:String!){
  repository(owner:$owner,name:$repo){
    pullRequests(headRefName:$head, first:1, states:[OPEN,MERGED,CLOSED]){
      nodes{
        number title state url isDraft headRefName headRefOid baseRefName
        mergeable mergeStateStatus reviewDecision
        commits(last:1){ nodes{ commit{ statusCheckRollup{
          contexts(first:100){ nodes{
            __typename
            ... on CheckRun   { name status conclusion detailsUrl startedAt completedAt }
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
            started_at: None,
            completed_at: None,
        },
        _ => CheckRun {
            name: s("name").unwrap_or_default(),
            status: s("status").unwrap_or_default(),
            conclusion: s("conclusion"),
            state: None,
            workflow_name: None,
            details_url: s("detailsUrl"),
            started_at: s("startedAt"),
            completed_at: s("completedAt"),
        },
    }
}

/// Parse a GraphQL response (the whole `{data,errors}` body, or just `data`)
/// into a `PrPanel`. Pure — the network call is elsewhere — so the mapping that
/// must match the CLI path is unit-tested against a fixture.
pub fn parse_graphql_pr(resp: &Value, worktree: &str, branch: &str, now: i64) -> PrPanel {
    let mut panel = PrPanel::from_result(
        parse_graphql_pr_status(resp),
        worktree.to_string(),
        branch.to_string(),
    );
    panel.fetched_at = now;
    panel
}

/// The GraphQL `pullRequests(headRefName:)` reply as a `Result` — the forge
/// trait's shape. No node ⇒ `NoPr`.
pub fn parse_graphql_pr_status(resp: &Value) -> Result<PrStatus, ForgeError> {
    let data = resp.get("data").unwrap_or(resp);
    let nodes = data
        .pointer("/repository/pullRequests/nodes")
        .and_then(Value::as_array);

    match nodes.and_then(|n| n.first()) {
        None => Err(ForgeError::NoPr),
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
                head_ref_oid: s("headRefOid"),
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
            Ok(pr)
        }
    }
}

/// Parse `owner/repo` from a git remote URL (ssh or https, with/without
/// `.git`). One parser for the workspace: `thegn_core::forge::model`'s.
pub fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    nwo_from_remote_url(url).and_then(|nwo| {
        nwo.split_once('/')
            .map(|(o, r)| (o.to_string(), r.to_string()))
    })
}

/// Per-request timeout on octocrab GraphQL calls. A stalled TLS handshake to
/// api.github.com blocks the refresh task for up to the reqwest default (15s)
/// with no user feedback; cap it at 10s so the fallback kicks in promptly.
const OCTOCRAB_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Open the circuit after this many consecutive transient failures. When open,
/// we skip the octocrab path entirely (going straight to the CLI fallback) for
/// `CIRCUIT_OPEN_SECS` seconds so a network partition doesn't spawn a hanging
/// octocrab task every 20s.
const CIRCUIT_OPEN_AFTER: u32 = 3;
const CIRCUIT_OPEN_SECS: u64 = 60;

/// Simple half-open circuit breaker shared across all `GithubNative` calls
/// (process-global; the native layer is cheap to construct).
static CIRCUIT: std::sync::OnceLock<GhCircuit> = std::sync::OnceLock::new();

struct GhCircuit {
    failures: std::sync::atomic::AtomicU32,
    open_until: std::sync::Mutex<Option<std::time::Instant>>,
}

impl GhCircuit {
    fn new() -> Self {
        Self {
            failures: std::sync::atomic::AtomicU32::new(0),
            open_until: std::sync::Mutex::new(None),
        }
    }

    /// Returns `true` if the circuit is open (skip octocrab this call).
    fn is_open(&self) -> bool {
        let guard = self.open_until.lock().unwrap_or_else(|e| e.into_inner());
        guard.is_some_and(|until| std::time::Instant::now() < until)
    }

    fn record_success(&self) {
        self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = self.open_until.lock() {
            *g = None;
        }
        // A GitHub round trip succeeded — the app is online.
        thegn_core::connectivity::report_success();
    }

    fn record_failure(&self) {
        // A transient GitHub network failure — offline evidence for the
        // app-wide connectivity holder (flips after its own threshold).
        thegn_core::connectivity::report_failure();
        let prev = self
            .failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if prev + 1 >= CIRCUIT_OPEN_AFTER {
            if let Ok(mut g) = self.open_until.lock() {
                *g = Some(
                    std::time::Instant::now() + std::time::Duration::from_secs(CIRCUIT_OPEN_SECS),
                );
            }
            tracing::warn!(
                target: "thegn::forge",
                consecutive_failures = prev + 1,
                open_secs = CIRCUIT_OPEN_SECS,
                "GitHub API unreachable — pausing native octocrab path"
            );
        }
    }
}

fn circuit() -> &'static GhCircuit {
    CIRCUIT.get_or_init(GhCircuit::new)
}

/// octocrab GraphQL for `pr_status` / `pr_list`; see the module doc.
#[derive(Debug, Clone, Copy, Default)]
pub struct GithubNative;

impl GithubNative {
    pub fn new() -> Self {
        Self
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

    /// The gate every native op runs first: local loc, closed circuit, token,
    /// origin. Any miss is `NotConfigured` — the ladder falls through.
    fn gate(&self, loc: &GitLoc) -> Result<(String, String, String), ForgeError> {
        if loc.is_remote() {
            return Err(ForgeError::NotConfigured("native layer is local-only"));
        }
        if circuit().is_open() {
            return Err(ForgeError::NotConfigured(
                "circuit open after repeated failures",
            ));
        }
        let Some(token) = resolve_token() else {
            return Err(ForgeError::NotConfigured("no GitHub token"));
        };
        let Some((owner, repo)) = self.owner_repo(loc) else {
            return Err(ForgeError::NotConfigured("origin is not a GitHub remote"));
        };
        Ok((token, owner, repo))
    }

    /// One GraphQL round trip under the request timeout, classified.
    fn graphql(&self, token: String, body: Value, what: &'static str) -> Result<Value, ForgeError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ForgeError::NotConfigured(Box::leak(format!("no runtime: {e}").into_boxed_str()))
            })?;
        let client = octocrab::OctocrabBuilder::new()
            .personal_token(token)
            .build()
            .map_err(|_| ForgeError::NotConfigured("octocrab client build failed"))?;
        let result = rt.block_on(async {
            tokio::time::timeout(OCTOCRAB_REQUEST_TIMEOUT, client.graphql::<Value>(&body)).await
        });
        match result {
            Ok(Ok(resp)) if resp.get("errors").is_none() => {
                circuit().record_success();
                Ok(resp)
            }
            Ok(Ok(resp)) => {
                // GraphQL-level errors (not a network failure) — CLI fallback.
                tracing::debug!(
                    target: "thegn::forge",
                    op = what,
                    errors = ?resp.get("errors"),
                    "octocrab GraphQL errors, falling back to cli"
                );
                Err(ForgeError::NotConfigured("GraphQL errors"))
            }
            Ok(Err(e)) => {
                let text = e.to_string().to_lowercase();
                let is_connect =
                    text.contains("connect") || text.contains("dns") || text.contains("tls");
                tracing::warn!(
                    target: "thegn::forge",
                    op = what,
                    error = %e,
                    is_connect,
                    "octocrab request failed"
                );
                if is_connect {
                    circuit().record_failure();
                    Err(ForgeError::Offline)
                } else {
                    // An HTTP-level answer (401/403/5xx): final, not a fallthrough.
                    Err(ForgeError::Other(e.to_string()))
                }
            }
            Err(_elapsed) => {
                tracing::warn!(
                    target: "thegn::forge",
                    op = what,
                    timeout_secs = OCTOCRAB_REQUEST_TIMEOUT.as_secs(),
                    "octocrab request timed out"
                );
                circuit().record_failure();
                Err(ForgeError::Offline)
            }
        }
    }
}

impl Probe for GithubNative {
    /// Offline by contract (probes never spawn network-bound work): only the
    /// env-var tokens are checked here; `gh auth token` is resolved per call.
    fn probe(&self) -> ProbeReport {
        let env_token = ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
        let availability = if env_token {
            Availability::Ready
        } else {
            Availability::Degraded(
                "no GH_TOKEN/GITHUB_TOKEN; `gh auth token` is tried per call, else the `gh` CLI serves it"
                    .into(),
            )
        };
        ProbeReport::new("forge", "github-native", availability)
            .with_caps(&self.caps())
            .note("octocrab GraphQL for pr_status / pr_list on local worktrees")
    }
}

impl Forge for GithubNative {
    fn id(&self) -> &'static str {
        "github"
    }
    fn caps(&self) -> ForgeCaps {
        ForgeCaps {
            pr_status: true,
            pr_list: true,
            ..ForgeCaps::default()
        }
    }
    fn repo_ref(&self, loc: &GitLoc) -> Option<RepoRef> {
        self.owner_repo(loc)
            .map(|(owner, repo)| RepoRef { owner, repo })
    }
    fn pr_status(&self, loc: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
        // By-number lookups are the queue's path; the CLI serves them.
        if pr != PrRef::Current {
            return Err(ForgeError::Unsupported("pr_status by number"));
        }
        let (token, owner, repo) = self.gate(loc)?;
        // Just the branch — a local `git rev-parse`, never a network fetch.
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_default();
        let body = serde_json::json!({
            "query": PR_QUERY,
            "variables": { "owner": owner, "repo": repo, "head": branch },
        });
        let resp = self.graphql(token, body, "pr_status")?;
        parse_graphql_pr_status(&resp)
    }
    fn pr_list(&self, loc: &GitLoc, _limit: usize) -> Result<Vec<PrHeader>, ForgeError> {
        let (token, owner, repo) = self.gate(loc)?;
        let body = serde_json::json!({
            "query": PR_LIST_QUERY,
            "variables": { "owner": owner, "repo": repo },
        });
        let resp = self.graphql(token, body, "pr_list")?;
        Ok(parse_graphql_pr_list(&resp))
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
            parse_owner_repo("git@github.com:blake/thegn.git"),
            Some(("blake".into(), "thegn".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/blake/thegn"),
            Some(("blake".into(), "thegn".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/blake/thegn.git"),
            Some(("blake".into(), "thegn".into()))
        );
        assert_eq!(
            parse_owner_repo("ssh://git@github.com/org/repo.git"),
            Some(("org".into(), "repo".into()))
        );
        assert_eq!(parse_owner_repo("not a url"), None);
    }

    #[test]
    fn graphql_pr_list_parses_headers() {
        let resp = serde_json::json!({
          "data": { "repository": { "pullRequests": { "nodes": [
            {"number": 7, "headRefName": "feat/x", "state": "OPEN",
             "url": "https://github.com/o/r/pull/7", "isDraft": true},
            {"number": 9, "headRefName": "fix/y", "state": "OPEN",
             "url": "https://github.com/o/r/pull/9", "isDraft": false}
          ]}}}
        });
        let prs = parse_graphql_pr_list(&resp);
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 7);
        assert_eq!(prs[0].head_ref, "feat/x");
        assert!(prs[0].is_draft);
        assert_eq!(prs[1].url, "https://github.com/o/r/pull/9");
        // Empty / malformed → empty.
        assert!(parse_graphql_pr_list(&serde_json::json!({})).is_empty());
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

    /// Regression: the native `pr_status` resolves the branch with a local
    /// `git rev-parse`, NOT `github::pr_status` (which also runs a full,
    /// network `gh pr view` whose result it discards — a double-fetch). This
    /// pins the cheap resolution the fix now uses: it returns the checked-out
    /// branch from a real repo with no `gh` in the loop.
    #[test]
    fn native_branch_resolution_is_local_rev_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Route test git through the scrubbing helper rather than a raw git
        // command, matching the repo invariant the lint guardrail enforces.
        let git = |args: &[&str]| {
            let ok = thegn_core::util::git_cmd(dir)
                // Signing off: this helper inherits the user's global config, so
                // `commit.gpgsign = true` in ~/.gitconfig would hang the commit
                // below on a pinentry the test runner has no terminal for.
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        git(&["checkout", "-q", "-b", "feat/native"]);

        let loc = GitLoc::Local(dir.to_path_buf());
        // This is exactly the resolution the native pr_status now performs.
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_default();
        assert_eq!(branch, "feat/native");
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
