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

    fn entities_under(&self, root_prefix: &str) -> Result<Vec<SemEntityRow>> {
        let conn = self.conn();
        // Prefix-anchor on a path boundary: `<root>/%` never captures a sibling
        // `<root>2/…`. `\` escapes SQL-LIKE wildcards in a path that happens to
        // contain `%` or `_`. The existing `idx_sem_entity_file` serves it.
        let pattern = format!("{}/%", like_escape(root_prefix));
        let mut stmt = conn.prepare(
            r#"SELECT id, file, name, kind, span, source_hash
               FROM sem_entity
               WHERE file LIKE ?1 ESCAPE '\'"#,
        )?;
        let rows = stmt.query_map(params![pattern], |r| {
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

    fn caller_degrees(&self) -> Result<Vec<(String, u32)>> {
        let conn = self.conn();
        // In-degree = distinct callers per callee. `idx_sem_edge_dst` serves the
        // group-by. A caller reaching a callee twice (a `ref` and a `test` edge)
        // counts once — the ranking signal is "how many entities depend on this".
        let mut stmt = conn.prepare(
            r#"SELECT dst_id, COUNT(DISTINCT src_id)
               FROM sem_edge
               GROUP BY dst_id"#,
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// Escape SQL-LIKE metacharacters (`%`, `_`, and the escape char `\`) so a path
/// used as a `LIKE` prefix matches literally. Paired with `ESCAPE '\'`.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

    #[test]
    fn entities_under_prefix_anchors_on_path_boundary() {
        let db = Db::open_memory().unwrap();
        // Two worktrees whose roots share a prefix: `/wt` and `/wt2`.
        let a = entity_id("/wt", "/wt/src/a.rs", "a", EntityKind::Function);
        let sib = entity_id("/wt2", "/wt2/src/b.rs", "b", EntityKind::Function);
        db.upsert_entity(&row(&a, "/wt/src/a.rs", "a", EntityKind::Function, "h1"))
            .unwrap();
        db.upsert_entity(&row(&sib, "/wt2/src/b.rs", "b", EntityKind::Function, "h2"))
            .unwrap();

        let under = db.entities_under("/wt").unwrap();
        // Only `/wt/…`, never the sibling `/wt2/…`.
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!(under[0].file, "/wt/src/a.rs");
        // The sibling resolves on its own root.
        assert_eq!(db.entities_under("/wt2").unwrap().len(), 1);
        // A root with no rows yields nothing.
        assert!(db.entities_under("/nowhere").unwrap().is_empty());
    }

    #[test]
    fn entities_under_escapes_like_wildcards() {
        let db = Db::open_memory().unwrap();
        // A path containing a `%` (a LIKE wildcard) must match literally, not as
        // "any characters".
        let pct = entity_id("/w%t", "/w%t/a.rs", "a", EntityKind::Function);
        let other = entity_id("/wXt", "/wXt/a.rs", "a", EntityKind::Function);
        db.upsert_entity(&row(&pct, "/w%t/a.rs", "a", EntityKind::Function, "h1"))
            .unwrap();
        db.upsert_entity(&row(&other, "/wXt/a.rs", "a", EntityKind::Function, "h2"))
            .unwrap();
        let under = db.entities_under("/w%t").unwrap();
        assert_eq!(under.len(), 1, "{under:?}");
        assert_eq!(under[0].file, "/w%t/a.rs");
    }

    #[test]
    fn caller_degrees_counts_distinct_callers_per_callee() {
        let db = Db::open_memory().unwrap();
        let callee = entity_id("/wt", "/wt/a.rs", "target", EntityKind::Function);
        let c1 = entity_id("/wt", "/wt/u1.rs", "u1", EntityKind::Function);
        let c2 = entity_id("/wt", "/wt/u2.rs", "u2", EntityKind::Function);
        db.replace_edges_for_dsts(
            std::slice::from_ref(&callee),
            &[
                // c1 reaches the callee via BOTH a ref and a test edge — counts once.
                SemEdgeRow {
                    src_id: c1.clone(),
                    dst_id: callee.clone(),
                    kind: "ref".to_string(),
                },
                SemEdgeRow {
                    src_id: c1.clone(),
                    dst_id: callee.clone(),
                    kind: "test".to_string(),
                },
                SemEdgeRow {
                    src_id: c2.clone(),
                    dst_id: callee.clone(),
                    kind: "ref".to_string(),
                },
            ],
        )
        .unwrap();
        let degrees = db.caller_degrees().unwrap();
        assert_eq!(degrees.len(), 1);
        assert_eq!(degrees[0].0, callee);
        assert_eq!(degrees[0].1, 2, "distinct callers, not edges");
        // Empty graph → no degrees.
        assert!(
            Db::open_memory()
                .unwrap()
                .caller_degrees()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn like_escape_escapes_metacharacters() {
        assert_eq!(like_escape("plain"), "plain");
        assert_eq!(like_escape("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }
}
