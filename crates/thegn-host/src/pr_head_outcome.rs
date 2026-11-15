//! What a finished PR-queue agent run did to the PR head (THE-549): the host half of the pure taxonomy in `thegn_core::pr_queue`.

use thegn_core::config_pr_queue::PrQueueConfig;
use thegn_core::git_run::{GitOutcome, GitTextError};
use thegn_core::pr_queue::{self, AgentHeadOutcome, AgentRunObservation};
use thegn_core::remote::GitLoc;

/// Pure: classify one finished run from already-read facts. `infra_failure` is
/// always false here: a run that never started returns earlier in the driver.
pub(crate) fn classify_run(
    admitted: &str,
    local_head: Option<&str>,
    local_descends: Option<bool>,
    remote_head: Option<&str>,
    lease_current: bool,
) -> AgentHeadOutcome {
    pr_queue::classify_agent_head(&AgentRunObservation {
        admitted,
        local_head,
        remote_head,
        local_descends,
        infra_failure: false,
        lease_current,
    })
}

/// Reads the worktree HEAD and ancestry (two local git spawns, off the UI loop:
/// the driver runs on a worker). None for a remote/provider checkout (the
/// observation is local-only; acceptance 2 stays open). An unreadable local HEAD
/// is classified as Unverified so callers cannot claim agent progress.
pub(crate) fn observe_outcome(
    loc: &GitLoc,
    admitted: &str,
    remote_head: Option<&str>,
    lease_current: bool,
) -> Option<AgentHeadOutcome> {
    match observe_outcome_with_reason(loc, admitted, remote_head, lease_current) {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(target: "thegn::prq", %error, "could not read local PR-agent HEAD; treating the outcome as unverified");
            Some(AgentHeadOutcome::Unverified)
        }
    }
}

fn observe_outcome_with_reason(
    loc: &GitLoc,
    admitted: &str,
    remote_head: Option<&str>,
    lease_current: bool,
) -> Result<Option<AgentHeadOutcome>, String> {
    let GitLoc::Local(path) = loc else {
        return Ok(None);
    };
    let Some(local) = read_head(loc)? else {
        return Ok(Some(classify_run(
            admitted,
            None,
            None,
            remote_head,
            lease_current,
        )));
    };
    let descends = if local == admitted {
        None
    } else {
        thegn_core::util::git_is_ancestor(path, admitted, &local)
    };
    Ok(Some(classify_run(
        admitted,
        Some(&local),
        descends,
        remote_head,
        lease_current,
    )))
}

fn read_head(loc: &GitLoc) -> Result<Option<String>, String> {
    match loc.git_text(&["rev-parse", "--verify", "--quiet", "HEAD"]) {
        Ok(head) if head.is_empty() => Ok(None),
        Ok(head) => Ok(Some(head)),
        Err(error) if is_unborn_head(&error) => Ok(None),
        Err(error) => Err(format!("git HEAD read failed: {error}")),
    }
}

fn is_unborn_head(error: &GitTextError) -> bool {
    matches!(
        error,
        GitTextError::Failed(GitOutcome::Exit {
            code: Some(1),
            stderr_tail,
        }) if stderr_tail.is_empty()
    )
}

/// The operator-facing line for an outcome, or None when nothing needs saying
/// (AgentProgress, InfraFailure, StaleCompletion are reported elsewhere).
pub(crate) fn outcome_warning(
    number: u64,
    outcome: AgentHeadOutcome,
    cfg: &PrQueueConfig,
) -> Option<String> {
    let effect = pr_queue::head_budget_effect(outcome, cfg);
    let warning = match outcome {
        AgentHeadOutcome::Unchanged => format!(
            "PR #{number}: the agent finished without changing the PR head; the next refresh re-evaluates the row"
        ),
        AgentHeadOutcome::LocalOnly => format!(
            "PR #{number}: the agent committed locally but the PR head did not move; nothing was pushed"
        ),
        AgentHeadOutcome::TeammatePush => {
            let suffix = if effect.hold {
                "; the row is held for a human"
            } else if effect.refill {
                "; the attempt budget is refilled"
            } else {
                ""
            };
            format!("PR #{number}: someone else pushed to the PR while the agent ran{suffix}")
        }
        AgentHeadOutcome::Unverified => {
            let suffix = if effect.hold {
                "; the row is held for a human"
            } else {
                "; the attempt budget is kept"
            };
            format!(
                "PR #{number}: thegn could not prove who moved the PR head (a force push, a short object id or an unreadable forge head){suffix}"
            )
        }
        AgentHeadOutcome::AgentProgress
        | AgentHeadOutcome::InfraFailure
        | AgentHeadOutcome::StaleCompletion => return None,
    };
    Some(warning)
}

/// Classify one finished run against the admitted head and render its operator
/// sentence in one step (None when nothing needs saying, or for a non-local
/// checkout). Shared by the PR driver's sibling call sites.
pub(crate) fn run_warning(
    loc: &GitLoc,
    number: u64,
    admitted: &str,
    remote_head: Option<&str>,
    lease_current: bool,
    cfg: &PrQueueConfig,
) -> Option<String> {
    match observe_outcome_with_reason(loc, admitted, remote_head, lease_current) {
        Ok(outcome) => outcome.and_then(|outcome| outcome_warning(number, outcome, cfg)),
        Err(error) => Some(format!(
            "PR #{number}: thegn could not prove who moved the PR head ({error}); the row is held for a human"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_run, observe_outcome, observe_outcome_with_reason, outcome_warning, read_head,
        run_warning,
    };
    use thegn_core::config_pr_queue::PrQueueConfig;
    use thegn_core::pr_queue::{self, AgentHeadOutcome, AgentRunObservation};
    use thegn_core::remote::GitLoc;

    #[test]
    fn classify_run_covers_every_outcome() {
        let admitted = "a".repeat(40);
        let local = "b".repeat(40);
        let teammate = "c".repeat(40);
        let cases = [
            (
                Some(local.as_str()),
                Some(true),
                Some(local.as_str()),
                true,
                AgentHeadOutcome::AgentProgress,
            ),
            (
                Some(admitted.as_str()),
                None,
                Some(admitted.as_str()),
                true,
                AgentHeadOutcome::Unchanged,
            ),
            (
                Some(local.as_str()),
                Some(false),
                Some(admitted.as_str()),
                true,
                AgentHeadOutcome::LocalOnly,
            ),
            (
                Some(local.as_str()),
                Some(false),
                Some(teammate.as_str()),
                true,
                AgentHeadOutcome::TeammatePush,
            ),
            (
                Some(local.as_str()),
                Some(false),
                None,
                true,
                AgentHeadOutcome::Unverified,
            ),
            (
                Some(local.as_str()),
                Some(false),
                Some(teammate.as_str()),
                false,
                AgentHeadOutcome::StaleCompletion,
            ),
        ];

        for (local_head, descends, remote_head, lease_current, expected) in cases {
            assert_eq!(
                classify_run(&admitted, local_head, descends, remote_head, lease_current),
                expected
            );
        }
        assert_eq!(
            pr_queue::classify_agent_head(&AgentRunObservation {
                admitted: &admitted,
                local_head: None,
                remote_head: None,
                local_descends: None,
                infra_failure: true,
                lease_current: true,
            }),
            AgentHeadOutcome::InfraFailure
        );
    }

    #[test]
    fn outcome_warning_text_follows_the_pause_and_reset_settings() {
        let paused = PrQueueConfig {
            enabled: true,
            pause_on_foreign_push: true,
            reset_attempts_on_push: true,
            ..Default::default()
        };
        let refill = PrQueueConfig {
            pause_on_foreign_push: false,
            reset_attempts_on_push: true,
            ..Default::default()
        };
        let unchanged = PrQueueConfig {
            pause_on_foreign_push: false,
            reset_attempts_on_push: false,
            ..Default::default()
        };

        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::TeammatePush, &paused).as_deref(),
            Some(
                "PR #7: someone else pushed to the PR while the agent ran; the row is held for a human"
            )
        );
        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::TeammatePush, &refill).as_deref(),
            Some(
                "PR #7: someone else pushed to the PR while the agent ran; the attempt budget is refilled"
            )
        );
        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::TeammatePush, &unchanged).as_deref(),
            Some("PR #7: someone else pushed to the PR while the agent ran")
        );
        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::Unverified, &unchanged).as_deref(),
            Some(
                "PR #7: thegn could not prove who moved the PR head (a force push, a short object id or an unreadable forge head); the attempt budget is kept"
            )
        );
        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::Unverified, &paused).as_deref(),
            Some(
                "PR #7: thegn could not prove who moved the PR head (a force push, a short object id or an unreadable forge head); the row is held for a human"
            )
        );
        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::Unchanged, &paused).as_deref(),
            Some(
                "PR #7: the agent finished without changing the PR head; the next refresh re-evaluates the row"
            )
        );
        assert_eq!(
            outcome_warning(7, AgentHeadOutcome::LocalOnly, &paused).as_deref(),
            Some(
                "PR #7: the agent committed locally but the PR head did not move; nothing was pushed"
            )
        );
    }

    #[test]
    fn outcome_warning_is_silent_for_progress_infra_and_stale() {
        let cfg = PrQueueConfig::default();
        for outcome in [
            AgentHeadOutcome::AgentProgress,
            AgentHeadOutcome::InfraFailure,
            AgentHeadOutcome::StaleCompletion,
        ] {
            assert_eq!(outcome_warning(1, outcome, &cfg), None);
        }
    }

    #[test]
    fn observe_outcome_is_local_only() {
        let admitted = "a".repeat(40);
        let remote = GitLoc::from_db(
            "/repo",
            Some(r#"{"host":"example.invalid","port":22,"path":"/repo"}"#),
        );
        assert!(matches!(&remote, GitLoc::Remote { .. }));
        assert_eq!(
            observe_outcome(&remote, &admitted, Some(&admitted), true),
            None
        );

        let provider = GitLoc::from_db(
            "/repo",
            Some(r#"{"control_prefix":["provider","exec"],"path":"/repo"}"#),
        );
        assert!(matches!(&provider, GitLoc::Provider { .. }));
        assert_eq!(
            observe_outcome(&provider, &admitted, Some(&admitted), true),
            None
        );
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture runs isolated local git commands"
    )]
    fn git_fixture_command(dir: &std::path::Path, args: &[&str]) -> String {
        let output = thegn_core::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn observe_outcome_reads_a_local_checkout() {
        let dir = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(dir.path()).unwrap();
        git_fixture_command(
            dir.path(),
            &["-c", "commit.gpgsign=false", "init", "--quiet"],
        );
        git_fixture_command(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git_fixture_command(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("tracked"), "base\n").unwrap();
        git_fixture_command(dir.path(), &["add", "tracked"]);
        git_fixture_command(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "base"],
        );
        let admitted = git_fixture_command(dir.path(), &["rev-parse", "HEAD"]);
        let loc = GitLoc::Local(dir.path().to_path_buf());

        assert_eq!(
            observe_outcome(&loc, &admitted, Some(&admitted), true),
            Some(AgentHeadOutcome::Unchanged)
        );

        std::fs::write(dir.path().join("tracked"), "local agent commit\n").unwrap();
        git_fixture_command(dir.path(), &["add", "tracked"]);
        git_fixture_command(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "agent"],
        );
        let local = git_fixture_command(dir.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            observe_outcome(&loc, &admitted, Some(&admitted), true),
            Some(AgentHeadOutcome::LocalOnly)
        );
        assert_eq!(
            observe_outcome(&loc, &admitted, Some(&local), true),
            Some(AgentHeadOutcome::AgentProgress)
        );
    }

    #[test]
    fn local_head_probe_table_keeps_unborn_distinct_and_fails_closed_on_git_error() {
        let fixture = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(fixture.path()).unwrap();
        git_fixture_command(
            fixture.path(),
            &["-c", "commit.gpgsign=false", "init", "--quiet"],
        );
        git_fixture_command(
            fixture.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git_fixture_command(fixture.path(), &["config", "user.name", "Test"]);
        std::fs::write(fixture.path().join("tracked"), "base\n").unwrap();
        git_fixture_command(fixture.path(), &["add", "tracked"]);
        git_fixture_command(
            fixture.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "base"],
        );
        let admitted = git_fixture_command(fixture.path(), &["rev-parse", "HEAD"]);
        let loc = GitLoc::Local(fixture.path().to_path_buf());
        assert_eq!(
            read_head(&loc),
            Ok(Some(admitted.clone())),
            "HEAD == admitted"
        );

        std::fs::write(fixture.path().join("tracked"), "descendant\n").unwrap();
        git_fixture_command(fixture.path(), &["add", "tracked"]);
        git_fixture_command(
            fixture.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "descendant"],
        );
        let descendant = git_fixture_command(fixture.path(), &["rev-parse", "HEAD"]);
        assert_ne!(descendant, admitted, "HEAD descends from admitted");

        git_fixture_command(fixture.path(), &["checkout", "--orphan", "diverged"]);
        git_fixture_command(fixture.path(), &["rm", "-rf", "."]);
        std::fs::write(fixture.path().join("diverged"), "diverged\n").unwrap();
        git_fixture_command(fixture.path(), &["add", "diverged"]);
        git_fixture_command(
            fixture.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "diverged"],
        );
        let diverged = git_fixture_command(fixture.path(), &["rev-parse", "HEAD"]);
        assert_ne!(diverged, admitted, "HEAD diverged from admitted");
        let foreign = "f".repeat(40);
        assert_eq!(
            observe_outcome(&loc, &admitted, Some(&foreign), true),
            Some(AgentHeadOutcome::TeammatePush),
            "diverged HEAD is not attributed to the agent"
        );

        let unborn = tempfile::tempdir().unwrap();
        git_fixture_command(
            unborn.path(),
            &["-c", "commit.gpgsign=false", "init", "--quiet"],
        );
        let unborn_loc = GitLoc::Local(unborn.path().to_path_buf());
        assert_eq!(read_head(&unborn_loc), Ok(None), "unborn HEAD");
        assert_eq!(
            observe_outcome(&unborn_loc, &admitted, Some(&admitted), true),
            Some(AgentHeadOutcome::Unchanged)
        );

        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(".git"), "gitdir: missing\n").unwrap();
        let broken_loc = GitLoc::Local(broken.path().to_path_buf());
        let error =
            observe_outcome_with_reason(&broken_loc, &admitted, Some(&admitted), true).unwrap_err();
        assert!(error.contains("git"), "{error}");
        assert_eq!(
            observe_outcome(&broken_loc, &admitted, Some(&admitted), true),
            Some(AgentHeadOutcome::Unverified)
        );
    }

    #[test]
    fn run_warning_renders_the_local_observation() {
        let dir = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(dir.path()).unwrap();
        git_fixture_command(
            dir.path(),
            &["-c", "commit.gpgsign=false", "init", "--quiet"],
        );
        git_fixture_command(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git_fixture_command(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("tracked"), "base\n").unwrap();
        git_fixture_command(dir.path(), &["add", "tracked"]);
        git_fixture_command(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "base"],
        );
        let admitted = git_fixture_command(dir.path(), &["rev-parse", "HEAD"]);
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let cfg = PrQueueConfig::default();

        assert_eq!(
            run_warning(&loc, 7, &admitted, Some(&admitted), true, &cfg).as_deref(),
            Some(
                "PR #7: the agent finished without changing the PR head; the next refresh re-evaluates the row"
            )
        );

        std::fs::write(dir.path().join("tracked"), "local agent commit\n").unwrap();
        git_fixture_command(dir.path(), &["add", "tracked"]);
        git_fixture_command(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "agent"],
        );
        let local = git_fixture_command(dir.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            run_warning(&loc, 7, &admitted, Some(&admitted), true, &cfg).as_deref(),
            Some(
                "PR #7: the agent committed locally but the PR head did not move; nothing was pushed"
            )
        );
        assert_eq!(
            run_warning(&loc, 7, &admitted, Some(&local), true, &cfg),
            None
        );
        assert_eq!(
            run_warning(&loc, 7, &admitted, Some(&admitted), false, &cfg),
            None
        );
    }

    #[test]
    fn run_warning_is_none_for_a_remote_checkout() {
        let admitted = "a".repeat(40);
        let remote = GitLoc::from_db(
            "/repo",
            Some(r#"{"host":"example.invalid","port":22,"path":"/repo"}"#),
        );
        assert_eq!(
            run_warning(
                &remote,
                7,
                &admitted,
                Some(&admitted),
                true,
                &PrQueueConfig::default()
            ),
            None
        );
    }

    #[test]
    fn teammate_push_warning_agrees_with_the_budget_functions() {
        for pause_on_foreign_push in [false, true] {
            for reset_attempts_on_push in [false, true] {
                let cfg = PrQueueConfig {
                    pause_on_foreign_push,
                    reset_attempts_on_push,
                    ..Default::default()
                };
                let warning = outcome_warning(1, AgentHeadOutcome::TeammatePush, &cfg).unwrap();
                let effect = pr_queue::head_budget_effect(AgentHeadOutcome::TeammatePush, &cfg);
                assert_eq!(effect.hold, pause_on_foreign_push);
                assert_eq!(effect.refill, reset_attempts_on_push);
                assert_eq!(
                    warning.ends_with("; the row is held for a human"),
                    effect.hold
                );
                assert_eq!(
                    warning.ends_with("; the attempt budget is refilled"),
                    effect.refill && !effect.hold
                );
            }
        }
    }
}
