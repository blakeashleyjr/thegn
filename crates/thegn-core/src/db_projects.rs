//! Project existence + membership + header order (the embedded-SQLite
//! [`ProjectStore`] impl): the `projects` table and the nullable
//! `workspaces.project_id` (schema v54).
//!
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs` only
//! carries the schema DDL, not these bodies. This is the zones *shape*
//! (`db_zones.rs`) with a `position` column for manual header ordering and ZERO
//! policy attached — membership is DB-tracked, never path-inferred.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::store::{ProjectDeleteOutcome, ProjectRow, ProjectStore};

/// SQL selecting a project row + its member count, parameterised by a WHERE
/// clause.
fn project_select(where_clause: &str) -> String {
    format!(
        "SELECT p.project_id, p.name, p.created_at, p.position,
                (SELECT COUNT(*) FROM workspaces w WHERE w.project_id = p.project_id)
           FROM projects p {where_clause}"
    )
}

fn row_to_project(r: &rusqlite::Row) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        project_id: r.get(0)?,
        name: r.get(1)?,
        created_at: r.get(2)?,
        position: r.get(3)?,
        member_count: r.get(4)?,
    })
}

impl ProjectStore for Db {
    fn create_project(&self, name: &str, now: i64) -> Result<i64> {
        // `projects.project_id` is INTEGER PRIMARY KEY *without* AUTOINCREMENT, so
        // SQLite may recycle a deleted project's rowid. If a prior crash/race left
        // a workspace pointing at that now-freed id (a dangling `project_id`), the
        // new project would silently inherit it as a member — grouping the user
        // never chose. Insert + orphan-sweep atomically so the fresh project
        // starts with exactly the members later assigned to it. (Same defense as
        // `db_zones::create_zone`; the DDL lives in `db_migrate`.) New projects
        // sort to the end: `position = MAX+1` over the existing rows.
        self.transaction(|db| {
            let next_pos: i64 = db.conn().query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM projects",
                [],
                |r| r.get(0),
            )?;
            db.conn().execute(
                "INSERT INTO projects(name, created_at, position) VALUES(?1, ?2, ?3)",
                params![name, now, next_pos],
            )?;
            let id = db.conn().last_insert_rowid();
            db.conn().execute(
                "UPDATE workspaces SET project_id=NULL WHERE project_id=?1",
                params![id],
            )?;
            Ok(id)
        })
    }

    fn rename_project(&self, project_id: i64, new_name: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE projects SET name=?2 WHERE project_id=?1",
            params![project_id, new_name],
        )?;
        Ok(())
    }

    fn delete_project(&self, project_id: i64, force: bool) -> Result<ProjectDeleteOutcome> {
        // Count → unassign → delete must be atomic: a concurrent process (a CLI
        // `thegn` subcommand shares this DB file with the live compositor by
        // design) running `assign_workspace_project` between the count and the
        // DELETE would otherwise leave a workspace pointing at the deleted
        // project, which a recycled rowid could then silently absorb. The
        // transaction closes that window; the unconditional unassign (dropping the
        // `members > 0` guard) also sweeps any orphan that raced in.
        self.transaction(|db| {
            let members: i64 = db.conn().query_row(
                "SELECT COUNT(*) FROM workspaces WHERE project_id=?1",
                params![project_id],
                |r| r.get(0),
            )?;
            if members > 0 && !force {
                return Ok(ProjectDeleteOutcome::RefusedNonEmpty(members));
            }
            db.conn().execute(
                "UPDATE workspaces SET project_id=NULL WHERE project_id=?1",
                params![project_id],
            )?;
            db.conn().execute(
                "DELETE FROM projects WHERE project_id=?1",
                params![project_id],
            )?;
            Ok(ProjectDeleteOutcome::Deleted)
        })
    }

    fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&project_select("ORDER BY p.position, p.name"))?;
        let rows = stmt
            .query_map([], row_to_project)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn assign_workspace_project(&self, repo_path: &str, project: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE workspaces SET project_id=?2 WHERE repo_path=?1",
            params![repo_path, project],
        )?;
        Ok(())
    }

    fn project_of_workspace(&self, repo_path: &str) -> Result<Option<ProjectRow>> {
        let conn = self.conn();
        let sql =
            project_select("JOIN workspaces w ON w.project_id = p.project_id WHERE w.repo_path=?1");
        let row = conn
            .query_row(&sql, params![repo_path], row_to_project)
            .optional()?;
        Ok(row)
    }

    fn project_members(&self, project_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT repo_path, COALESCE(name, '')
               FROM workspaces
              WHERE project_id = ?1
              ORDER BY position, last_active DESC",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn set_project_order(&self, order: &[i64]) -> Result<()> {
        self.transaction(|db| {
            for (i, id) in order.iter().enumerate() {
                db.conn().execute(
                    "UPDATE projects SET position=?2 WHERE project_id=?1",
                    params![id, i as i64],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WorkspaceStore;

    fn db() -> Db {
        Db::open_memory().expect("in-memory db")
    }

    fn add_ws(db: &Db, repo: &str) {
        db.put_workspace(repo, "ws", "repo").unwrap();
    }

    #[test]
    fn create_list_and_member_count() {
        let db = db();
        let a = db.create_project("shop", 10).unwrap();
        db.create_project("infra", 11).unwrap();
        add_ws(&db, "/repo1");
        db.assign_workspace_project("/repo1", Some(a)).unwrap();
        let projects = db.list_projects().unwrap();
        assert_eq!(projects.len(), 2);
        // Ordered by position (creation order): shop first, infra second.
        assert_eq!(projects[0].name, "shop");
        assert_eq!(projects[0].member_count, 1);
        assert_eq!(projects[1].name, "infra");
        assert_eq!(projects[1].member_count, 0);
    }

    #[test]
    fn duplicate_name_rejected() {
        let db = db();
        db.create_project("dup", 1).unwrap();
        assert!(db.create_project("dup", 2).is_err());
    }

    #[test]
    fn delete_refuses_nonempty_unless_forced() {
        let db = db();
        let p = db.create_project("p", 1).unwrap();
        add_ws(&db, "/r");
        db.assign_workspace_project("/r", Some(p)).unwrap();
        assert_eq!(
            db.delete_project(p, false).unwrap(),
            ProjectDeleteOutcome::RefusedNonEmpty(1)
        );
        // Force unassigns then deletes.
        assert_eq!(
            db.delete_project(p, true).unwrap(),
            ProjectDeleteOutcome::Deleted
        );
        assert!(db.project_of_workspace("/r").unwrap().is_none());
        assert!(db.list_projects().unwrap().is_empty());
    }

    #[test]
    fn membership_lookup_and_unassign() {
        let db = db();
        let p = db.create_project("p", 1).unwrap();
        add_ws(&db, "/repo");
        db.assign_workspace_project("/repo", Some(p)).unwrap();
        assert_eq!(db.project_of_workspace("/repo").unwrap().unwrap().name, "p");
        // Unassign clears membership and drops the member count.
        db.assign_workspace_project("/repo", None).unwrap();
        assert!(db.project_of_workspace("/repo").unwrap().is_none());
        assert_eq!(db.list_projects().unwrap()[0].member_count, 0);
    }

    #[test]
    fn rename_project_updates_name() {
        let db = db();
        let p = db.create_project("old", 1).unwrap();
        db.rename_project(p, "new").unwrap();
        assert_eq!(db.list_projects().unwrap()[0].name, "new");
    }

    #[test]
    fn set_project_order_persists_exact_sequence() {
        let db = db();
        let a = db.create_project("a", 1).unwrap();
        let b = db.create_project("b", 2).unwrap();
        let c = db.create_project("c", 3).unwrap();
        // Default order is creation order (a, b, c).
        let names: Vec<String> = db
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        // Reorder to c, a, b.
        db.set_project_order(&[c, a, b]).unwrap();
        let names: Vec<String> = db
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["c", "a", "b"]);
    }

    #[test]
    fn create_project_sweeps_recycled_rowid_orphans() {
        // A workspace left pointing at a project_id that a later create_project
        // recycles must NOT silently become a member of the new project.
        let db = db();
        let a = db.create_project("first", 1).unwrap();
        add_ws(&db, "/orphan");
        db.assign_workspace_project("/orphan", Some(a)).unwrap();
        // Simulate the race: the project row is deleted out from under the
        // assignment, leaving `/orphan` dangling at the now-free id `a`.
        db.conn()
            .execute("DELETE FROM projects WHERE project_id=?1", params![a])
            .unwrap();
        assert_eq!(
            db.project_of_workspace("/orphan").unwrap().map(|p| p.name),
            None,
            "dangling id renders as unprojected"
        );
        // A new project recycles the freed rowid (no AUTOINCREMENT). It must
        // start empty — the orphan is swept, not inherited.
        let b = db.create_project("second", 2).unwrap();
        assert_eq!(b, a, "SQLite recycled the freed rowid (test precondition)");
        assert_eq!(
            db.list_projects()
                .unwrap()
                .into_iter()
                .find(|p| p.project_id == b)
                .unwrap()
                .member_count,
            0,
            "recycled-id project did not absorb the orphaned workspace"
        );
        assert!(
            db.project_of_workspace("/orphan").unwrap().is_none(),
            "orphan stays unprojected after the recycled create"
        );
    }

    #[test]
    fn delete_project_is_atomic_across_count_and_delete() {
        let db = db();
        let p = db.create_project("p", 1).unwrap();
        add_ws(&db, "/a");
        add_ws(&db, "/b");
        db.assign_workspace_project("/a", Some(p)).unwrap();
        db.assign_workspace_project("/b", Some(p)).unwrap();
        assert_eq!(
            db.delete_project(p, true).unwrap(),
            ProjectDeleteOutcome::Deleted
        );
        assert!(db.list_projects().unwrap().is_empty());
        let dangling: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE project_id=?1",
                params![p],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0, "no dangling project_id survives the delete");
    }

    #[test]
    fn project_members_lists_assigned_workspaces() {
        let db = db();
        let p = db.create_project("shop", 1).unwrap();
        add_ws(&db, "/api");
        add_ws(&db, "/web");
        add_ws(&db, "/unrelated");
        db.assign_workspace_project("/api", Some(p)).unwrap();
        db.assign_workspace_project("/web", Some(p)).unwrap();
        let mut members = db.project_members(p).unwrap();
        members.sort();
        assert_eq!(
            members,
            vec![
                ("/api".to_string(), "ws".to_string()),
                ("/web".to_string(), "ws".to_string()),
            ]
        );
        // An empty project lists nothing.
        let q = db.create_project("empty", 2).unwrap();
        assert!(db.project_members(q).unwrap().is_empty());
    }

    #[test]
    fn membership_is_orthogonal_to_zones() {
        // A workspace can be in one zone AND one project at once; assigning a
        // project never touches zone_id (and vice versa).
        use crate::store::ZoneStore;
        let db = db();
        let z = db.create_zone("clientA", 1).unwrap();
        let p = db.create_project("shop", 1).unwrap();
        add_ws(&db, "/repo");
        db.assign_workspace_zone("/repo", Some(z)).unwrap();
        db.assign_workspace_project("/repo", Some(p)).unwrap();
        assert_eq!(
            db.zone_of_workspace("/repo").unwrap().unwrap().name,
            "clientA"
        );
        assert_eq!(
            db.project_of_workspace("/repo").unwrap().unwrap().name,
            "shop"
        );
        // Reassigning the project leaves the zone intact.
        db.assign_workspace_project("/repo", None).unwrap();
        assert_eq!(
            db.zone_of_workspace("/repo").unwrap().unwrap().name,
            "clientA"
        );
        assert!(db.project_of_workspace("/repo").unwrap().is_none());
    }

    #[test]
    fn migrates_projects_additive_from_v53() {
        use rusqlite::Connection;
        let dir = std::env::temp_dir().join(format!("tg-db-proj-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 53;
                 CREATE TABLE workspaces (repo_path TEXT PRIMARY KEY, name TEXT);
                 INSERT INTO workspaces(repo_path,name) VALUES('/keep','k');",
            )
            .unwrap();
        }
        let db = Db::open_at(&path).unwrap();
        let p = db.create_project("shop", 1).unwrap();
        db.assign_workspace_project("/keep", Some(p)).unwrap();
        assert_eq!(
            db.project_of_workspace("/keep").unwrap().unwrap().name,
            "shop"
        );
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
    }
}
