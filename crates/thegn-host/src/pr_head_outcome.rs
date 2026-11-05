//! What a finished PR-queue agent run did to the PR head (THE-549): the host half of the pure taxonomy in `thegn_core::pr_queue`.

use thegn_core::config_pr_queue::PrQueueConfig;
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
/// observation is local-only; acceptance 2 stays open) or when HEAD is unreadable.
pub(crate) fn observe_outcome(
    loc: &GitLoc,
    admitted: &str,
    remote_head: Option<&str>,
    lease_current: bool,
) -> Option<AgentHeadOutcome> {
    let GitLoc::Local(path) = loc else {
        return None;
    };
    let local = loc.git_out(&["rev-parse", "HEAD"])?;
    let descends = if local == admitted {
        None
    } else {
        thegn_core::util::git_is_ancestor(path, admitted, &local)
    };
    Some(classify_run(
        admitted,
        Some(&local),
        descends,
        remote_head,
        lease_current,
    ))
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

#[cfg(test)]
mod tests {
    use super::{classify_run, observe_outcome, outcome_warning};
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
