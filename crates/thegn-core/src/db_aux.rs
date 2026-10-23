//! WorktreeAuxStore state — the embedded-SQLite implementation of the [`WorktreeAuxStore`] seam.
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs`
//! only carries the schema DDL, not these bodies. The DB is a cache; git /
//! the live source is truth. A server backend implements this trait against
//! Postgres for shared, multi-user state.

use crate::attention::MqStatus;
use crate::db::{Db, ForwardRow, MergeQueueRow, PrQueueRow, ShareRow};
use crate::models::ContainerEvent;
use crate::store::{
    MERGE_CLAIM_LOST, MERGE_CLAIM_MAX_LEASE_MS, MERGE_CLAIM_OWNER_HINT_MAX, MergeAttemptResult,
    MergeCandidate, MergeClaim, MergeClaimActive, MergeClaimPhase, MergeClaimRequest,
    MergeClaimView, MergeEnqueueIdentity, MergeFinalOutcome, MergeFinalStatus,
    MergeOutcomeObservation, MergeOutcomeWrite, MergeRegistryIdentity, MergeRetryOutcome,
    MergeStatusFields, WorktreeAuxStore,
};
use crate::util;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "db_merge_outcome_tests.rs"]
mod merge_outcome_tests;

#[cfg(test)]
#[path = "db_merge_status_tests.rs"]
mod merge_status_tests;

#[cfg(test)]
#[path = "db_merge_candidate_tests.rs"]
mod merge_candidate_tests;

fn ensure_known_merge_status(status: &str) -> Result<()> {
    anyhow::ensure!(
        MqStatus::parse(status).is_some(),
        "unknown merge-queue status {status:?}: not in the canonical MqStatus vocabulary"
    );
    Ok(())
}

fn merge_queue_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MergeQueueRow> {
    Ok(MergeQueueRow {
        worktree: row.get(0)?,
        branch: row.get(1)?,
        target_branch: row.get(2)?,
        status: row.get(3)?,
        queued_at: row.get(4)?,
        updated_at: row.get(5)?,
        result_oid: row.get(6)?,
        conflict_paths: row.get(7)?,
        error_detail: row.get(8)?,
        // NULL (pre-v44 / unregistered worktree) = local / same store.
        location: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        agent_attempts: row.get::<_, Option<u32>>(10)?.unwrap_or_default(),
    })
}

/// SQLite stores share generations as signed integers; preserve all u64 bits.
fn encode_share_generation(generation: u64) -> i64 {
    generation as i64
}

/// SQLite stores share generations as signed integers; this is a bit cast, not a range check.
pub(crate) fn decode_share_generation(generation: i64) -> u64 {
    generation as u64
}

// Caller holds a read snapshot or a write transaction for both SELECTs.
fn merge_outcome_observation(conn: &Connection, worktree: &str) -> Result<MergeOutcomeObservation> {
    let registry = conn
        .query_row(
            "SELECT branch,repo_path,location FROM worktrees WHERE worktree=?1",
            [worktree],
            |row| {
                Ok(MergeRegistryIdentity {
                    branch: row.get(0)?,
                    repo_path: row.get(1)?,
                    location: row.get(2)?,
                })
            },
        )
        .optional()?;
    let queue = conn
        .query_row(
            "SELECT worktree,branch,target_branch,status,queued_at,updated_at,\
                    result_oid,conflict_paths,error_detail,location,agent_attempts \
             FROM merge_queue WHERE worktree=?1",
            [worktree],
            |row| Ok((merge_queue_row(row)?, row.get::<_, Option<String>>(9)?)),
        )
        .optional()?;
    let (queue, queue_location) = match queue {
        Some((row, location)) => (Some(row), location),
        None => (None, None),
    };
    Ok(MergeOutcomeObservation {
        worktree: worktree.to_owned(),
        registry,
        queue,
        queue_location,
    })
}

fn merge_location(location: Option<&str>) -> &str {
    match location {
        None | Some("" | "local") => "",
        Some(other) => other,
    }
}

fn validate_merge_outcome(
    observed: &MergeOutcomeObservation,
    outcome: &MergeFinalOutcome<'_>,
) -> Result<()> {
    anyhow::ensure!(
        observed.worktree == outcome.worktree
            && !outcome.worktree.is_empty()
            && !outcome.branch.is_empty()
            && !outcome.repo_root.is_empty()
            && !outcome.target_branch.is_empty(),
        "merge outcome does not match its observed worktree/context"
    );
    if let Some(registry) = &observed.registry {
        anyhow::ensure!(
            registry.branch.as_deref() == Some(outcome.branch)
                && registry.repo_path.as_deref() == Some(outcome.repo_root)
                && merge_location(registry.location.as_deref())
                    == merge_location(Some(outcome.location)),
            "merge outcome does not match its observed registry context"
        );
    }
    if let Some(queue) = &observed.queue {
        anyhow::ensure!(
            queue.branch == outcome.branch
                && merge_location(Some(&queue.location)) == merge_location(Some(outcome.location)),
            "merge outcome does not match its observed queue branch/location"
        );
    }
    anyhow::ensure!(
        observed.registry.is_some()
            || observed.queue.is_some()
            || merge_location(Some(outcome.location)).is_empty(),
        "unregistered merge outcome has no observed remote location"
    );
    anyhow::ensure!(
        match outcome.status {
            MergeFinalStatus::Landed => outcome.result_oid.is_some_and(|oid| !oid.is_empty()),
            _ => outcome.result_oid.is_none(),
        },
        "merge outcome result OID is inconsistent with final status"
    );
    Ok(())
}

#[cfg(test)]
#[path = "db_cleanup_hold_tests.rs"]
mod cleanup_hold_tests;

#[cfg(test)]
#[path = "db_merge_claim_tests.rs"]
mod merge_claim_tests;

#[cfg(test)]
#[path = "db_pr_agent_head_tests.rs"]
mod pr_agent_head_tests;
#[cfg(test)]
#[path = "db_pr_agent_lease_tests.rs"]
mod pr_agent_lease_tests;
#[cfg(test)]
#[path = "db_pr_queue_removal_tests.rs"]
mod pr_queue_removal_tests;

#[cfg(test)]
#[path = "db_pr_agent_reap_tests.rs"]
mod pr_agent_reap_tests;
#[cfg(test)]
#[path = "db_pr_lease_fence_tests.rs"]
mod pr_lease_fence_tests;

static MERGE_CLAIM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The token is a row ownership identity, not a secret: any database writer can rewrite the row.
fn mint_claim_token() -> String {
    let state = std::collections::hash_map::RandomState::new();
    let counter = MERGE_CLAIM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let first = state.hash_one((std::process::id(), nanos, counter));
    let second = state.hash_one((counter, nanos, std::process::id()));
    format!(
        "{:x}",
        Sha256::digest(format!("{first}:{second}:{nanos}:{counter}").as_bytes())
    )
}

fn valid_claim_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

#[derive(Clone, Copy)]
enum ClaimIdentity {
    Registered,
    UnregisteredLocal,
}

fn validate_claim_request(request: &MergeClaimRequest) -> Result<()> {
    for value in [
        &request.worktree,
        &request.branch,
        &request.repo_root,
        &request.target_branch,
    ] {
        anyhow::ensure!(
            valid_claim_text(value, 4096),
            "invalid merge claim identity"
        );
    }
    anyhow::ensure!(
        request.location.len() <= 4096 && !request.location.chars().any(char::is_control),
        "invalid merge claim location"
    );
    anyhow::ensure!(
        matches!(
            request.expected_status.as_str(),
            "queued"
                | "deferred"
                | "gate_failed"
                | "gate_error"
                | "needs_human"
                | "ready"
                | "agent_blocked"
        ),
        "invalid merge claim status"
    );
    if let Some(hint) = request.owner_hint.as_deref() {
        anyhow::ensure!(
            valid_claim_text(hint, MERGE_CLAIM_OWNER_HINT_MAX),
            "invalid merge claim owner hint"
        );
    }
    Ok(())
}

fn claim_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MergeClaim> {
    let phase: String = row.get(6)?;
    let phase = MergeClaimPhase::parse(&phase).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(6, "claim_phase".to_owned(), rusqlite::types::Type::Text)
    })?;
    Ok(MergeClaim {
        worktree: row.get(0)?,
        branch: row.get(1)?,
        repo_root: row.get(2)?,
        target_branch: row.get(3)?,
        location: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        generation: row.get(5)?,
        phase,
        lease_until_ms: row.get(7)?,
        owner_hint: row.get(8)?,
        owner_token: row.get(9)?,
    })
}

fn read_claim(tx: &Connection, worktree: &str, repo_root: &str) -> Result<Option<MergeClaim>> {
    Ok(tx
        .query_row(
            "SELECT q.worktree,q.branch,COALESCE(w.repo_path,?2),q.target_branch,q.location,q.claim_generation,\
                q.claim_phase,q.claim_lease_until_ms,q.claim_owner_hint,q.claim_owner \
         FROM merge_queue q LEFT JOIN worktrees w ON w.worktree=q.worktree WHERE q.worktree=?1",
            params![worktree, repo_root],
            claim_from_row,
        )
        .optional()?)
}

fn ensure_unclaimed(conn: &Connection, worktree: &str) -> Result<()> {
    let claimed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM merge_queue WHERE worktree=?1 AND claim_owner IS NOT NULL)",
        [worktree],
        |row| row.get(0),
    )?;
    if claimed {
        return Err(anyhow::Error::new(MergeClaimActive));
    }
    Ok(())
}

impl Db {
    fn admit_merge_claim_tx(
        &self,
        request: &MergeClaimRequest,
        token: &str,
        identity: ClaimIdentity,
    ) -> Result<Option<MergeClaim>> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now = util::now_ms();
        let location = merge_location(Some(&request.location));
        if request.lease_until_ms <= now
            || request.lease_until_ms.saturating_sub(now) > MERGE_CLAIM_MAX_LEASE_MS
        {
            tx.commit()?;
            return Ok(None);
        }
        let identity_matches: bool = match identity {
            ClaimIdentity::Registered => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM worktrees WHERE worktree=?1 AND branch=?2 \
                   AND COALESCE(repo_path,'')=?3 \
                   AND CASE WHEN COALESCE(location,'') IN ('','local') THEN '' ELSE location END=?4)",
                params![request.worktree, request.branch, request.repo_root, location],
                |row| row.get(0),
            )?,
            ClaimIdentity::UnregisteredLocal => {
                tx.query_row(
                    "SELECT NOT EXISTS(SELECT 1 FROM worktrees WHERE worktree=?1)",
                    [&request.worktree],
                    |row| row.get(0),
                )?
            }
        };
        if !identity_matches {
            tx.commit()?;
            return Ok(None);
        }
        let changed = tx.execute(
            "UPDATE merge_queue SET claim_owner=?1,claim_generation=claim_generation+1, \
               claim_phase='claimed_no_child',claim_lease_until_ms=?2,claim_owner_hint=?3, \
               claim_diagnostic=NULL,claim_launch_nonce=NULL WHERE worktree=?4 AND branch=?5 \
               AND target_branch=?6 \
               AND ((claim_phase='unclaimed' AND status=?7) OR \
                    (claim_phase IN ('claimed_no_child','settled') AND claim_lease_until_ms<?9)) \
               AND CASE WHEN COALESCE(location,'') IN ('','local') THEN '' ELSE location END=?8 \
               AND claim_generation<9223372036854775807",
            params![
                token,
                request.lease_until_ms,
                request.owner_hint,
                request.worktree,
                request.branch,
                request.target_branch,
                request.expected_status,
                location,
                now
            ],
        )?;
        let claim = if changed == 1 {
            read_claim(&tx, &request.worktree, &request.repo_root)?
        } else {
            None
        };
        tx.commit()?;
        Ok(claim)
    }

    /// Enqueue a remotely prepared worktree only while every registry fact
    /// verified before the provider/SSH round trip is still authoritative.
    /// `BEGIN IMMEDIATE` closes the final read/write gap: a concurrent rebind
    /// either wins before this transaction and returns `false`, or waits until
    /// the queue row has captured the verified metadata.
    pub fn enqueue_merge_if_worktree_matches(
        &self,
        worktree: &str,
        branch: &str,
        repo_root: &str,
        location: &str,
        target_branch: &str,
    ) -> Result<bool> {
        let conn = self.conn();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<bool> {
            let matches: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM worktrees \
                 WHERE worktree=?1 AND COALESCE(branch,'')=?2 \
                   AND COALESCE(repo_path,'')=?3 AND COALESCE(location,'')=?4)",
                params![worktree, branch, repo_root, location],
                |row| row.get(0),
            )?;
            if !matches {
                return Ok(false);
            }
            let now = util::now();
            let changed = conn.execute(
                r#"INSERT INTO merge_queue
                     (worktree,branch,target_branch,status,queued_at,updated_at,
                      result_oid,conflict_paths,error_detail,location,branch_ref,candidate_oid,
                      enqueue_common_dir,enqueue_admin_dir)
                   VALUES(?1,?2,?3,'queued',?4,?4,NULL,NULL,NULL,?5,NULL,NULL,NULL,NULL)
                   ON CONFLICT(worktree) DO UPDATE SET
                     branch=?2, target_branch=?3, status='queued',
                     queued_at=?4, updated_at=?4,
                     result_oid=NULL, conflict_paths=NULL, error_detail=NULL,
                     agent_attempts=CASE WHEN merge_queue.status IN
                       ('queued','folding','verifying','agent_running')
                     THEN merge_queue.agent_attempts ELSE 0 END,
                     location=?5, branch_ref=NULL, candidate_oid=NULL,
                     enqueue_common_dir=NULL, enqueue_admin_dir=NULL
                   WHERE merge_queue.claim_owner IS NULL"#,
                params![worktree, branch, target_branch, now, location],
            )?;
            if changed != 1 {
                return Err(anyhow::Error::new(MergeClaimActive));
            }
            Ok(true)
        })();
        match &result {
            Ok(true) => conn.execute_batch("COMMIT")?,
            _ => conn.execute_batch("ROLLBACK")?,
        }
        result
    }

    fn validate_merge_candidate(branch: &str, candidate: &MergeCandidate) -> Result<()> {
        anyhow::ensure!(
            candidate.branch_ref == format!("refs/heads/{branch}"),
            "invalid merge candidate branch_ref"
        );
        anyhow::ensure!(
            matches!(candidate.candidate_oid.len(), 40 | 64)
                && candidate
                    .candidate_oid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid merge candidate candidate_oid"
        );
        Ok(())
    }
}

fn enqueue_merge_row(
    db: &Db,
    worktree: &str,
    branch: &str,
    target_branch: &str,
    candidate: Option<&MergeCandidate>,
    identity: Option<&MergeEnqueueIdentity>,
) -> Result<()> {
    let now = util::now();
    let changed = db.conn().execute(
        r#"INSERT INTO merge_queue
             (worktree,branch,target_branch,status,queued_at,updated_at,
              result_oid,conflict_paths,error_detail,location,branch_ref,candidate_oid,
              enqueue_common_dir,enqueue_admin_dir)
           VALUES(?1,?2,?3,'queued',?4,?4,NULL,NULL,NULL,
              (SELECT location FROM worktrees WHERE worktree=?1),?5,?6,?7,?8)
           ON CONFLICT(worktree) DO UPDATE SET
             branch=?2, target_branch=?3, status='queued',
             queued_at=?4, updated_at=?4,
             result_oid=NULL, conflict_paths=NULL, error_detail=NULL,
             agent_attempts=CASE WHEN merge_queue.status IN
                   ('queued','folding','verifying','agent_running')
                 THEN merge_queue.agent_attempts ELSE 0 END,
             location=(SELECT location FROM worktrees WHERE worktree=?1),
             branch_ref=?5, candidate_oid=?6,
             enqueue_common_dir=?7, enqueue_admin_dir=?8
           WHERE merge_queue.claim_owner IS NULL"#,
        params![
            worktree,
            branch,
            target_branch,
            now,
            candidate.map(|value| value.branch_ref.as_str()),
            candidate.map(|value| value.candidate_oid.as_str()),
            identity.map(|value| value.common_dir.as_str()),
            identity.map(|value| value.admin_dir.as_str())
        ],
    )?;
    if changed != 1 {
        return Err(anyhow::Error::new(MergeClaimActive));
    }
    Ok(())
}

impl WorktreeAuxStore for Db {
    // --- registers (persisted yank registers, v27) ------------------------
    /// Persist a register's value (upsert). The single-char `name` is the
    /// register id; the volatile `+` clipboard register is never stored here.
    fn put_register(&self, name: char, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO registers(name,value,updated_at) VALUES(?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![name.to_string(), value.as_bytes(), util::now()],
        )?;
        Ok(())
    }

    /// Load every persisted register as `(name, value)` pairs.
    fn all_registers(&self) -> Result<Vec<(char, String)>> {
        let mut stmt = self.conn().prepare("SELECT name, value FROM registers")?;
        let rows = stmt.query_map([], |r| {
            let name: String = r.get(0)?;
            let value: Vec<u8> = r.get(1)?;
            Ok((name, value))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, value) = row?;
            if let Some(ch) = name.chars().next() {
                out.push((ch, String::from_utf8_lossy(&value).into_owned()));
            }
        }
        Ok(out)
    }

    // --- ingress shares (`[share]`; resurrection layer for tunnels) --------
    /// Insert or update the share record for `(worktree, local_port)`.
    fn upsert_share(
        &self,
        worktree: &str,
        local_port: u16,
        generation: u64,
        provider: &str,
        public_url: Option<&str>,
        state: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO shares(worktree,local_port,provider,public_url,state,created_at,generation)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(worktree,local_port) DO UPDATE SET
               provider=excluded.provider,
               public_url=excluded.public_url,
               state=excluded.state,
               generation=excluded.generation",
            params![
                worktree,
                local_port as i64,
                provider,
                public_url,
                state,
                util::now(),
                encode_share_generation(generation)
            ],
        )?;
        Ok(())
    }

    /// All persisted shares, newest first (restore + panel listing).
    fn list_shares(&self) -> Result<Vec<ShareRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree,local_port,provider,public_url,state,created_at,generation \
             FROM shares ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_share_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn delete_share_generation(
        &self,
        worktree: &str,
        local_port: u16,
        generation: u64,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "DELETE FROM shares WHERE worktree=?1 AND local_port=?2 AND (generation=?3 OR generation=0)",
            params![worktree, local_port as i64, encode_share_generation(generation)],
        )?;
        Ok(changed > 0)
    }

    fn rebind_share_generation(
        &self,
        worktree: &str,
        local_port: u16,
        from: u64,
        to: u64,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE shares SET generation=?4 WHERE worktree=?1 AND local_port=?2 AND generation=?3",
            params![
                worktree,
                local_port as i64,
                encode_share_generation(from),
                encode_share_generation(to)
            ],
        )?;
        Ok(changed > 0)
    }

    /// Unconditionally remove the share record; reserved for explicit CLI `share stop`.
    fn delete_share(&self, worktree: &str, local_port: u16) -> Result<()> {
        self.conn().execute(
            "DELETE FROM shares WHERE worktree=?1 AND local_port=?2",
            params![worktree, local_port as i64],
        )?;
        Ok(())
    }

    // --- auto port forwards (`[forward]`; resurrection layer) --------------
    /// Insert or update the forward record for `(worktree, container_port)`.
    fn upsert_forward(
        &self,
        worktree: &str,
        container_port: u16,
        host_port: u16,
        url: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO forwards(worktree,container_port,host_port,url,created_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(worktree,container_port) DO UPDATE SET
               host_port=excluded.host_port,
               url=excluded.url",
            params![
                worktree,
                container_port as i64,
                host_port as i64,
                url,
                util::now()
            ],
        )?;
        Ok(())
    }

    /// All persisted forwards, newest first (restore + panel listing).
    fn list_forwards(&self) -> Result<Vec<ForwardRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT worktree,container_port,host_port,url,created_at \
             FROM forwards ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_forward_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove the forward record for `(worktree, container_port)`.
    fn delete_forward(&self, worktree: &str, container_port: u16) -> Result<()> {
        self.conn().execute(
            "DELETE FROM forwards WHERE worktree=?1 AND container_port=?2",
            params![worktree, container_port as i64],
        )?;
        Ok(())
    }

    /// Enqueue (or re-enqueue) a worktree branch for the next fold. Re-enqueueing
    /// resets the row to `queued`, clears any prior result/conflict/error and
    /// zeroes `agent_attempts` in the same upsert (only when the existing row is
    /// finished, never for an active queued/folding/verifying/agent_running row,
    /// so a periodic re-add cannot refill an in-flight budget), so a branch that was deferred
    /// and then rebased starts fresh with a full agent budget (same as `merge
    /// retry`). Every caller is a deliberate user/API enqueue; internal outcome
    /// persistence goes through `persist_merge_outcome`, which preserves the
    /// counter. `location` mirrors `worktrees.location` via a correlated subquery;
    /// NULL when the worktree is unregistered is treated as local.
    fn enqueue_merge(&self, worktree: &str, branch: &str, target_branch: &str) -> Result<()> {
        enqueue_merge_row(self, worktree, branch, target_branch, None, None)
    }

    fn enqueue_merge_candidate(
        &self,
        worktree: &str,
        branch: &str,
        target_branch: &str,
        candidate: &MergeCandidate,
    ) -> Result<()> {
        Self::validate_merge_candidate(branch, candidate)?;
        enqueue_merge_row(self, worktree, branch, target_branch, Some(candidate), None)
    }

    fn enqueue_merge_candidate_with_identity(
        &self,
        worktree: &str,
        branch: &str,
        target_branch: &str,
        candidate: &MergeCandidate,
        identity: &MergeEnqueueIdentity,
    ) -> Result<()> {
        Self::validate_merge_candidate(branch, candidate)?;
        anyhow::ensure!(
            valid_claim_text(&identity.common_dir, 4096)
                && valid_claim_text(&identity.admin_dir, 4096),
            "invalid merge enqueue identity"
        );
        enqueue_merge_row(
            self,
            worktree,
            branch,
            target_branch,
            Some(candidate),
            Some(identity),
        )
    }

    fn merge_enqueue_identity(&self, worktree: &str) -> Result<Option<MergeEnqueueIdentity>> {
        let pair = self
            .conn()
            .query_row(
                "SELECT enqueue_common_dir, enqueue_admin_dir FROM merge_queue WHERE worktree=?1",
                [worktree],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            pair.and_then(|(common_dir, admin_dir)| match (common_dir, admin_dir) {
                (Some(common_dir), Some(admin_dir)) => Some(MergeEnqueueIdentity {
                    common_dir,
                    admin_dir,
                }),
                _ => None,
            }),
        )
    }

    fn merge_candidate(&self, worktree: &str) -> Result<Option<MergeCandidate>> {
        let pair = self
            .conn()
            .query_row(
                "SELECT branch_ref, candidate_oid FROM merge_queue WHERE worktree=?1",
                [worktree],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        Ok(pair.and_then(
            |(branch_ref, candidate_oid)| match (branch_ref, candidate_oid) {
                (Some(branch_ref), Some(candidate_oid)) => Some(MergeCandidate {
                    branch_ref,
                    candidate_oid,
                }),
                _ => None,
            },
        ))
    }

    fn adopt_merge_candidate(
        &self,
        worktree: &str,
        expected: &MergeCandidate,
        candidate_oid: &str,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE merge_queue SET candidate_oid=?1, updated_at=?2 \
             WHERE worktree=?3 AND branch_ref=?4 AND candidate_oid=?5 \
             AND status='folding'",
            params![
                candidate_oid,
                util::now(),
                worktree,
                expected.branch_ref,
                expected.candidate_oid
            ],
        )?;
        Ok(changed == 1)
    }

    fn observe_merge_outcome(&self, worktree: &str) -> Result<MergeOutcomeObservation> {
        let tx = self.conn().unchecked_transaction()?;
        let observed = merge_outcome_observation(&tx, worktree)?;
        tx.commit()?;
        Ok(observed)
    }

    fn persist_merge_outcome(
        &self,
        observed: &MergeOutcomeObservation,
        outcome: &MergeFinalOutcome<'_>,
    ) -> Result<MergeOutcomeWrite> {
        validate_merge_outcome(observed, outcome)?;
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let has_claim_owner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('merge_queue') WHERE name = 'claim_owner')",
            [],
            |row| row.get(0),
        )?;
        if has_claim_owner
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM merge_queue WHERE worktree=?1 AND claim_owner IS NOT NULL)",
                [outcome.worktree],
                |row| row.get(0),
            )?
        {
            return Ok(MergeOutcomeWrite::Claimed);
        }
        let current = merge_outcome_observation(&tx, outcome.worktree)?;
        if current.registry != observed.registry {
            return Ok(MergeOutcomeWrite::RegistryChanged);
        }
        if current.queue != observed.queue || current.queue_location != observed.queue_location {
            return Ok(MergeOutcomeWrite::QueueChanged);
        }
        // A repeated manual land is deliberately idempotent.  Once a row is
        // landed, its result identity and updated_at are part of the sweep's
        // grace-period record; refreshing either would move the clock and could
        // keep a worktree alive forever.  Other final statuses retain the
        // queue driver's existing transition behavior.
        if outcome.status == MergeFinalStatus::Landed
            && current
                .queue
                .as_ref()
                .is_some_and(|row| row.status == "landed")
        {
            tx.commit()?;
            return Ok(MergeOutcomeWrite::Written);
        }
        // Mirror the observed registry's exact location representation, or the
        // observed queue's when unregistered. Never canonicalize a remote descriptor.
        let location = observed
            .registry
            .as_ref()
            .map(|registry| registry.location.as_deref())
            .unwrap_or(observed.queue_location.as_deref());
        let upsert = if has_claim_owner {
            "INSERT INTO merge_queue \
                (worktree,branch,target_branch,status,queued_at,updated_at,\
                 result_oid,conflict_paths,error_detail,location,agent_attempts) \
             VALUES(?1,?2,?3,?4,?5,?5,?6,?7,?8,?9,0) \
             ON CONFLICT(worktree) DO UPDATE SET \
                branch=excluded.branch,target_branch=excluded.target_branch,\
                status=excluded.status,updated_at=excluded.updated_at,\
                result_oid=excluded.result_oid,conflict_paths=excluded.conflict_paths,\
                error_detail=excluded.error_detail,location=excluded.location \
             WHERE merge_queue.claim_owner IS NULL"
        } else {
            "INSERT INTO merge_queue \
                (worktree,branch,target_branch,status,queued_at,updated_at,\
                 result_oid,conflict_paths,error_detail,location,agent_attempts) \
             VALUES(?1,?2,?3,?4,?5,?5,?6,?7,?8,?9,0) \
             ON CONFLICT(worktree) DO UPDATE SET \
                branch=excluded.branch,target_branch=excluded.target_branch,\
                status=excluded.status,updated_at=excluded.updated_at,\
                result_oid=excluded.result_oid,conflict_paths=excluded.conflict_paths,\
                error_detail=excluded.error_detail,location=excluded.location"
        };
        let changed = tx.execute(
            upsert,
            params![
                outcome.worktree,
                outcome.branch,
                outcome.target_branch,
                outcome.status.as_str(),
                util::now(),
                outcome.result_oid,
                outcome.conflict_paths,
                outcome.error_detail,
                location,
            ],
        )?;
        if has_claim_owner && changed == 0 {
            return Ok(MergeOutcomeWrite::Claimed);
        }
        tx.commit()?;
        Ok(MergeOutcomeWrite::Written)
    }

    /// Replace every nullable outcome field; `None` clears it to SQL NULL.
    /// Missing rows fail rather than pretending the transition was persisted.
    fn replace_merge_status(
        &self,
        worktree: &str,
        status: &str,
        fields: &MergeStatusFields,
    ) -> Result<()> {
        ensure_known_merge_status(status)?;
        anyhow::ensure!(
            status != "landed"
                || fields
                    .result_oid
                    .as_deref()
                    .is_some_and(|oid| !oid.is_empty()),
            "landed merge status requires a nonempty result OID"
        );
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE merge_queue SET status=?2, updated_at=?3, result_oid=?4, \
             conflict_paths=?5, error_detail=?6 WHERE worktree=?1 AND claim_owner IS NULL",
            params![
                worktree,
                status,
                util::now(),
                fields.result_oid,
                fields.conflict_paths,
                fields.error_detail
            ],
        )?;
        if changed == 0 {
            ensure_unclaimed(&tx, worktree)?;
        }
        anyhow::ensure!(
            changed == 1,
            "merge status replacement requires one existing row"
        );
        tx.commit()?;
        Ok(())
    }

    /// Update a queued worktree's status and (optionally) its result oid,
    /// conflicted paths (newline-joined), and error detail. Passing `None` leaves
    /// the corresponding column unchanged.
    fn update_merge_status(
        &self,
        worktree: &str,
        status: &str,
        result_oid: Option<&str>,
        conflict_paths: Option<&str>,
        error_detail: Option<&str>,
    ) -> Result<()> {
        ensure_known_merge_status(status)?;
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        if status == "landed" {
            anyhow::ensure!(
                result_oid.is_none_or(|oid| !oid.is_empty()),
                "landed merge status requires a nonempty result OID"
            );
            // `result_oid` is nullable, so read it as `Option<String>`: `.optional()`
            // only absorbs a MISSING ROW, and `row.get::<_, String>` on a NULL
            // column is a hard rusqlite type error. Reading it as non-optional
            // panicked on precisely the rows this guard exists for — a landed row
            // whose OID is NULL (THE-687).
            let existing: Option<String> = tx
                .query_row(
                    "SELECT result_oid FROM merge_queue WHERE worktree=?1",
                    params![worktree],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            anyhow::ensure!(
                result_oid
                    .or(existing.as_deref())
                    .is_some_and(|oid| !oid.is_empty()),
                "landed merge status requires a nonempty result OID"
            );
        }
        let changed = tx.execute(
            r#"UPDATE merge_queue SET
                 status=?2, updated_at=?3,
                 result_oid=COALESCE(?4, result_oid),
                 conflict_paths=COALESCE(?5, conflict_paths),
                 error_detail=COALESCE(?6, error_detail)
               WHERE worktree=?1 AND claim_owner IS NULL"#,
            params![
                worktree,
                status,
                util::now(),
                result_oid,
                conflict_paths,
                error_detail
            ],
        )?;
        if changed == 0 {
            ensure_unclaimed(&tx, worktree)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn backfill_landed_result_oid(
        &self,
        expected: &MergeQueueRow,
        result_oid: &str,
    ) -> Result<bool> {
        anyhow::ensure!(!result_oid.is_empty(), "landed result OID cannot be empty");
        anyhow::ensure!(
            expected.status == "landed" && expected.result_oid.as_deref().is_none_or(str::is_empty),
            "backfill requires a landed row with a missing result OID"
        );
        let changed = self.conn().execute(
            "UPDATE merge_queue SET result_oid=?1 \
             WHERE worktree=?2 AND branch=?3 AND target_branch=?4 \
               AND status='landed' AND updated_at=?5 \
               AND (result_oid IS NULL OR result_oid='')",
            params![
                result_oid,
                expected.worktree,
                expected.branch,
                expected.target_branch,
                expected.updated_at,
            ],
        )?;
        Ok(changed == 1)
    }

    fn set_merge_target(&self, worktree: &str, target_branch: &str) -> Result<()> {
        let conn = self.conn();
        let changed = conn.execute(
            r#"UPDATE merge_queue SET target_branch=?2, updated_at=?3
               WHERE worktree=?1 AND claim_owner IS NULL"#,
            params![worktree, target_branch, util::now()],
        )?;
        if changed == 0 {
            ensure_unclaimed(conn, worktree)?;
        }
        Ok(())
    }

    fn set_merge_agent_attempts(&self, worktree: &str, attempts: u32) -> Result<()> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE merge_queue SET agent_attempts=?2, updated_at=?3 WHERE worktree=?1 AND claim_owner IS NULL",
            params![worktree, attempts, util::now()],
        )?;
        if changed == 0 {
            ensure_unclaimed(conn, worktree)?;
        }
        Ok(())
    }

    fn retry_merge_entry(&self, worktree: &str) -> Result<bool> {
        let n = self.conn().execute(
            r#"UPDATE merge_queue SET
                 status='queued', agent_attempts=0, updated_at=?2,
                 result_oid=NULL, conflict_paths=NULL, error_detail=NULL
               WHERE worktree=?1 AND claim_owner IS NULL"#,
            params![worktree, util::now()],
        )?;
        Ok(n > 0)
    }

    fn retry_held_merge_entry(&self, worktree: &str) -> Result<MergeRetryOutcome> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT status, claim_owner FROM merge_queue WHERE worktree=?1",
                [worktree],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((status, claim_owner)) = row else {
            tx.commit()?;
            return Ok(MergeRetryOutcome::NotQueued);
        };
        if claim_owner.is_some() {
            tx.commit()?;
            return Ok(MergeRetryOutcome::Claimed);
        }
        if !MqStatus::parse(&status).is_some_and(MqStatus::is_held) {
            tx.commit()?;
            return Ok(MergeRetryOutcome::NotRetryable { status });
        }

        let held = MqStatus::ALL
            .iter()
            .filter(|status| status.is_held())
            .map(|status| format!("'{}'", status.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE merge_queue SET status='queued', agent_attempts=0, updated_at=?2, \
             result_oid=NULL, conflict_paths=NULL, error_detail=NULL \
             WHERE worktree=?1 AND claim_owner IS NULL AND status IN ({held})"
        );
        let changed = tx.execute(&sql, params![worktree, util::now()])?;
        let outcome = if changed == 1 {
            MergeRetryOutcome::Rearmed
        } else {
            MergeRetryOutcome::NotRetryable { status }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Drop a worktree's merge-queue row (e.g. after a clean land is recorded
    /// elsewhere, or the worktree is removed).
    fn remove_merge_entry(&self, worktree: &str) -> Result<()> {
        let conn = self.conn();
        let changed = conn.execute(
            "DELETE FROM merge_queue WHERE worktree=?1 AND claim_owner IS NULL",
            params![worktree],
        )?;
        if changed == 0 {
            ensure_unclaimed(conn, worktree)?;
        }
        Ok(())
    }

    fn hold_merge_cleanup(&self, row: &MergeQueueRow) -> Result<bool> {
        use crate::merge_sweep::CleanupHold;
        if row.status != "landed"
            || row.result_oid.as_deref().is_none_or(str::is_empty)
            || CleanupHold::from_detail(row.error_detail.as_deref()).is_some()
        {
            return Ok(false);
        }
        let changed = self.conn().execute(
            "UPDATE merge_queue SET error_detail=?12 WHERE worktree=?1 AND branch=?2 AND target_branch=?3 AND status=?4 AND queued_at=?5 AND updated_at=?6 AND result_oid IS ?7 AND conflict_paths IS ?8 AND error_detail IS ?9 AND COALESCE(location,'')=?10 AND COALESCE(agent_attempts,0)=?11",
            params![row.worktree, row.branch, row.target_branch, row.status, row.queued_at, row.updated_at,
                row.result_oid, row.conflict_paths, row.error_detail, row.location, row.agent_attempts,
                CleanupHold::BranchRetained.marker()],
        )?;
        Ok(changed == 1)
    }

    fn defer_merge_cleanup(
        &self,
        row: &MergeQueueRow,
        retry: crate::merge_sweep::CleanupRetry,
    ) -> Result<bool> {
        use crate::merge_sweep::CleanupHold;
        if row.status != "landed"
            || row.result_oid.as_deref().is_none_or(str::is_empty)
            || CleanupHold::from_detail(row.error_detail.as_deref()).is_some()
        {
            return Ok(false);
        }
        let changed = self.conn().execute(
            "UPDATE merge_queue SET error_detail=?12 WHERE worktree=?1 AND branch=?2 AND target_branch=?3 AND status=?4 AND queued_at=?5 AND updated_at=?6 AND result_oid IS ?7 AND conflict_paths IS ?8 AND error_detail IS ?9 AND COALESCE(location,'')=?10 AND COALESCE(agent_attempts,0)=?11",
            params![row.worktree, row.branch, row.target_branch, row.status, row.queued_at, row.updated_at,
                row.result_oid, row.conflict_paths, row.error_detail, row.location, row.agent_attempts,
                retry.marker()],
        )?;
        Ok(changed == 1)
    }

    /// The whole queue, oldest-queued first (the fold order + UI feed).
    fn list_merge_queue(&self) -> Result<Vec<MergeQueueRow>> {
        let mut stmt = self.conn().prepare(
            r#"SELECT worktree,branch,target_branch,status,queued_at,updated_at,
                      result_oid,conflict_paths,error_detail,location,agent_attempts
               FROM merge_queue ORDER BY queued_at"#,
        )?;
        let rows = stmt.query_map([], merge_queue_row)?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row?);
        }
        Ok(v)
    }

    fn admit_merge_claim(&self, request: &MergeClaimRequest) -> Result<Option<MergeClaim>> {
        validate_claim_request(request)?;
        let token = mint_claim_token();
        self.admit_merge_claim_tx(request, &token, ClaimIdentity::Registered)
    }

    fn admit_unregistered_local_merge_claim(
        &self,
        request: &MergeClaimRequest,
    ) -> Result<Option<MergeClaim>> {
        validate_claim_request(request)?;
        anyhow::ensure!(
            merge_location(Some(&request.location)).is_empty(),
            "unregistered merge claim requires a local row"
        );
        let token = mint_claim_token();
        self.admit_merge_claim_tx(request, &token, ClaimIdentity::UnregisteredLocal)
    }

    fn observe_merge_claim(&self, worktree: &str) -> Result<Option<MergeClaimView>> {
        Ok(self.conn().query_row(
            "SELECT worktree,claim_generation,claim_phase,claim_lease_until_ms,claim_owner_hint \
             FROM merge_queue WHERE worktree=?1",
            [worktree],
            |row| Ok(MergeClaimView {
                worktree: row.get(0)?, generation: row.get(1)?, phase: row.get(2)?,
                lease_until_ms: row.get(3)?, owner_hint: row.get(4)?,
            }),
        ).optional()?)
    }

    fn renew_merge_claim(&self, claim: &MergeClaim, lease_until_ms: i64) -> Result<MergeClaim> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now = util::now_ms();
        anyhow::ensure!(
            lease_until_ms > now && lease_until_ms.saturating_sub(now) <= MERGE_CLAIM_MAX_LEASE_MS,
            "invalid merge claim lease"
        );
        let changed = tx.execute(
            "UPDATE merge_queue SET claim_lease_until_ms=?1 WHERE worktree=?2 AND claim_owner=?3 \
             AND claim_generation=?4 AND claim_phase='claimed_no_child' AND claim_lease_until_ms>=?5",
            params![lease_until_ms, claim.worktree, claim.owner_token, claim.generation, now],
        )?;
        anyhow::ensure!(changed == 1, MERGE_CLAIM_LOST);
        let mut renewed = read_claim(&tx, &claim.worktree, &claim.repo_root)?
            .ok_or_else(|| anyhow::anyhow!(MERGE_CLAIM_LOST))?;
        tx.commit()?;
        renewed.lease_until_ms = lease_until_ms;
        Ok(renewed)
    }

    fn settle_merge_claim_no_child(&self, claim: &MergeClaim) -> Result<MergeClaim> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE merge_queue SET claim_phase='settled' WHERE worktree=?1 AND claim_owner=?2 \
             AND claim_generation=?3 AND claim_phase='claimed_no_child'",
            params![claim.worktree, claim.owner_token, claim.generation],
        )?;
        anyhow::ensure!(changed == 1, MERGE_CLAIM_LOST);
        let settled = read_claim(&tx, &claim.worktree, &claim.repo_root)?
            .ok_or_else(|| anyhow::anyhow!(MERGE_CLAIM_LOST))?;
        tx.commit()?;
        Ok(settled)
    }

    fn release_merge_claim(&self, claim: &MergeClaim) -> Result<()> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE merge_queue SET claim_owner=NULL,claim_phase='unclaimed',claim_lease_until_ms=NULL, \
             claim_owner_hint=NULL,claim_launch_nonce=NULL,claim_diagnostic=NULL WHERE worktree=?1 \
             AND claim_owner=?2 AND claim_generation=?3 AND claim_phase='settled'",
            params![claim.worktree, claim.owner_token, claim.generation],
        )?;
        anyhow::ensure!(changed == 1, MERGE_CLAIM_LOST);
        tx.commit()?;
        Ok(())
    }

    fn consume_merge_agent_attempt(
        &self,
        claim: &MergeClaim,
        max: u32,
    ) -> Result<MergeAttemptResult> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now = util::now_ms();
        let changed = tx.execute(
            "UPDATE merge_queue SET agent_attempts=agent_attempts+1,updated_at=?1 WHERE worktree=?2 \
             AND claim_owner=?3 AND claim_generation=?4 AND claim_phase='claimed_no_child' \
             AND claim_lease_until_ms>=?5 AND agent_attempts<?6",
            params![util::now(), claim.worktree, claim.owner_token, claim.generation, now, max],
        )?;
        let result =
            if changed == 1 {
                MergeAttemptResult::Consumed
            } else {
                let row: Option<(Option<i64>, Option<u32>)> = tx.query_row(
                "SELECT claim_lease_until_ms,agent_attempts FROM merge_queue WHERE worktree=?1 \
                 AND claim_owner=?2 AND claim_generation=?3 AND claim_phase='claimed_no_child'",
                params![claim.worktree, claim.owner_token, claim.generation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
                if row.is_some_and(|(lease, attempts)| {
                    lease.is_some_and(|lease| lease >= now) && attempts.unwrap_or_default() >= max
                }) {
                    MergeAttemptResult::BudgetExhausted
                } else {
                    MergeAttemptResult::ClaimLost
                }
            };
        tx.commit()?;
        Ok(result)
    }

    fn refund_merge_agent_attempt(&self, claim: &MergeClaim) -> Result<bool> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now = util::now_ms();
        let changed = tx.execute(
            "UPDATE merge_queue SET agent_attempts=agent_attempts-1,updated_at=?1 WHERE worktree=?2 \
             AND claim_owner=?3 AND claim_generation=?4 AND claim_phase='claimed_no_child' \
             AND claim_lease_until_ms>=?5 AND agent_attempts>0",
            params![util::now(), claim.worktree, claim.owner_token, claim.generation, now],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    fn replace_merge_status_claimed(
        &self,
        claim: &MergeClaim,
        status: &str,
        fields: &MergeStatusFields,
    ) -> Result<MergeClaim> {
        ensure_known_merge_status(status)?;
        anyhow::ensure!(
            status != "landed"
                || fields
                    .result_oid
                    .as_deref()
                    .is_some_and(|oid| !oid.is_empty()),
            "landed merge status requires a nonempty result OID"
        );
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now = util::now_ms();
        let changed = tx.execute(
            "UPDATE merge_queue SET status=?1,updated_at=?2,result_oid=?3,conflict_paths=?4,error_detail=?5 \
             WHERE worktree=?6 AND claim_owner=?7 AND claim_generation=?8 \
             AND (claim_phase='settled' OR \
                  (claim_phase='claimed_no_child' AND claim_lease_until_ms>=?9))",
            params![status, util::now(), fields.result_oid, fields.conflict_paths, fields.error_detail,
                claim.worktree, claim.owner_token, claim.generation, now],
        )?;
        anyhow::ensure!(changed == 1, MERGE_CLAIM_LOST);
        let updated = read_claim(&tx, &claim.worktree, &claim.repo_root)?
            .ok_or_else(|| anyhow::anyhow!(MERGE_CLAIM_LOST))?;
        tx.commit()?;
        Ok(updated)
    }

    // --- PR queue (v50) ---------------------------------------------------

    fn enqueue_pr(
        &self,
        repo_root: &str,
        number: u64,
        worktree: Option<&str>,
        branch: &str,
        base_branch: &str,
        forge: &str,
    ) -> Result<()> {
        let now = util::now();
        let key = crate::db::PrQueueRow::make_key(repo_root, forge, None, number);
        // Re-queueing resets the row: back to `watching`, blocker/detail/attempts
        // cleared. That is the "I fixed it, watch again" gesture, and it is why
        // there is no separate retry verb for the PR queue.
        //
        // `worktree` is COALESCEd rather than overwritten so re-queueing by
        // number from outside a checkout doesn't drop the checkout an earlier
        // enqueue found — losing it would silently disable agent handoff.
        self.conn().execute(
            "INSERT INTO pr_queue(key,repo_root,number,worktree,branch,base_branch,forge,
                                  status,blocker,detail,agent_attempts,last_head_oid,
                                  queued_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'watching',NULL,NULL,0,NULL,?8,?8)
             ON CONFLICT(key) DO UPDATE SET
               worktree=COALESCE(excluded.worktree, pr_queue.worktree),
               branch=excluded.branch,
               base_branch=excluded.base_branch,
               forge=excluded.forge,
               status='watching',
               blocker=NULL,
               detail=NULL,
               agent_attempts=0,
               updated_at=excluded.updated_at",
            // SQLite integers are signed; a PR number never approaches i64::MAX.
            params![
                key,
                repo_root,
                number as i64,
                worktree,
                branch,
                base_branch,
                forge,
                now
            ],
        )?;
        Ok(())
    }

    fn enqueue_bound_pr(
        &self,
        repo_root: &str,
        number: u64,
        worktree: Option<&str>,
        branch: &str,
        base_branch: &str,
        forge: &str,
        pr_url: &str,
    ) -> Result<()> {
        let identity = crate::forge::model::repo_identity_from_pr_url(pr_url).ok_or_else(|| {
            anyhow::anyhow!("invalid PR URL; rebind the queue row after verifying its identity")
        })?;
        anyhow::ensure!(
            crate::pr_queue::pr_url_number_matches(pr_url, number),
            "PR URL does not identify PR #{number}"
        );
        let now = util::now();
        let key = crate::db::PrQueueRow::make_key(repo_root, forge, Some(&identity), number);
        let host = identity.host.to_ascii_lowercase();
        let project = identity.path.to_ascii_lowercase();
        let tx = self.conn().unchecked_transaction()?;
        let legacy_key = crate::db::PrQueueRow::make_key(repo_root, forge, None, number);
        tx.execute(
            "DELETE FROM pr_queue WHERE key=?1 AND pr_url IS NULL",
            [&legacy_key],
        )?;
        let existing = tx
            .query_row(
                "SELECT worktree,branch FROM pr_queue WHERE key=?1",
                [&key],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((old_worktree, old_branch)) = existing {
            anyhow::ensure!(
                old_worktree.as_deref() == worktree && old_branch == branch,
                "PR queue checkout changed; run `thegn pr queue rebind --pr {number}`"
            );
            tx.execute("UPDATE pr_queue SET base_branch=?2,forge_host=?3,project=?4,pr_url=?5,status='watching',blocker=NULL,detail=NULL,agent_attempts=0,updated_at=?6 WHERE key=?1",
                params![key,base_branch,host,project,pr_url,now])?;
            tx.commit()?;
            return Ok(());
        }
        tx.execute(
            "INSERT INTO pr_queue(key,repo_root,number,worktree,branch,base_branch,forge,
                                  status,blocker,detail,agent_attempts,last_head_oid,
                                  queued_at,updated_at,pr_url,forge_host,project,checkout_gen)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'watching',NULL,NULL,0,NULL,?8,?8,?9,?10,?11,0)",
            params![
                key,
                repo_root,
                number as i64,
                worktree,
                branch,
                base_branch,
                forge,
                now,
                pr_url,
                host,
                project
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn rebind_pr(&self, old_key: &str, binding: &crate::db::PrRebind) -> Result<String> {
        let identity = crate::forge::model::repo_identity_from_pr_url(&binding.pr_url)
            .ok_or_else(|| anyhow::anyhow!("invalid PR URL for rebind"))?;
        let tx = self.conn().unchecked_transaction()?;
        let old = tx
            .query_row(
                "SELECT repo_root,number,agent_claim_owner,agent_claim_lease_until_ms FROM pr_queue WHERE key=?1",
                [old_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<i64>>(3)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("PR queue row {old_key} no longer exists"))?;
        if old.2.is_some() && old.3.is_some_and(|until| until >= util::now_ms()) {
            anyhow::bail!("a PR agent is running for this row; rebind after it finishes");
        }
        anyhow::ensure!(
            crate::pr_queue::pr_url_number_matches(&binding.pr_url, old.1.max(0) as u64),
            "PR URL does not match the queued PR number"
        );
        let new_key = crate::db::PrQueueRow::make_key(
            &old.0,
            &binding.forge,
            Some(&identity),
            old.1.max(0) as u64,
        );
        let occupied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pr_queue WHERE key=?1 AND key<>?2)",
            params![new_key, old_key],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !occupied,
            "PR queue identity already belongs to another row"
        );
        let host = identity.host.to_ascii_lowercase();
        let project = identity.path.to_ascii_lowercase();
        tx.execute("UPDATE pr_queue SET key=?2,worktree=?3,branch=?4,base_branch=?5,forge=?6,pr_url=?7,forge_host=?8,project=?9,checkout_gen=checkout_gen+1,status='watching',blocker=NULL,detail=NULL,ci_rerun_head=NULL,updated_at=?10 WHERE key=?1",
            params![old_key,new_key,binding.worktree,binding.branch,binding.base_branch,binding.forge,binding.pr_url,host,project,util::now()])?;
        tx.commit()?;
        Ok(new_key)
    }

    fn update_pr_status(
        &self,
        key: &str,
        status: &str,
        blocker: Option<&str>,
        detail: Option<&str>,
        last_head_oid: Option<&str>,
    ) -> Result<()> {
        // COALESCE semantics: `None` leaves the column alone. A failed refresh
        // can record a note without clobbering the last known-good head, which
        // is what `foreign_push` compares against.
        self.conn().execute(
            "UPDATE pr_queue SET
               status=?2,
               blocker=COALESCE(?3, blocker),
               detail=COALESCE(?4, detail),
               ci_rerun_head=CASE WHEN ?5 IS NOT NULL AND last_head_oid IS NOT ?5
                                  THEN NULL ELSE ci_rerun_head END,
               last_head_oid=COALESCE(?5, last_head_oid),
               updated_at=?6
             WHERE key=?1
               AND (agent_claim_owner IS NULL OR agent_claim_lease_until_ms IS NULL
                    OR agent_claim_lease_until_ms < ?7)",
            params![
                key,
                status,
                blocker,
                detail,
                last_head_oid,
                util::now(),
                util::now_ms()
            ],
        )?;
        Ok(())
    }

    fn update_pr_detail(&self, key: &str, detail: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE pr_queue SET detail=?2, updated_at=?3 WHERE key=?1
             AND (agent_claim_owner IS NULL OR agent_claim_lease_until_ms IS NULL
                  OR agent_claim_lease_until_ms < ?4)",
            params![key, detail, util::now(), util::now_ms()],
        )?;
        Ok(())
    }

    fn set_pr_agent_attempts(&self, key: &str, attempts: u32) -> Result<()> {
        self.conn().execute(
            "UPDATE pr_queue SET agent_attempts=?2, updated_at=?3 WHERE key=?1
             AND (agent_claim_owner IS NULL OR agent_claim_lease_until_ms IS NULL
                  OR agent_claim_lease_until_ms < ?4)",
            params![key, attempts, util::now(), util::now_ms()],
        )?;
        Ok(())
    }

    fn pr_agent_lease_view(&self, key: &str) -> Result<Option<crate::pr_queue::PrAgentLeaseView>> {
        let view = self
            .conn()
            .query_row(
                "SELECT agent_claim_owner IS NOT NULL, agent_claim_generation,
                        agent_claim_lease_until_ms, agent_claim_owner_hint
                 FROM pr_queue WHERE key=?1",
                [key],
                |row| {
                    let has_owner: bool = row.get(0)?;
                    let generation: i64 = row.get(1)?;
                    let lease_until_ms: Option<i64> = row.get(2)?;
                    let owner_hint: Option<String> = row.get(3)?;
                    Ok((has_owner, generation, lease_until_ms, owner_hint))
                },
            )
            .optional()?
            .and_then(|(has_owner, generation, lease_until_ms, owner_hint)| {
                if has_owner {
                    lease_until_ms.map(|lease_until_ms| crate::pr_queue::PrAgentLeaseView {
                        generation,
                        lease_until_ms,
                        owner_hint,
                    })
                } else {
                    None
                }
            });
        Ok(view)
    }

    fn admit_pr_agent(
        &self,
        request: &crate::pr_queue::PrAgentAdmit<'_>,
    ) -> Result<crate::pr_queue::PrAgentAdmission> {
        use crate::pr_queue::{PrAgentAdmission, PrAgentLease, PrAgentRefusal, PrAgentRowSnapshot};

        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now = util::now_ms();
        if request.lease_until_ms <= now
            || request.lease_until_ms.saturating_sub(now) > crate::pr_queue::PR_AGENT_MAX_LEASE_MS
        {
            return Ok(PrAgentAdmission::Refused(PrAgentRefusal::InvalidLease));
        }
        let row = tx
            .query_row(
                "SELECT status,agent_attempts,checkout_gen,agent_claim_owner IS NOT NULL,
                        agent_claim_generation,agent_claim_lease_until_ms
                 FROM pr_queue WHERE key=?1",
                [request.key],
                |row| {
                    Ok(PrAgentRowSnapshot {
                        status: row.get(0)?,
                        agent_attempts: u32::try_from(row.get::<_, i64>(1)?.max(0))
                            .unwrap_or(u32::MAX),
                        checkout_gen: row.get(2)?,
                        claim_owner_present: row.get(3)?,
                        claim_generation: row.get(4)?,
                        claim_lease_until_ms: row.get(5)?,
                    })
                },
            )
            .optional()?;
        let Some(row) = row else {
            return Ok(PrAgentAdmission::Refused(PrAgentRefusal::Missing));
        };
        if let Err(refusal) = crate::pr_queue::decide_pr_agent_admission(&row, request, now) {
            return Ok(PrAgentAdmission::Refused(refusal));
        }
        if let Some(candidate) = request.ci_candidate {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO ci_autofix_dedupe
                 (worktree,run_id,job_id,head_sha,claimed_at) VALUES (?1,?2,?3,?4,?5)",
                params![
                    candidate.worktree,
                    candidate.run_id,
                    candidate.job_id,
                    candidate.head_sha,
                    util::now()
                ],
            )?;
            if inserted == 0 {
                return Ok(PrAgentAdmission::Refused(
                    PrAgentRefusal::DuplicateCandidate,
                ));
            }
        }
        let token = mint_claim_token();
        let changed = tx.execute(
            "UPDATE pr_queue SET status='agent_running',blocker=?1,detail=?2,last_head_oid=?3,
               agent_attempts=agent_attempts+1,agent_claim_owner=?4,
               agent_claim_generation=agent_claim_generation+1,agent_claim_lease_until_ms=?5,
               agent_claim_owner_hint=?6,updated_at=?7
             WHERE key=?8 AND agent_claim_generation=?9",
            params![
                request.blocker,
                request.detail,
                request.head,
                token,
                request.lease_until_ms,
                request.owner_hint,
                util::now(),
                request.key,
                row.claim_generation
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "PR-agent admission row changed during immediate transaction"
        );
        tx.commit()?;
        Ok(PrAgentAdmission::Admitted(PrAgentLease {
            key: request.key.to_owned(),
            generation: row.claim_generation + 1,
            attempt: row.agent_attempts + 1,
            lease_until_ms: request.lease_until_ms,
            owner_token: token,
        }))
    }

    fn renew_pr_agent(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        lease_until_ms: i64,
    ) -> Result<crate::pr_queue::PrAgentLease> {
        let now = util::now_ms();
        anyhow::ensure!(
            lease_until_ms > now
                && lease_until_ms.saturating_sub(now) <= crate::pr_queue::PR_AGENT_MAX_LEASE_MS,
            "invalid PR-agent lease"
        );
        let changed = self.conn().execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=?1
             WHERE key=?2 AND agent_claim_owner=?3 AND agent_claim_generation=?4
               AND agent_claim_lease_until_ms>=?5",
            params![
                lease_until_ms,
                lease.key,
                lease.owner_token,
                lease.generation,
                now
            ],
        )?;
        anyhow::ensure!(changed == 1, "{}", crate::pr_queue::PR_AGENT_LEASE_LOST);
        let mut renewed = lease.clone();
        renewed.lease_until_ms = lease_until_ms;
        Ok(renewed)
    }

    fn release_pr_agent(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        finished: Option<(&str, &str)>,
    ) -> Result<bool> {
        let (status, detail) = finished.map_or((None, None), |(status, detail)| {
            (Some(status), Some(detail))
        });
        let changed = self.conn().execute(
            "UPDATE pr_queue SET agent_claim_owner=NULL,agent_claim_lease_until_ms=NULL,
               agent_claim_owner_hint=NULL,
               status=CASE WHEN ?3 IS NOT NULL AND status='agent_running' THEN ?3 ELSE status END,
               detail=CASE WHEN ?3 IS NOT NULL AND status='agent_running' THEN ?4 ELSE detail END,
               updated_at=?5
             WHERE key=?1 AND agent_claim_owner=?2 AND agent_claim_generation=?6",
            params![
                lease.key,
                lease.owner_token,
                status,
                detail,
                util::now(),
                lease.generation
            ],
        )?;
        Ok(changed == 1)
    }

    fn refund_pr_agent_attempt(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        status: &str,
        detail: &str,
    ) -> Result<bool> {
        anyhow::ensure!(
            crate::pr_queue::PrqStatus::parse(status).is_some(),
            "unknown PR-queue status {status:?}"
        );
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE pr_queue SET agent_attempts=agent_attempts-1,
               agent_claim_owner=NULL,agent_claim_lease_until_ms=NULL,agent_claim_owner_hint=NULL,
               status=CASE WHEN status='agent_running' THEN ?3 ELSE status END,
               detail=CASE WHEN status='agent_running' THEN ?4 ELSE detail END, updated_at=?5
             WHERE key=?1 AND agent_claim_owner=?2 AND agent_claim_generation=?6
               AND agent_claim_lease_until_ms>=?7 AND agent_attempts=?8 AND agent_attempts>0",
            params![
                lease.key,
                lease.owner_token,
                status,
                detail,
                util::now(),
                lease.generation,
                util::now_ms(),
                lease.attempt,
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    fn record_pr_agent_head(
        &self,
        lease: &crate::pr_queue::PrAgentLease,
        admitted_head: &str,
        own_head: &str,
    ) -> Result<bool> {
        anyhow::ensure!(
            !admitted_head.is_empty() && !own_head.is_empty() && admitted_head != own_head,
            "invalid PR-agent head record"
        );
        let changed = self.conn().execute(
            "UPDATE pr_queue SET last_head_oid=?4, ci_rerun_head=NULL, updated_at=?5
             WHERE key=?1 AND agent_claim_owner=?2 AND agent_claim_generation=?3
               AND agent_claim_lease_until_ms>=?6 AND last_head_oid IS ?7",
            params![
                lease.key,
                lease.owner_token,
                lease.generation,
                own_head,
                util::now(),
                util::now_ms(),
                admitted_head
            ],
        )?;
        Ok(changed == 1)
    }

    fn reap_expired_pr_agent(&self, key: &str, generation: i64, detail: &str) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE pr_queue SET agent_claim_owner=NULL, agent_claim_lease_until_ms=NULL,
                    agent_claim_owner_hint=NULL,
                    status=CASE WHEN status='agent_running' THEN 'watching' ELSE status END,
                    detail=CASE WHEN status='agent_running' THEN ?3 ELSE detail END,
                    updated_at=?4
             WHERE key=?1 AND agent_claim_generation=?2 AND agent_claim_owner IS NOT NULL
               AND agent_claim_lease_until_ms IS NOT NULL AND agent_claim_lease_until_ms < ?5",
            params![key, generation, detail, util::now(), util::now_ms()],
        )?;
        Ok(changed == 1)
    }

    fn reserve_pr_ci_rerun(
        &self,
        key: &str,
        expected_head: Option<&str>,
        head: &str,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE pr_queue SET ci_rerun_head=?3, last_head_oid=?3, updated_at=?4
             WHERE key=?1 AND last_head_oid IS ?2
               AND (ci_rerun_head IS NULL OR ci_rerun_head <> ?3)",
            params![key, expected_head, head, util::now()],
        )?;
        Ok(changed == 1)
    }

    fn remove_pr_entry(&self, key: &str) -> Result<crate::store::PrRemoval> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now_ms = util::now_ms();
        let removed = tx.execute(
            "DELETE FROM pr_queue WHERE key=?1
             AND (agent_claim_owner IS NULL OR agent_claim_lease_until_ms IS NULL
                  OR agent_claim_lease_until_ms < ?2)",
            params![key, now_ms],
        )?;
        let outcome = if removed == 1 {
            crate::store::PrRemoval::Removed
        } else {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pr_queue WHERE key=?1)",
                [key],
                |row| row.get(0),
            )?;
            if exists {
                crate::store::PrRemoval::AgentRunning
            } else {
                crate::store::PrRemoval::Missing
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    fn clear_pr_queue(&self, repo_root: &str) -> Result<crate::store::PrClearOutcome> {
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        let now_ms = util::now_ms();
        let kept_running = {
            let mut stmt = tx.prepare(
                "SELECT number FROM pr_queue WHERE repo_root=?1
                 AND agent_claim_owner IS NOT NULL
                 AND agent_claim_lease_until_ms IS NOT NULL
                 AND agent_claim_lease_until_ms >= ?2
                 ORDER BY number",
            )?;
            let numbers = stmt.query_map(params![repo_root, now_ms], |row| row.get::<_, i64>(0))?;
            numbers
                .map(|number| number.map(|number| number.max(0) as u64))
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let removed = tx.execute(
            "DELETE FROM pr_queue WHERE repo_root=?1
             AND (agent_claim_owner IS NULL OR agent_claim_lease_until_ms IS NULL
                  OR agent_claim_lease_until_ms < ?2)",
            params![repo_root, now_ms],
        )?;
        tx.commit()?;
        Ok(crate::store::PrClearOutcome {
            removed,
            kept_running,
        })
    }

    fn list_pr_queue(&self) -> Result<Vec<PrQueueRow>> {
        let mut stmt = self.conn().prepare(
            r#"SELECT key,repo_root,number,worktree,branch,base_branch,forge,status,
                      blocker,detail,agent_attempts,ci_rerun_head,last_head_oid,queued_at,updated_at,pr_url,
                      forge_host,project,checkout_gen
               FROM pr_queue ORDER BY queued_at"#,
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PrQueueRow {
                key: r.get(0)?,
                repo_root: r.get(1)?,
                number: r.get::<_, i64>(2)?.max(0) as u64,
                worktree: r.get(3)?,
                branch: r.get(4)?,
                base_branch: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                forge: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                status: r.get(7)?,
                blocker: r.get(8)?,
                detail: r.get(9)?,
                agent_attempts: r.get::<_, Option<u32>>(10)?.unwrap_or_default(),
                ci_rerun_head: r.get(11)?,
                last_head_oid: r.get(12)?,
                queued_at: r.get(13)?,
                updated_at: r.get(14)?,
                pr_url: r.get(15)?,
                forge_host: r.get(16)?,
                project: r.get(17)?,
                checkout_gen: r.get(18)?,
            })
        })?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row?);
        }
        Ok(v)
    }

    // --- per-worktree disk usage cache (v20) -------------------------------
    /// `(size_bytes, target_bytes, fetched_at)` for one worktree, or `None`.
    fn get_worktree_disk(&self, worktree: &str) -> Result<Option<(i64, i64, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT size_bytes, target_bytes, fetched_at FROM worktree_disk WHERE worktree=?1",
                params![worktree],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .ok(); // best-effort: pure cache read (module doc): a failed read is a miss; the live source repopulates
        Ok(r)
    }

    fn put_worktree_disk(&self, worktree: &str, size_bytes: i64, target_bytes: i64) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO worktree_disk(worktree,size_bytes,target_bytes,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(worktree) DO UPDATE SET size_bytes=?2, target_bytes=?3, fetched_at=?4"#,
            params![worktree, size_bytes, target_bytes, util::now()],
        )?;
        Ok(())
    }

    /// All cached disk sizes keyed by worktree path → `(size_bytes, target_bytes)`.
    /// One bulk read for the sidebar/statusbar; never scans.
    fn all_worktree_disk(&self) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT worktree, size_bytes, target_bytes FROM worktree_disk")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    }

    fn all_worktree_disk_stamps(&self) -> Result<std::collections::HashMap<String, i64>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT worktree, COALESCE(fetched_at,0) FROM worktree_disk")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// Drop a worktree's cached size (e.g. right after a `clean`) so the badge
    /// clears without waiting for the next scan.
    fn delete_worktree_disk(&self, worktree: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM worktree_disk WHERE worktree=?1",
            params![worktree],
        )?;
        Ok(())
    }

    // --- worktree↔issue links (badge + palette surfacing) -------------------
    /// Associate `issue_id` (in `"<provider>:<key>"` form) with a worktree path.
    fn link_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO issue_links(worktree_path,issue_id,linked_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree_path,issue_id) DO UPDATE SET linked_at=?3"#,
            params![worktree_path, issue_id, util::now()],
        )?;
        Ok(())
    }

    /// Remove a worktree↔issue association.
    fn unlink_issue(&self, worktree_path: &str, issue_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM issue_links WHERE worktree_path=?1 AND issue_id=?2",
            params![worktree_path, issue_id],
        )?;
        Ok(())
    }

    /// All issue ids linked to a worktree, newest first.
    fn linked_issues(&self, worktree_path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT issue_id FROM issue_links WHERE worktree_path=?1 ORDER BY linked_at DESC",
        )?;
        let rows = stmt.query_map(params![worktree_path], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Every issue/worktree association, newest links first.
    fn issue_link_worktrees(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT issue_id,worktree_path FROM issue_links ORDER BY linked_at DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // --- reflog-undo bookkeeping (which resets are OURS, per worktree) ------
    /// Record a reset target we are about to create, pruning each worktree's
    /// mark set to the freshest 100 (the undo planner only reads ~100 reflog
    /// entries anyway).
    fn add_undo_mark(&self, worktree: &str, sha: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO undo_marks(worktree,sha,ts) VALUES(?1,?2,?3)
               ON CONFLICT(worktree,sha) DO UPDATE SET ts=?3"#,
            params![worktree, sha, util::now()],
        )?;
        self.conn().execute(
            r#"DELETE FROM undo_marks WHERE worktree=?1 AND sha NOT IN (
                 SELECT sha FROM undo_marks WHERE worktree=?1
                 ORDER BY ts DESC LIMIT 100)"#,
            params![worktree],
        )?;
        Ok(())
    }

    /// All recorded undo-reset targets for a worktree (newest first).
    fn undo_marks(&self, worktree: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT sha FROM undo_marks WHERE worktree=?1 ORDER BY ts DESC")?;
        let rows = stmt.query_map(params![worktree], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Record a sandbox event (exec, network, dns, orphan_gc) in the audit log.
    fn insert_container_event(
        &self,
        worktree: &str,
        ts: i64,
        kind: &str,
        detail: Option<&str>,
        exit_code: Option<i64>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO container_events (worktree, ts, kind, detail, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![worktree, ts, kind, detail, exit_code],
        )?;
        Ok(())
    }

    /// Retrieve the most recent `limit` container events for a worktree,
    /// newest first.
    fn container_events(&self, worktree: &str, limit: usize) -> Result<Vec<ContainerEvent>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, worktree, ts, kind, detail, exit_code
             FROM container_events
             WHERE worktree = ?1
             ORDER BY ts DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![worktree, limit as i64], |r| {
            Ok(ContainerEvent {
                id: r.get(0)?,
                worktree: r.get(1)?,
                ts: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
                exit_code: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete container events older than `older_than_secs` seconds. Called on
    /// startup to keep the audit table from growing unbounded.
    fn prune_container_events(&self, older_than_secs: i64) -> Result<usize> {
        let cutoff = crate::util::now() - older_than_secs;
        let n = self.conn().execute(
            "DELETE FROM container_events WHERE ts < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }
}
