//! GitHub integration via the `gh` CLI.

use crate::forge::models::*;
use crate::forge::Forge;
use crate::remote::GitLoc;
use serde::Deserialize;

pub struct GitHubForge;

/// Run `gh <args>` with `cwd = worktree` (local, or over ssh on the remote host);
/// trimmed stdout on success, else a classified error.
pub fn gh_out(loc: &GitLoc, args: &[&str]) -> Result<String, ForgeError> {
    let out = loc
        .gh_command(args)
        .output()
        .map_err(|e| ForgeError::Other(e.to_string()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(classify(
        &String::from_utf8_lossy(&out.stderr).to_lowercase(),
    ))
}

/// Run `gh <args>` for its exit code (output discarded). Errors classified.
pub fn gh_run(loc: &GitLoc, args: &[&str]) -> Result<(), ForgeError> {
    let out = loc
        .gh_command(args)
        .output()
        .map_err(|e| ForgeError::Other(e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(classify(
            &String::from_utf8_lossy(&out.stderr).to_lowercase(),
        ))
    }
}

fn classify(stderr: &str) -> ForgeError {
    if stderr.contains("command not found")
        || stderr.contains("not found")
        || stderr.contains("no such file")
    {
        ForgeError::NotInstalled
    } else if stderr.contains("no pull requests found")
        || stderr.contains("no default remote repository")
        || stderr.contains("no open pull request")
        || stderr.contains("no pr ")
    {
        ForgeError::NoPr
    } else if stderr.contains("not logged")
        || stderr.contains("authentication")
        || stderr.contains("gh auth login")
        || stderr.contains("http 401")
    {
        ForgeError::NotAuthenticated
    } else if stderr.contains("rate limit") || stderr.contains("api rate") {
        ForgeError::RateLimited
    } else {
        ForgeError::Other(stderr.trim().to_string())
    }
}

const PR_FIELDS: &str = "number,title,state,url,isDraft,headRefName,baseRefName,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup";

impl Forge for GitHubForge {
    fn pr_status(&self, loc: &GitLoc) -> PrPanel {
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_default();
        let state = match gh_out(loc, &["pr", "view", "--json", PR_FIELDS]) {
            Ok(json) => match serde_json::from_str::<PrStatus>(&json) {
                Ok(mut pr) => {
                    pr.checks = summarize(&pr.status_check_rollup);
                    PanelState::Pr(Box::new(pr))
                }
                Err(e) => PanelState::Error {
                    message: format!("parse error: {e}"),
                },
            },
            Err(ForgeError::NotInstalled) => PanelState::NoForgeCli,
            Err(ForgeError::NotAuthenticated) => PanelState::NotAuthenticated,
            Err(ForgeError::NoPr) => PanelState::NoPr,
            Err(ForgeError::RateLimited) => PanelState::RateLimited,
            Err(ForgeError::Other(m)) => PanelState::Error { message: m },
        };
        PrPanel {
            state,
            worktree: loc.path(),
            branch,
            fetched_at: crate::util::now(),
        }
    }

    fn create_pr(&self, loc: &GitLoc, o: &CreateOpts) -> Result<String, ForgeError> {
        let mut args: Vec<String> = vec!["pr".into(), "create".into()];
        if o.fill {
            args.push("--fill".into());
        }
        if o.draft {
            args.push("--draft".into());
        }
        if o.web {
            args.push("--web".into());
        }
        if let Some(t) = &o.title {
            args.push("--title".into());
            args.push(t.clone());
        }
        if let Some(b) = &o.body {
            args.push("--body".into());
            args.push(b.clone());
        }
        if let Some(b) = &o.base {
            args.push("--base".into());
            args.push(b.clone());
        }
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        gh_out(loc, &refs)
    }

    fn open_pr(&self, loc: &GitLoc) -> Result<(), ForgeError> {
        gh_run(loc, &["pr", "view", "--web"])
    }

    fn approve_pr(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), ForgeError> {
        let mut args = vec!["pr", "review", "--approve"];
        if let Some(b) = body {
            args.push("--body");
            args.push(b);
        }
        gh_run(loc, &args)
    }

    fn merge_pr(
        &self,
        loc: &GitLoc,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), ForgeError> {
        let mut args = vec!["pr", "merge", method.flag()];
        if delete_branch {
            args.push("--delete-branch");
        }
        if auto {
            args.push("--auto");
        }
        gh_run(loc, &args)
    }

    fn reviews(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        gh_out(
            loc,
            &["pr", "view", "--json", "reviews,latestReviews,comments"],
        )
    }

    fn rerun_failed_checks(&self, loc: &GitLoc) -> Result<u32, ForgeError> {
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .ok_or_else(|| ForgeError::Other("could not resolve branch".into()))?;
        let json = gh_out(
            loc,
            &[
                "run",
                "list",
                "--branch",
                &branch,
                "--json",
                "databaseId,conclusion",
                "--limit",
                "20",
            ],
        )?;
        #[derive(Deserialize)]
        struct Run {
            #[serde(rename = "databaseId")]
            database_id: u64,
            conclusion: Option<String>,
        }
        let runs: Vec<Run> = serde_json::from_str(&json).unwrap_or_default();
        let mut count = 0;
        for r in runs {
            if matches!(
                r.conclusion.as_deref().map(|s| s.to_uppercase()).as_deref(),
                Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("STARTUP_FAILURE")
            ) {
                let id = r.database_id.to_string();
                if gh_run(loc, &["run", "rerun", &id, "--failed"]).is_ok() {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}

/// Short human-readable description of an error (for CLI output).
pub fn describe(e: &ForgeError) -> String {
    e.message()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cr(status: &str, conclusion: Option<&str>, state: Option<&str>) -> CheckRun {
        CheckRun {
            name: "ci".into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
            state: state.map(String::from),
            workflow_name: None,
            details_url: None,
        }
    }

    #[test]
    fn buckets_handle_both_shapes() {
        assert_eq!(
            check_bucket(&cr("COMPLETED", Some("SUCCESS"), None)),
            Bucket::Pass
        );
        assert_eq!(
            check_bucket(&cr("COMPLETED", Some("FAILURE"), None)),
            Bucket::Fail
        );
        assert_eq!(
            check_bucket(&cr("IN_PROGRESS", None, None)),
            Bucket::Pending
        );
        assert_eq!(check_bucket(&cr("", None, Some("SUCCESS"))), Bucket::Pass);
        assert_eq!(
            check_bucket(&cr("", None, Some("PENDING"))),
            Bucket::Pending
        );
        assert_eq!(check_bucket(&cr("", None, Some("ERROR"))), Bucket::Fail);
    }

    #[test]
    fn parses_gh_pr_view_and_summarizes() {
        let json = r#"{
            "number": 42, "title": "Add thing", "state": "OPEN",
            "url": "https://example/pr/42", "isDraft": false,
            "headRefName": "sz/add-thing", "baseRefName": "main",
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
                {"name":"test","status":"COMPLETED","conclusion":"FAILURE"},
                {"name":"lint","status":"IN_PROGRESS"},
                {"context":"legacy","state":"PENDING"}
            ]
        }"#;
        let mut pr: PrStatus = serde_json::from_str(json).expect("parse");
        pr.checks = summarize(&pr.status_check_rollup);
        assert_eq!(pr.number, 42);
        assert_eq!(pr.checks.total, 4);
        assert_eq!(pr.checks.passed, 1);
        assert_eq!(pr.checks.failed, 1);
        assert_eq!(pr.checks.pending, 2);
    }

    #[test]
    fn panel_state_serializes_with_kind_tag() {
        let panel = PrPanel {
            state: PanelState::NoPr,
            worktree: "/tmp/wt".into(),
            branch: "sz/x".into(),
            fetched_at: 0,
        };
        let v: serde_json::Value = serde_json::to_value(&panel).unwrap();
        assert_eq!(v["kind"], "no_pr");
        assert_eq!(v["branch"], "sz/x");
    }

    #[test]
    fn pr_variant_flattens_for_the_panel() {
        let json = r#"{"number":7,"title":"x","state":"OPEN","url":"u",
            "isDraft":false,"headRefName":"sz/x","baseRefName":"main",
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"APPROVED",
            "statusCheckRollup":[{"name":"b","status":"COMPLETED","conclusion":"SUCCESS"}]}"#;
        let mut pr: PrStatus = serde_json::from_str(json).unwrap();
        pr.checks = summarize(&pr.status_check_rollup);
        let panel = PrPanel {
            state: PanelState::Pr(Box::new(pr)),
            worktree: "/tmp/wt".into(),
            branch: "sz/x".into(),
            fetched_at: 0,
        };
        let v: serde_json::Value = serde_json::to_value(&panel).unwrap();
        assert_eq!(v["kind"], "pr");
        assert_eq!(v["number"], 7);
        assert_eq!(v["reviewDecision"], "APPROVED");
        assert_eq!(v["checks"]["passed"], 1);
        assert_eq!(v["branch"], "sz/x");
    }
}
