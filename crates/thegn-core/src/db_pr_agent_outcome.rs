//! What the last PR-agent run did to the PR head, persisted on its queue row (THE-549).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::pr_queue::{AgentHeadOutcome, PrAgentLease};
use crate::util;

impl AgentHeadOutcome {
    /// The stable word stored in `pr_queue.agent_outcome`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentProgress => "agent_progress",
            Self::Unchanged => "unchanged",
            Self::LocalOnly => "local_only",
            Self::TeammatePush => "teammate_push",
            Self::Unverified => "unverified",
            Self::InfraFailure => "infra_failure",
            Self::StaleCompletion => "stale_completion",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for an unknown word.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "agent_progress" => Some(Self::AgentProgress),
            "unchanged" => Some(Self::Unchanged),
            "local_only" => Some(Self::LocalOnly),
            "teammate_push" => Some(Self::TeammatePush),
            "unverified" => Some(Self::Unverified),
            "infra_failure" => Some(Self::InfraFailure),
            "stale_completion" => Some(Self::StaleCompletion),
            _ => None,
        }
    }

    /// The operator-facing label.
    pub fn label(self) -> &'static str {
        match self {
            Self::AgentProgress => "pushed progress",
            Self::Unchanged => "changed nothing",
            Self::LocalOnly => "committed locally, nothing pushed",
            Self::TeammatePush => "someone else pushed",
            Self::Unverified => "head provenance unproven",
            Self::InfraFailure => "could not run",
            Self::StaleCompletion => "lease lost before completion",
        }
    }
}

/// The recorded outcome of a row's latest agent generation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PrAgentOutcomeRecord {
    pub outcome: &'static str,
    pub label: &'static str,
    pub recorded_at: i64,
}

impl Db {
    /// Write an outcome fenced on the live lease (owner token, generation, unexpired).
    /// Returns false without changing anything for a stale, expired or foreign lease.
    pub fn record_pr_agent_outcome(
        &self,
        lease: &PrAgentLease,
        outcome: AgentHeadOutcome,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE pr_queue SET agent_outcome=?4, agent_outcome_gen=?3, agent_outcome_at=?5
             WHERE key=?1 AND agent_claim_owner=?2 AND agent_claim_generation=?3
               AND agent_claim_lease_until_ms>=?6",
            params![
                lease.key,
                lease.owner_token,
                lease.generation,
                outcome.as_str(),
                util::now(),
                util::now_ms()
            ],
        )?;
        Ok(changed == 1)
    }

    /// The outcome of the row's latest agent generation, or `None` when absent,
    /// stale, unknown, or the row does not exist.
    pub fn pr_agent_outcome(&self, key: &str) -> Result<Option<PrAgentOutcomeRecord>> {
        let stored: Option<(String, i64)> = self
            .conn()
            .query_row(
                "SELECT agent_outcome, agent_outcome_at FROM pr_queue
                 WHERE key=?1 AND agent_outcome IS NOT NULL
                   AND agent_outcome_gen = agent_claim_generation",
                params![key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                    ))
                },
            )
            .optional()?;
        Ok(stored.and_then(|(word, recorded_at)| {
            AgentHeadOutcome::parse(&word).map(|outcome| PrAgentOutcomeRecord {
                outcome: outcome.as_str(),
                label: outcome.label(),
                recorded_at,
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::AgentHeadOutcome;
    use crate::ci_log::CiLogCandidate;
    use crate::db::Db;
    use crate::pr_queue::{PrAgentAdmission, PrAgentAdmit, PrAgentLease};
    use crate::store::WorktreeAuxStore;

    fn enqueue(db: &Db, n: u64) -> String {
        db.enqueue_bound_pr(
            "/repo",
            n,
            Some("/repo/wt"),
            "feature",
            "main",
            "github",
            &format!("https://github.com/me/repo/pull/{n}"),
        )
        .unwrap();
        db.list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.number == n)
            .unwrap()
            .key
    }

    fn request<'a>(key: &'a str, head: &'a str, attempts: u32) -> PrAgentAdmit<'a> {
        PrAgentAdmit {
            key,
            expected_attempts: attempts,
            expected_checkout_gen: 0,
            max_attempts: 3,
            head,
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: crate::util::now_ms() + 60_000,
            owner_hint: Some("test owner"),
            ci_candidate: None::<&'a CiLogCandidate>,
        }
    }

    fn admitted(db: &Db, key: &str, head: &str) -> PrAgentLease {
        match db.admit_pr_agent(&request(key, head, 0)).unwrap() {
            PrAgentAdmission::Admitted(lease) => lease,
            other => panic!("expected admission, got {other:?}"),
        }
    }

    fn variants() -> [AgentHeadOutcome; 7] {
        [
            AgentHeadOutcome::AgentProgress,
            AgentHeadOutcome::Unchanged,
            AgentHeadOutcome::LocalOnly,
            AgentHeadOutcome::TeammatePush,
            AgentHeadOutcome::Unverified,
            AgentHeadOutcome::InfraFailure,
            AgentHeadOutcome::StaleCompletion,
        ]
    }

    #[test]
    fn outcome_words_round_trip_and_unknown_is_none() {
        let expected = [
            "agent_progress",
            "unchanged",
            "local_only",
            "teammate_push",
            "unverified",
            "infra_failure",
            "stale_completion",
        ];
        let outcomes = variants();
        let words: Vec<_> = outcomes.iter().map(|outcome| outcome.as_str()).collect();
        assert_eq!(words, expected);
        assert_eq!(words.iter().collect::<HashSet<_>>().len(), outcomes.len());
        for outcome in outcomes {
            assert_eq!(AgentHeadOutcome::parse(outcome.as_str()), Some(outcome));
        }
        assert_eq!(AgentHeadOutcome::parse(""), None);
        assert_eq!(AgentHeadOutcome::parse("nope"), None);
        assert_eq!(AgentHeadOutcome::parse("Unchanged"), None);
    }

    #[test]
    fn labels_are_distinct_and_non_empty() {
        let expected = [
            "pushed progress",
            "changed nothing",
            "committed locally, nothing pushed",
            "someone else pushed",
            "head provenance unproven",
            "could not run",
            "lease lost before completion",
        ];
        let outcomes = variants();
        let labels: Vec<_> = outcomes.iter().map(|outcome| outcome.label()).collect();
        assert_eq!(labels, expected);
        assert!(labels.iter().all(|label| !label.is_empty()));
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), outcomes.len());
    }

    #[test]
    fn record_is_fenced_on_owner_generation_and_live_lease() {
        let db = Db::open_memory().unwrap();
        let key = enqueue(&db, 1);
        let lease = admitted(&db, &key, "head-a");
        assert!(
            db.record_pr_agent_outcome(&lease, AgentHeadOutcome::TeammatePush)
                .unwrap()
        );
        let recorded = db.pr_agent_outcome(&key).unwrap().unwrap();
        assert_eq!(recorded.outcome, "teammate_push");
        assert_eq!(recorded.label, "someone else pushed");
        assert!(recorded.recorded_at > 0);

        let stale_generation = PrAgentLease {
            generation: lease.generation + 1,
            ..lease.clone()
        };
        assert!(
            !db.record_pr_agent_outcome(&stale_generation, AgentHeadOutcome::AgentProgress)
                .unwrap()
        );
        assert_eq!(
            db.pr_agent_outcome(&key).unwrap().unwrap().outcome,
            "teammate_push"
        );

        let foreign_owner = PrAgentLease {
            owner_token: "other".to_owned(),
            ..lease.clone()
        };
        assert!(
            !db.record_pr_agent_outcome(&foreign_owner, AgentHeadOutcome::AgentProgress)
                .unwrap()
        );
        assert_eq!(
            db.pr_agent_outcome(&key).unwrap().unwrap().outcome,
            "teammate_push"
        );

        db.conn()
            .execute(
                "UPDATE pr_queue SET agent_claim_lease_until_ms=1 WHERE key=?1",
                [&key],
            )
            .unwrap();
        assert!(
            !db.record_pr_agent_outcome(&lease, AgentHeadOutcome::AgentProgress)
                .unwrap()
        );
        assert_eq!(
            db.pr_agent_outcome(&key).unwrap().unwrap().outcome,
            "teammate_push"
        );
    }

    #[test]
    fn reader_returns_the_latest_generations_outcome_only() {
        let db = Db::open_memory().unwrap();
        let key = enqueue(&db, 2);
        let lease = admitted(&db, &key, "head-a");
        assert!(
            db.record_pr_agent_outcome(&lease, AgentHeadOutcome::AgentProgress)
                .unwrap()
        );
        assert_eq!(
            db.pr_agent_outcome(&key).unwrap().unwrap().outcome,
            "agent_progress"
        );

        db.conn()
            .execute(
                "UPDATE pr_queue SET agent_claim_generation=agent_claim_generation+1 WHERE key=?1",
                [&key],
            )
            .unwrap();
        assert_eq!(db.pr_agent_outcome(&key).unwrap(), None);
        db.conn()
            .execute(
                "UPDATE pr_queue SET agent_claim_generation=agent_claim_generation-1 WHERE key=?1",
                [&key],
            )
            .unwrap();
        assert_eq!(
            db.pr_agent_outcome(&key).unwrap().unwrap().outcome,
            "agent_progress"
        );
    }

    #[test]
    fn a_later_record_in_the_same_generation_replaces_the_earlier() {
        let db = Db::open_memory().unwrap();
        let key = enqueue(&db, 3);
        let lease = admitted(&db, &key, "head-a");
        assert!(
            db.record_pr_agent_outcome(&lease, AgentHeadOutcome::Unchanged)
                .unwrap()
        );
        assert!(
            db.record_pr_agent_outcome(&lease, AgentHeadOutcome::LocalOnly)
                .unwrap()
        );
        assert_eq!(
            db.pr_agent_outcome(&key).unwrap().unwrap().outcome,
            "local_only"
        );
    }

    #[test]
    fn unknown_stored_word_and_missing_row_read_as_none() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.pr_agent_outcome("no-such-key").unwrap(), None);
        let key = enqueue(&db, 4);
        let lease = admitted(&db, &key, "head-a");
        assert!(
            db.record_pr_agent_outcome(&lease, AgentHeadOutcome::Unchanged)
                .unwrap()
        );
        db.conn()
            .execute(
                "UPDATE pr_queue SET agent_outcome='future_word' WHERE key=?1",
                [&key],
            )
            .unwrap();
        assert_eq!(db.pr_agent_outcome(&key).unwrap(), None);
    }
}
