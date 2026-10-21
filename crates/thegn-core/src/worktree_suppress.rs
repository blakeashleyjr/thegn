//! Durable user intent to keep explicitly closed worktrees out of discovery.

use crate::db::Db;
use crate::identity::{ExactPath, hex_bytes};
use crate::store::WorkspaceStore;
use std::path::Path;

pub const SCOPE: &str = "worktree_suppress";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionKeys {
    pub admin: Option<String>,
    pub path: String,
}

impl SuppressionKeys {
    pub fn all(&self) -> impl Iterator<Item = &str> {
        self.admin
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.path.as_str()))
    }
}

pub fn keys_for(path: &Path) -> anyhow::Result<SuppressionKeys> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let exact = ExactPath::from_path(&canonical)?;
    let admin = crate::repo::main_worktree(&canonical)
        .and_then(|root| crate::worktree::inspect_registered(&root, &canonical).ok())
        .map(|identity| format!("admin:{}", hex_bytes(identity.admin_id())));
    Ok(SuppressionKeys {
        admin,
        path: format!("path:{}", hex_bytes(exact.as_bytes())),
    })
}

pub fn record_json(path: &str, group: &str) -> String {
    serde_json::json!({"path": path, "group": group, "closed_at": crate::util::now()}).to_string()
}

pub fn close_with_suppression(
    db: &Db,
    worktree: &str,
    keys: &SuppressionKeys,
    record: &str,
) -> anyhow::Result<()> {
    db.transaction(|db| {
        for key in keys.all() {
            db.set_ui_state(SCOPE, key, record)?;
        }
        db.del_worktree(worktree)
    })
}

pub fn is_suppressed(db: &Db, keys: &SuppressionKeys) -> anyhow::Result<bool> {
    for key in keys.all() {
        if db.get_ui_state(SCOPE, key)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn reopen(
    db: &Db,
    keys: &SuppressionKeys,
    tab: &str,
    root: &str,
    worktree: &str,
    branch: &str,
) -> anyhow::Result<bool> {
    db.transaction(|db| {
        let was_suppressed = is_suppressed(db, keys)?;
        for key in keys.all() {
            db.del_ui_state(SCOPE, key)?;
        }
        db.put_worktree(tab, root, worktree, branch, None, None)?;
        Ok(was_suppressed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WorkspaceStore;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        crate::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn close_records_suppression_and_deletes_the_row() {
        let db = db();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("external");
        let path = path.to_string_lossy().into_owned();
        db.put_worktree("repo/feature", "/repo", &path, "feature", None, None)
            .unwrap();
        let keys = keys_for(Path::new(&path)).unwrap();
        assert!(keys.admin.is_none());
        close_with_suppression(&db, &path, &keys, &record_json(&path, "repo/feature")).unwrap();
        assert!(is_suppressed(&db, &keys).unwrap());
        assert!(db.worktree_record(&path).unwrap().is_none());
    }

    #[test]
    fn close_rolls_back_when_the_suppression_write_fails() {
        let db = db();
        db.put_worktree("repo/feature", "/repo", "/repo/wt", "feature", None, None)
            .unwrap();
        let keys = keys_for(Path::new("/repo/wt")).unwrap();
        db.conn().execute_batch("DROP TABLE ui_state").unwrap();
        assert!(close_with_suppression(&db, "/repo/wt", &keys, "{}").is_err());
        assert!(db.worktree_record("/repo/wt").unwrap().is_some());
    }

    #[test]
    fn keys_for_uses_the_git_admin_dir_for_a_linked_worktree() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "test@example.invalid"],
            &["config", "user.name", "test"],
            &["config", "commit.gpgsign", "false"],
            &["commit", "--allow-empty", "-q", "-m", "init"],
        ] {
            assert!(git_ok(&root, args));
        }
        let old = scratch.path().join("old");
        assert!(git_ok(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                old.to_str().unwrap()
            ]
        ));
        let before = keys_for(&old).unwrap();
        assert!(
            before
                .admin
                .as_deref()
                .is_some_and(|key| key.starts_with("admin:"))
        );
        let moved = scratch.path().join("moved");
        assert!(git_ok(
            &root,
            &[
                "worktree",
                "move",
                old.to_str().unwrap(),
                moved.to_str().unwrap()
            ]
        ));
        let after = keys_for(&moved).unwrap();
        assert_eq!(before.admin, after.admin);
        assert_ne!(before.path, after.path);
    }

    #[test]
    fn keys_for_refuses_a_relative_path() {
        assert!(keys_for(Path::new("relative/worktree")).is_err());
    }

    #[test]
    fn reopen_clears_only_its_own_keys_and_registers() {
        let db = db();
        let first = SuppressionKeys {
            admin: Some("admin:first".into()),
            path: "path:first".into(),
        };
        let second = SuppressionKeys {
            admin: Some("admin:second".into()),
            path: "path:second".into(),
        };
        for key in first.all().chain(second.all()) {
            db.set_ui_state(SCOPE, key, "{}").unwrap();
        }
        assert!(reopen(&db, &first, "repo/first", "/repo", "/repo/first", "first").unwrap());
        assert!(!is_suppressed(&db, &first).unwrap());
        assert!(is_suppressed(&db, &second).unwrap());
        assert!(db.worktree_record("/repo/first").unwrap().is_some());
        assert!(!reopen(&db, &first, "repo/first", "/repo", "/repo/first", "first").unwrap());
    }

    #[test]
    fn suppression_survives_reopening_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let database_path = temp.path().join("s.db");
        let worktree_path = temp.path().join("worktree");
        std::fs::create_dir(&worktree_path).unwrap();
        let worktree = worktree_path.to_string_lossy().into_owned();
        let db = Db::open_at(&database_path).unwrap();
        db.put_worktree("repo/feature", "/repo", &worktree, "feature", None, None)
            .unwrap();
        let keys = keys_for(Path::new(&worktree)).unwrap();

        close_with_suppression(
            &db,
            &worktree,
            &keys,
            &record_json(&worktree, "repo/feature"),
        )
        .unwrap();
        drop(db);

        let reopened = Db::open_at(&database_path).unwrap();
        assert!(is_suppressed(&reopened, &keys).unwrap());
        assert!(reopened.worktree_record(&worktree).unwrap().is_none());
    }

    #[test]
    fn an_unrelated_worktree_is_not_suppressed() {
        let db = db();
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first");
        let second_path = temp.path().join("second");
        std::fs::create_dir(&first_path).unwrap();
        std::fs::create_dir(&second_path).unwrap();
        let first_worktree = first_path.to_string_lossy().into_owned();
        let second_worktree = second_path.to_string_lossy().into_owned();
        db.put_worktree("repo/first", "/repo", &first_worktree, "first", None, None)
            .unwrap();
        db.put_worktree(
            "repo/second",
            "/repo",
            &second_worktree,
            "second",
            None,
            None,
        )
        .unwrap();
        let first_keys = keys_for(&first_path).unwrap();
        let second_keys = keys_for(&second_path).unwrap();

        close_with_suppression(
            &db,
            &first_worktree,
            &first_keys,
            &record_json(&first_worktree, "repo/first"),
        )
        .unwrap();

        assert!(!is_suppressed(&db, &second_keys).unwrap());
        assert!(
            reopen(
                &db,
                &first_keys,
                "repo/first",
                "/repo",
                &first_worktree,
                "first"
            )
            .unwrap()
        );
        assert!(!is_suppressed(&db, &second_keys).unwrap());
        assert!(db.worktree_record(&second_worktree).unwrap().is_some());
    }
}
