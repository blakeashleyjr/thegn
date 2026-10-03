//! Scope/bounds tests that drive a fake `gh` shell script (Unix-only: needs
//! `sh` and executable-bit permissions). No real gh, no network.

use super::*;
use crate::issue::IssueError;
use thegn_core::issue::IssueStatus;

// ---- fake-`gh` driven scope/bounds tests (no real gh, no network) -----

fn make_exec(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Fake gh: logs argv (one line per call) and answers `create` with
/// `create_url`; `view` echoes the repo it was asked for so a wrong scope
/// is visible in the result.
fn fake_gh(dir: &std::path::Path, create_url: &str) -> std::path::PathBuf {
    let script = dir.join("gh");
    let log = dir.join("argv.log");
    let body = format!(
        r#"#!/bin/sh
echo "$*" >> {log}
case "$2" in
  create) echo {create_url} ;;
  view|close|reopen|edit)
repo=""; num="$3"
while [ $# -gt 0 ]; do [ "$1" = "--repo" ] && repo="$2"; shift; done
[ -z "$repo" ] && repo="cwd/default"
echo '{{"number":'"$num"',"title":"t","state":"OPEN","url":"https://github.com/'"$repo"'/issues/'"$num"'"}}' ;;
  *) echo '[]' ;;
esac
"#,
        log = log.display()
    );
    make_exec(&script, &body);
    script
}

fn backend(
    dir: &std::path::Path,
    gh: std::path::PathBuf,
    flags: &[&str],
    anchored: bool,
) -> GitHubIssuesBackend {
    let mut b = GitHubIssuesBackend::new(flags.iter().map(|s| s.to_string()).collect());
    b.program = Some(gh.into_os_string());
    if anchored {
        b.set_dir(Some(dir.to_path_buf()));
    }
    b
}

fn calls(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn draft(project: Option<&str>) -> IssueDraft {
    IssueDraft {
        title: "t".into(),
        project_id: project.map(str::to_owned),
        ..Default::default()
    }
}

#[tokio::test]
async fn create_draft_override_scopes_create_and_view() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "https://github.com/drafted/repo/issues/5");
    let b = backend(d.path(), gh, &["--repo", "configured/repo"], true);
    let issue = b.create_issue(&draft(Some("drafted/repo"))).await.unwrap();
    assert_eq!(issue.id, "github:drafted/repo#5");
    let c = calls(d.path());
    assert!(c[0].contains("--repo drafted/repo"), "{c:?}");
    assert!(!c[0].contains("configured/repo"), "{c:?}");
    assert!(c[1].contains("--repo drafted/repo"), "{c:?}");
}

#[tokio::test]
async fn create_uses_configured_repo_and_rejects_foreign_result() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "https://github.com/other/repo/issues/9");
    let b = backend(d.path(), gh, &["--repo", "configured/repo"], true);
    let err = b.create_issue(&draft(None)).await.unwrap_err();
    assert!(matches!(err, IssueError::Parse(_)), "{err:?}");
    assert!(calls(d.path())[0].contains("--repo configured/repo"));
    assert_eq!(calls(d.path()).len(), 1, "no follow-up view on mismatch");
}

#[tokio::test]
async fn mutations_without_any_scope_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "https://github.com/o/r/issues/1");
    let b = backend(d.path(), gh, &[], false);
    assert!(matches!(
        b.create_issue(&draft(None)).await,
        Err(IssueError::Policy(_))
    ));
    let patch = IssuePatch {
        title: Some("x".into()),
        ..Default::default()
    };
    assert!(matches!(
        b.update_issue("github:7", &patch).await,
        Err(IssueError::Policy(_))
    ));
    assert!(calls(d.path()).is_empty(), "gh never invoked");
}

#[tokio::test]
async fn same_number_issues_stay_in_their_repo() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "");
    let b = backend(d.path(), gh, &["--repo", "acct/default"], true);
    // scoped id wins over the account default
    let a = b.get_issue("github:a/one#3").await.unwrap();
    assert_eq!(a.issue.id, "github:a/one#3");
    // legacy bare id picks up the configured repo, not cwd
    let l = b.get_issue("github:3").await.unwrap();
    assert_eq!(l.issue.id, "github:acct/default#3");
    let p = IssuePatch {
        status: Some(IssueStatus::Done),
        ..Default::default()
    };
    let u = b.update_issue("3", &p).await.unwrap();
    assert_eq!(u.id, "github:acct/default#3");
    assert!(calls(d.path()).iter().all(|c| c.contains("--repo")));
}

#[tokio::test]
async fn conflicting_or_malformed_repo_flags_are_rejected() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "");
    let b = backend(d.path(), gh.clone(), &["--repo", "a/b", "-R", "c/d"], true);
    assert!(matches!(
        b.create_issue(&draft(None)).await,
        Err(IssueError::Parse(_))
    ));
    let b = backend(d.path(), gh, &[], true);
    assert!(matches!(
        b.create_issue(&draft(Some("../evil"))).await,
        Err(IssueError::Parse(_))
    ));
    assert!(calls(d.path()).is_empty());
}

#[tokio::test]
async fn gh_errors_are_classified_and_redacted() {
    let d = tempfile::tempdir().unwrap();
    let script = d.path().join("gh");
    make_exec(
        &script,
        "#!/bin/sh\necho 'HTTP 500 token ghp_SECRET123 failed' >&2\nexit 1\n",
    );
    let b = backend(d.path(), script, &["--repo", "o/r"], true);
    let e = b.get_issue("github:o/r#1").await.unwrap_err();
    assert!(matches!(e, IssueError::Api(_)), "{e:?}");
    assert!(!e.to_string().contains("ghp_SECRET"), "{e}");

    let script = d.path().join("gh2");
    make_exec(&script, "#!/bin/sh\necho 'run gh auth login' >&2\nexit 4\n");
    let b = backend(d.path(), script, &["--repo", "o/r"], true);
    assert!(matches!(
        b.get_issue("github:o/r#1").await,
        Err(IssueError::Auth(_))
    ));
}

#[tokio::test]
async fn hung_gh_times_out_and_missing_gh_is_reported() {
    let d = tempfile::tempdir().unwrap();
    let script = d.path().join("gh");
    make_exec(&script, "#!/bin/sh\nsleep 30\n");
    let mut b = backend(d.path(), script, &[], true);
    b.limits.timeout = std::time::Duration::from_millis(150);
    assert!(matches!(
        b.get_issue("github:o/r#1").await,
        Err(IssueError::Timeout(_))
    ));
    b.program = Some(d.path().join("nope").into_os_string());
    let e = b.get_issue("github:o/r#1").await.unwrap_err();
    assert!(e.to_string().contains("not installed"), "{e}");
}

#[tokio::test]
async fn bare_id_read_without_any_scope_fails_closed() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "");
    let b = backend(d.path(), gh, &[], false);
    assert!(matches!(
        b.get_issue("github:7").await,
        Err(IssueError::Policy(_))
    ));
    assert!(calls(d.path()).is_empty(), "gh never invoked");
}

#[tokio::test]
async fn attached_short_repo_flag_scopes_and_disagreement_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let gh = fake_gh(d.path(), "");
    let b = backend(d.path(), gh.clone(), &["-Ro/r"], false);
    let got = b.get_issue("github:3").await.unwrap();
    assert_eq!(got.issue.id, "github:o/r#3");
    let b = backend(d.path(), gh.clone(), &["-Ro/r", "--repo=a/b"], true);
    assert!(matches!(
        b.list_issues(&IssueFilter::default()).await,
        Err(IssueError::Parse(_))
    ));
    // a filter repo that disagrees with the configured flag is rejected too
    let b = backend(d.path(), gh, &["-R", "o/r"], true);
    let f = IssueFilter {
        repo: Some("x/y".into()),
        ..Default::default()
    };
    assert!(matches!(b.list_issues(&f).await, Err(IssueError::Parse(_))));
}
