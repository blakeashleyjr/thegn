//! One-time superzej → thegn brand migration of on-disk state.
//!
//! thegn used to be called superzej; its state lived under superzej-named
//! roots. On startup (before the first `Db::open`, so the WAL sidecars move
//! with no connection open) the host calls [`run_startup_migration`], which —
//! for each root independently — renames the old directory to its new name
//! **iff** the new one doesn't exist yet:
//!
//! - `$XDG_STATE_HOME/superzej/` → `$XDG_STATE_HOME/thegn/` (then
//!   `superzej.db{,-wal,-shm}` → `thegn.db*` inside it),
//! - `$XDG_CONFIG_HOME/superzej/` → `$XDG_CONFIG_HOME/thegn/`,
//! - `~/.superzej/` → `~/.thegn/` (skipped when `THEGN_DIR` relocates the
//!   app home — there is nothing default-located to migrate).
//!
//! Everything is **best-effort and never blocks startup**: a failed move logs
//! a warning and thegn simply starts fresh on the new paths (git is the
//! source of truth; the DB is a cache). Two safety rails:
//!
//! - **Kill-switch**: `THEGN_NO_MIGRATE=1` skips the whole pass. Dev/bench
//!   recipes set it so an isolated dev instance (`just start` isolates only
//!   `XDG_STATE_HOME`) can never yank `~/.config/superzej` / `~/.superzej`
//!   out from under a live daily-driver instance.
//! - **Busy check**: before touching anything we try `BEGIN IMMEDIATE` on the
//!   old DB; if another process holds it (a running superzej), the whole
//!   migration aborts with a warning instead of moving directories under it.
//!
//! After the moves, two comfort fixups (also best-effort): cached absolute
//! paths inside the moved DB are rewritten from `~/.superzej/…` to
//! `~/.thegn/…`, and moved git worktrees get `git worktree repair` so the
//! two-way gitdir pointers heal. A `.migrated-from-superzej` marker in the new
//! app home records what happened for forensics / manual rollback.

use std::path::{Path, PathBuf};

use crate::util;

/// What one root-pair migration decided / did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootAction {
    /// Old exists, new absent → move it.
    Migrate,
    /// New already exists (old may too — new wins, old left untouched).
    NewExists,
    /// Nothing old to migrate.
    OldAbsent,
}

/// Outcome summary, for logging at the call site.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// `(old, new)` pairs actually renamed.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Human-readable warnings for anything skipped or failed.
    pub warnings: Vec<String>,
}

impl MigrationReport {
    pub fn migrated_anything(&self) -> bool {
        !self.moved.is_empty()
    }
}

/// Pure decision for one root pair.
pub(crate) fn plan_root(old: &Path, new: &Path) -> RootAction {
    if new.exists() {
        RootAction::NewExists
    } else if old.exists() {
        RootAction::Migrate
    } else {
        RootAction::OldAbsent
    }
}

/// Rename `old` → `new` (parents created). Errors are returned, not fatal.
pub(crate) fn migrate_root(old: &Path, new: &Path) -> std::io::Result<()> {
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(old, new) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Lost a benign race (another thegn process migrated first)?
            if new.exists() {
                return Ok(());
            }
            Err(e)
        }
    }
}

/// True when the old DB is locked by a live process. Uses a zero busy-timeout
/// `BEGIN IMMEDIATE` probe; any non-lock error reads as "not busy" (the move
/// itself will surface real problems).
fn old_db_busy(db: &Path) -> bool {
    let Ok(conn) = rusqlite::Connection::open(db) else {
        return false;
    };
    let _ = conn.busy_timeout(std::time::Duration::from_millis(0));
    match conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;") {
        Ok(()) => false,
        Err(e) => {
            let s = e.to_string();
            s.contains("locked") || s.contains("busy")
        }
    }
}

/// Rename `superzej.db{,-wal,-shm}` → `thegn.db*` inside the moved state dir.
fn rename_db_files(state_new: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let old = state_new.join(format!("superzej.db{suffix}"));
        let new = state_new.join(format!("thegn.db{suffix}"));
        if old.exists()
            && !new.exists()
            && let Err(e) = std::fs::rename(&old, &new)
        {
            warnings.push(format!("rename {} failed: {e}", old.display()));
        }
    }
    warnings
}

/// Best-effort rewrite of cached absolute paths inside the migrated DB:
/// `<old_root>/…` → `<new_root>/…` in every known path-bearing column. Rows
/// that stay stale merely re-discover (resurrection skips vanished paths).
fn rewrite_db_path_prefixes(db: &Path, old_root: &str, new_root: &str) -> Vec<String> {
    const PATH_COLUMNS: &[(&str, &str)] = &[
        ("worktrees", "worktree"),
        ("worktrees", "repo_path"),
        ("workspaces", "repo_path"),
        ("repo_slugs", "repo_path"),
        ("session_state", "session_name"),
        ("worktree_disk", "worktree"),
        ("undo_marks", "worktree"),
        ("issue_links", "worktree_path"),
    ];
    let mut warnings = Vec::new();
    let conn = match rusqlite::Connection::open(db) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("db path rewrite: open failed: {e}"));
            return warnings;
        }
    };
    let _ = conn.busy_timeout(std::time::Duration::from_millis(250));
    let old_prefix = format!("{old_root}/");
    let new_prefix = format!("{new_root}/");
    for (table, col) in PATH_COLUMNS {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !exists {
            continue;
        }
        let sql = format!(
            "UPDATE {table} SET {col} = ?1 || substr({col}, ?3) WHERE substr({col}, 1, ?2) = ?4"
        );
        let res = conn.execute(
            &sql,
            rusqlite::params![
                new_prefix,
                old_prefix.len() as i64,
                old_prefix.len() as i64 + 1,
                old_prefix
            ],
        );
        if let Err(e) = res {
            warnings.push(format!("db path rewrite {table}.{col}: {e}"));
        }
    }
    warnings
}

/// The main repo root a linked worktree points at, read from its `.git` file
/// (`gitdir: <repo>/.git/worktrees/<id>`).
fn worktree_main_repo(wt: &Path) -> Option<PathBuf> {
    let gitfile = std::fs::read_to_string(wt.join(".git")).ok()?;
    let gitdir = gitfile.strip_prefix("gitdir:")?.trim();
    // …/<repo>/.git/worktrees/<id> → <repo>
    let p = Path::new(gitdir);
    let id_dir = p.parent()?; // …/.git/worktrees
    if id_dir.file_name()? != "worktrees" {
        return None;
    }
    let git_dir = id_dir.parent()?; // …/.git
    git_dir.parent().map(PathBuf::from)
}

/// `git worktree repair <wt>` from each moved worktree's main repo, healing
/// the two-way gitdir pointers broken by the directory rename.
fn repair_git_worktrees(worktrees_dir: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(repos) = std::fs::read_dir(worktrees_dir) else {
        return warnings;
    };
    for repo_entry in repos.flatten() {
        let Ok(wts) = std::fs::read_dir(repo_entry.path()) else {
            continue;
        };
        for wt in wts.flatten() {
            let wt_path = wt.path();
            if !wt_path.is_dir() {
                continue;
            }
            let Some(main_repo) = worktree_main_repo(&wt_path) else {
                continue;
            };
            let out = util::git_cmd(&main_repo)
                .args(["worktree", "repair"])
                .arg(&wt_path)
                .output();
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => warnings.push(format!(
                    "git worktree repair {} failed: {}",
                    wt_path.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                )),
                Err(e) => warnings.push(format!(
                    "git worktree repair {} failed: {e}",
                    wt_path.display()
                )),
            }
        }
    }
    warnings
}

/// Explicit-path core of the migration; [`run_startup_migration`] feeds it the
/// env-resolved roots. Split out so tests drive it against temp dirs directly.
pub(crate) fn migrate_at(
    state_parent: &Path,
    config_parent: &Path,
    home: &Path,
    migrate_app_home: bool,
) -> MigrationReport {
    let mut report = MigrationReport::default();

    // Rail: never move directories out from under a live superzej instance.
    let old_state = state_parent.join("superzej");
    let old_db = old_state.join("superzej.db");
    if old_db.exists() && old_db_busy(&old_db) {
        report.warnings.push(
            "superzej → thegn migration skipped: a running superzej instance holds the \
             database — quit it and restart thegn"
                .into(),
        );
        return report;
    }

    let mut roots: Vec<(PathBuf, PathBuf)> = vec![
        (old_state.clone(), state_parent.join("thegn")),
        (config_parent.join("superzej"), config_parent.join("thegn")),
    ];
    if migrate_app_home {
        roots.push((home.join(".superzej"), home.join(".thegn")));
    }

    for (old, new) in &roots {
        match plan_root(old, new) {
            RootAction::OldAbsent => {}
            RootAction::NewExists => {
                if old.exists() {
                    report.warnings.push(format!(
                        "both {} and {} exist — preferring the new path, old left untouched",
                        old.display(),
                        new.display()
                    ));
                }
            }
            RootAction::Migrate => match migrate_root(old, new) {
                Ok(()) => report.moved.push((old.clone(), new.clone())),
                Err(e) => {
                    // EXDEV (cross-filesystem): never recursive-copy — worktrees
                    // can be tens of GB. Tell the user the one-liner instead.
                    let hint = if e.raw_os_error() == Some(18) {
                        format!(
                            " (cross-filesystem; migrate manually: mv {} {})",
                            old.display(),
                            new.display()
                        )
                    } else {
                        String::new()
                    };
                    report
                        .warnings
                        .push(format!("move {} failed: {e}{hint}", old.display()));
                }
            },
        }
    }

    // Post-move fixups, each independently best-effort.
    let new_state = state_parent.join("thegn");
    if report.moved.iter().any(|(_, n)| *n == new_state) {
        report.warnings.extend(rename_db_files(&new_state));
    }
    let new_home = home.join(".thegn");
    let home_moved = report.moved.iter().any(|(_, n)| *n == new_home);
    if home_moved {
        let db = new_state.join("thegn.db");
        if db.exists() {
            report.warnings.extend(rewrite_db_path_prefixes(
                &db,
                &home.join(".superzej").to_string_lossy(),
                &new_home.to_string_lossy(),
            ));
        }
        report
            .warnings
            .extend(repair_git_worktrees(&new_home.join("worktrees")));
    }

    // Forensics marker (best-effort): what moved, and when (epoch seconds).
    if report.migrated_anything() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let body = report
            .moved
            .iter()
            .map(|(o, n)| format!("{} -> {}\n", o.display(), n.display()))
            .collect::<String>();
        let marker_dir = if home_moved { &new_home } else { &new_state };
        let _ = std::fs::create_dir_all(marker_dir);
        let _ = std::fs::write(
            marker_dir.join(".migrated-from-superzej"),
            format!("migrated_at_epoch_s: {stamp}\n{body}"),
        );
    }

    report
}

/// Run the one-time superzej → thegn migration against the live environment.
/// Call once at process start, **before the first `Db::open`**. No-op cost
/// when there is nothing to do: three `stat` calls.
pub fn run_startup_migration() -> MigrationReport {
    if std::env::var_os("THEGN_NO_MIGRATE").is_some_and(|v| !v.is_empty()) {
        return MigrationReport::default();
    }
    // A relocated app home (THEGN_DIR) has no default-located ~/.superzej
    // counterpart to migrate.
    let migrate_app_home = std::env::var_os("THEGN_DIR").is_none();
    let state_parent = util::xdg_state_home();
    let config_parent = util::xdg_config_home();
    let home = util::home();
    migrate_at(&state_parent, &config_parent, &home, migrate_app_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn plan_root_truth_table() {
        let t = tempfile::tempdir().unwrap();
        let old = t.path().join("old");
        let new = t.path().join("new");
        assert_eq!(plan_root(&old, &new), RootAction::OldAbsent);
        std::fs::create_dir(&old).unwrap();
        assert_eq!(plan_root(&old, &new), RootAction::Migrate);
        std::fs::create_dir(&new).unwrap();
        assert_eq!(plan_root(&old, &new), RootAction::NewExists);
        std::fs::remove_dir(&old).unwrap();
        assert_eq!(plan_root(&old, &new), RootAction::NewExists);
    }

    #[test]
    fn migrate_root_moves_and_wins_races() {
        let t = tempfile::tempdir().unwrap();
        let old = t.path().join("old");
        let new = t.path().join("deep").join("new");
        touch(&old.join("f"));
        migrate_root(&old, &new).unwrap();
        assert!(new.join("f").exists());
        assert!(!old.exists());
        // Old gone + new present (the race case) is Ok, not an error.
        migrate_root(&old, &new).unwrap();
    }

    #[test]
    fn full_migration_happy_path_and_idempotence() {
        let t = tempfile::tempdir().unwrap();
        let (state, config, home) = (
            t.path().join("st"),
            t.path().join("cf"),
            t.path().join("hm"),
        );
        touch(&state.join("superzej/superzej.db"));
        touch(&state.join("superzej/superzej.db-wal"));
        touch(&state.join("superzej/logs/thegn.log"));
        touch(&config.join("superzej/config.toml"));
        touch(&home.join(".superzej/activity.json"));

        let report = migrate_at(&state, &config, &home, true);
        assert_eq!(report.moved.len(), 3, "{:?}", report.warnings);
        assert!(state.join("thegn/thegn.db").exists());
        // The busy probe may legitimately checkpoint away a stale -wal; the
        // contract is only that no superzej-named db files remain.
        assert!(!state.join("thegn/superzej.db").exists());
        assert!(!state.join("thegn/superzej.db-wal").exists());
        assert!(state.join("thegn/logs/thegn.log").exists());
        assert!(config.join("thegn/config.toml").exists());
        assert!(home.join(".thegn/activity.json").exists());
        assert!(home.join(".thegn/.migrated-from-superzej").exists());
        assert!(!state.join("superzej").exists());
        assert!(!config.join("superzej").exists());
        assert!(!home.join(".superzej").exists());

        // Second run: nothing to do.
        let again = migrate_at(&state, &config, &home, true);
        assert!(!again.migrated_anything());
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    }

    #[test]
    fn new_paths_win_and_old_is_preserved() {
        let t = tempfile::tempdir().unwrap();
        let (state, config, home) = (
            t.path().join("st"),
            t.path().join("cf"),
            t.path().join("hm"),
        );
        touch(&config.join("superzej/config.toml"));
        touch(&config.join("thegn/config.toml"));
        std::fs::write(config.join("thegn/config.toml"), b"new").unwrap();

        let report = migrate_at(&state, &config, &home, true);
        assert!(!report.migrated_anything());
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert_eq!(
            std::fs::read(config.join("thegn/config.toml")).unwrap(),
            b"new",
            "existing new config untouched"
        );
        assert!(
            config.join("superzej/config.toml").exists(),
            "old preserved"
        );
    }

    #[test]
    fn skips_app_home_when_relocated() {
        let t = tempfile::tempdir().unwrap();
        let (state, config, home) = (
            t.path().join("st"),
            t.path().join("cf"),
            t.path().join("hm"),
        );
        touch(&home.join(".superzej/activity.json"));
        let report = migrate_at(&state, &config, &home, false);
        assert!(!report.migrated_anything());
        assert!(home.join(".superzej").exists(), "relocated home untouched");
    }

    #[test]
    fn busy_old_db_aborts_everything() {
        let t = tempfile::tempdir().unwrap();
        let (state, config, home) = (
            t.path().join("st"),
            t.path().join("cf"),
            t.path().join("hm"),
        );
        let old_db = state.join("superzej/superzej.db");
        std::fs::create_dir_all(old_db.parent().unwrap()).unwrap();
        let holder = rusqlite::Connection::open(&old_db).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
        touch(&config.join("superzej/config.toml"));

        let report = migrate_at(&state, &config, &home, true);
        assert!(!report.migrated_anything(), "{:?}", report.moved);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("running superzej")),
            "{:?}",
            report.warnings
        );
        assert!(state.join("superzej/superzej.db").exists());
        assert!(config.join("superzej/config.toml").exists());
    }

    #[test]
    fn db_path_prefixes_rewritten() {
        let t = tempfile::tempdir().unwrap();
        let (state, config, home) = (
            t.path().join("st"),
            t.path().join("cf"),
            t.path().join("hm"),
        );
        let old_db = state.join("superzej/superzej.db");
        std::fs::create_dir_all(old_db.parent().unwrap()).unwrap();
        {
            let conn = rusqlite::Connection::open(&old_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE worktrees (worktree TEXT PRIMARY KEY, repo_path TEXT);
                 CREATE TABLE workspaces (repo_path TEXT PRIMARY KEY);",
            )
            .unwrap();
            let wt = home.join(".superzej/worktrees/repo/wt-a");
            let repo = home.join("code/repo");
            conn.execute(
                "INSERT INTO worktrees (worktree, repo_path) VALUES (?1, ?2)",
                rusqlite::params![wt.to_string_lossy(), repo.to_string_lossy()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO workspaces (repo_path) VALUES (?1)",
                [repo.to_string_lossy()],
            )
            .unwrap();
        }
        touch(&home.join(".superzej/worktrees/repo/wt-a/marker"));

        let report = migrate_at(&state, &config, &home, true);
        assert!(report.migrated_anything());

        let conn = rusqlite::Connection::open(state.join("thegn/thegn.db")).unwrap();
        let wt: String = conn
            .query_row("SELECT worktree FROM worktrees", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            wt,
            home.join(".thegn/worktrees/repo/wt-a")
                .to_string_lossy()
                .to_string(),
            "app-home worktree path rewritten"
        );
        let repo: String = conn
            .query_row("SELECT repo_path FROM workspaces", [], |r| r.get(0))
            .unwrap();
        assert!(
            repo.contains("code/repo") && !repo.contains(".thegn"),
            "non-app-home repo path untouched: {repo}"
        );
    }

    #[test]
    fn repairs_moved_git_worktrees() {
        let t = tempfile::tempdir().unwrap();
        let (state, config, home) = (
            t.path().join("st"),
            t.path().join("cf"),
            t.path().join("hm"),
        );
        // A real repo with a linked worktree under the old app home.
        let repo = home.join("code/repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let out = util::git_cmd(cwd).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"], &repo);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
            &repo,
        );
        let wt = home.join(".superzej/worktrees/repo/wt-a");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git(
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt-a"],
            &repo,
        );

        let report = migrate_at(&state, &config, &home, true);
        assert!(report.migrated_anything());

        let moved_wt = home.join(".thegn/worktrees/repo/wt-a");
        let out = util::git_cmd(&moved_wt)
            .args(["status", "--short"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "moved worktree healthy after repair: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn kill_switch_and_env_resolution() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("hm");
        touch(&home.join(".superzej/activity.json"));
        touch(&home.join("st/superzej/superzej.db"));
        touch(&home.join("cf/superzej/config.toml"));
        let state = home.join("st").to_string_lossy().into_owned();
        let config = home.join("cf").to_string_lossy().into_owned();
        let home_s = home.to_string_lossy().into_owned();

        // Kill-switch on → untouched.
        {
            let _env = EnvGuard::set(&[
                ("HOME", &home_s),
                ("XDG_STATE_HOME", &state),
                ("XDG_CONFIG_HOME", &config),
                ("THEGN_NO_MIGRATE", "1"),
            ]);
            let report = run_startup_migration();
            assert!(!report.migrated_anything());
            assert!(home.join(".superzej").exists());
        }
        // THEGN_DIR set → app home skipped, XDG roots still migrate.
        {
            let thegn_dir = home.join("elsewhere").to_string_lossy().into_owned();
            let _env = EnvGuard::mutate_pairs(&[
                ("HOME", Some(home_s.as_str())),
                ("XDG_STATE_HOME", Some(state.as_str())),
                ("XDG_CONFIG_HOME", Some(config.as_str())),
                ("THEGN_NO_MIGRATE", None),
                ("THEGN_DIR", Some(thegn_dir.as_str())),
            ]);
            let report = run_startup_migration();
            assert_eq!(report.moved.len(), 2, "{:?}", report.warnings);
            assert!(home.join(".superzej").exists(), "app home skipped");
            assert!(home.join("st/thegn/thegn.db").exists());
            assert!(home.join("cf/thegn/config.toml").exists());
        }
    }
}
