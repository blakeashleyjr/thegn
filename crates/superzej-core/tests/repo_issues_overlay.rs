//! Integration coverage for the per-repo `[issues]` overlay (`Config::repo_issues`)
//! — kept out of the god-file `config.rs` per the file-size ratchet.

use superzej_core::config::{Config, IssueProviderKind};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sz-cfg-issues-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn repo_issues_overlay_scopes_linear_and_jira() {
    let mut cfg = Config::default();
    cfg.issues.linear.team_id = "GLOBAL".into();
    cfg.issues.jira.project_key = "GLOB".into();
    cfg.issues.providers = vec![IssueProviderKind::Github];
    let dir = tmpdir("overlay");
    std::fs::write(
        dir.join(".superzej.toml"),
        "[issues]\nproviders = [\"linear\", \"jira\"]\n\
         [issues.linear]\nteam_id = \"TEAM-42\"\n\
         [issues.jira]\nproject_key = \"ACME\"\n",
    )
    .unwrap();
    let eff = cfg.repo_issues(Some(&dir));
    assert_eq!(eff.linear.team_id, "TEAM-42");
    assert_eq!(eff.jira.project_key, "ACME");
    assert_eq!(
        eff.providers,
        vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
    );
    // No repo_root ⇒ global config verbatim (no overlay applied).
    assert_eq!(cfg.repo_issues(None).linear.team_id, "GLOBAL");
    // A repo with no [issues] overlay inherits the global config.
    let empty = tmpdir("empty");
    assert_eq!(cfg.repo_issues(Some(&empty)).jira.project_key, "GLOB");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&empty);
}
