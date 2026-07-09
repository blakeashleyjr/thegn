//! TTL'd read-through caches (the embedded-SQLite [`CacheStore`] impl): PR
//! status, CI runs, per-repo open-PRs-by-branch, issue-tracker payloads, the
//! unified "My Work" feed, and the per-worktree diff/commit/test/LOC snapshots
//! that feed the panel's instant paint.
//!
//! These are pure caches — best-effort, `git`/live-API is the source of truth.
//! Sibling `impl` block (via the `conn()` accessor) so the pinned `db.rs` only
//! carries the schema DDL, not these bodies. A server backend would implement
//! [`CacheStore`] against Postgres for shared, multi-user cache state.

use anyhow::Result;
use rusqlite::params;

use crate::db::Db;
use crate::store::CacheStore;
use crate::util;

impl CacheStore for Db {
    fn get_pr_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM pr_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn list_pr_cache(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT worktree, json, fetched_at FROM pr_cache")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn put_pr_cache(&self, worktree: &str, branch: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO pr_cache(worktree,branch,json,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(worktree) DO UPDATE SET branch=?2, json=?3, fetched_at=?4"#,
            params![worktree, branch, json, util::now()],
        )?;
        Ok(())
    }

    fn get_ci_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM ci_runs_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_ci_cache(&self, worktree: &str, branch: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO ci_runs_cache(worktree,branch,json,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(worktree) DO UPDATE SET branch=?2, json=?3, fetched_at=?4"#,
            params![worktree, branch, json, util::now()],
        )?;
        Ok(())
    }

    fn get_pr_branch_cache(&self, repo_root: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM pr_branch_cache WHERE repo_root=?1",
                params![repo_root],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_pr_branch_cache(&self, repo_root: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO pr_branch_cache(repo_root,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(repo_root) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![repo_root, json, util::now()],
        )?;
        Ok(())
    }

    fn get_open_pr_counts_by_branch(
        &self,
        repo_root: &str,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut counts = std::collections::BTreeMap::new();
        let Some((json, _)) = self.get_pr_branch_cache(repo_root)? else {
            return Ok(counts);
        };
        for pr in crate::github::parse_pr_headers(&json) {
            if pr.state.eq_ignore_ascii_case("open") {
                *counts.entry(pr.head_ref).or_insert(0) += 1;
            }
        }
        Ok(counts)
    }

    fn get_issue_cache(&self, repo_root: &str, provider: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM issue_cache WHERE repo_root=?1 AND provider=?2",
                params![repo_root, provider],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn get_all_issue_cache(&self, repo_root: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT provider, json FROM issue_cache WHERE repo_root=?1")?;
        let rows = stmt.query_map(params![repo_root], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn put_issue_cache(&self, repo_root: &str, provider: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO issue_cache(repo_root,provider,json,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(repo_root,provider) DO UPDATE SET json=?3, fetched_at=?4"#,
            params![repo_root, provider, json, util::now()],
        )?;
        Ok(())
    }

    fn get_my_work_cache(&self, scope: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM my_work_cache WHERE scope=?1",
                params![scope],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_my_work_cache(&self, scope: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO my_work_cache(scope,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(scope) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![scope, json, util::now()],
        )?;
        Ok(())
    }

    fn get_diff_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT files, fetched_at FROM diff_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_diff_cache(&self, worktree: &str, files: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO diff_cache(worktree,files,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET files=?2, fetched_at=?3"#,
            params![worktree, files, util::now()],
        )?;
        Ok(())
    }

    fn get_commit_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM commit_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_commit_cache(&self, worktree: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO commit_cache(worktree,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![worktree, json, util::now()],
        )?;
        Ok(())
    }

    fn get_test_cache(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM test_cache WHERE worktree=?1",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_test_cache(&self, worktree: &str, json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO test_cache(worktree,json,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(worktree) DO UPDATE SET json=?2, fetched_at=?3"#,
            params![worktree, json, util::now()],
        )?;
        Ok(())
    }

    fn get_loc_cache_entry(&self, worktree: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT report_json, fetched_at FROM loc_cache \
                 WHERE worktree=?1 AND report_json IS NOT NULL",
                params![worktree],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_loc_cache(&self, worktree: &str, total: usize, report_json: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO loc_cache(worktree,loc,report_json,fetched_at)
               VALUES(?1,?2,?3,?4)
               ON CONFLICT(worktree) DO UPDATE SET loc=?2, report_json=?3, fetched_at=?4"#,
            params![worktree, total as i64, report_json, util::now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_pr_cache_returns_every_row() {
        let db = Db::open_memory().unwrap();
        assert!(db.list_pr_cache().unwrap().is_empty());
        db.put_pr_cache("/wt/a", "br-a", "{\"n\":1}").unwrap();
        db.put_pr_cache("/wt/b", "br-b", "{\"n\":2}").unwrap();
        db.put_pr_cache("/wt/a", "br-a", "{\"n\":3}").unwrap(); // upsert, not duplicate
        let mut rows = db.list_pr_cache().unwrap();
        rows.sort();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "/wt/a");
        assert_eq!(rows[0].1, "{\"n\":3}");
        assert!(rows[0].2 > 0);
        assert_eq!(rows[1].0, "/wt/b");
    }
}
