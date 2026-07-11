//! Semantic blast-radius graph (schema **v42**): `sem_entity` (id → file / name /
//! kind / span / source_hash) + `sem_edge` (caller `src_id` → callee `dst_id`).
//!
//! The embedded-SQLite implementation of the [`SemanticStore`] seam. A sibling
//! `impl` block (using the `pub(crate) conn()` accessor) so the pinned `db.rs`
//! carries only the DDL + version bump. The graph is pure derived state — a
//! fresh DB rebuilds it from the fs-watcher — so writes are best-effort caches.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::semantic::EntityKind;
use crate::store::{SemEdgeRow, SemEntityRow, SemanticStore};

/// Serialize a 1-based inclusive line span as "start-end".
fn span_str(start: u32, end: u32) -> String {
    format!("{start}-{end}")
}

/// Parse a "start-end" span back to `(start, end)`, defaulting to `(0, 0)` on a
/// malformed value (a stale row — harmless, the graph is derived state).
fn parse_span(s: &str) -> (u32, u32) {
    let mut it = s.splitn(2, '-');
    let start = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let end = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (start, end)
}

impl SemanticStore for Db {
    fn replace_file_entities(&self, file: &str, entities: &[SemEntityRow]) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute("DELETE FROM sem_entity WHERE file=?1", params![file])?;
        for e in entities {
            tx.execute(
                r#"INSERT OR REPLACE INTO sem_entity
                     (id, file, name, kind, span, source_hash)
                   VALUES(?1,?2,?3,?4,?5,?6)"#,
                params![
                    e.id,
                    e.file,
                    e.name,
                    e.kind.as_db_str(),
                    span_str(e.start_line, e.end_line),
                    e.source_hash
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn upsert_entity(&self, e: &SemEntityRow) -> Result<()> {
        self.conn().execute(
            r#"INSERT OR REPLACE INTO sem_entity
                 (id, file, name, kind, span, source_hash)
               VALUES(?1,?2,?3,?4,?5,?6)"#,
            params![
                e.id,
                e.file,
                e.name,
                e.kind.as_db_str(),
                span_str(e.start_line, e.end_line),
                e.source_hash
            ],
        )?;
        Ok(())
    }

    fn file_source_hash(&self, file: &str) -> Result<Option<String>> {
        let got = self
            .conn()
            .query_row(
                "SELECT source_hash FROM sem_entity WHERE file=?1 LIMIT 1",
                params![file],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(got)
    }

    fn replace_edges_for_dsts(&self, dst_ids: &[String], edges: &[SemEdgeRow]) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        for dst in dst_ids {
            tx.execute("DELETE FROM sem_edge WHERE dst_id=?1", params![dst])?;
        }
        for e in edges {
            tx.execute(
                r#"INSERT OR REPLACE INTO sem_edge (src_id, dst_id, kind)
                   VALUES(?1,?2,?3)"#,
                params![e.src_id, e.dst_id, e.kind],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn callers_of(&self, dst_id: &str) -> Result<Vec<SemEntityRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            r#"SELECT e.id, e.file, e.name, e.kind, e.span, e.source_hash
               FROM sem_edge g
               JOIN sem_entity e ON e.id = g.src_id
               WHERE g.dst_id = ?1"#,
        )?;
        let rows = stmt.query_map(params![dst_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, file, name, kind, span, source_hash) = row?;
            // Drop rows with an unrecognized (stale/newer-schema) kind.
            let Some(kind) = EntityKind::from_db_str(&kind) else {
                continue;
            };
            let (start_line, end_line) = parse_span(&span);
            out.push(SemEntityRow {
                id,
                file,
                name,
                kind,
                start_line,
                end_line,
                source_hash,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_graph::entity_id;

    fn row(id: &str, file: &str, name: &str, kind: EntityKind, hash: &str) -> SemEntityRow {
        SemEntityRow {
            id: id.to_string(),
            file: file.to_string(),
            name: name.to_string(),
            kind,
            start_line: 1,
            end_line: 10,
            source_hash: hash.to_string(),
        }
    }

    #[test]
    fn round_trip_entities_edges_and_callers() {
        let db = Db::open_memory().unwrap();

        let callee_id = entity_id("/wt", "/wt/src/lib.rs", "target", EntityKind::Function);
        let caller_id = entity_id("/wt", "/wt/src/use.rs", "user", EntityKind::Function);
        let test_id = entity_id(
            "/wt",
            "/wt/tests/it.rs",
            "test_target",
            EntityKind::Function,
        );

        // Replace the changed file's entities (the callee).
        db.replace_file_entities(
            "/wt/src/lib.rs",
            &[row(
                &callee_id,
                "/wt/src/lib.rs",
                "target",
                EntityKind::Function,
                "h1",
            )],
        )
        .unwrap();
        // Upsert caller entities that live in other files.
        db.upsert_entity(&row(
            &caller_id,
            "/wt/src/use.rs",
            "user",
            EntityKind::Function,
            "h2",
        ))
        .unwrap();
        db.upsert_entity(&row(
            &test_id,
            "/wt/tests/it.rs",
            "test_target",
            EntityKind::Function,
            "h3",
        ))
        .unwrap();

        // Skip key.
        assert_eq!(
            db.file_source_hash("/wt/src/lib.rs").unwrap(),
            Some("h1".to_string())
        );
        assert_eq!(db.file_source_hash("/wt/nope.rs").unwrap(), None);

        // Edges caller→callee and test→callee.
        db.replace_edges_for_dsts(
            std::slice::from_ref(&callee_id),
            &[
                SemEdgeRow {
                    src_id: caller_id.clone(),
                    dst_id: callee_id.clone(),
                    kind: "ref".to_string(),
                },
                SemEdgeRow {
                    src_id: test_id.clone(),
                    dst_id: callee_id.clone(),
                    kind: "test".to_string(),
                },
            ],
        )
        .unwrap();

        let mut callers = db.callers_of(&callee_id).unwrap();
        callers.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(callers.len(), 2);
        assert_eq!(callers[0].name, "user");
        assert_eq!(callers[1].name, "test_target");
        assert_eq!(callers[1].kind, EntityKind::Function);

        // Re-replacing the callee's edges clears the old set.
        db.replace_edges_for_dsts(
            std::slice::from_ref(&callee_id),
            &[SemEdgeRow {
                src_id: caller_id.clone(),
                dst_id: callee_id.clone(),
                kind: "ref".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(db.callers_of(&callee_id).unwrap().len(), 1);
    }

    #[test]
    fn replace_file_entities_drops_vanished() {
        let db = Db::open_memory().unwrap();
        let a = entity_id("/wt", "/wt/f.rs", "a", EntityKind::Function);
        let b = entity_id("/wt", "/wt/f.rs", "b", EntityKind::Function);
        db.replace_file_entities(
            "/wt/f.rs",
            &[
                row(&a, "/wt/f.rs", "a", EntityKind::Function, "h1"),
                row(&b, "/wt/f.rs", "b", EntityKind::Function, "h1"),
            ],
        )
        .unwrap();
        // Re-parse: only `a` remains.
        db.replace_file_entities(
            "/wt/f.rs",
            &[row(&a, "/wt/f.rs", "a", EntityKind::Function, "h2")],
        )
        .unwrap();
        assert_eq!(
            db.file_source_hash("/wt/f.rs").unwrap(),
            Some("h2".to_string())
        );
        // `b` no longer resolvable as a caller.
        db.upsert_entity(&row(
            &entity_id("/wt", "/wt/g.rs", "callee", EntityKind::Function),
            "/wt/g.rs",
            "callee",
            EntityKind::Function,
            "h3",
        ))
        .unwrap();
    }
}
