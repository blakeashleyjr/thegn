//! Pure selection and typed refusal for merge-queue pipeline admission.

use crate::issue::{AgentDispatch, AgentDispatchStatus};
use std::fmt;

/// The newest pipeline row that controls admission for a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatingRow<'a> {
    /// No pipeline row exists; legacy worktrees retain their existing behavior.
    None,
    /// The newest applicable row reached a terminal successful status.
    Finished(&'a AgentDispatch),
    /// The newest applicable row has not reached a terminal status.
    Unfinished(&'a AgentDispatch),
    /// The newest applicable row has a status this build cannot interpret.
    Unrecognized(&'a AgentDispatch),
}

/// Select the newest pipeline row, ignoring failed and abandoned history.
pub fn gating_row(rows_newest_first: &[AgentDispatch]) -> GatingRow<'_> {
    for row in rows_newest_first {
        match row.status {
            AgentDispatchStatus::Failed | AgentDispatchStatus::Abandoned => continue,
            AgentDispatchStatus::Done | AgentDispatchStatus::Merged => {
                return GatingRow::Finished(row);
            }
            AgentDispatchStatus::Queued
            | AgentDispatchStatus::Spawning
            | AgentDispatchStatus::Running
            | AgentDispatchStatus::WaitingHuman
            | AgentDispatchStatus::PrOpen => return GatingRow::Unfinished(row),
            AgentDispatchStatus::Unknown => return GatingRow::Unrecognized(row),
        }
    }
    GatingRow::None
}

/// Typed refusal to enqueue a worktree whose newest pipeline row is not trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineMergeRefused {
    /// Worktree supplied to merge admission.
    pub worktree: String,
    /// Pipeline dispatch row that controls admission.
    pub row: i64,
    /// Specific gate or status reason.
    pub reason: String,
}

impl fmt::Display for PipelineMergeRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "merge admission refused for {}: pipeline row {} {}; run `thegn dispatch gate {}` on the committed HEAD, or `thegn merge add <worktree> --admin --why '<reason>'`",
            self.worktree, self.row, self.reason, self.row
        )
    }
}

impl std::error::Error for PipelineMergeRefused {}

#[cfg(test)]
mod tests {
    use super::{GatingRow, gating_row};
    use crate::db::Db;
    use crate::issue::{AgentDispatchStatus, NewDispatch};
    use crate::store::NotificationStore;

    fn dispatch(
        status: AgentDispatchStatus,
        path: &str,
        stage: Option<&str>,
        artifact: Option<&str>,
    ) -> crate::issue::AgentDispatch {
        let db = Db::open_memory().unwrap();
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage,
                artifact_path: artifact,
                ..NewDispatch::new("linear:THE-1155", path, "fixture")
            })
            .unwrap();
        let mut row = db.get_dispatch(id).unwrap().unwrap();
        row.status = status;
        row
    }

    #[test]
    fn gating_row_skips_failed_and_abandoned_and_classifies_every_status() {
        let statuses = [
            (AgentDispatchStatus::Queued, 0),
            (AgentDispatchStatus::Spawning, 0),
            (AgentDispatchStatus::Running, 0),
            (AgentDispatchStatus::WaitingHuman, 0),
            (AgentDispatchStatus::PrOpen, 0),
            (AgentDispatchStatus::Merged, 1),
            (AgentDispatchStatus::Abandoned, 2),
            (AgentDispatchStatus::Done, 1),
            (AgentDispatchStatus::Failed, 2),
            (AgentDispatchStatus::Unknown, 3),
        ];
        for (status, expected) in statuses {
            let row = dispatch(status, "/wt", Some("code"), Some("artifact"));
            let rows = [
                dispatch(AgentDispatchStatus::Failed, "/wt", Some("old"), Some("old")),
                dispatch(
                    AgentDispatchStatus::Abandoned,
                    "/wt",
                    Some("old"),
                    Some("old"),
                ),
                row,
            ];
            match (expected, gating_row(&rows)) {
                (0, GatingRow::Unfinished(found)) => assert_eq!(found.status, status),
                (1, GatingRow::Finished(found)) => assert_eq!(found.status, status),
                (3, GatingRow::Unrecognized(found)) => assert_eq!(found.status, status),
                (2, GatingRow::None) => {}
                (expected, actual) => panic!("unexpected classification {expected}: {actual:?}"),
            }
        }
    }

    #[test]
    fn pipeline_rows_for_worktree_matches_raw_or_canonical_and_ignores_plain_rows() {
        let db = Db::open_memory().unwrap();
        for (path, stage, artifact) in [
            ("/wt/a", Some("code"), Some("code.md")),
            ("/wt/a", None, None),
            ("/wt/b", Some("code"), None),
            ("/canonical/a", Some("review"), Some("review.md")),
        ] {
            db.put_agent_dispatch(NewDispatch {
                stage,
                artifact_path: artifact,
                ..NewDispatch::new("linear:THE-1155", path, "fixture")
            })
            .unwrap();
        }
        let rows = db
            .pipeline_rows_for_worktree("/wt/a", "/canonical/a")
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].stage.as_deref(), Some("review"));
        assert_eq!(rows[1].stage.as_deref(), Some("code"));
    }

    #[test]
    fn pipeline_merge_refusal_names_gate_and_override_commands() {
        let refusal = super::PipelineMergeRefused {
            worktree: "/wt/a".to_string(),
            row: 42,
            reason: "has no trusted gate".to_string(),
        };
        let text = refusal.to_string();
        assert!(text.contains("thegn dispatch gate 42"));
        assert!(text.contains("--admin --why"));
    }
}
