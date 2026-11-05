//! The source tip, target base and result OID a merge-queue drain landed (THE-401).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;

/// What a drain landing folded and replaced. `source_oid` is the row's captured candidate tip
/// (None for rows enqueued without one); `target_base_oid` is the OID the landing CAS replaced
/// (None for landings that did not record one: `thegn land`, rows landed before v107).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MergeLandedProvenance {
    pub source_oid: Option<String>,
    pub target_base_oid: Option<String>,
    pub result_oid: String,
}

impl Db {
    /// `Some` only for a row whose status is `landed` with a non-empty `result_oid`.
    pub fn merge_landed_provenance(&self, worktree: &str) -> Result<Option<MergeLandedProvenance>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT landed_source_oid, landed_target_base_oid, result_oid FROM merge_queue \
                 WHERE worktree=?1 AND status='landed' AND result_oid IS NOT NULL AND result_oid<>''",
                params![worktree],
                |row| {
                    Ok(MergeLandedProvenance {
                        source_oid: row.get(0)?,
                        target_base_oid: row.get(1)?,
                        result_oid: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::MergeLandedProvenance;
    use crate::db::Db;
    use crate::store::{
        MergeCandidate, MergeClaimRequest, MergeStatusFields, WorkspaceStore, WorktreeAuxStore,
    };
    use rusqlite::params;

    const WT: &str = "/private/repo/feature";
    const SRC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const RES: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn request() -> MergeClaimRequest {
        MergeClaimRequest {
            worktree: WT.into(),
            branch: "feature".into(),
            repo_root: "/private/repo".into(),
            target_branch: "main".into(),
            location: String::new(),
            expected_status: "queued".into(),
            lease_until_ms: crate::util::now_ms() + 60_000,
            owner_hint: Some("worker-a".into()),
        }
    }

    fn seeded(candidate: Option<&str>) -> Db {
        let db = Db::open_memory().unwrap();
        db.put_worktree("feature", "/private/repo", WT, "feature", None, None)
            .unwrap();
        if let Some(candidate_oid) = candidate {
            db.enqueue_merge_candidate(
                WT,
                "feature",
                "main",
                &MergeCandidate {
                    branch_ref: "refs/heads/feature".into(),
                    candidate_oid: candidate_oid.into(),
                },
            )
            .unwrap();
        } else {
            db.enqueue_merge(WT, "feature", "main").unwrap();
        }
        db
    }

    fn land(db: &Db, base: &str) {
        let claim = db.admit_merge_claim(&request()).unwrap().unwrap();
        let claim = db
            .replace_merge_status_claimed(&claim, "folding", &MergeStatusFields::default())
            .unwrap();
        db.land_merge_status_claimed(
            &claim,
            &MergeStatusFields {
                result_oid: Some(RES.into()),
                ..Default::default()
            },
            base,
        )
        .unwrap();
    }

    fn seed_landed_columns(db: &Db, status: &str) {
        db.conn()
            .execute(
                "UPDATE merge_queue SET status=?1, result_oid=?2, landed_source_oid=?3, \
                 landed_target_base_oid=?4 WHERE worktree=?5",
                params![status, RES, SRC, BASE, WT],
            )
            .unwrap();
    }

    fn columns(db: &Db) -> (Option<String>, Option<String>) {
        db.conn()
            .query_row(
                "SELECT landed_source_oid, landed_target_base_oid FROM merge_queue WHERE worktree=?1",
                [WT],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn claimed_landing_records_source_base_and_result() {
        let db = seeded(Some(SRC));
        land(&db, BASE);
        assert_eq!(
            db.merge_landed_provenance(WT).unwrap(),
            Some(MergeLandedProvenance {
                source_oid: Some(SRC.into()),
                target_base_oid: Some(BASE.into()),
                result_oid: RES.into(),
            })
        );
        assert_eq!(
            db.merge_queue_events(WT, 10).unwrap()[0]
                .target_base_oid
                .as_deref(),
            Some(BASE)
        );
    }

    #[test]
    fn landing_without_a_captured_candidate_records_base_and_no_source() {
        let db = seeded(None);
        land(&db, BASE);
        let landed = db.merge_landed_provenance(WT).unwrap().unwrap();
        assert_eq!(landed.source_oid, None);
        assert_eq!(landed.target_base_oid.as_deref(), Some(BASE));
    }

    #[test]
    fn non_landed_claimed_write_clears_the_columns() {
        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "queued");
        let request = request();
        let claim = db.admit_merge_claim(&request).unwrap().unwrap();
        db.replace_merge_status_claimed(&claim, "deferred", &MergeStatusFields::default())
            .unwrap();
        assert_eq!(columns(&db), (None, None));
    }

    #[test]
    fn every_requeue_path_clears_the_columns() {
        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "landed");
        assert!(db.retry_merge_entry(WT).unwrap());
        assert_eq!(columns(&db), (None, None));

        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "landed");
        db.enqueue_merge(WT, "feature", "main").unwrap();
        assert_eq!(columns(&db), (None, None));

        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "landed");
        db.replace_merge_status(WT, "queued", &MergeStatusFields::default())
            .unwrap();
        assert_eq!(columns(&db), (None, None));

        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "deferred");
        db.retry_held_merge_entry(WT).unwrap();
        assert_eq!(columns(&db), (None, None));

        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "landed");
        db.enqueue_merge_candidate(
            WT,
            "feature",
            "main",
            &MergeCandidate {
                branch_ref: "refs/heads/feature".into(),
                candidate_oid: SRC.into(),
            },
        )
        .unwrap();
        assert_eq!(columns(&db), (None, None));
    }

    #[test]
    fn reader_returns_none_unless_the_row_is_landed_with_a_result() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.merge_landed_provenance(WT).unwrap(), None);
        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "queued");
        assert_eq!(db.merge_landed_provenance(WT).unwrap(), None);
        seed_landed_columns(&db, "landed");
        db.conn()
            .execute(
                "UPDATE merge_queue SET result_oid='' WHERE worktree=?1",
                [WT],
            )
            .unwrap();
        assert_eq!(db.merge_landed_provenance(WT).unwrap(), None);
        db.conn()
            .execute(
                "UPDATE merge_queue SET result_oid=?1 WHERE worktree=?2",
                params![RES, WT],
            )
            .unwrap();
        assert!(db.merge_landed_provenance(WT).unwrap().is_some());
    }

    #[test]
    fn update_to_landed_keeps_and_other_status_clears_the_columns() {
        let db = seeded(Some(SRC));
        seed_landed_columns(&db, "landed");
        db.update_merge_status(WT, "landed", Some(RES), None, None)
            .unwrap();
        assert_eq!(columns(&db), (Some(SRC.into()), Some(BASE.into())));
        db.update_merge_status(WT, "queued", None, None, None)
            .unwrap();
        assert_eq!(columns(&db), (None, None));
    }
}
