//! SQLite implementation of the session migration store seam.
//!
//! Keep this outside `db.rs`: migration SQL is an explicit allowlist and must
//! not accidentally grow when unrelated cache tables are added.

use crate::db::Db;
use crate::issue::{AgentDispatchStatus, DispatchNote};
use crate::models::{GroupTabRow, TabGroupRow};
use crate::session_migration::{
    MigrationBundle, MigrationCleanupResult, MigrationCounts, MigrationDispatch, MigrationGroup,
    MigrationImportResult, MigrationNote, MigrationPlan, MigrationTarget, MigrationUiState,
    MigrationWorktree, make_bundle, migration_tab, persisted_group, persisted_tab, select_groups,
    select_sidebar_state,
};
use crate::store::SessionMigrationStore;
use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};
use std::collections::BTreeMap;

impl SessionMigrationStore for Db {
    fn migration_snapshot(
        &self,
        source_profile: &str,
        target_profile: &str,
        active_session: &str,
        worktree_path: &str,
    ) -> Result<MigrationBundle> {
        let worktree = self
            .conn()
            .query_row(
                "SELECT worktree, COALESCE(session_name,''), COALESCE(tab_name,''),
                        COALESCE(repo_path,''), COALESCE(branch,''), COALESCE(agent,''),
                        COALESCE(created_at,0), COALESCE(location,''), COALESCE(position,0),
                        sandbox_backend, observed_backend, folder_id, env_name
                   FROM worktrees WHERE worktree=?1",
                params![worktree_path],
                map_worktree,
            )
            .optional()?
            .map(MigrationWorktree::from);

        let groups = session_groups(self, active_session)?;
        let tabs = session_tabs(self, active_session)?;
        let selected_groups = select_groups(active_session, worktree_path, &groups, &tabs);
        let group_names = selected_groups.iter().map(|g| g.name.as_str());
        let sidebar = sidebar_rows(self)?;
        let ui_state = select_sidebar_state(&sidebar, group_names);
        let dispatches = dispatches_for_worktree(self, worktree_path)?;
        let source_ids: Vec<i64> = dispatches.iter().map(|row| row.source_id).collect();
        let notes = notes_for_dispatches(self, &source_ids)?;
        let (pin_state, pin_updated_at) = session_pin(self, active_session)?;

        Ok(make_bundle(
            source_profile,
            target_profile,
            active_session,
            worktree_path,
            worktree,
            selected_groups,
            ui_state,
            dispatches,
            notes,
            pin_state,
            pin_updated_at,
        ))
    }

    fn migration_target_snapshot(
        &self,
        active_session: &str,
        worktree_path: &str,
    ) -> Result<MigrationTarget> {
        let worktree = self
            .conn()
            .query_row(
                "SELECT worktree, COALESCE(session_name,''), COALESCE(tab_name,''),
                        COALESCE(repo_path,''), COALESCE(branch,''), COALESCE(agent,''),
                        COALESCE(created_at,0), COALESCE(location,''), COALESCE(position,0),
                        sandbox_backend, observed_backend, folder_id, env_name
                   FROM worktrees WHERE worktree=?1",
                params![worktree_path],
                map_worktree,
            )
            .optional()?
            .map(MigrationWorktree::from);
        let groups = session_groups(self, active_session)?;
        let tabs = session_tabs(self, active_session)?;
        let all_groups: Vec<MigrationGroup> = groups
            .iter()
            .map(|group| MigrationGroup {
                session_name: active_session.to_string(),
                name: group.name.clone(),
                kind: group.kind.clone(),
                worktree: group.worktree.clone(),
                ordinal: group.ordinal,
                active_tab: group.active_tab,
                tabs: tabs
                    .iter()
                    .filter(|tab| tab.group_name == group.name)
                    .map(|tab| migration_tab(active_session, tab.clone()))
                    .collect(),
            })
            .collect();
        // Keep every sidebar row in the target snapshot. The pure planner
        // filters this to the source bundle's exact keys, and it must also see
        // orphaned/stale keys that no longer have a matching target group so a
        // conflicting value is rejected before the import transaction starts.
        let ui_state = sidebar_rows(self)?;
        let dispatches = dispatches_for_worktree(self, worktree_path)?;
        let ids: Vec<i64> = dispatches.iter().map(|row| row.source_id).collect();
        let notes = notes_for_dispatches(self, &ids)?;
        let (pin_state, pin_updated_at) = session_pin(self, active_session)?;
        let (active_tab, updated_at) = self
            .conn()
            .query_row(
                "SELECT active_tab, updated_at FROM session_state WHERE session_name=?1",
                params![active_session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        Ok(MigrationTarget {
            worktree,
            groups: all_groups,
            ui_state,
            dispatches,
            notes,
            pin_state,
            pin_updated_at,
            active_tab,
            updated_at,
        })
    }

    fn import_migration(&self, plan: &MigrationPlan) -> Result<MigrationImportResult> {
        // The fingerprint deliberately excludes target-owned worktree metadata.
        // Therefore an empty target can otherwise look confirmed for a bundle
        // whose only transferable row is the worktree registration.
        let worktree_ready = plan.bundle.worktree.is_none() || plan.target.worktree.is_some();
        if plan.resumed || (worktree_ready && self.confirm_migration(plan)?) {
            return Ok(MigrationImportResult {
                counts: MigrationCounts::default(),
                dispatch_id_map: BTreeMap::new(),
                fingerprint: plan.fingerprint.clone(),
            });
        }
        let bundle = &plan.bundle;
        let mut result = MigrationImportResult {
            counts: MigrationCounts::default(),
            dispatch_id_map: BTreeMap::new(),
            fingerprint: plan.fingerprint.clone(),
        };
        self.transaction(|db| {
            if let Some(worktree) = &bundle.worktree {
                let changed = db.conn().execute(
                    "INSERT OR IGNORE INTO worktrees
                       (worktree, session_name, tab_name, repo_path, branch, agent,
                        created_at, location, position, sandbox_backend, observed_backend,
                        folder_id, env_name)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        worktree.worktree,
                        worktree.session_name,
                        worktree.tab_name,
                        worktree.repo_root,
                        worktree.branch,
                        worktree.agent,
                        worktree.created_at,
                        worktree.location,
                        worktree.position,
                        worktree.sandbox_backend,
                        worktree.observed_backend,
                        worktree.folder_id,
                        worktree.env_name,
                    ],
                )?;
                result.counts.worktrees = changed;
            }
            for group in &bundle.groups {
                let row = persisted_group(group);
                result.counts.tab_groups += db.conn().execute(
                    "INSERT OR IGNORE INTO tab_groups
                       (session_name,name,kind,worktree,ordinal,active_tab)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        group.session_name,
                        row.name,
                        row.kind,
                        row.worktree,
                        row.ordinal,
                        row.active_tab,
                    ],
                )?;
                for tab in &group.tabs {
                    let row = persisted_tab(tab);
                    result.counts.group_tabs += db.conn().execute(
                        "INSERT OR IGNORE INTO group_tabs
                           (session_name,group_name,ordinal,title,pane_tree,focused_pane,
                            pane_cwds,pane_cmds,pane_sessions,scrollback_snapshot)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULLIF(?9,''),?10)",
                        params![
                            tab.session_name,
                            row.group_name,
                            row.ordinal,
                            row.title,
                            row.pane_tree,
                            row.focused_pane,
                            row.pane_cwds,
                            row.pane_cmds,
                            "",
                            row.scrollback_snapshot,
                        ],
                    )?;
                }
            }
            for row in &bundle.ui_state {
                result.counts.ui_state += db.conn().execute(
                    "INSERT OR IGNORE INTO ui_state(scope,key,value) VALUES (?1,?2,?3)",
                    params![row.scope, row.key, row.value],
                )?;
            }
            if let Some(pin) = &bundle.pin_state {
                db.conn().execute(
                    "INSERT INTO session_state(session_name,pin_state,updated_at)
                       VALUES (?1,?2,?3)
                     ON CONFLICT(session_name) DO UPDATE SET pin_state=excluded.pin_state",
                    params![bundle.session_name, pin, bundle.pin_updated_at],
                )?;
            }
            for dispatch in &bundle.dispatches {
                db.conn().execute(
                    "INSERT INTO agent_dispatches
                       (issue_id,worktree_path,agent_name,dispatched_at_ms,status,stage,
                        parent_id,session_id,artifact_path,note,chunk_path,report)
                     VALUES (?1,?2,?3,?4,?5,?6,NULL,NULL,?7,?8,?9,?10)",
                    params![
                        dispatch.issue_id,
                        dispatch.worktree_path,
                        dispatch.agent_name,
                        dispatch.dispatched_at_ms,
                        dispatch.status.as_str(),
                        dispatch.stage,
                        dispatch.artifact_path,
                        dispatch.note,
                        dispatch.chunk_path,
                        dispatch.report,
                    ],
                )?;
                result
                    .dispatch_id_map
                    .insert(dispatch.source_id, db.conn().last_insert_rowid());
                result.counts.dispatches += 1;
            }
            for dispatch in &bundle.dispatches {
                let Some(target_id) = result.dispatch_id_map.get(&dispatch.source_id) else {
                    continue;
                };
                let parent = dispatch
                    .parent_id
                    .and_then(|id| result.dispatch_id_map.get(&id).copied());
                db.conn().execute(
                    "UPDATE agent_dispatches SET parent_id=?1,session_id=NULL WHERE id=?2",
                    params![parent, target_id],
                )?;
            }
            for note in &bundle.notes {
                let Some(dispatch_id) = result.dispatch_id_map.get(&note.dispatch_id) else {
                    continue;
                };
                db.conn().execute(
                    "INSERT INTO agent_dispatch_notes(dispatch_id,created_at_ms,text)
                     VALUES (?1,?2,?3)",
                    params![dispatch_id, note.created_at_ms, note.text],
                )?;
                result.counts.dispatch_notes += 1;
            }
            Ok(())
        })?;
        Ok(result)
    }

    fn confirm_migration(&self, plan: &MigrationPlan) -> Result<bool> {
        let target =
            self.migration_target_snapshot(&plan.bundle.session_name, &plan.bundle.worktree_path)?;
        let actual = crate::session_migration::target_subset(&plan.bundle, &target);
        Ok(actual.fingerprint() == plan.fingerprint)
    }

    fn cleanup_migration(&self, bundle: &MigrationBundle) -> Result<MigrationCleanupResult> {
        let mut result = MigrationCleanupResult::default();
        self.transaction(|db| {
            let group_names: Vec<&str> = bundle
                .groups
                .iter()
                .map(|group| group.name.as_str())
                .collect();
            for name in group_names {
                result.counts.group_tabs += db.conn().execute(
                    "DELETE FROM group_tabs WHERE session_name=?1 AND group_name=?2",
                    params![bundle.session_name, name],
                )?;
                result.counts.tab_groups += db.conn().execute(
                    "DELETE FROM tab_groups WHERE session_name=?1 AND name=?2 AND worktree=?3",
                    params![bundle.session_name, name, bundle.worktree_path],
                )?;
            }
            for row in &bundle.ui_state {
                result.counts.ui_state += db.conn().execute(
                    "DELETE FROM ui_state WHERE scope=?1 AND key=?2",
                    params![row.scope, row.key],
                )?;
            }
            for note in &bundle.notes {
                result.counts.dispatch_notes += db.conn().execute(
                    "DELETE FROM agent_dispatch_notes WHERE id=?1 AND dispatch_id=?2",
                    params![note.source_id, note.dispatch_id],
                )?;
            }
            for dispatch in &bundle.dispatches {
                result.counts.dispatches += db.conn().execute(
                    "DELETE FROM agent_dispatches WHERE id=?1 AND worktree_path=?2",
                    params![dispatch.source_id, bundle.worktree_path],
                )?;
            }
            result.counts.attention = db.conn().execute(
                "DELETE FROM session_attention WHERE worktree_path=?1",
                params![bundle.worktree_path],
            )?;
            if bundle.pin_state.is_some() {
                db.conn().execute(
                    "UPDATE session_state SET pin_state=NULL WHERE session_name=?1",
                    params![bundle.session_name],
                )?;
            }
            result.counts.worktrees = db.conn().execute(
                "DELETE FROM worktrees WHERE worktree=?1",
                params![bundle.worktree_path],
            )?;
            Ok(())
        })?;
        result.source_deleted = true;
        Ok(result)
    }
}

fn map_worktree(row: &Row<'_>) -> rusqlite::Result<crate::models::WorktreeRow> {
    Ok(crate::models::WorktreeRow {
        worktree: row.get(0)?,
        session_name: row.get(1)?,
        tab_name: row.get(2)?,
        repo_root: row.get(3)?,
        branch: row.get(4)?,
        agent: row.get(5)?,
        created_at: row.get(6)?,
        location: row.get(7)?,
        position: row.get(8)?,
        sandbox_backend: row.get(9)?,
        observed_backend: row.get(10)?,
        folder_id: row.get(11)?,
        env_name: row.get(12)?,
    })
}

fn session_groups(db: &Db, session: &str) -> Result<Vec<TabGroupRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT name,kind,worktree,ordinal,active_tab FROM tab_groups
           WHERE session_name=?1 ORDER BY ordinal,name",
    )?;
    Ok(stmt
        .query_map(params![session], |row| {
            Ok(TabGroupRow {
                name: row.get(0)?,
                kind: row.get(1)?,
                worktree: row.get(2)?,
                ordinal: row.get(3)?,
                active_tab: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn session_tabs(db: &Db, session: &str) -> Result<Vec<GroupTabRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT group_name,ordinal,title,pane_tree,focused_pane,COALESCE(pane_cwds,''),
                COALESCE(pane_cmds,''),COALESCE(pane_sessions,''),COALESCE(scrollback_snapshot,'')
           FROM group_tabs WHERE session_name=?1 ORDER BY group_name,ordinal",
    )?;
    Ok(stmt
        .query_map(params![session], |row| {
            Ok(GroupTabRow {
                group_name: row.get(0)?,
                ordinal: row.get(1)?,
                title: row.get(2)?,
                pane_tree: row.get(3)?,
                focused_pane: row.get(4)?,
                pane_cwds: row.get(5)?,
                pane_cmds: row.get(6)?,
                pane_sessions: row.get(7)?,
                scrollback_snapshot: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn sidebar_rows(db: &Db) -> Result<Vec<MigrationUiState>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT scope,key,value FROM ui_state WHERE scope='sidebar' ORDER BY key")?;
    Ok(stmt
        .query_map([], |row| {
            Ok(MigrationUiState {
                scope: row.get(0)?,
                key: row.get(1)?,
                value: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn dispatches_for_worktree(db: &Db, worktree: &str) -> Result<Vec<MigrationDispatch>> {
    let mut stmt = db.conn().prepare(
        "SELECT id,issue_id,worktree_path,agent_name,dispatched_at_ms,status,stage,parent_id,
                session_id,artifact_path,note,chunk_path,report
           FROM agent_dispatches WHERE worktree_path=?1 ORDER BY id",
    )?;
    Ok(stmt
        .query_map(params![worktree], |row| {
            Ok(MigrationDispatch {
                source_id: row.get(0)?,
                issue_id: row.get(1)?,
                worktree_path: row.get(2)?,
                agent_name: row.get(3)?,
                dispatched_at_ms: row.get(4)?,
                status: AgentDispatchStatus::parse(&row.get::<_, String>(5)?),
                stage: row.get(6)?,
                parent_id: row.get(7)?,
                session_id: row.get(8)?,
                artifact_path: row.get(9)?,
                note: row.get(10)?,
                chunk_path: row.get(11)?,
                report: row.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn notes_for_dispatches(db: &Db, dispatch_ids: &[i64]) -> Result<Vec<MigrationNote>> {
    let mut out = Vec::new();
    for dispatch_id in dispatch_ids {
        let mut stmt = db.conn().prepare(
            "SELECT id,dispatch_id,created_at_ms,text FROM agent_dispatch_notes
               WHERE dispatch_id=?1 ORDER BY created_at_ms,id",
        )?;
        let rows = stmt.query_map(params![dispatch_id], |row| {
            Ok(MigrationNote::from(DispatchNote {
                id: row.get(0)?,
                dispatch_id: row.get(1)?,
                created_at_ms: row.get(2)?,
                text: row.get(3)?,
            }))
        })?;
        out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    Ok(out)
}

fn session_pin(db: &Db, session: &str) -> Result<(Option<String>, Option<i64>)> {
    Ok(db
        .conn()
        .query_row(
            "SELECT pin_state,updated_at FROM session_state WHERE session_name=?1",
            params![session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, None)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_migration::{MigrationConflict, plan_migration};
    use crate::store::SessionMigrationStore;

    #[test]
    fn db_import_clears_ephemeral_ids_remaps_dispatches_and_cleans_exact_rows() {
        let source = Db::open_memory().unwrap();
        let target = Db::open_memory().unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location,position)
                 VALUES('/w','source','tab','/repo','feature','agent',1,'',3)",
                [],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO tab_groups(session_name,name,kind,worktree,ordinal,active_tab)
                 VALUES('source','g','branch','/w',0,1)",
                [],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO group_tabs(session_name,group_name,ordinal,title,pane_tree,pane_sessions)
                 VALUES('source','g',0,'t','{}','source-daemon')",
                [],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO ui_state(scope,key,value) VALUES('sidebar','pin:g','1')",
                [],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO session_state(session_name,active_tab,pin_state,updated_at)
                 VALUES('source','keep','[\"g\"]',7)",
                [],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status,session_id)
                 VALUES('a','/w','one',1,'queued','source-daemon')",
                [],
            )
            .unwrap();
        let parent = source.conn().last_insert_rowid();
        source
            .conn()
            .execute(
                "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status,parent_id,session_id)
                 VALUES('b','/w','two',2,'running',?1,'source-daemon-2')",
                params![parent],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO agent_dispatch_notes(dispatch_id,created_at_ms,text)
                 VALUES(?1,3,'progress')",
                params![parent],
            )
            .unwrap();
        target
            .conn()
            .execute(
                "INSERT INTO session_state(session_name,active_tab,updated_at)
                 VALUES('source','keep',99)",
                [],
            )
            .unwrap();
        target
            .conn()
            .execute(
                "INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location,position)
                 VALUES('/w','target','target-tab','/target-repo','target-branch','target-agent',88,'target-location',8)",
                [],
            )
            .unwrap();
        let bundle = source
            .migration_snapshot("default", "target", "source", "/w")
            .unwrap();
        let target_state = target.migration_target_snapshot("source", "/w").unwrap();
        let plan = plan_migration(bundle.clone(), target_state).unwrap();
        let imported = target.import_migration(&plan).unwrap();
        assert_eq!(imported.counts.dispatches, 2);
        assert_eq!(imported.counts.dispatch_notes, 1);
        assert!(target.confirm_migration(&plan).unwrap());
        let rows = dispatches_for_worktree(&target, "/w").unwrap();
        assert!(rows.iter().all(|row| row.session_id.is_none()));
        assert_eq!(rows[1].parent_id, Some(rows[0].source_id));
        let note_dispatch: i64 = target
            .conn()
            .query_row("SELECT dispatch_id FROM agent_dispatch_notes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(note_dispatch, rows[0].source_id);
        let tabs = session_tabs(&target, "source").unwrap();
        assert!(tabs[0].pane_sessions.is_empty());
        let state = target
            .conn()
            .query_row(
                "SELECT active_tab,pin_state FROM session_state WHERE session_name='source'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, (Some("keep".into()), Some("[\"g\"]".into())));
        let target_branch: String = target
            .conn()
            .query_row(
                "SELECT branch FROM worktrees WHERE worktree='/w'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_branch, "target-branch");
        source.cleanup_migration(&bundle).unwrap();
        assert!(
            source
                .migration_snapshot("default", "target", "source", "/w")
                .unwrap()
                .worktree
                .is_none()
        );
        assert_eq!(
            target.import_migration(&plan).unwrap().counts,
            MigrationCounts::default()
        );
    }

    #[test]
    fn db_import_does_not_skip_a_worktree_only_bundle() {
        let source = Db::open_memory().unwrap();
        let target = Db::open_memory().unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO worktrees(worktree,session_name,tab_name,repo_path,branch,agent,created_at,location,position)
                 VALUES('/only','source','tab','/repo','feature','agent',1,'',3)",
                [],
            )
            .unwrap();

        let bundle = source
            .migration_snapshot("default", "target", "source", "/only")
            .unwrap();
        let plan = plan_migration(
            bundle,
            target.migration_target_snapshot("source", "/only").unwrap(),
        )
        .unwrap();
        let imported = target.import_migration(&plan).unwrap();

        assert_eq!(imported.counts.worktrees, 1);
        assert!(
            target
                .migration_target_snapshot("source", "/only")
                .unwrap()
                .worktree
                .is_some()
        );
    }

    #[test]
    fn orphan_target_sidebar_key_conflicts_during_preflight() {
        let source = Db::open_memory().unwrap();
        let target = Db::open_memory().unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO tab_groups(session_name,name,kind,worktree,ordinal,active_tab)
                 VALUES('default','stale','worktree','/w',0,0)",
                [],
            )
            .unwrap();
        source
            .conn()
            .execute(
                "INSERT INTO ui_state(scope,key,value) VALUES('sidebar','pin:stale','source')",
                [],
            )
            .unwrap();
        target
            .conn()
            .execute(
                "INSERT INTO ui_state(scope,key,value) VALUES('sidebar','pin:stale','target')",
                [],
            )
            .unwrap();

        let bundle = source
            .migration_snapshot("default", "target", "default", "/w")
            .unwrap();
        let target_state = target.migration_target_snapshot("default", "/w").unwrap();

        assert_eq!(
            plan_migration(bundle, target_state),
            Err(MigrationConflict::UiState("pin:stale".into()))
        );
    }
}
