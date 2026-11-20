//! `thegn env sync-retry [--json]`: retry retained provider sync pull-backs.
//!
//! This command only pulls data back into an existing worktree. It never
//! destroys or tears down the provider sandbox; that remains an explicit
//! `env down` operation after the user has checked the result.

use std::path::Path;

use anyhow::Result;
use thegn_core::outln;
use thegn_svc::sync_journal::{SyncJournalEntry, SyncState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryOutcome {
    Pulled,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RetryRow {
    pub worktree: String,
    pub env: String,
    pub sandbox: String,
    pub outcome: RetryOutcome,
    pub detail: Option<String>,
}

const WORKTREE_GONE: &str = "worktree directory is gone; the entry is kept (thegn doctor lists it)";
const PULL_IN_PROGRESS: &str = "a pull may be running in another thegn process";
const PULL_ON_CLOSE: &str = "pull-back runs when the worktree closes";

pub fn run(json: bool) -> Result<()> {
    let rows = retry_failed()?;

    if json {
        super::emit_json(&serde_json::json!({ "rows": rows }))?;
    } else if !rows.is_empty() {
        let table_rows = rows
            .iter()
            .map(|row| {
                vec![
                    row.worktree.clone(),
                    row.env.clone(),
                    row.sandbox.clone(),
                    outcome_name(row.outcome).to_owned(),
                    row.detail.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        outln!("{}", super::fleet::table(&table_rows));
        for row in rows
            .iter()
            .filter(|row| row.outcome == RetryOutcome::Pulled)
        {
            outln!(
                "sandbox {} is kept; thegn env down --worktree {} removes it",
                row.sandbox,
                row.worktree
            );
        }
    }

    let failed = rows
        .iter()
        .filter(|row| row.outcome == RetryOutcome::Failed)
        .count();
    if failed > 0 {
        anyhow::bail!("{failed} pull-back(s) still failing");
    }
    Ok(())
}

/// Retry every failed pull-back once (the `env sync-retry` core). Never destroys a sandbox.
pub(crate) fn retry_failed() -> Result<Vec<RetryRow>> {
    let dir = thegn_svc::sync_journal::dir();
    let entries = thegn_svc::sync_journal::list_at(&dir)?;
    Ok(retry_with(
        entries,
        &|path| Path::new(path).is_dir(),
        &mut |path| match thegn_svc::sync_journal::load_at(&dir, path) {
            Ok(Some(entry)) if matches!(entry.state, SyncState::Failed { .. }) => {
                crate::agent::deprovision_sync(path)
            }
            Ok(Some(_)) => Err(
                "the journal entry changed since it was listed (no longer failed); not pulled"
                    .to_owned(),
            ),
            Ok(None) => {
                Err("the journal entry was removed since it was listed; not pulled".to_owned())
            }
            Err(error) => Err(format!("journal entry unreadable: {error:#}")),
        },
    ))
}

/// One toast line: "pull-backs: N pulled, M still failing, K skipped" or "no failed pull-backs".
pub(crate) fn summary_line(rows: &[RetryRow]) -> String {
    if rows.is_empty() {
        return "no failed pull-backs".to_owned();
    }

    let pulled = rows
        .iter()
        .filter(|row| row.outcome == RetryOutcome::Pulled)
        .count();
    let failed = rows
        .iter()
        .filter(|row| row.outcome == RetryOutcome::Failed)
        .count();
    let skipped = rows
        .iter()
        .filter(|row| row.outcome == RetryOutcome::Skipped)
        .count();
    format!("pull-backs: {pulled} pulled, {failed} still failing, {skipped} skipped")
}

/// The palette action: runs [`retry_failed`] off the loop, then posts one toast and wakes the loop.
pub(crate) fn spawn_retry(
    toast_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: termwiz::terminal::TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let (message, priority) = match retry_failed() {
            Ok(rows) => {
                let priority = if rows.iter().any(|row| row.outcome == RetryOutcome::Failed) {
                    thegn_core::notification::Priority::Alert
                } else {
                    thegn_core::notification::Priority::Info
                };
                (summary_line(&rows), priority)
            }
            Err(error) => {
                let first_line = format!("{error:#}")
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect::<String>();
                (
                    format!("pull-back retry failed: {first_line}"),
                    thegn_core::notification::Priority::Alert,
                )
            }
        };
        if let Err(error) = toast_tx.send(crate::hydrate::RefreshKind::Toast { message, priority })
        {
            tracing::debug!(target: "thegn::env", %error, "pull-back retry toast receiver is gone");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::env", %error, "pull-back retry toast wake failed");
        }
    });
}

pub(crate) fn retry_with(
    entries: Vec<SyncJournalEntry>,
    worktree_exists: &dyn Fn(&str) -> bool,
    pull: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Vec<RetryRow> {
    entries
        .into_iter()
        .map(|entry| {
            let worktree = sanitize(&entry.worktree);
            let env = sanitize(&entry.env_name);
            let sandbox = sanitize(&entry.sandbox);
            let (outcome, detail) = match entry.state {
                SyncState::Failed { reason: _ } if !worktree_exists(&entry.worktree) => {
                    (RetryOutcome::Skipped, Some(WORKTREE_GONE.to_owned()))
                }
                SyncState::Failed { reason: _ } => match pull(&entry.worktree) {
                    Ok(()) => (RetryOutcome::Pulled, None),
                    Err(error) => (RetryOutcome::Failed, Some(sanitize(&error))),
                },
                SyncState::Uploaded => (RetryOutcome::Skipped, Some(PULL_ON_CLOSE.to_owned())),
                SyncState::Pulling => (RetryOutcome::Skipped, Some(PULL_IN_PROGRESS.to_owned())),
            };
            RetryRow {
                worktree,
                env,
                sandbox,
                outcome,
                detail,
            }
        })
        .collect()
}

fn outcome_name(outcome: RetryOutcome) -> &'static str {
    match outcome {
        RetryOutcome::Pulled => "pulled",
        RetryOutcome::Failed => "failed",
        RetryOutcome::Skipped => "skipped",
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .take(300)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use std::collections::BTreeMap;

    fn entry(worktree: &str, state: SyncState) -> SyncJournalEntry {
        SyncJournalEntry {
            version: thegn_svc::sync_journal::JOURNAL_VERSION,
            worktree: worktree.into(),
            env_name: "remote".into(),
            provider: "sprites".into(),
            sandbox: "sprite-1".into(),
            workdir: "/workspace".into(),
            baseline: BTreeMap::new(),
            state,
            uploaded_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn failed_entry_with_worktree_is_pulled() {
        let mut pulled = Vec::new();
        let rows = retry_with(
            vec![entry(
                "/worktree/a",
                SyncState::Failed {
                    reason: "previous failure".into(),
                },
            )],
            &|path| path == "/worktree/a",
            &mut |path| {
                pulled.push(path.to_owned());
                Ok(())
            },
        );

        assert_eq!(pulled, ["/worktree/a"]);
        assert_eq!(rows[0].outcome, RetryOutcome::Pulled);
        assert_eq!(rows[0].detail, None);
    }

    #[test]
    fn failed_entry_without_worktree_is_skipped_and_kept() {
        let mut called = false;
        let rows = retry_with(
            vec![entry(
                "/worktree/gone",
                SyncState::Failed {
                    reason: "previous failure".into(),
                },
            )],
            &|_| false,
            &mut |_| {
                called = true;
                Ok(())
            },
        );

        assert!(!called);
        assert_eq!(rows[0].outcome, RetryOutcome::Skipped);
        assert_eq!(rows[0].detail.as_deref(), Some(WORKTREE_GONE));
    }

    #[test]
    fn uploaded_and_pulling_are_skipped() {
        let rows = retry_with(
            vec![
                entry("/worktree/uploaded", SyncState::Uploaded),
                entry("/worktree/pulling", SyncState::Pulling),
            ],
            &|_| panic!("non-failed entries must not inspect the worktree"),
            &mut |_| panic!("non-failed entries must not pull"),
        );

        assert_eq!(rows[0].outcome, RetryOutcome::Skipped);
        assert_eq!(rows[0].detail.as_deref(), Some(PULL_ON_CLOSE));
        assert_eq!(rows[1].outcome, RetryOutcome::Skipped);
        assert_eq!(rows[1].detail.as_deref(), Some(PULL_IN_PROGRESS));
    }

    #[test]
    fn pull_error_is_failed_and_sanitized() {
        let rows = retry_with(
            vec![entry(
                "/worktree/a",
                SyncState::Failed {
                    reason: "old failure".into(),
                },
            )],
            &|_| true,
            &mut |_| Err("bad\n\u{1b}[31mnetwork\u{7}".into()),
        );

        let detail = rows[0].detail.as_deref().expect("pull error detail");
        assert_eq!(rows[0].outcome, RetryOutcome::Failed);
        assert!(!detail.chars().any(char::is_control));
        assert!(detail.len() <= 300);
        assert_eq!(detail, "bad??[31mnetwork?");
    }

    #[test]
    fn summary_line_table() {
        let row = |outcome| RetryRow {
            worktree: "/worktree/a".into(),
            env: "remote".into(),
            sandbox: "sprite-1".into(),
            outcome,
            detail: None,
        };
        let cases = [
            (vec![], "no failed pull-backs"),
            (
                vec![row(RetryOutcome::Pulled), row(RetryOutcome::Pulled)],
                "pull-backs: 2 pulled, 0 still failing, 0 skipped",
            ),
            (
                vec![
                    row(RetryOutcome::Pulled),
                    row(RetryOutcome::Failed),
                    row(RetryOutcome::Skipped),
                ],
                "pull-backs: 1 pulled, 1 still failing, 1 skipped",
            ),
            (
                vec![row(RetryOutcome::Skipped), row(RetryOutcome::Skipped)],
                "pull-backs: 0 pulled, 0 still failing, 2 skipped",
            ),
        ];

        for (rows, expected) in cases {
            assert_eq!(summary_line(&rows), expected);
        }
    }

    #[test]
    fn clap_parses_env_sync_retry() {
        let cli = crate::Cli::try_parse_from(["thegn", "env", "sync-retry", "--json"])
            .expect("env sync-retry should parse");
        assert!(matches!(
            cli.command,
            Some(crate::Command::Env {
                action: crate::cmd::env::Action::SyncRetry { json: true }
            })
        ));
    }
}
