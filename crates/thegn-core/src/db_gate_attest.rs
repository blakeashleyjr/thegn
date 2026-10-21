//! Durable append-only records for trusted pipeline gate runs.
use crate::db::Db;
use crate::pipeline_gate_attest::{GateAttestation, GateOutcome, GateRunner};
use crate::store::NotificationStore;
use anyhow::Result;
use rusqlite::OptionalExtension as _;
use serde::Serialize;

/// A refusal to persist malformed or untrusted gate evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAttestationRefusal {
    WorkerRunner,
    MalformedOid { field: &'static str },
    EmptyCommand,
    EmptyLog,
    InvertedClock,
    PassWithoutZeroExit,
    MissingDispatch { id: i64 },
}

impl std::fmt::Display for GateAttestationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerRunner => f.write_str("a worker cannot record a gate attestation"),
            Self::MalformedOid { field } => {
                write!(f, "{field} is not a valid lowercase Git object ID")
            }
            Self::EmptyCommand => f.write_str("gate command is empty"),
            Self::EmptyLog => f.write_str("gate log path is empty"),
            Self::InvertedClock => f.write_str("gate end time precedes its start time"),
            Self::PassWithoutZeroExit => {
                f.write_str("a passing gate attestation requires exit status 0")
            }
            Self::MissingDispatch { id } => write!(f, "dispatch row {id} does not exist"),
        }
    }
}

impl std::error::Error for GateAttestationRefusal {}

/// One stored attestation, with stable strings suitable for JSON output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredGateAttestation {
    pub id: i64,
    pub dispatch_row: i64,
    pub repo: String,
    pub worktree: String,
    pub branch_ref: String,
    pub commit_oid: String,
    pub tree_oid: String,
    pub command: String,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub exit: Option<i32>,
    pub log_path: String,
    pub runner: String,
    pub outcome: GateOutcome,
    pub reason: String,
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Db {
    /// Append one validated trusted-runner attestation.
    pub fn record_gate_attestation(
        &self,
        att: &GateAttestation,
        outcome: GateOutcome,
        reason: &str,
    ) -> Result<i64> {
        if matches!(&att.runner, GateRunner::Worker) {
            return Err(GateAttestationRefusal::WorkerRunner.into());
        }
        for (field, oid) in [
            ("commit_oid", att.commit_oid.as_str()),
            ("tree_oid", att.tree_oid.as_str()),
        ] {
            if !valid_oid(oid) {
                return Err(GateAttestationRefusal::MalformedOid { field }.into());
            }
        }
        if att.command.trim().is_empty() {
            return Err(GateAttestationRefusal::EmptyCommand.into());
        }
        if att.log_path.trim().is_empty() {
            return Err(GateAttestationRefusal::EmptyLog.into());
        }
        if att.ended_ms < att.started_ms {
            return Err(GateAttestationRefusal::InvertedClock.into());
        }
        if outcome == GateOutcome::Passed && att.exit != Some(0) {
            return Err(GateAttestationRefusal::PassWithoutZeroExit.into());
        }
        if self.get_dispatch(att.dispatch_row)?.is_none() {
            return Err(GateAttestationRefusal::MissingDispatch {
                id: att.dispatch_row,
            }
            .into());
        }
        let conn = self.conn();
        conn.execute(
            "INSERT INTO pipeline_gate_attestations
             (dispatch_id,repo,worktree,branch_ref,commit_oid,tree_oid,command,started_ms,ended_ms,exit_code,outcome,reason,log_path,runner)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![att.dispatch_row,att.repo,att.worktree,att.branch_ref,att.commit_oid,att.tree_oid,att.command,att.started_ms,att.ended_ms,att.exit,outcome.as_str(),reason,att.log_path,att.runner.as_str()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Read the newest attestations for a row, newest first.
    pub fn gate_attestations_for(
        &self,
        dispatch_id: i64,
        limit: usize,
    ) -> Result<Vec<StoredGateAttestation>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id,dispatch_id,repo,worktree,branch_ref,commit_oid,tree_oid,command,started_ms,ended_ms,exit_code,outcome,reason,log_path,runner FROM pipeline_gate_attestations WHERE dispatch_id=?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(
            rusqlite::params![dispatch_id, i64::try_from(limit)?],
            decode,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Read the latest attestation for one row.
    pub fn latest_gate_attestation(
        &self,
        dispatch_id: i64,
    ) -> Result<Option<StoredGateAttestation>> {
        let conn = self.conn();
        Ok(conn.query_row("SELECT id,dispatch_id,repo,worktree,branch_ref,commit_oid,tree_oid,command,started_ms,ended_ms,exit_code,outcome,reason,log_path,runner FROM pipeline_gate_attestations WHERE dispatch_id=?1 ORDER BY id DESC LIMIT 1", [dispatch_id], decode).optional()?)
    }
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredGateAttestation> {
    let outcome_text: String = row.get(11)?;
    let runner: String = row.get(14)?;
    let outcome = GateOutcome::parse(&outcome_text).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(11, "outcome".to_string(), rusqlite::types::Type::Text)
    })?;
    Ok(StoredGateAttestation {
        id: row.get(0)?,
        dispatch_row: row.get(1)?,
        repo: row.get(2)?,
        worktree: row.get(3)?,
        branch_ref: row.get(4)?,
        commit_oid: row.get(5)?,
        tree_oid: row.get(6)?,
        command: row.get(7)?,
        started_ms: row.get(8)?,
        ended_ms: row.get(9)?,
        exit: row.get(10)?,
        outcome,
        reason: row.get(12)?,
        log_path: row.get(13)?,
        runner,
    })
}

#[cfg(test)]
mod tests {
    use super::{GateAttestationRefusal, StoredGateAttestation};
    use crate::db::Db;
    use crate::issue::NewDispatch;
    use crate::pipeline_gate_attest::{GateAttestation, GateOutcome, GateRunner};
    use crate::store::NotificationStore;

    fn fixture() -> (Db, i64, GateAttestation) {
        let db = Db::open_memory().unwrap();
        let row = db
            .put_agent_dispatch(NewDispatch::new(
                "linear:THE-1103",
                "/tmp/worktree",
                "agent",
            ))
            .unwrap();
        let att = GateAttestation {
            dispatch_row: row,
            repo: "/repo/.git".to_string(),
            worktree: "/tmp/worktree".to_string(),
            branch_ref: "refs/heads/topic".to_string(),
            commit_oid: "a".repeat(40),
            tree_oid: "b".repeat(40),
            command: "just test".to_string(),
            started_ms: 10,
            ended_ms: 20,
            exit: Some(0),
            log_path: "/logs/gate.log".to_string(),
            runner: GateRunner::LeadRunner,
        };
        (db, row, att)
    }

    #[test]
    fn records_and_reads_newest_first() {
        let (db, row, att) = fixture();
        let first = db
            .record_gate_attestation(&att, GateOutcome::Passed, "")
            .unwrap();
        let mut later = att.clone();
        later.started_ms = 30;
        later.ended_ms = 40;
        later.exit = None;
        let second = db
            .record_gate_attestation(&later, GateOutcome::Failed, "red")
            .unwrap();
        let rows = db.gate_attestations_for(row, 10).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![second, first]
        );
        assert_eq!(db.latest_gate_attestation(row).unwrap().unwrap().id, second);
        let encoded = serde_json::to_string(&rows[0]).unwrap();
        assert!(encoded.contains("\"runner\":\"lead_runner\""));
        assert!(encoded.contains("\"outcome\":\"failed\""));
    }

    #[test]
    fn refuses_worker_runner() {
        let (db, _, mut att) = fixture();
        att.runner = GateRunner::Worker;
        assert!(
            matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if error.downcast_ref::<GateAttestationRefusal>() == Some(&GateAttestationRefusal::WorkerRunner))
        );
    }

    #[test]
    fn refuses_malformed_oids() {
        let (db, _, mut att) = fixture();
        for invalid in ["a".repeat(39), "A".repeat(40)] {
            att.commit_oid = invalid;
            assert!(
                matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if matches!(error.downcast_ref::<GateAttestationRefusal>(), Some(GateAttestationRefusal::MalformedOid { field: "commit_oid" })))
            );
        }
        att.commit_oid = "c".repeat(64);
        assert!(
            db.record_gate_attestation(&att, GateOutcome::Passed, "")
                .is_ok()
        );
    }

    #[test]
    fn refuses_pass_without_zero_exit() {
        let (db, _, mut att) = fixture();
        for exit in [None, Some(1)] {
            att.exit = exit;
            assert!(
                matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if error.downcast_ref::<GateAttestationRefusal>() == Some(&GateAttestationRefusal::PassWithoutZeroExit))
            );
        }
    }

    #[test]
    fn refuses_missing_row() {
        let (db, _, mut att) = fixture();
        att.dispatch_row = i64::MAX;
        assert!(
            matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if matches!(error.downcast_ref::<GateAttestationRefusal>(), Some(GateAttestationRefusal::MissingDispatch { id }) if *id == i64::MAX))
        );
    }

    #[test]
    fn refuses_inverted_clock() {
        let (db, _, mut att) = fixture();
        att.ended_ms = 9;
        assert!(
            matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if error.downcast_ref::<GateAttestationRefusal>() == Some(&GateAttestationRefusal::InvertedClock))
        );
    }

    #[test]
    fn refuses_empty_command_or_log() {
        let (db, _, mut att) = fixture();
        att.command.clear();
        assert!(
            matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if error.downcast_ref::<GateAttestationRefusal>() == Some(&GateAttestationRefusal::EmptyCommand))
        );
        att.command = "just test".to_string();
        att.log_path.clear();
        assert!(
            matches!(db.record_gate_attestation(&att, GateOutcome::Passed, ""), Err(error) if error.downcast_ref::<GateAttestationRefusal>() == Some(&GateAttestationRefusal::EmptyLog))
        );
    }

    #[test]
    fn returned_record_has_public_facts() {
        let (db, _, att) = fixture();
        let id = db
            .record_gate_attestation(&att, GateOutcome::Passed, "")
            .unwrap();
        let stored: StoredGateAttestation = db
            .latest_gate_attestation(att.dispatch_row)
            .unwrap()
            .unwrap();
        assert_eq!(stored.id, id);
        assert_eq!(stored.commit_oid, att.commit_oid);
    }

    #[test]
    fn refuses_unknown_stored_outcome() {
        let (db, row, _) = fixture();
        db.conn()
            .execute_batch("PRAGMA ignore_check_constraints=ON;")
            .unwrap();
        db.conn().execute(
            "INSERT INTO pipeline_gate_attestations(dispatch_id,repo,worktree,branch_ref,commit_oid,tree_oid,command,started_ms,ended_ms,outcome,log_path,runner) VALUES(?1,'r','w','b','a','t','cmd',1,2,'unknown','log','lead_runner')",
            [row],
        ).unwrap();
        assert!(db.latest_gate_attestation(row).is_err());
    }
}
