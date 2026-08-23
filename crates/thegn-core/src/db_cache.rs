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

    fn get_kaneo_token(&self, base_url: &str) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT token, fetched_at FROM kaneo_auth WHERE base_url=?1",
                params![base_url],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        Ok(r)
    }

    fn put_kaneo_token(&self, base_url: &str, token: &str) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO kaneo_auth(base_url,token,fetched_at)
               VALUES(?1,?2,?3)
               ON CONFLICT(base_url) DO UPDATE SET token=?2, fetched_at=?3"#,
            params![base_url, token, util::now()],
        )?;
        Ok(())
    }

    fn delete_kaneo_token(&self, base_url: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM kaneo_auth WHERE base_url=?1",
            params![base_url],
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

    /// Each branch's open PR **number**, for branches with exactly one open
    /// PR (the sidebar's compact `⬡N` chip names a single PR; a multi-PR
    /// branch falls back to the count form via
    /// [`Self::get_open_pr_counts_by_branch`]).
    fn get_open_pr_numbers_by_branch(
        &self,
        repo_root: &str,
    ) -> Result<std::collections::BTreeMap<String, u64>> {
        let mut numbers: std::collections::BTreeMap<String, Option<u64>> = Default::default();
        let Some((json, _)) = self.get_pr_branch_cache(repo_root)? else {
            return Ok(Default::default());
        };
        for pr in crate::github::parse_pr_headers(&json) {
            if pr.state.eq_ignore_ascii_case("open") {
                // Second open PR on the branch ⇒ ambiguous ⇒ None.
                numbers
                    .entry(pr.head_ref)
                    .and_modify(|n| *n = None)
                    .or_insert(Some(pr.number));
            }
        }
        Ok(numbers
            .into_iter()
            .filter_map(|(b, n)| n.map(|n| (b, n)))
            .collect())
    }

    fn get_issue_cache(
        &self,
        repo_root: &str,
        provider: &str,
        account: &str,
    ) -> Result<Option<(String, i64)>> {
        let r = self
            .conn()
            .query_row(
                "SELECT json, fetched_at FROM issue_cache \
                 WHERE repo_root=?1 AND provider=?2 AND account=?3",
                params![repo_root, provider, account],
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

    fn put_issue_cache(
        &self,
        repo_root: &str,
        provider: &str,
        account: &str,
        json: &str,
    ) -> Result<()> {
        self.conn().execute(
            r#"INSERT INTO issue_cache(repo_root,provider,account,json,fetched_at)
               VALUES(?1,?2,?3,?4,?5)
               ON CONFLICT(repo_root,provider,account) DO UPDATE SET json=?4, fetched_at=?5"#,
            params![repo_root, provider, account, json, util::now()],
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

    fn all_loc_cache_stamps(&self) -> Result<std::collections::HashMap<String, i64>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT worktree, COALESCE(fetched_at,0) FROM loc_cache")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        Ok(map)
    }

    fn delete_loc_cache(&self, worktree: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM loc_cache WHERE worktree=?1", params![worktree])?;
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
