//! Collecting landed worktrees once their grace period is up.
//!
//! The `when` is [`thegn_core::merge_sweep`]; this is the `what`. Under
//! `on_landed = "expire"` a landed branch keeps its worktree — filed into
//! `merged_folder` — until `merged_ttl_secs` has passed, at which point the sweep
//! attempts verified, non-forced worktree removal. Automatic cleanup retains
//! branch refs and a queue hold until safe atomic branch deletion is established;
//! explicit queue dismissal remains available.
//!
//! Runs only in the requested repository, off-loop, and on demand via
//! `thegn merge sweep` / the `sweep-merged` action. There is deliberately **no
//! timer**: a worktree that comes due while thegn sits idle is collected at the
//! next natural wake, which is the whole point of an idle loop that never polls.

use std::path::Path;
use thegn_core::config::{Config, OnLanded};
use thegn_core::db::{Db, MergeQueueRow};
use thegn_core::merge_sweep::{self, MergedEntry};
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;

/// Treat database/Git/path diagnostics as text, never terminal instructions.
pub(crate) fn safe_display(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|c| {
            if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                '�'
            } else {
                c
            }
        })
        .collect()
}

/// What one sweep did, for the caller to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Branches whose worktree was removed.
    pub collected: Vec<String>,
    /// Worktree keys actually deleted by successful cleanup bookkeeping.
    pub cleared_rows: Vec<String>,
    /// Branches left alone because their worktree had become dirty again.
    pub kept_dirty: Vec<String>,
    /// Branches whose status snapshot changed during final cleanup validation.
    pub kept_changed: Vec<String>,
    /// Branches removed after admitting ignored-only build state.
    pub discarded_build_state: Vec<String>,
    /// Refused or failed physical cleanup, with an actionable reason.
    pub kept: Vec<(String, String)>,
    /// Enumeration or post-removal bookkeeping failures, never "collected".
    pub bookkeeping_errors: Vec<String>,
}

impl SweepReport {
    pub fn is_empty(&self) -> bool {
        self.collected.is_empty()
            && self.cleared_rows.is_empty()
            && self.kept_dirty.is_empty()
            && self.kept_changed.is_empty()
            && self.discarded_build_state.is_empty()
            && self.kept.is_empty()
            && self.bookkeeping_errors.is_empty()
    }
}

/// The landed rows for `repo_root`'s target, projected for the expiry check.
///
/// Only `landed` rows are candidates. Anything still queued, deferred or
/// mid-flight is someone's open work by definition.
fn landed_entries(db: &Db, target: &str) -> anyhow::Result<Vec<MergeQueueRow>> {
    Ok(db
        .list_merge_queue()?
        .into_iter()
        .filter(|r| r.status == "landed" && r.target_branch == target)
        .collect())
}

/// Collect every landed worktree whose grace period has elapsed.
///
/// `force` ignores the clock and collects all of them — the manual "clear
/// merged now" gesture. It does NOT ignore the dirty guard: a worktree you have
/// gone back to and edited is never removed by a sweep, deliberate or not, since
/// the gesture means "tidy what I'm done with", never "discard my edits".
pub fn sweep(cfg: &Config, repo_root: &Path, force: bool) -> SweepReport {
    let mut report = SweepReport::default();
    if let Some(refusal) = cfg.workspace_overlay_refusal(repo_root) {
        report.bookkeeping_errors.push(format!(
            "sweep skipped for {}: {refusal}",
            repo_root.display()
        ));
        return report;
    }
    let mq = cfg.repo_merge_queue(repo_root);
    // `expire` is the only mode with a grace period to end. Under `move` the
    // worktree is meant to stay; under `remove`/`detach` it is already gone.
    // A forced sweep still honors this — "clear merged" under `move` would
    // delete worktrees the config says to keep.
    if mq.on_landed != OnLanded::Expire {
        return report;
    }
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            report
                .bookkeeping_errors
                .push(format!("sweep database unavailable: {error}"));
            return report;
        }
    };
    sweep_with_db(cfg, repo_root, force, &db)
}

/// Manual non-expiry row clearing is a short compare-and-delete transaction;
/// retrying or reassigning an item revokes the old UI selection's authority.
pub(crate) fn clear_selected_landed(db: &Db, selected: &[MergeQueueRow]) -> anyhow::Result<usize> {
    db.transaction(|db| {
        let mut current = db.list_merge_queue()?;
        let mut removed = 0;
        for row in selected {
            if row.status == "landed" && current.iter().any(|now| now == row) {
                db.remove_merge_entry(&row.worktree)?;
                current.retain(|now| now.worktree != row.worktree);
                removed += 1;
            }
        }
        Ok(removed)
    })
}

fn sweep_with_db(cfg: &Config, repo_root: &Path, force: bool, db: &Db) -> SweepReport {
    use crate::merge_lifecycle::CleanupOutcome;
    let mut report = SweepReport::default();
    // THE-515: a refused trusted overlay may have said `on_landed = "move"`;
    // never delete worktrees under the global policy in its place (the
    // selector also forces a keep mode, this names why).
    if let Some(refusal) = cfg.workspace_overlay_refusal(repo_root) {
        report.bookkeeping_errors.push(format!(
            "sweep skipped for {}: {refusal}",
            repo_root.display()
        ));
        return report;
    }
    let mq = cfg.repo_merge_queue(repo_root);
    if mq.on_landed != OnLanded::Expire {
        return report;
    }
    let target = crate::integrate::resolve_target(&mq, repo_root);
    let rows = match landed_entries(db, &target) {
        Ok(rows) => rows,
        Err(error) => {
            report
                .bookkeeping_errors
                .push(format!("sweep enumeration unavailable: {error}"));
            return report;
        }
    };
    let identity = match crate::merge_lifecycle::repository_identity(repo_root) {
        Ok(identity) => identity,
        Err(error) => {
            report
                .bookkeeping_errors
                .push(format!("sweep repository identity unavailable: {error}"));
            return report;
        }
    };
    let entries: Vec<_> = rows
        .iter()
        // A global queue may contain the same target name in unrelated repos.
        // Exclude proven foreign Git identities before applying this repo's
        // clock or reporting holds. Unknown/missing paths remain conservative
        // candidates; the final cleanup admission still proves all ownership.
        .filter(|row| {
            crate::merge_lifecycle::repository_identity(Path::new(&row.worktree))
                .map_or(true, |candidate| candidate == identity)
        })
        .filter(|row| {
            if merge_sweep::CleanupHold::from_detail(row.error_detail.as_deref()).is_some() {
                if force { report.kept.push((row.branch.clone(), "worktree previously collected; branch and queue evidence retained for explicit cleanup (THE-596)".into())); }
                false
            } else { true }
        })
        .map(|r| MergedEntry {
            worktree: r.worktree.clone(),
            branch: r.branch.clone(),
            landed_at: r.updated_at,
        })
        .collect();
    let now = util::now();
    let due: Vec<&MergedEntry> = if force {
        entries.iter().collect()
    } else {
        merge_sweep::due(&entries, now, mq.merged_ttl_secs)
    };
    for entry in due {
        let row = rows
            .iter()
            .find(|r| r.worktree == entry.worktree)
            .expect("projected row");
        if !row.location.is_empty() && row.location != "local" {
            report.kept.push((
                entry.branch.clone(),
                "remote/unknown worktree location".into(),
            ));
            continue;
        }
        let Some(landed) = row.result_oid.as_deref() else {
            report.kept.push((
                entry.branch.clone(),
                "landed commit identity is missing".into(),
            ));
            continue;
        };
        match crate::merge_lifecycle::remove_landed_with_config(
            cfg,
            db,
            repo_root,
            &entry.worktree,
            &entry.branch,
            &target,
            Some(landed),
            row,
            /* delete_branch */ true,
        ) {
            CleanupOutcome::Removed {
                discarded_build_state,
                bookkeeping_errors,
                queue_removed,
                ..
            } => {
                report.collected.push(entry.branch.clone());
                if discarded_build_state {
                    report.discarded_build_state.push(entry.branch.clone());
                }
                if queue_removed {
                    report.cleared_rows.push(entry.worktree.clone());
                }
                report.bookkeeping_errors.extend(bookkeeping_errors);
            }
            CleanupOutcome::KeptDirty => report.kept_dirty.push(entry.branch.clone()),
            CleanupOutcome::KeptChanged => report.kept_changed.push(entry.branch.clone()),
            CleanupOutcome::Refused { reason } => report.kept.push((
                entry.branch.clone(),
                format!("{}: {reason}", entry.worktree),
            )),
        }
    }
    report
}

/// Resolve the sweep root from any path inside the repo and sweep it.
/// `None` when the path is not in a git repository.
///
/// The root is the MAIN checkout, never `toplevel`: the startup and post-fold
/// entries are usually handed a LINKED worktree (the launch cwd, the active
/// tab), whose toplevel is the worktree itself. Keying off that resolved the
/// per-repo `[project.*]` layer — and the THE-515 refusal — under the worktree
/// directory's name, so a repo whose block said `on_landed = "move"` had its
/// merged worktrees swept under the global policy instead.
pub(crate) fn sweep_from(cfg: &Config, dir: &Path, force: bool) -> Option<SweepReport> {
    Some(sweep(cfg, &sweep_root(dir)?, force))
}

/// The repository a sweep of `dir` must be keyed by: its MAIN checkout.
/// Off-thread (THE-78): it shells out to git, so it must not run on the
/// pre-first-frame launch path.
pub(crate) fn sweep_root(dir: &Path) -> Option<std::path::PathBuf> {
    crate::integrate::main_checkout(dir)
}

/// Fire-and-forget sweep on a blocking thread — the startup and post-fold entry
/// point. Never on the event loop: it resolves the repo root, stats worktrees
/// and shells out to git. `dir` may be any path inside the repo (the launch dir
/// at startup, an arbitrary worktree path after a fold); a non-repo path is a
/// no-op.
pub fn spawn(cfg: Config, dir: std::path::PathBuf) {
    tokio::task::spawn_blocking(move || {
        let Some(report) = sweep_from(&cfg, &dir, false) else {
            return;
        };
        if !report.collected.is_empty() {
            thegn_core::msg::info(&format!(
                "merge queue: swept {} merged worktree(s): {}",
                report.collected.len(),
                report
                    .collected
                    .iter()
                    .map(|b| safe_display(b))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for b in &report.kept_dirty {
            thegn_core::msg::warn(&format!(
                "merge queue: kept {} — edited since landing",
                safe_display(b)
            ));
        }
        for b in &report.kept_changed {
            thegn_core::msg::warn(&format!(
                "merge queue: kept {} — changed during cleanup",
                safe_display(b)
            ));
        }
        for b in &report.discarded_build_state {
            thegn_core::msg::info(&format!(
                "merge queue: swept {} (discarded build state)",
                safe_display(b)
            ));
        }
        for (branch, reason) in &report.kept {
            thegn_core::msg::warn(&format!(
                "merge cleanup: kept {} — {}",
                safe_display(branch),
                safe_display(reason)
            ));
        }
        for error in &report.bookkeeping_errors {
            thegn_core::msg::warn(&safe_display(error));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::WorkspaceStore;

    fn local_config() -> Config {
        let mut cfg = Config::default();
        cfg.sandbox.enabled = false;
        cfg
    }

    #[expect(clippy::disallowed_methods)]
    fn fixture(
        parent: &Path,
        name: &str,
        db: &Db,
        isolation: &crate::merge_lifecycle::TestIsolation,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = parent.join(name);
        let wt = parent.join(format!("{name}-feature"));
        std::fs::create_dir(&root).unwrap();
        let git = |args: &[&str]| {
            let output = isolation
                .git(&root)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "private fixture"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(root.join("tracked"), "keep\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        git(&["add", "tracked", ".gitignore"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
            "main",
        ]);
        db.put_worktree(
            "feature",
            root.to_str().unwrap(),
            wt.to_str().unwrap(),
            "feature",
            None,
            None,
        )
        .unwrap();
        db.enqueue_merge(wt.to_str().unwrap(), "feature", "main")
            .unwrap();
        let oid = git(&["rev-parse", "HEAD"]);
        db.update_merge_status(wt.to_str().unwrap(), "landed", Some(&oid), None, None)
            .unwrap();
        (root, wt)
    }

    #[test]
    fn shared_database_collects_only_verified_repository_and_reports_actual_removal() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "one", &db, &isolation);
        let (foreign_root, foreign) = fixture(&parent, "two", &db, &isolation);
        let oid = db
            .list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.worktree == wt.to_str().unwrap())
            .unwrap()
            .result_oid
            .unwrap();
        db.enqueue_merge(root.to_str().unwrap(), "forged-main-row", "main")
            .unwrap();
        db.update_merge_status(root.to_str().unwrap(), "landed", Some(&oid), None, None)
            .unwrap();
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert_eq!(report.collected, ["feature"]);
        assert_eq!(
            report.kept.len(),
            1,
            "foreign is outside the sweep; forged main is refused"
        );
        assert_eq!(
            report.bookkeeping_errors.len(),
            1,
            "branch cleanup explicitly held"
        );
        assert!(report.cleared_rows.is_empty());
        assert!(!wt.exists());
        assert!(
            root.join("tracked").exists()
                && foreign_root.join("tracked").exists()
                && foreign.join("tracked").exists()
        );
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|r| Path::new(&r.worktree) == foreign));
        assert!(rows.iter().any(|r| Path::new(&r.worktree) == root));
    }

    #[test]
    fn refused_trusted_overlay_sweeps_nothing() {
        // THE-515: global on_landed = expire, the repo's own block would keep
        // its worktrees, and a stray alias makes that block ambiguous: the
        // sweep must not fall back to the global policy and delete them.
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "keep", &db, &isolation);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let key = thegn_core::workspace_overlay::legacy_key_for_root(&root).unwrap();
        cfg.workspace.insert(key.clone(), Default::default());
        cfg.workspace.insert(key.to_uppercase(), Default::default());
        let before = db.list_merge_queue().unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert!(report.bookkeeping_errors[0].contains("refused"));
        assert!(wt.exists());
        assert_eq!(db.list_merge_queue().unwrap(), before);
    }

    #[test]
    fn startup_sweep_from_a_linked_worktree_resolves_the_main_checkout() {
        // THE-515 / F2: the startup and post-fold entries are handed a LINKED
        // worktree. Keying the per-repo layer off its directory name missed
        // the repo's `[project.*]` block entirely — here, the refusal — and
        // swept under the global `expire`.
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "linked", &db, &isolation);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let key = thegn_core::workspace_overlay::legacy_key_for_root(&root).unwrap();
        cfg.workspace.insert(key.clone(), Default::default());
        cfg.workspace.insert(key.to_uppercase(), Default::default());

        // The entry resolves the MAIN checkout from a linked worktree. The old
        // `toplevel` resolution returned `wt` itself, whose directory name is
        // not the repository's key — so the refusal below was never computed.
        let resolved = sweep_root(&wt).expect("inside a git repository");
        assert_eq!(resolved, root);

        let report = sweep_with_db(&cfg, &resolved, true, &db);
        assert!(report.collected.is_empty());
        assert!(
            report
                .bookkeeping_errors
                .iter()
                .any(|e| e.contains("refused")),
            "{:?}",
            report.bookkeeping_errors
        );
        assert!(wt.exists());
        // (Keying by `wt` instead — the old behaviour — makes the block
        // invisible and sweeps under the global `expire`; not asserted here
        // because it would delete the worktree this test still needs.)
        // Without the stray alias the repository's own policy applies again.
        cfg.workspace.remove(&key.to_uppercase());
        let report = sweep_with_db(&cfg, &resolved, true, &db);
        assert_eq!(report.collected, ["feature"]);
        assert!(!wt.exists());
    }

    #[test]
    fn dirty_conflicted_claim_and_wrong_target_rows_keep_retry_metadata() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "dirty", &db, &isolation);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        std::fs::write(wt.join("untracked"), "private user content").unwrap();
        let before = db.list_merge_queue().unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert_eq!(report.kept_dirty, ["feature"]);
        assert_eq!(db.list_merge_queue().unwrap(), before);
        let claim = crate::worktree_lifecycle::try_scoped_destroy_path(&wt).unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert_eq!(report.kept.len(), 1);
        assert_eq!(db.list_merge_queue().unwrap(), before);
        drop(claim);
        cfg.merge_queue.target_branch = "other".into();
        assert!(sweep_with_db(&cfg, &root, true, &db).is_empty());
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(wt.join("untracked")).unwrap(),
            "private user content"
        );
    }

    #[test]
    fn landed_worktree_status_matrix_respects_expiry_and_force_without_discarding_edits() {
        let states = ["clean", "ignored", "tracked", "untracked"];
        for state in states {
            for force in [false, true] {
                let isolation = crate::merge_lifecycle::TestIsolation::new();
                let dir = tempfile::tempdir().unwrap();
                let parent = dir.path().canonicalize().unwrap();
                let db_path = parent.join("private.db");
                let db = Db::open_at(&db_path).unwrap();
                let name = format!("matrix-{state}-{}", if force { "force" } else { "due" });
                let (root, wt) = fixture(&parent, &name, &db, &isolation);
                match state {
                    "clean" => {}
                    "ignored" => {
                        std::fs::write(wt.join("ignored"), "build output\n").unwrap();
                    }
                    "tracked" => {
                        std::fs::write(wt.join("tracked"), "edited user work\n").unwrap();
                    }
                    "untracked" => {
                        std::fs::write(wt.join("untracked"), "new user work\n").unwrap();
                    }
                    _ => unreachable!(),
                }
                rusqlite::Connection::open(&db_path)
                    .unwrap()
                    .execute("UPDATE merge_queue SET queued_at=1, updated_at=1", [])
                    .unwrap();
                let before = db.list_merge_queue().unwrap();
                let mut cfg = local_config();
                cfg.merge_queue.on_landed = OnLanded::Expire;
                cfg.merge_queue.target_branch = "main".into();
                cfg.merge_queue.merged_ttl_secs = 1;
                let report = sweep_with_db(&cfg, &root, force, &db);
                let removable = matches!(state, "clean" | "ignored");
                if removable {
                    assert_eq!(report.collected, ["feature"], "{state}, force={force}");
                    assert!(!wt.exists(), "{state}, force={force}");
                    assert!(report.kept_dirty.is_empty());
                    assert!(report.kept_changed.is_empty());
                    assert_eq!(
                        report.discarded_build_state,
                        if state == "ignored" {
                            vec!["feature".to_string()]
                        } else {
                            Vec::<String>::new()
                        },
                        "{state}, force={force}"
                    );
                    assert_eq!(db.list_merge_queue().unwrap().len(), 1);
                    assert!(db.worktree_record(wt.to_str().unwrap()).unwrap().is_none());
                } else {
                    assert!(report.collected.is_empty(), "{state}, force={force}");
                    assert_eq!(report.kept_dirty, ["feature"], "{state}, force={force}");
                    assert!(report.kept_changed.is_empty());
                    assert!(report.discarded_build_state.is_empty());
                    assert!(wt.exists(), "{state}, force={force}");
                    assert_eq!(db.list_merge_queue().unwrap(), before);
                    assert!(db.worktree_record(wt.to_str().unwrap()).unwrap().is_some());
                }
            }
        }
    }

    fn seed_persisted_layout(db: &Db, worktree: &str) {
        use thegn_core::models::{GroupTabRow, TabGroupRow};
        for (session, name) in [("layout-a", "app/feature"), ("layout-b", "other/feature")] {
            db.put_tab_group(
                session,
                &TabGroupRow {
                    name: name.into(),
                    kind: "branch".into(),
                    worktree: worktree.into(),
                    ordinal: 0,
                    active_tab: 0,
                },
            )
            .unwrap();
            db.put_group_tab(
                session,
                &GroupTabRow {
                    group_name: name.into(),
                    ordinal: 0,
                    title: "1".into(),
                    pane_tree: r#"{"leaf":0}"#.into(),
                    focused_pane: 0,
                    pane_cwds: String::new(),
                    pane_cmds: String::new(),
                    pane_sessions: String::new(),
                    scrollback_snapshot: String::new(),
                },
            )
            .unwrap();
        }
    }

    fn age_landed_row(db_path: &Path, worktree: &str) {
        rusqlite::Connection::open(db_path)
            .unwrap()
            .execute(
                "UPDATE merge_queue SET queued_at=1, updated_at=1 WHERE worktree=?1",
                [worktree],
            )
            .unwrap();
    }

    #[test]
    fn persisted_layout_is_deleted_for_every_session_after_collection() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        for force in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().canonicalize().unwrap();
            let db_path = parent.join("private.db");
            let db = Db::open_at(&db_path).unwrap();
            let (root, wt) = fixture(&parent, "layout", &db, &isolation);
            let path = wt.to_str().unwrap();
            seed_persisted_layout(&db, path);
            age_landed_row(&db_path, path);

            let mut cfg = local_config();
            cfg.merge_queue.on_landed = OnLanded::Expire;
            cfg.merge_queue.target_branch = "main".into();
            cfg.merge_queue.merged_ttl_secs = 1;
            let report = sweep_with_db(&cfg, &root, force, &db);

            assert_eq!(report.collected, ["feature"], "force={force}");
            assert!(!wt.exists(), "force={force}");
            assert!(db.groups_for_session("layout-a").unwrap().is_empty());
            assert!(db.groups_for_session("layout-b").unwrap().is_empty());
            assert!(db.group_tabs_for_session("layout-a").unwrap().is_empty());
            assert!(db.group_tabs_for_session("layout-b").unwrap().is_empty());
            assert!(db.worktree_record(path).unwrap().is_none());
            assert_eq!(db.list_merge_queue().unwrap().len(), 1);
        }
    }

    struct SessionLatch(std::path::PathBuf);

    impl Drop for SessionLatch {
        fn drop(&mut self) {
            crate::worktree_lifecycle::release_session_start(&self.0);
        }
    }

    #[test]
    fn live_session_latch_still_refuses_collection_in_both_force_modes() {
        for force in [false, true] {
            let isolation = crate::merge_lifecycle::TestIsolation::new();
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().canonicalize().unwrap();
            let db_path = parent.join("private.db");
            let db = Db::open_at(&db_path).unwrap();
            let (root, wt) = fixture(&parent, "live", &db, &isolation);
            let path = wt.to_str().unwrap();
            seed_persisted_layout(&db, path);
            age_landed_row(&db_path, path);
            assert!(
                crate::worktree_lifecycle::session_start_once(&Config::default(), &wt, None,)
                    .unwrap()
            );
            let _latch = SessionLatch(wt.clone());
            let before = db.list_merge_queue().unwrap();

            let mut cfg = local_config();
            cfg.merge_queue.on_landed = OnLanded::Expire;
            cfg.merge_queue.target_branch = "main".into();
            cfg.merge_queue.merged_ttl_secs = 1;
            let report = sweep_with_db(&cfg, &root, force, &db);

            assert!(report.collected.is_empty(), "force={force}: {report:?}");
            assert_eq!(report.kept.len(), 1, "force={force}");
            assert!(
                report.kept[0]
                    .1
                    .contains("active session requires explicit cleanup"),
                "force={force}: {report:?}"
            );
            assert!(wt.exists(), "force={force}");
            assert_eq!(db.list_merge_queue().unwrap(), before, "force={force}");
            assert_eq!(db.groups_for_session("layout-a").unwrap().len(), 1);
            assert_eq!(db.groups_for_session("layout-b").unwrap().len(), 1);
        }
    }

    #[test]
    fn tenancy_and_nonterminal_dispatch_still_refuse_both_force_modes() {
        for (hold, table) in [("tenancy", "tenancy"), ("dispatch", "dispatch")] {
            for force in [false, true] {
                let isolation = crate::merge_lifecycle::TestIsolation::new();
                let dir = tempfile::tempdir().unwrap();
                let parent = dir.path().canonicalize().unwrap();
                let db_path = parent.join("private.db");
                let db = Db::open_at(&db_path).unwrap();
                let (root, wt) = fixture(&parent, hold, &db, &isolation);
                let path = wt.to_str().unwrap();
                seed_persisted_layout(&db, path);
                age_landed_row(&db_path, path);
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                if table == "tenancy" {
                    conn.execute(
                        "INSERT INTO host_tenancy(sandbox,host_id,worktree,cpu_floor_milli,mem_floor_mb,state,reserved_at) VALUES(?1,'invalid-host-id',?1,0,0,'released',0)",
                        [path],
                    )
                    .unwrap();
                } else {
                    conn.execute(
                        "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status) VALUES('private',?1,'private',0,'running')",
                        [path],
                    )
                    .unwrap();
                }
                let before = db.list_merge_queue().unwrap();

                let mut cfg = local_config();
                cfg.merge_queue.on_landed = OnLanded::Expire;
                cfg.merge_queue.target_branch = "main".into();
                cfg.merge_queue.merged_ttl_secs = 1;
                let report = sweep_with_db(&cfg, &root, force, &db);

                assert!(report.collected.is_empty(), "{hold}, force={force}");
                assert_eq!(report.kept.len(), 1, "{hold}, force={force}");
                assert!(
                    report.kept[0]
                        .1
                        .contains("runtime tenancy or dispatch ownership"),
                    "{hold}, force={force}: {report:?}"
                );
                assert!(wt.exists(), "{hold}, force={force}");
                assert_eq!(
                    db.list_merge_queue().unwrap(),
                    before,
                    "{hold}, force={force}"
                );
                assert_eq!(db.groups_for_session("layout-a").unwrap().len(), 1);
                assert_eq!(db.groups_for_session("layout-b").unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn fresh_landed_row_waits_for_grace_period_but_force_bypasses_only_the_clock() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "fresh", &db, &isolation);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        cfg.merge_queue.merged_ttl_secs = 3600;

        let before = db.list_merge_queue().unwrap();
        let waiting = sweep_with_db(&cfg, &root, false, &db);
        assert!(waiting.is_empty());
        assert!(wt.exists());
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert!(db.worktree_record(wt.to_str().unwrap()).unwrap().is_some());

        let forced = sweep_with_db(&cfg, &root, true, &db);
        assert_eq!(forced.collected, ["feature"]);
        assert!(!wt.exists());
        assert_eq!(db.list_merge_queue().unwrap().len(), 1);
        assert!(db.worktree_record(wt.to_str().unwrap()).unwrap().is_none());
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn held_cleanup_survives_refresh_without_repeating_missing_worktree_errors_or_touching_refs() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "held", &db, &isolation);
        let selected = db.list_merge_queue().unwrap().remove(0);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let first = sweep_with_db(&cfg, &root, true, &db);
        assert_eq!(first.collected, ["feature"]);
        assert!(first.cleared_rows.is_empty());
        assert!(!wt.exists());
        let mut held = selected;
        held.error_detail = Some(merge_sweep::CleanupHold::BranchRetained.marker().into());
        assert_eq!(db.list_merge_queue().unwrap(), [held.clone()]);
        // These are the real generic reconciliation/cache-pruning calls.
        db.del_worktree(wt.to_str().unwrap()).unwrap();
        db.del_worktree(wt.to_str().unwrap()).unwrap();
        assert_eq!(db.list_merge_queue().unwrap(), [held]);
        for args in [
            vec!["branch", "victim"],
            vec!["symbolic-ref", "refs/heads/feature", "refs/heads/victim"],
            vec!["symbolic-ref", "refs/heads/main", "refs/heads/victim"],
        ] {
            let out = isolation.git(&root).args(args).output().unwrap();
            assert!(out.status.success());
        }
        let refs = || {
            let out = isolation
                .git(&root)
                .args([
                    "for-each-ref",
                    "--format=%(refname) %(symref) %(objectname)",
                ])
                .output()
                .unwrap();
            assert!(out.status.success());
            out.stdout
        };
        let before = refs();
        assert!(sweep_with_db(&cfg, &root, false, &db).is_empty());
        let forced = sweep_with_db(&cfg, &root, true, &db);
        assert!(forced.collected.is_empty() && forced.cleared_rows.is_empty());
        assert_eq!(forced.kept.len(), 1);
        assert!(forced.kept[0].1.contains("previously collected"));
        assert_eq!(
            refs(),
            before,
            "source, victim and symbolic target all survive"
        );
        db.retry_merge_entry(wt.to_str().unwrap()).unwrap();
        let retried = db.list_merge_queue().unwrap().remove(0);
        assert_eq!(retried.status, "queued");
        assert!(retried.error_detail.is_none());
    }

    #[test]
    fn foreign_cached_repository_is_refused_before_teardown() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "one", &db, &isolation);
        let (foreign, _) = fixture(&parent, "two", &db, &isolation);
        db.put_worktree(
            "feature",
            foreign.to_str().unwrap(),
            wt.to_str().unwrap(),
            "feature",
            None,
            None,
        )
        .unwrap();
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let before = db.list_merge_queue().unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert!(
            report
                .kept
                .iter()
                .any(|(_, why)| why.contains("cached repository"))
        );
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert!(wt.join("tracked").exists());
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn malformed_remote_cache_refuses_before_hooks_and_preserves_worktree_refs_queue() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db_path = parent.join("private.db");
        let db = Db::open_at(&db_path).unwrap();
        let (root, wt) = fixture(&parent, "malformed", &db, &isolation);
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE worktrees SET location='remote-placement', position='malformed' WHERE worktree=?1",
                [wt.to_str().unwrap()],
            )
            .unwrap();
        assert!(
            db.worktrees().unwrap().is_empty(),
            "exercise hidden legacy row"
        );
        assert!(db.worktree_record(wt.to_str().unwrap()).is_err());
        let refs = || {
            let output = isolation
                .git(&root)
                .args([
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before_refs = refs();
        let before_queue = db.list_merge_queue().unwrap();
        let before_contents = std::fs::read(wt.join("tracked")).unwrap();
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Command(
            "printf ran > cleanup-hook".into(),
        )];
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty() && report.cleared_rows.is_empty());
        assert_eq!(report.kept.len(), 1);
        assert!(
            report.kept[0]
                .1
                .contains("worktree cache identity unavailable")
        );
        assert_eq!(refs(), before_refs);
        assert_eq!(db.list_merge_queue().unwrap(), before_queue);
        assert_eq!(std::fs::read(wt.join("tracked")).unwrap(), before_contents);
        assert!(!wt.join("cleanup-hook").exists());
        assert!(!root.join("cleanup-hook").exists());
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn unknown_resource_metadata_refuses_before_hooks_and_preserves_worktree_refs_queue() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        for kind in [
            "tenancy-key",
            "tenancy-association",
            "dispatch",
            "pending-dispatch",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().canonicalize().unwrap();
            let db_path = parent.join("private.db");
            let db = Db::open_at(&db_path).unwrap();
            let (root, wt) = fixture(&parent, kind, &db, &isolation);
            let fixture_conn = rusqlite::Connection::open(&db_path).unwrap();
            let path = wt.to_str().unwrap();
            match kind {
                "tenancy-key" | "tenancy-association" => {
                    let (sandbox, association) = if kind == "tenancy-key" {
                        (path, "")
                    } else {
                        ("private-reservation", path)
                    };
                    fixture_conn.execute(
                        "INSERT INTO host_tenancy(sandbox,host_id,worktree,cpu_floor_milli,mem_floor_mb,reserved_at) VALUES(?1,'invalid-host-id',?2,0,0,0)",
                        [sandbox, association],
                    ).unwrap();
                }
                _ => {
                    let (active, pending, status) = if kind == "dispatch" {
                        (path, "", "unknown-future-state")
                    } else {
                        ("private-other", path, "done")
                    };
                    fixture_conn.execute(
                        "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status,pending_worktree_path) VALUES('private',?1,'private',0,?2,?3)",
                        [active, status, pending],
                    ).unwrap();
                }
            }
            let refs = || {
                let output = isolation
                    .git(&root)
                    .args([
                        "for-each-ref",
                        "--format=%(refname) %(objectname)",
                        "refs/heads",
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                output.stdout
            };
            let before_refs = refs();
            let before_queue = db.list_merge_queue().unwrap();
            let mut cfg = local_config();
            cfg.merge_queue.on_landed = OnLanded::Expire;
            cfg.merge_queue.target_branch = "main".into();
            cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Command(
                "printf ran > cleanup-hook".into(),
            )];
            let report = sweep_with_db(&cfg, &root, true, &db);
            assert!(
                report.collected.is_empty() && report.cleared_rows.is_empty(),
                "{kind}"
            );
            assert_eq!(report.kept.len(), 1, "{kind}");
            assert!(
                report.kept[0]
                    .1
                    .contains("runtime tenancy or dispatch ownership"),
                "{kind}: {report:?}"
            );
            assert_eq!(refs(), before_refs, "{kind}");
            assert_eq!(db.list_merge_queue().unwrap(), before_queue, "{kind}");
            assert_eq!(
                std::fs::read(wt.join("tracked")).unwrap(),
                b"keep\n",
                "{kind}"
            );
            assert!(!wt.join("cleanup-hook").exists(), "{kind}");
            assert!(!root.join("cleanup-hook").exists(), "{kind}");
        }
    }

    #[test]
    fn post_admission_unknown_dispatch_stops_removal_after_the_pre_destroy_hook() {
        assert_post_admission_mutation_stops_removal(LateCleanupMutation::UnknownDispatch);
    }

    #[test]
    fn post_admission_malformed_registry_stops_removal_after_the_pre_destroy_hook() {
        assert_post_admission_mutation_stops_removal(LateCleanupMutation::MalformedRegistry);
    }

    #[derive(Clone, Copy)]
    enum LateCleanupMutation {
        UnknownDispatch,
        MalformedRegistry,
    }

    #[expect(clippy::disallowed_methods)]
    fn assert_post_admission_mutation_stops_removal(mutation: LateCleanupMutation) {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db_path = parent.join("private.db");
        let db = Db::open_at(&db_path).unwrap();
        let (root, wt) = fixture(&parent, "late-resource", &db, &isolation);
        assert!(db.worktree_record(wt.to_str().unwrap()).unwrap().is_some());
        let before_queue = db.list_merge_queue().unwrap();
        let refs = || {
            let output = isolation
                .git(&root)
                .args([
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before_refs = refs();
        let started = parent.join("hook-started");
        let release = parent.join("hook-release");
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Spec(
            thegn_core::hooks::HookEntrySpec {
                command: format!(
                    "printf started > {}; while [ ! -e {} ]; do sleep 0.01; done",
                    util::sh_quote(&started.to_string_lossy()),
                    util::sh_quote(&release.to_string_lossy()),
                ),
                wait: Some(true),
                timeout_secs: Some(10),
                on_failure: Some(thegn_core::hooks::HookFailure::Warn),
            },
        )];
        let writer_started = started.clone();
        let writer_release = release.clone();
        let writer_path = wt.to_str().unwrap().to_owned();
        // Scoped ownership joins the writer even if the sweep/assertion panics.
        let report = std::thread::scope(|scope| {
            let writer = std::thread::Builder::new().name("private-late-resource".into())
                .spawn_scoped(scope, move || -> anyhow::Result<()> {
                    struct Release(std::path::PathBuf);
                    impl Drop for Release {
                        fn drop(&mut self) {
                            if let Err(error) = std::fs::write(&self.0, "release") {
                                std::io::Write::write_fmt(
                                    &mut std::io::stderr(),
                                    format_args!("private hook release failed: {error}\n"),
                                )
                                .unwrap_or(()); // Best-effort diagnostic during cleanup/unwind.
                            }
                        }
                    }
                    let _release = Release(writer_release);
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !writer_started.exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    anyhow::ensure!(writer_started.exists(), "pre-destroy rendezvous did not start");
                    let writer_conn = rusqlite::Connection::open(&db_path)?;
                    let changed = match mutation {
                        LateCleanupMutation::UnknownDispatch => writer_conn.execute(
                            "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status) VALUES('private',?1,'private',0,'unknown-after-admission')",
                            [writer_path],
                        )?,
                        // Change only an unrelated decoder field AFTER the real
                        // hook starts. Initial registry admission was valid.
                        LateCleanupMutation::MalformedRegistry => writer_conn.execute(
                            "UPDATE worktrees SET position='malformed-after-admission' WHERE worktree=?1",
                            [writer_path],
                        )?,
                    };
                    anyhow::ensure!(changed == 1, "private mutation did not reach its row");
                    Ok(())
                }).unwrap();
            let report = sweep_with_db(&cfg, &root, true, &db);
            writer.join().unwrap().unwrap();
            report
        });
        assert!(
            started.exists() && release.exists(),
            "the earlier hook really ran"
        );
        assert!(report.collected.is_empty() && report.cleared_rows.is_empty());
        assert_eq!(report.kept.len(), 1);
        match mutation {
            LateCleanupMutation::UnknownDispatch => {
                assert!(
                    report.kept[0]
                        .1
                        .contains("runtime tenancy or dispatch ownership"),
                    "{report:?}"
                );
                assert!(
                    thegn_core::store::NotificationStore::has_cleanup_dispatch(
                        &db,
                        wt.to_str().unwrap()
                    )
                    .unwrap()
                );
            }
            LateCleanupMutation::MalformedRegistry => {
                let decode_error = db.worktree_record(wt.to_str().unwrap()).unwrap_err();
                assert!(
                    report.kept[0].1.contains(&decode_error.to_string()),
                    "{report:?}"
                );
                assert!(
                    !thegn_core::store::NotificationStore::has_cleanup_dispatch(
                        &db,
                        wt.to_str().unwrap()
                    )
                    .unwrap()
                );
            }
        }
        assert_eq!(refs(), before_refs);
        assert_eq!(db.list_merge_queue().unwrap(), before_queue);
        assert_eq!(std::fs::read(wt.join("tracked")).unwrap(), b"keep\n");
    }

    #[test]
    fn diagnostics_never_emit_terminal_controls_and_are_bounded() {
        assert_eq!(safe_display("α\u{061c}\u{200e}\u{200f}β"), "α���β");
        assert_eq!(safe_display("a\x1b[31m\n\u{202e}b"), "a�[31m��b");
        assert_eq!(safe_display(&"x".repeat(1000)).len(), 512);
    }

    #[test]
    fn selected_landed_row_cannot_authorize_requeued_or_revoked_cleanup() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        for revoke in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().canonicalize().unwrap();
            let db = Db::open_memory().unwrap();
            let (root, wt) = fixture(&parent, "selected", &db, &isolation);
            let selected = db.list_merge_queue().unwrap().remove(0);
            if revoke {
                db.remove_merge_entry(wt.to_str().unwrap()).unwrap();
            } else {
                db.enqueue_merge(wt.to_str().unwrap(), "feature", "main")
                    .unwrap();
            }
            let before = db.list_merge_queue().unwrap();
            let outcome = crate::merge_lifecycle::remove_landed_with_config(
                &Config::default(),
                &db,
                &root,
                wt.to_str().unwrap(),
                "feature",
                "main",
                selected.result_oid.as_deref(),
                &selected,
                true,
            );
            assert!(
                matches!(outcome, crate::merge_lifecycle::CleanupOutcome::Refused { reason } if reason.contains("selected landed"))
            );
            assert_eq!(db.list_merge_queue().unwrap(), before);
            assert!(wt.join("tracked").exists());
        }
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn refinalized_result_oid_revokes_selected_cleanup_before_hooks() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db_path = parent.join("private.db");
        let db = Db::open_at(&db_path).unwrap();
        let (root, wt) = fixture(&parent, "refinalized", &db, &isolation);
        let selected = db.list_merge_queue().unwrap().remove(0);
        let git = |args: &[&str]| {
            let output = isolation
                .git(&root)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        // Both results are real commits, and the old landed commit is still an
        // ancestor of main. Git eligibility cannot mask the stale DB identity.
        git(&[
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "private replacement result",
        ]);
        let replacement = git(&["rev-parse", "HEAD"]);
        assert_ne!(Some(replacement.as_str()), selected.result_oid.as_deref());
        let writer = rusqlite::Connection::open(&db_path).unwrap();
        assert_eq!(
            writer
                .execute(
                    "UPDATE merge_queue SET result_oid=?1 WHERE worktree=?2",
                    [replacement.as_str(), wt.to_str().unwrap()],
                )
                .unwrap(),
            1
        );
        let mut expected = selected.clone();
        expected.result_oid = Some(replacement);
        assert_eq!(db.list_merge_queue().unwrap(), [expected.clone()]);
        let before_refs = git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ]);
        let marker = parent.join("unexpected-pre-destroy");
        let mut cfg = local_config();
        cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Command(format!(
            "printf ran > {}",
            util::sh_quote(marker.to_str().unwrap())
        ))];
        let outcome = crate::merge_lifecycle::remove_landed_with_config(
            &cfg,
            &db,
            &root,
            wt.to_str().unwrap(),
            "feature",
            "main",
            selected.result_oid.as_deref(),
            &selected,
            true,
        );
        assert!(
            matches!(outcome, crate::merge_lifecycle::CleanupOutcome::Refused { reason }
            if reason.contains("selected landed queue entry changed"))
        );
        assert!(!marker.exists(), "stale selection reached pre_destroy");
        assert_eq!(std::fs::read(wt.join("tracked")).unwrap(), b"keep\n");
        assert_eq!(
            git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads"
            ]),
            before_refs
        );
        // Manual asynchronous clearing must respect the same stale selected
        // value, even though status, branch, timestamps and location all match.
        assert_eq!(clear_selected_landed(&db, &[selected]).unwrap(), 0);
        assert_eq!(db.list_merge_queue().unwrap(), [expected]);
    }

    #[test]
    fn manual_clear_only_deletes_exact_selected_landed_rows() {
        let db = Db::open_memory().unwrap();
        for wt in ["unchanged", "retried", "revoked", "refinalized"] {
            db.enqueue_merge(wt, "feature", "main").unwrap();
            db.update_merge_status(wt, "landed", Some("fixture-oid"), None, None)
                .unwrap();
        }
        let mut selected = db.list_merge_queue().unwrap();
        selected.extend(selected.clone()); // duplicate UI selection must not inflate success
        db.enqueue_merge("retried", "feature", "main").unwrap();
        db.remove_merge_entry("revoked").unwrap();
        db.update_merge_status("refinalized", "landed", Some("new-result"), None, None)
            .unwrap();
        assert_eq!(clear_selected_landed(&db, &selected).unwrap(), 1);
        let remaining = db.list_merge_queue().unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining
                .iter()
                .any(|row| row.worktree == "retried" && row.status == "queued")
        );
        assert!(remaining.iter().any(|row| row.worktree == "refinalized"
            && row.status == "landed"
            && row.result_oid.as_deref() == Some("new-result")));
    }

    fn report(collected: &[&str], kept: &[&str]) -> SweepReport {
        SweepReport {
            collected: collected.iter().map(|s| (*s).to_string()).collect(),
            kept_dirty: kept.iter().map(|s| (*s).to_string()).collect(),
            ..SweepReport::default()
        }
    }

    #[test]
    fn an_empty_report_is_empty_only_with_neither_list() {
        assert!(SweepReport::default().is_empty());
        assert!(!report(&["a"], &[]).is_empty());
        assert!(!report(&[], &["a"]).is_empty());
        assert!(
            !SweepReport {
                kept: vec![("a".into(), "refused".into())],
                ..Default::default()
            }
            .is_empty()
        );
        assert!(
            !SweepReport {
                bookkeeping_errors: vec!["unavailable".into()],
                ..Default::default()
            }
            .is_empty()
        );
    }

    /// The sweep is inert in every mode but `expire`, forced or not — under
    /// `move` the worktrees are meant to persist, and under `remove`/`detach`
    /// they were already collected at landing.
    #[test]
    fn only_expire_mode_sweeps() {
        let root = std::env::temp_dir();
        for mode in [
            OnLanded::Off,
            OnLanded::Move,
            OnLanded::Detach,
            OnLanded::Remove,
        ] {
            let mut cfg = Config::default();
            cfg.merge_queue.on_landed = mode;
            for force in [false, true] {
                assert!(
                    sweep(&cfg, &root, force).is_empty(),
                    "{mode:?} (force={force}) must not sweep"
                );
            }
        }
    }
}
