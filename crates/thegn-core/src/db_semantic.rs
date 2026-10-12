//! Semantic blast-radius graph (schema **v42**): `sem_entity` (id → file / name /
//! kind / span / source_hash) + `sem_edge` (caller `src_id` → callee `dst_id`).
//!
//! The embedded-SQLite implementation of the [`SemanticStore`] seam. A sibling
//! `impl` block (using the `pub(crate) conn()` accessor) so the pinned `db.rs`
//! carries only the DDL + version bump. The graph is pure derived state — a
//! fresh DB rebuilds it from the fs-watcher — so writes are best-effort caches.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;

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

impl Db {
    /// Sweep one bounded slice of semantic entities whose source files are gone.
    /// A missing file under the longest matching registered root is pruned only
    /// when that root is locally live (`.git` exists); remote or unavailable
    /// roots stay protected. When the worker reaches the terminal slice
    /// (`scanned == 0`), this also triggers the bounded orphan-worktree cache
    /// sweep (up to [`crate::db_workspace::ORPHAN_CACHE_SWEEP_MAX_KEYS`] keys);
    /// see `db.rs`'s worker loop.
    pub(crate) fn prune_missing_semantic_files(
        &self,
        after_rowid: i64,
        limit: usize,
    ) -> Result<(usize, i64)> {
        let roots = {
            let mut stmt = self
                .conn()
                .prepare("SELECT worktree FROM worktrees WHERE worktree != ''")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let candidates = {
            let mut stmt = self.conn().prepare(
                "SELECT rowid, id, file FROM sem_entity WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![after_rowid, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let scanned = candidates.len();
        let next_rowid = candidates
            .last()
            .map_or(after_rowid, |(rowid, _, _)| *rowid);
        let candidate_files: std::collections::BTreeSet<&str> = candidates
            .iter()
            .map(|(_, _, file)| file.as_str())
            .collect();
        let mut root_liveness: HashMap<&str, bool> = HashMap::new();
        let stale_files: Vec<&str> = candidate_files
            .into_iter()
            .filter(|file| {
                let path = Path::new(file);
                if path.exists() {
                    return false;
                }
                let matching_root = roots
                    .iter()
                    .filter(|root| path.starts_with(Path::new(root.as_str())))
                    .max_by_key(|root| root.len());
                matching_root.is_none_or(|root| {
                    *root_liveness.entry(root.as_str()).or_insert_with(|| {
                        let root_path = Path::new(root);
                        root_path.is_dir() && root_path.join(".git").exists()
                    })
                })
            })
            .collect();
        if !stale_files.is_empty() {
            let stale_file_set: std::collections::BTreeSet<&str> =
                stale_files.into_iter().collect();
            let tx = self.conn().unchecked_transaction()?;
            for (_, id, file) in &candidates {
                if !stale_file_set.contains(file.as_str()) {
                    continue;
                }
                tx.execute(
                    "DELETE FROM sem_edge WHERE src_id=?1 OR dst_id=?1",
                    params![id],
                )?;
                tx.execute("DELETE FROM sem_entity WHERE id=?1", params![id])?;
            }
            tx.commit()?;
        }
        if scanned == 0 {
            self.prune_orphan_worktree_caches(crate::db_workspace::ORPHAN_CACHE_SWEEP_MAX_KEYS)?;
        }
        Ok((scanned, next_rowid))
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
    fn missing_file_sweep_removes_dead_rows_and_keeps_registered_roots() {
        use crate::store::WorkspaceStore;

        let db = Db::open_memory().unwrap();
        db.put_worktree("live", "/repo", "/live", "main", None, None)
            .unwrap();
        db.put_worktree(
            "remote",
            "/repo",
            "/remote/wt",
            "remote-branch",
            Some(r#"{"path":"/remote/wt","host":"builder"}"#),
            None,
        )
        .unwrap();
        db.upsert_entity(&row(
            "dead",
            "/gone/src/lib.rs",
            "dead",
            EntityKind::Function,
            "h1",
        ))
        .unwrap();
        db.upsert_entity(&row(
            "live",
            "/live/src/lib.rs",
            "live",
            EntityKind::Function,
            "h2",
        ))
        .unwrap();
        db.upsert_entity(&row(
            "remote",
            "/remote/wt/src/lib.rs",
            "remote",
            EntityKind::Function,
            "h3",
        ))
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO pr_cache(worktree,branch,json,fetched_at) VALUES('/gone/wt','main','[]',1)",
                [],
            )
            .unwrap();

        let mut after_rowid = 0;
        let mut first_scanned = None;
        loop {
            let (scanned, next_rowid) = db.prune_missing_semantic_files(after_rowid, 512).unwrap();
            if first_scanned.is_none() {
                first_scanned = Some(scanned);
            }
            if scanned == 0 {
                break;
            }
            after_rowid = next_rowid;
        }

        assert_eq!(first_scanned.unwrap(), 3);
        assert!(db.file_source_hash("/gone/src/lib.rs").unwrap().is_none());
        assert_eq!(
            db.file_source_hash("/live/src/lib.rs").unwrap().as_deref(),
            Some("h2")
        );
        assert_eq!(
            db.file_source_hash("/remote/wt/src/lib.rs")
                .unwrap()
                .as_deref(),
            Some("h3")
        );
        let orphan_cache_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM pr_cache WHERE worktree='/gone/wt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            orphan_cache_count, 0,
            "terminal sweep must prune orphan caches"
        );
    }

    #[test]
    fn missing_file_sweep_prunes_deleted_files_under_live_root() {
        use crate::store::WorkspaceStore;
        use std::fs;

        let db = Db::open_memory().unwrap();
        let live_root = tempfile::tempdir().unwrap();
        fs::create_dir(live_root.path().join(".git")).unwrap();
        fs::create_dir(live_root.path().join("src")).unwrap();
        let kept_file = live_root.path().join("src/kept.rs");
        fs::write(&kept_file, "pub fn kept() {}\n").unwrap();
        let live_root_path = live_root.path().to_str().unwrap();
        let kept_path = kept_file.to_str().unwrap();
        let gone_path = live_root.path().join("src/gone.rs");
        let gone_path = gone_path.to_str().unwrap();

        db.put_worktree("live", live_root_path, live_root_path, "main", None, None)
            .unwrap();

        let remote_root = tempfile::tempdir().unwrap();
        let remote_root_path = remote_root.path().to_str().unwrap();
        db.put_worktree(
            "remote",
            remote_root_path,
            remote_root_path,
            "remote-branch",
            Some(r#"{"path":"remote","host":"builder"}"#),
            None,
        )
        .unwrap();
        let remote_missing = remote_root.path().join("src/missing.rs");
        let remote_missing = remote_missing.to_str().unwrap();

        let nested_root = live_root.path().join("nested");
        let nested_root_path = nested_root.to_str().unwrap();
        db.put_worktree(
            "nested",
            live_root_path,
            nested_root_path,
            "nested",
            None,
            None,
        )
        .unwrap();
        let nested_missing = nested_root.join("src/missing.rs");
        let nested_missing = nested_missing.to_str().unwrap();

        db.upsert_entity(&row("kept", kept_path, "kept", EntityKind::Function, "h1"))
            .unwrap();
        db.upsert_entity(&row("gone", gone_path, "gone", EntityKind::Function, "h2"))
            .unwrap();
        db.upsert_entity(&row(
            "remote-missing",
            remote_missing,
            "remote_missing",
            EntityKind::Function,
            "h3",
        ))
        .unwrap();
        db.upsert_entity(&row(
            "nested-missing",
            nested_missing,
            "nested_missing",
            EntityKind::Function,
            "h4",
        ))
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO sem_edge(src_id,dst_id,kind) VALUES('kept','gone','call')",
                [],
            )
            .unwrap();

        let mut after_rowid = 0;
        loop {
            let (scanned, next_rowid) = db.prune_missing_semantic_files(after_rowid, 512).unwrap();
            if scanned == 0 {
                break;
            }
            after_rowid = next_rowid;
        }

        assert!(db.file_source_hash(gone_path).unwrap().is_none());
        assert_eq!(
            db.file_source_hash(kept_path).unwrap().as_deref(),
            Some("h1")
        );
        assert_eq!(
            db.file_source_hash(remote_missing).unwrap().as_deref(),
            Some("h3")
        );
        assert_eq!(
            db.file_source_hash(nested_missing).unwrap().as_deref(),
            Some("h4")
        );
        let edge_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sem_edge", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            edge_count, 0,
            "pruning a missing entity also prunes its edges"
        );
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
