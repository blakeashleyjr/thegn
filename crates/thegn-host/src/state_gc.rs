//! Bounded retention for state files that only grow (pty-shutdown notes,
//! stale database copies). Runs only inside the database maintenance pass
//! (`db_maintain`), on the disk worker thread, never on the event loop.
//!
//! Why each store is safe to prune:
//! - pty-shutdown notes are diagnostic and never loaded as signal targets
//!   (`daemon::pty_diagnostics` module doc).
//! - `thegn.db.bak-*` are copies superseded by the live database. The database
//!   is a cache and resurrection layer; git and the forge are the source of
//!   truth. The newest copy is always kept.
//!
//! Nothing else in the state dir is touched: `recordings/` is user work,
//! `locks/` files must never be unlinked (flock race, see
//! `worktree_admission.rs`), and `live-backup-*` belongs to `scripts/live.py`.

use std::path::{Path, PathBuf};

/// What one pass removed, for the maintenance debug line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GcReport {
    pub pty_notes: usize,
    pub db_copies: usize,
}

/// Most files a single pass may delete, across every store.
pub(crate) const MAX_UNLINKS_PER_PASS: usize = 256;

/// pty-shutdown notes: delete if older than this, or beyond the newest PTY_NOTES_KEEP.
pub(crate) const PTY_NOTES_MAX_AGE_SECS: i64 = 14 * 86_400;
pub(crate) const PTY_NOTES_KEEP: usize = 500;
/// Stale `thegn.db.bak-*` copies: delete if older than this; always keep the newest.
pub(crate) const DB_COPY_MAX_AGE_SECS: i64 = 30 * 86_400;
/// Most directory entries one pass will read per store.
pub(crate) const MAX_ENTRIES_READ: usize = 4096;

/// Exact prefix of a stale database copy. Never widen this to a glob: it must
/// not reach `thegn.db`, `thegn.db-wal`, `thegn.db-shm` or `thegn.db.bak`.
const DB_COPY_PREFIX: &str = "thegn.db.bak-";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    mtime: i64,
}

/// Retention rule for one store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Policy {
    /// Older than this (strictly) is a victim, unless protected by `keep_newest`.
    max_age: i64,
    /// The newest N are never deleted, whatever their age (a floor).
    keep_newest: usize,
    /// Anything ranked beyond the newest N is a victim, whatever its age (a cap).
    max_count: usize,
}

const PTY_NOTES_POLICY: Policy = Policy {
    max_age: PTY_NOTES_MAX_AGE_SECS,
    keep_newest: 0,
    max_count: PTY_NOTES_KEEP,
};

const DB_COPIES_POLICY: Policy = Policy {
    max_age: DB_COPY_MAX_AGE_SECS,
    keep_newest: 1,
    max_count: usize::MAX,
};

/// Pure: which names to delete (oldest first), capped at `max_unlinks`.
///
/// Entries are ranked newest first by `(mtime, name)`, so equal mtimes order
/// deterministically by name. Rank `< keep_newest` is never a victim; rank
/// `>= max_count` always is; otherwise an entry is a victim when strictly older
/// than `max_age`.
fn select_victims(entries: &[Entry], now: i64, policy: Policy, max_unlinks: usize) -> Vec<String> {
    let mut ranked: Vec<&Entry> = entries.iter().collect();
    ranked.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.name.cmp(&a.name)));
    let mut victims: Vec<&Entry> = ranked
        .into_iter()
        .enumerate()
        .filter(|(rank, entry)| {
            *rank >= policy.keep_newest
                && (*rank >= policy.max_count || now.saturating_sub(entry.mtime) > policy.max_age)
        })
        .map(|(_, entry)| entry)
        .collect();
    // Ranked newest first; delete oldest first.
    victims.reverse();
    victims.truncate(max_unlinks);
    victims
        .into_iter()
        .map(|entry| entry.name.clone())
        .collect()
}

/// Read at most MAX_ENTRIES_READ entries of `dir` that match `accept`, are
/// regular files (symlink_metadata; never follow), and are owned by us.
///
/// The flag is true when the read stopped at the cap, so the listing may be a
/// subset of the directory.
fn list_files(dir: &Path, accept: impl Fn(&str) -> bool) -> (Vec<Entry>, bool) {
    let mut out = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(target: "thegn::db", %error, dir = %dir.display(), "state gc: read_dir failed");
            }
            return (out, false);
        }
    };
    let mut seen = 0usize;
    let mut truncated = false;
    for dirent in read {
        if seen >= MAX_ENTRIES_READ {
            truncated = true;
            break;
        }
        seen += 1;
        let Ok(dirent) = dirent else { continue };
        let Some(name) = dirent.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !accept(&name) {
            continue;
        }
        // `symlink_metadata` never follows: a symlink is not a regular file
        // here, so neither it nor its target is ever a candidate.
        let Ok(meta) = std::fs::symlink_metadata(dir.join(&name)) else {
            continue;
        };
        if !meta.file_type().is_file() || !thegn_core::fsperm::owned_by_current_user(&meta) {
            continue;
        }
        let Some(mtime) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
        else {
            // Unknown age: leave it alone.
            continue;
        };
        out.push(Entry { name, mtime });
    }
    (out, truncated)
}

fn is_pty_note(name: &str) -> bool {
    name.len() > ".json".len() && name.ends_with(".json")
}

fn is_db_copy(name: &str) -> bool {
    name.len() > DB_COPY_PREFIX.len()
        && name.starts_with(DB_COPY_PREFIX)
        // A copy that was ever opened may carry SQLite sidecars; those are
        // never selected on their own.
        && !name.ends_with("-wal")
        && !name.ends_with("-shm")
        && !name.ends_with("-journal")
}

fn prune_dir(
    dir: &Path,
    accept: impl Fn(&str) -> bool,
    now: i64,
    policy: Policy,
    budget: usize,
) -> usize {
    if budget == 0 {
        return 0;
    }
    let (entries, truncated) = list_files(dir, accept);
    // A capped listing is a subset: its "newest N" may not be the directory's,
    // so the count cap only applies to a complete listing. Age still applies.
    let policy = if truncated {
        Policy {
            max_count: usize::MAX,
            ..policy
        }
    } else {
        policy
    };
    let mut removed = 0;
    for name in select_victims(&entries, now, policy, budget) {
        let path = dir.join(&name);
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::debug!(target: "thegn::db", %error, path = %path.display(), "state gc: remove failed");
            }
        }
    }
    removed
}

fn state_dir() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn")
}

/// pty-shutdown notes (`*.json` in `pty_diagnostics::root_dir()`).
fn prune_pty_notes(now: i64, budget: usize) -> usize {
    prune_pty_notes_in(&crate::daemon::pty_diagnostics::root_dir(), now, budget)
}

fn prune_pty_notes_in(dir: &Path, now: i64, budget: usize) -> usize {
    prune_dir(dir, is_pty_note, now, PTY_NOTES_POLICY, budget)
}

/// `thegn.db.bak-*` in the state dir; the newest copy is always kept.
fn prune_db_copies(now: i64, budget: usize) -> usize {
    prune_db_copies_in(&state_dir(), now, budget)
}

fn prune_db_copies_in(dir: &Path, now: i64, budget: usize) -> usize {
    prune_dir(dir, is_db_copy, now, DB_COPIES_POLICY, budget)
}

/// One bounded pass over the thegn state directory. `now` is Unix seconds.
/// The `MAX_UNLINKS_PER_PASS` budget is shared, pty notes first.
pub(crate) fn run_pass(now: i64) -> GcReport {
    let pty_notes = prune_pty_notes(now, MAX_UNLINKS_PER_PASS);
    let db_copies = prune_db_copies(now, MAX_UNLINKS_PER_PASS.saturating_sub(pty_notes));
    GcReport {
        pty_notes,
        db_copies,
    }
}

/// [`run_pass`] over explicit directories (tests: the pty notes root is a
/// process-private temp dir under `cfg(test)`, not the isolated state home).
#[cfg(test)]
fn run_pass_in(pty_dir: &Path, state: &Path, now: i64) -> GcReport {
    let pty_notes = prune_pty_notes_in(pty_dir, now, MAX_UNLINKS_PER_PASS);
    let db_copies = prune_db_copies_in(state, now, MAX_UNLINKS_PER_PASS.saturating_sub(pty_notes));
    GcReport {
        pty_notes,
        db_copies,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    const DAY: i64 = 86_400;
    const NOW: i64 = 2_000_000_000;

    fn e(name: &str, mtime: i64) -> Entry {
        Entry {
            name: name.to_owned(),
            mtime,
        }
    }

    fn policy(max_age: i64, keep_newest: usize, max_count: usize) -> Policy {
        Policy {
            max_age,
            keep_newest,
            max_count,
        }
    }

    fn touch(path: &Path, mtime: i64) {
        std::fs::write(path, b"x").expect("write fixture");
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open fixture");
        file.set_modified(UNIX_EPOCH + Duration::from_secs(u64::try_from(mtime).expect("mtime")))
            .expect("set mtime");
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|d| d.expect("dirent").file_name().into_string().expect("utf8"))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn select_victims_age_only() {
        let entries = [
            e("a", NOW - 20 * DAY),
            e("b", NOW - DAY),
            e("c", NOW - 15 * DAY),
        ];
        assert_eq!(
            select_victims(&entries, NOW, policy(14 * DAY, 0, usize::MAX), 256),
            vec!["a".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn select_victims_exactly_max_age_is_kept() {
        let entries = [e("a", NOW - 14 * DAY)];
        assert!(select_victims(&entries, NOW, policy(14 * DAY, 0, usize::MAX), 256).is_empty());
    }

    #[test]
    fn select_victims_count_only() {
        let entries = [
            e("a", NOW - 3),
            e("b", NOW - 1),
            e("c", NOW - 2),
            e("d", NOW - 4),
        ];
        assert_eq!(
            select_victims(&entries, NOW, policy(i64::MAX, 0, 2), 256),
            vec!["d".to_owned(), "a".to_owned()]
        );
    }

    #[test]
    fn select_victims_age_and_count() {
        // Newest first: n1 n2 old1 old2; cap 3 takes old2; age takes old1 too.
        let entries = [
            e("n1", NOW - 1),
            e("n2", NOW - 2),
            e("old1", NOW - 20 * DAY),
            e("old2", NOW - 21 * DAY),
        ];
        assert_eq!(
            select_victims(&entries, NOW, policy(14 * DAY, 0, 3), 256),
            vec!["old2".to_owned(), "old1".to_owned()]
        );
        // Only the cap applies when nothing is old.
        let fresh = [
            e("a", NOW - 1),
            e("b", NOW - 2),
            e("c", NOW - 3),
            e("d", NOW - 4),
        ];
        assert_eq!(
            select_victims(&fresh, NOW, policy(14 * DAY, 0, 3), 256),
            vec!["d".to_owned()]
        );
    }

    #[test]
    fn select_victims_keep_newest_floor_beats_age() {
        let entries = [e("a", NOW - 40 * DAY), e("b", NOW - 35 * DAY)];
        assert_eq!(
            select_victims(&entries, NOW, policy(30 * DAY, 1, usize::MAX), 256),
            vec!["a".to_owned()]
        );
        let lone = [e("only", NOW - 400 * DAY)];
        assert!(select_victims(&lone, NOW, policy(30 * DAY, 1, usize::MAX), 256).is_empty());
        // The floor also beats the count cap.
        assert!(select_victims(&lone, NOW, policy(30 * DAY, 1, 0), 256).is_empty());
    }

    #[test]
    fn select_victims_respects_the_unlink_cap_oldest_first() {
        let entries: Vec<Entry> = (0..10)
            .map(|i| e(&format!("n{i}"), NOW - 100 * DAY - i))
            .collect();
        assert_eq!(
            select_victims(&entries, NOW, policy(14 * DAY, 0, usize::MAX), 3),
            vec!["n9".to_owned(), "n8".to_owned(), "n7".to_owned()]
        );
        assert!(select_victims(&entries, NOW, policy(14 * DAY, 0, usize::MAX), 0).is_empty());
    }

    #[test]
    fn select_victims_ties_order_by_name_deterministically() {
        let forward = [e("b", NOW), e("a", NOW), e("c", NOW)];
        let backward = [e("c", NOW), e("a", NOW), e("b", NOW)];
        let expect = vec!["a".to_owned()];
        assert_eq!(
            select_victims(&forward, NOW, policy(i64::MAX, 0, 2), 256),
            expect
        );
        assert_eq!(
            select_victims(&backward, NOW, policy(i64::MAX, 0, 2), 256),
            expect
        );
        assert_eq!(
            select_victims(&forward, NOW, policy(i64::MAX, 0, 0), 256),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn db_copy_prefix_is_exact() {
        for name in [
            "thegn.db",
            "thegn.db-wal",
            "thegn.db-shm",
            "thegn.db.bak",
            "thegn.db.bak-",
            "thegn.db.bak-1-wal",
            "thegn.db.bak-1-shm",
            "xthegn.db.bak-1",
            "thegn.db.backup-1",
        ] {
            assert!(!is_db_copy(name), "{name}");
        }
        assert!(is_db_copy("thegn.db.bak-1731900000"));
        assert!(is_pty_note("d-s.json"));
        assert!(!is_pty_note(".json"));
        assert!(!is_pty_note("d-s.json.tmp"));
    }

    #[test]
    fn list_files_skips_dirs_and_non_matching_names() {
        let state = crate::testenv::IsolatedState::new();
        let dir = state.root().join("notes");
        std::fs::create_dir_all(&dir).expect("notes dir");
        std::fs::create_dir(dir.join("sub.json")).expect("dir named like a note");
        touch(&dir.join("keep.txt"), NOW - 100 * DAY);
        touch(&dir.join("real.json"), NOW - 100 * DAY);

        let (entries, truncated) = list_files(&dir, is_pty_note);
        assert!(!truncated);
        assert_eq!(entries, vec![e("real.json", NOW - 100 * DAY)]);

        // A full prune of the dir removes only the real, matching file.
        assert_eq!(prune_pty_notes_in(&dir, NOW, 256), 1);
        assert!(dir.join("sub.json").is_dir());
        assert!(dir.join("keep.txt").exists());
        assert!(!dir.join("real.json").exists());
    }

    #[test]
    fn list_files_of_a_missing_dir_is_empty() {
        let state = crate::testenv::IsolatedState::new();
        assert_eq!(
            list_files(&state.root().join("absent"), is_pty_note),
            (Vec::new(), false)
        );
    }

    // Test-only: creating a symlink is platform API; the code under test is not.
    #[cfg(unix)]
    #[test]
    fn list_files_skips_symlinks_and_never_touches_their_targets() {
        let state = crate::testenv::IsolatedState::new();
        let dir = state.root().join("notes");
        let outside = state.root().join("outside");
        std::fs::create_dir_all(&dir).expect("notes dir");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let target = outside.join("real.json");
        touch(&target, NOW - 100 * DAY);
        std::os::unix::fs::symlink(&target, dir.join("x.json")).expect("symlink");

        let (entries, _) = list_files(&dir, is_pty_note);
        assert!(entries.is_empty(), "{entries:?}");
        assert_eq!(prune_pty_notes_in(&dir, NOW, 256), 0);
        assert!(target.exists(), "symlink target outside must survive");
        assert!(
            std::fs::symlink_metadata(dir.join("x.json")).is_ok(),
            "symlink itself must survive"
        );
    }

    #[test]
    fn list_files_stops_at_max_entries_read() {
        let state = crate::testenv::IsolatedState::new();
        let dir = state.root().join("many");
        std::fs::create_dir_all(&dir).expect("dir");
        for i in 0..MAX_ENTRIES_READ + 5 {
            std::fs::write(dir.join(format!("{i}.json")), b"x").expect("write");
        }
        let (entries, truncated) = list_files(&dir, is_pty_note);
        assert_eq!(entries.len(), MAX_ENTRIES_READ);
        assert!(truncated);
    }

    #[test]
    fn truncated_listing_applies_age_but_not_the_count_cap() {
        let state = crate::testenv::IsolatedState::new();
        let dir = state.root().join("many-fresh");
        std::fs::create_dir_all(&dir).expect("dir");
        for i in 0..MAX_ENTRIES_READ + 5 {
            touch(&dir.join(format!("{i}.json")), NOW - 1);
        }
        // Every note is fresh and the listing is partial: nothing is removed,
        // even though the directory holds far more than PTY_NOTES_KEEP.
        assert_eq!(prune_pty_notes_in(&dir, NOW, 256), 0);
    }

    #[test]
    fn run_pass_end_to_end_in_isolated_state() {
        let state = crate::testenv::IsolatedState::new();
        let thegn = state.state_home().join("thegn");
        let pty = thegn.join("pty-shutdown");
        std::fs::create_dir_all(&pty).expect("pty dir");

        // 600 notes: 300 fresh (1..=300 s old), 300 older than 14 days.
        for i in 0..300i64 {
            touch(&pty.join(format!("fresh-{i:03}.json")), NOW - 1 - i);
            touch(&pty.join(format!("old-{i:03}.json")), NOW - 15 * DAY - i);
        }
        touch(&thegn.join("thegn.db.bak-40"), NOW - 40 * DAY);
        touch(&thegn.join("thegn.db.bak-35"), NOW - 35 * DAY);
        touch(&thegn.join("thegn.db.bak-10"), NOW - 10 * DAY);
        for keep in ["thegn.db", "thegn.db-wal", "thegn.db-shm", "thegn.db.bak"] {
            touch(&thegn.join(keep), NOW - 400 * DAY);
        }

        // Pass 1: the shared budget goes to pty notes first, oldest first.
        let first = run_pass_in(&pty, &thegn, NOW);
        assert_eq!(
            first,
            GcReport {
                pty_notes: 256,
                db_copies: 0
            }
        );
        assert_eq!(names(&pty).len(), 344);
        assert!(!pty.join("old-299.json").exists(), "oldest goes first");

        // Pass 2: the remaining 44 old notes, then the stale copies.
        let second = run_pass_in(&pty, &thegn, NOW);
        assert_eq!(
            second,
            GcReport {
                pty_notes: 44,
                db_copies: 2
            }
        );
        let remaining = names(&pty);
        assert_eq!(remaining.len(), 300);
        assert!(remaining.iter().all(|n| n.starts_with("fresh-")));

        // Steady state: nothing more to do.
        assert_eq!(run_pass_in(&pty, &thegn, NOW), GcReport::default());

        assert!(!thegn.join("thegn.db.bak-40").exists());
        assert!(!thegn.join("thegn.db.bak-35").exists());
        assert!(thegn.join("thegn.db.bak-10").exists());
        for keep in ["thegn.db", "thegn.db-wal", "thegn.db-shm", "thegn.db.bak"] {
            assert!(thegn.join(keep).exists(), "{keep} must never be matched");
        }
    }

    #[test]
    fn run_pass_count_cap_keeps_the_newest_500() {
        let state = crate::testenv::IsolatedState::new();
        let pty = state.state_home().join("thegn/pty-shutdown");
        std::fs::create_dir_all(&pty).expect("pty dir");
        for i in 0..600i64 {
            touch(&pty.join(format!("n-{i:03}.json")), NOW - 1 - i);
        }
        let report = run_pass_in(&pty, &state.state_home().join("thegn"), NOW);
        assert_eq!(report.pty_notes, 100);
        let remaining = names(&pty);
        assert_eq!(remaining.len(), PTY_NOTES_KEEP);
        assert_eq!(remaining.last().map(String::as_str), Some("n-499.json"));
    }

    #[test]
    fn run_pass_uses_the_isolated_state_dir_for_db_copies() {
        let state = crate::testenv::IsolatedState::new();
        let now = thegn_core::util::now();
        let thegn = state.state_home().join("thegn");
        std::fs::create_dir_all(&thegn).expect("state dir");
        touch(&thegn.join("thegn.db.bak-a"), now - 40 * DAY);
        touch(&thegn.join("thegn.db.bak-b"), now - DAY);
        touch(&thegn.join("thegn.db"), now - 400 * DAY);
        let report = run_pass(now);
        assert_eq!(report.db_copies, 1);
        assert!(!thegn.join("thegn.db.bak-a").exists());
        assert!(thegn.join("thegn.db.bak-b").exists());
        assert!(thegn.join("thegn.db").exists());
    }
}
