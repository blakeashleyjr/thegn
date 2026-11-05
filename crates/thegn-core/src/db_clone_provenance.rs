//! Durable, credential-free provenance for cloned workspace destinations.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::db::Db;

pub const MAX_CLONE_PROVENANCE_ROWS: i64 = 1024;
const MAX_DEST_BYTES: usize = 4096;
const MAX_ORIGIN_BYTES: usize = 2048;
const MAX_LABEL_BYTES: usize = 128;
const MAX_VERSION_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CloneProvenanceState {
    Cloning,
    Complete,
    Abandoned,
}

impl CloneProvenanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloning => "cloning",
            Self::Complete => "complete",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "cloning" => Some(Self::Cloning),
            "complete" => Some(Self::Complete),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NewCloneProvenance<'a> {
    pub dest: &'a str,
    pub canonical_origin: &'a str,
    pub label: &'a str,
    pub clone_pid: u32,
    pub thegn_version: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CloneProvenance {
    pub dest: String,
    pub canonical_origin: String,
    pub label: String,
    pub state: CloneProvenanceState,
    pub clone_pid: Option<u32>,
    pub thegn_version: String,
    pub started_at: i64,
    pub validated_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceStatus {
    Verified,
    OriginChanged,
    Missing,
    Unverifiable,
    Cloning,
    Interrupted,
    Abandoned,
}

impl ProvenanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::OriginChanged => "origin-changed",
            Self::Missing => "missing",
            Self::Unverifiable => "unverifiable",
            Self::Cloning => "cloning",
            Self::Interrupted => "interrupted",
            Self::Abandoned => "abandoned",
        }
    }
}

pub fn validate_new_clone_provenance(new: &NewCloneProvenance<'_>) -> Result<()> {
    if !std::path::Path::new(new.dest).is_absolute() {
        bail!("clone destination must be absolute");
    }
    if new.dest.len() > MAX_DEST_BYTES {
        bail!("clone destination exceeds {MAX_DEST_BYTES} bytes");
    }
    if new.canonical_origin.len() > MAX_ORIGIN_BYTES {
        bail!("canonical origin exceeds {MAX_ORIGIN_BYTES} bytes");
    }
    let Some((_, authority_and_path)) = new.canonical_origin.split_once("://") else {
        bail!("canonical origin must contain ://");
    };
    let authority = authority_and_path
        .split_once('/')
        .map_or(authority_and_path, |(authority, _)| authority);
    if authority.contains('@') {
        bail!("canonical origin must not carry credentials");
    }
    if new.label.is_empty() {
        bail!("clone label must not be empty");
    }
    if new.label.len() > MAX_LABEL_BYTES {
        bail!("clone label exceeds {MAX_LABEL_BYTES} bytes");
    }
    if new.thegn_version.is_empty() {
        bail!("thegn version must not be empty");
    }
    if new.thegn_version.len() > MAX_VERSION_BYTES {
        bail!("thegn version exceeds {MAX_VERSION_BYTES} bytes");
    }
    if [new.dest, new.canonical_origin, new.label, new.thegn_version]
        .iter()
        .any(|field| field.chars().any(char::is_control))
    {
        bail!("clone provenance fields must not contain control characters");
    }
    if new.clone_pid == 0 {
        bail!("clone pid must be nonzero");
    }
    Ok(())
}

pub fn classify_clone_provenance(
    row: &CloneProvenance,
    dest_present: bool,
    observed_origin: Option<&str>,
    owner_alive: bool,
) -> ProvenanceStatus {
    match row.state {
        CloneProvenanceState::Abandoned => ProvenanceStatus::Abandoned,
        CloneProvenanceState::Cloning => {
            if owner_alive {
                ProvenanceStatus::Cloning
            } else {
                ProvenanceStatus::Interrupted
            }
        }
        CloneProvenanceState::Complete => {
            if !dest_present {
                ProvenanceStatus::Missing
            } else {
                match observed_origin {
                    None => ProvenanceStatus::Unverifiable,
                    Some(origin) if origin == row.canonical_origin => ProvenanceStatus::Verified,
                    Some(_) => ProvenanceStatus::OriginChanged,
                }
            }
        }
    }
}

impl Db {
    pub fn begin_clone_provenance(
        &self,
        new: &NewCloneProvenance<'_>,
        owner_alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool> {
        validate_new_clone_provenance(new)?;
        let tx = self.conn().unchecked_transaction()?;
        let existing: Option<(String, Option<i64>)> = tx
            .query_row(
                "SELECT state, clone_pid FROM workspace_clone_provenance WHERE dest=?1",
                [new.dest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((state, Some(pid))) = existing
            && state == CloneProvenanceState::Cloning.as_str()
            && u32::try_from(pid).is_ok_and(|pid| pid != new.clone_pid && owner_alive(pid))
        {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO workspace_clone_provenance
                (dest, canonical_origin, label, state, clone_pid, thegn_version, started_at, validated_at)
             VALUES (?1, ?2, ?3, 'cloning', ?4, ?5, ?6, NULL)
             ON CONFLICT(dest) DO UPDATE SET
                canonical_origin=excluded.canonical_origin,
                label=excluded.label,
                state='cloning',
                clone_pid=excluded.clone_pid,
                thegn_version=excluded.thegn_version,
                started_at=excluded.started_at,
                validated_at=NULL",
            params![
                new.dest,
                new.canonical_origin,
                new.label,
                i64::from(new.clone_pid),
                new.thegn_version,
                crate::util::now()
            ],
        )?;
        tx.execute(
            "DELETE FROM workspace_clone_provenance
             WHERE dest NOT IN (
                 SELECT dest FROM workspace_clone_provenance
                 ORDER BY started_at DESC, dest ASC LIMIT ?1
             )",
            [MAX_CLONE_PROVENANCE_ROWS],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn finish_clone_provenance(
        &self,
        dest: &str,
        clone_pid: u32,
        state: CloneProvenanceState,
    ) -> Result<bool> {
        if state == CloneProvenanceState::Cloning {
            bail!("finish_clone_provenance needs a terminal state");
        }
        let validated_at = (state == CloneProvenanceState::Complete).then(crate::util::now);
        let changed = self.conn().execute(
            "UPDATE workspace_clone_provenance SET state=?1, validated_at=?2
             WHERE dest=?3 AND state='cloning' AND clone_pid=?4",
            params![state.as_str(), validated_at, dest, i64::from(clone_pid)],
        )?;
        Ok(changed == 1)
    }

    pub fn clone_provenance(&self, dest: &str) -> Result<Option<CloneProvenance>> {
        let row = self
            .conn()
            .query_row(
                "SELECT dest, canonical_origin, label, state, clone_pid, thegn_version, started_at, validated_at
                 FROM workspace_clone_provenance WHERE dest=?1",
                [dest],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(decode_row).transpose()
    }

    pub fn recent_clone_provenance(&self, limit: i64) -> Result<Vec<CloneProvenance>> {
        let mut statement = self.conn().prepare(
            "SELECT dest, canonical_origin, label, state, clone_pid, thegn_version, started_at, validated_at
             FROM workspace_clone_provenance ORDER BY started_at DESC, dest ASC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.clamp(1, MAX_CLONE_PROVENANCE_ROWS)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        })?;
        rows.map(|row| decode_row(row?)).collect()
    }
}

type RawCloneProvenance = (
    String,
    String,
    String,
    String,
    Option<i64>,
    String,
    i64,
    Option<i64>,
);

fn decode_row(row: RawCloneProvenance) -> Result<CloneProvenance> {
    let (dest, canonical_origin, label, state, clone_pid, thegn_version, started_at, validated_at) =
        row;
    let state = CloneProvenanceState::parse(&state)
        .ok_or_else(|| anyhow::anyhow!("unknown clone provenance state for {dest}: {state}"))?;
    Ok(CloneProvenance {
        dest,
        canonical_origin,
        label,
        state,
        clone_pid: clone_pid.and_then(|pid| u32::try_from(pid).ok()),
        thegn_version,
        started_at,
        validated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CloneProvenance, CloneProvenanceState, MAX_CLONE_PROVENANCE_ROWS, NewCloneProvenance,
        ProvenanceStatus, classify_clone_provenance, validate_new_clone_provenance,
    };
    use crate::db::Db;
    use rusqlite::OptionalExtension;

    fn new(dest: &str, pid: u32) -> NewCloneProvenance<'_> {
        NewCloneProvenance {
            dest,
            canonical_origin: "https://example.com/o/r",
            label: "r",
            clone_pid: pid,
            thegn_version: "0.1.0-test",
        }
    }

    #[test]
    fn begin_finish_complete_round_trips_and_stamps_validation() {
        let db = Db::open_memory().unwrap();
        assert!(
            db.begin_clone_provenance(&new("/ws/r", 11), &|_| false)
                .unwrap()
        );
        let cloning = db.clone_provenance("/ws/r").unwrap().unwrap();
        assert_eq!(cloning.state, CloneProvenanceState::Cloning);
        assert_eq!(cloning.clone_pid, Some(11));
        assert_eq!(cloning.validated_at, None);
        assert!(cloning.started_at > 0);
        assert!(
            db.finish_clone_provenance("/ws/r", 11, CloneProvenanceState::Complete)
                .unwrap()
        );
        let complete = db.clone_provenance("/ws/r").unwrap().unwrap();
        assert_eq!(complete.state, CloneProvenanceState::Complete);
        assert!(complete.validated_at.is_some());
        assert_eq!(complete.canonical_origin, "https://example.com/o/r");
        assert!(
            !db.finish_clone_provenance("/ws/r", 11, CloneProvenanceState::Complete)
                .unwrap()
        );
        assert!(db.clone_provenance("/ws/none").unwrap().is_none());
    }

    #[test]
    fn begin_refuses_a_live_foreign_cloner_and_takes_over_a_dead_one() {
        let db = Db::open_memory().unwrap();
        assert!(
            db.begin_clone_provenance(&new("/ws/r", 11), &|_| false)
                .unwrap()
        );
        assert!(
            !db.begin_clone_provenance(&new("/ws/r", 22), &|pid| pid == 11)
                .unwrap()
        );
        assert_eq!(
            db.clone_provenance("/ws/r").unwrap().unwrap().clone_pid,
            Some(11)
        );
        assert!(
            db.begin_clone_provenance(&new("/ws/r", 22), &|_| false)
                .unwrap()
        );
        assert_eq!(
            db.clone_provenance("/ws/r").unwrap().unwrap().clone_pid,
            Some(22)
        );
        assert!(
            db.begin_clone_provenance(&new("/ws/r", 22), &|pid| pid == 22)
                .unwrap()
        );
        db.finish_clone_provenance("/ws/r", 22, CloneProvenanceState::Complete)
            .unwrap();
        assert!(
            db.begin_clone_provenance(&new("/ws/r", 22), &|_| false)
                .unwrap()
        );
        let restarted = db.clone_provenance("/ws/r").unwrap().unwrap();
        assert_eq!(restarted.state, CloneProvenanceState::Cloning);
        assert_eq!(restarted.validated_at, None);
    }

    #[test]
    fn finish_only_updates_the_owners_cloning_row() {
        let db = Db::open_memory().unwrap();
        db.begin_clone_provenance(&new("/ws/r", 11), &|_| false)
            .unwrap();
        assert!(
            !db.finish_clone_provenance("/ws/r", 99, CloneProvenanceState::Complete)
                .unwrap()
        );
        assert_eq!(
            db.clone_provenance("/ws/r").unwrap().unwrap().state,
            CloneProvenanceState::Cloning
        );
        assert!(
            db.finish_clone_provenance("/ws/r", 11, CloneProvenanceState::Cloning)
                .is_err()
        );
        assert!(
            db.finish_clone_provenance("/ws/r", 11, CloneProvenanceState::Abandoned)
                .unwrap()
        );
        let abandoned = db.clone_provenance("/ws/r").unwrap().unwrap();
        assert_eq!(abandoned.state, CloneProvenanceState::Abandoned);
        assert_eq!(abandoned.validated_at, None);
        assert!(
            !db.finish_clone_provenance("/ws/none", 11, CloneProvenanceState::Complete)
                .unwrap()
        );
    }

    #[test]
    fn validation_refuses_credentials_controls_and_oversize_fields() {
        assert!(validate_new_clone_provenance(&new("/ws/r", 1)).is_ok());
        for origin in [
            "https://user:pw@example.com/o/r",
            "ssh://git@example.com/o/r",
            "https/example.com/o/r",
        ] {
            assert!(
                validate_new_clone_provenance(&NewCloneProvenance {
                    canonical_origin: origin,
                    ..new("/ws/r", 1)
                })
                .is_err()
            );
        }
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                dest: "relative/path",
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                dest: "/ws/\nr",
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                canonical_origin: "https://example.com/o/\nr",
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                label: "",
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                label: "r\n",
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                label: &"x".repeat(129),
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                thegn_version: &"x".repeat(65),
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                thegn_version: "0\n",
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                canonical_origin: &"x".repeat(2049),
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                dest: &format!("/{}", "x".repeat(4096)),
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                clone_pid: 0,
                ..new("/ws/r", 1)
            })
            .is_err()
        );
        assert!(
            validate_new_clone_provenance(&NewCloneProvenance {
                canonical_origin: "https://example.com/o/r@v1",
                ..new("/ws/r", 1)
            })
            .is_ok()
        );
        let db = Db::open_memory().unwrap();
        assert!(
            db.begin_clone_provenance(
                &NewCloneProvenance {
                    canonical_origin: "https://user:pw@example.com/o/r",
                    ..new("/ws/r", 1)
                },
                &|_| false
            )
            .is_err()
        );
        assert!(db.recent_clone_provenance(10).unwrap().is_empty());
    }

    #[test]
    fn begin_prunes_to_the_row_cap_keeping_the_newest() {
        let db = Db::open_memory().unwrap();
        for index in 0..MAX_CLONE_PROVENANCE_ROWS {
            let dest = format!("/ws/r{index}");
            assert!(
                db.begin_clone_provenance(&new(&dest, 7), &|_| false)
                    .unwrap()
            );
        }
        db.conn()
            .execute(
                "UPDATE workspace_clone_provenance SET started_at = rowid",
                [],
            )
            .unwrap();
        assert!(
            db.begin_clone_provenance(&new("/ws/new", 8), &|_| false)
                .unwrap()
        );
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM workspace_clone_provenance",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, MAX_CLONE_PROVENANCE_ROWS);
        assert!(db.clone_provenance("/ws/new").unwrap().is_some());
        let oldest: Option<String> = db
            .conn()
            .query_row(
                "SELECT dest FROM workspace_clone_provenance ORDER BY started_at ASC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(db.clone_provenance("/ws/r0").unwrap().is_none());
        assert_ne!(oldest.as_deref(), Some("/ws/r0"));
    }

    #[test]
    fn recent_is_newest_first_and_clamps_the_limit() {
        let db = Db::open_memory().unwrap();
        for dest in ["/ws/10", "/ws/30", "/ws/20"] {
            db.begin_clone_provenance(&new(dest, 7), &|_| false)
                .unwrap();
        }
        db.conn()
            .execute(
                "UPDATE workspace_clone_provenance SET started_at = CASE dest
                     WHEN '/ws/10' THEN 10 WHEN '/ws/30' THEN 30 ELSE 20 END",
                [],
            )
            .unwrap();
        let recent = db.recent_clone_provenance(10).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|row| row.dest.as_str())
                .collect::<Vec<_>>(),
            ["/ws/30", "/ws/20", "/ws/10"]
        );
        assert_eq!(db.recent_clone_provenance(2).unwrap().len(), 2);
        assert_eq!(db.recent_clone_provenance(0).unwrap().len(), 1);
        assert_eq!(db.recent_clone_provenance(-5).unwrap().len(), 1);
        assert_eq!(db.recent_clone_provenance(i64::MAX).unwrap().len(), 3);
    }

    #[test]
    fn classify_covers_every_state_and_observation() {
        let row = CloneProvenance {
            dest: "/ws/r".into(),
            canonical_origin: "https://example.com/o/r".into(),
            label: "r".into(),
            state: CloneProvenanceState::Complete,
            clone_pid: Some(11),
            thegn_version: "0.1.0-test".into(),
            started_at: 1,
            validated_at: Some(2),
        };
        assert_eq!(
            classify_clone_provenance(&row, false, None, false),
            ProvenanceStatus::Missing
        );
        assert_eq!(
            classify_clone_provenance(&row, true, None, false),
            ProvenanceStatus::Unverifiable
        );
        assert_eq!(
            classify_clone_provenance(&row, true, Some("https://example.com/o/r"), false),
            ProvenanceStatus::Verified
        );
        assert_eq!(
            classify_clone_provenance(&row, true, Some("https://other.example/o/r"), false),
            ProvenanceStatus::OriginChanged
        );
        let cloning = CloneProvenance {
            state: CloneProvenanceState::Cloning,
            ..row.clone()
        };
        assert_eq!(
            classify_clone_provenance(&cloning, true, None, true),
            ProvenanceStatus::Cloning
        );
        assert_eq!(
            classify_clone_provenance(&cloning, true, None, false),
            ProvenanceStatus::Interrupted
        );
        let abandoned = CloneProvenance {
            state: CloneProvenanceState::Abandoned,
            ..row
        };
        assert_eq!(
            classify_clone_provenance(&abandoned, false, None, true),
            ProvenanceStatus::Abandoned
        );
        assert_eq!(
            [
                ProvenanceStatus::Verified,
                ProvenanceStatus::OriginChanged,
                ProvenanceStatus::Missing,
                ProvenanceStatus::Unverifiable,
                ProvenanceStatus::Cloning,
                ProvenanceStatus::Interrupted,
                ProvenanceStatus::Abandoned,
            ]
            .map(ProvenanceStatus::as_str),
            [
                "verified",
                "origin-changed",
                "missing",
                "unverifiable",
                "cloning",
                "interrupted",
                "abandoned"
            ]
        );
    }

    #[test]
    fn state_round_trips_through_its_text_form() {
        for state in [
            CloneProvenanceState::Cloning,
            CloneProvenanceState::Complete,
            CloneProvenanceState::Abandoned,
        ] {
            assert_eq!(CloneProvenanceState::parse(state.as_str()), Some(state));
        }
        assert_eq!(CloneProvenanceState::parse("bogus"), None);
        assert_eq!(CloneProvenanceState::parse(""), None);
        assert_eq!(CloneProvenanceState::parse("Complete"), None);

        let db = Db::open_memory().unwrap();
        db.conn()
            .execute_batch(
                "PRAGMA ignore_check_constraints=ON;
                 INSERT INTO workspace_clone_provenance
                    (dest, canonical_origin, label, state, thegn_version, started_at)
                 VALUES ('/ws/bad', 'https://example.com/o/r', 'r', 'bogus', '0', 1);",
            )
            .unwrap();
        let error = db.clone_provenance("/ws/bad").unwrap_err();
        assert!(error.to_string().contains("/ws/bad"));
    }
}
