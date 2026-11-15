use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use thegn_core::remote::GitLoc;

pub const META_COUNT_CAP: u32 = 999;
const META_MEMO_CAP: usize = 256;
/// The most bytes of `logs/refs/stash` read for the sidebar stash count; a
/// larger reflog reports the count cap instead of being read in full.
const STASH_REFLOG_READ_CAP: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BranchMeta {
    pub ahead_base: Option<u32>,
    pub behind_base: Option<u32>,
    pub dirty_files: Option<u32>,
    pub last_commit_unix: Option<i64>,
    pub last_subject: Option<String>,
    /// Sanitised author name from the last commit, when available.
    #[serde(default)]
    pub last_author: Option<String>,
    /// Repo-wide stash entry count (capped), read from the stash reflog.
    /// `None` = unknown (remote location or a failed read), never `Some(0)`
    /// for a failure.
    #[serde(default)]
    pub stash: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitMeta {
    ahead_base: Option<u32>,
    behind_base: Option<u32>,
    last_commit_unix: Option<i64>,
    last_subject: Option<String>,
    last_author: Option<String>,
}

impl CommitMeta {
    fn with_dirty(self, dirty_files: Option<u32>) -> BranchMeta {
        BranchMeta {
            ahead_base: self.ahead_base,
            behind_base: self.behind_base,
            dirty_files,
            last_commit_unix: self.last_commit_unix,
            last_subject: self.last_subject,
            last_author: self.last_author,
            stash: None,
        }
    }
}

type MetaKey = (PathBuf, String, String);
type MetaMemo = Mutex<HashMap<MetaKey, CommitMeta>>;
static META_MEMO: LazyLock<MetaMemo> = LazyLock::new(|| Mutex::new(HashMap::new()));
impl BranchMeta {
    /// Attach the stash count. Applied after the commit memo, because the
    /// stash changes without HEAD or the base moving.
    fn with_stash(mut self, stash: Option<u32>) -> Self {
        self.stash = stash;
        self
    }
}

/// Count stash entries from `<common-dir>/logs/refs/stash` without spawning
/// git: one entry per non-empty line (a final line without a trailing newline
/// counts; a CRLF line counts once). At most [`STASH_REFLOG_READ_CAP`] bytes
/// are read; a larger reflog reports `cap`. A missing reflog is an empty stash
/// (`Some(0)`); any other IO error is unknown (`None`).
fn stash_from_reflog(common_dir: &Path, cap: u32) -> Option<u32> {
    let path = common_dir.join("logs").join("refs").join("stash");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    let mut bytes = Vec::new();
    if file
        .take(STASH_REFLOG_READ_CAP + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return None;
    }
    if bytes.len() as u64 > STASH_REFLOG_READ_CAP {
        return Some(cap);
    }
    let count = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| line.iter().any(|byte| *byte != b'\r'))
        .count();
    Some(count.min(cap as usize) as u32)
}
static META_COMPUTES: AtomicU64 = AtomicU64::new(0);

pub fn branch_meta_compute_count() -> u64 {
    META_COMPUTES.load(Ordering::Relaxed)
}

fn memoized(key: MetaKey, compute: impl FnOnce() -> Result<CommitMeta>) -> Result<CommitMeta> {
    if let Some(meta) = META_MEMO
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(meta);
    }
    META_COMPUTES.fetch_add(1, Ordering::Relaxed);
    let meta = compute()?;
    let mut memo = META_MEMO.lock().unwrap_or_else(|error| error.into_inner());
    if memo.len() >= META_MEMO_CAP {
        memo.clear();
    }
    memo.insert(key, meta.clone());
    Ok(meta)
}

fn sanitized_text(message: &[u8], cap: usize) -> String {
    let first_line = message
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let mut subject = String::new();
    for character in String::from_utf8_lossy(first_line).chars() {
        if character.is_control() {
            continue;
        }
        if subject.len() + character.len_utf8() > cap {
            break;
        }
        subject.push(character);
    }
    subject
}

fn sanitized_subject(message: &[u8]) -> String {
    sanitized_text(message, 200)
}

fn sanitized_author(message: &[u8]) -> String {
    sanitized_text(message, 64)
}

fn split_at_nul(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|byte| *byte == 0)?;
    Some((&bytes[..index], &bytes[index + 1..]))
}

pub(crate) fn parse_log_record(output: &[u8]) -> Result<(i64, String, String)> {
    let (timestamp, rest) =
        split_at_nul(output).context("git log metadata missing NUL separator")?;
    let (author, subject) =
        split_at_nul(rest).context("git log metadata missing author separator")?;
    let timestamp = std::str::from_utf8(timestamp)
        .context("git log metadata timestamp is not UTF-8")?
        .trim()
        .parse::<i64>()
        .context("git log metadata timestamp is not an integer")?;
    Ok((
        timestamp,
        sanitized_author(author),
        sanitized_subject(subject),
    ))
}

pub(crate) fn parse_ahead_behind(output: &str, cap: u32) -> Option<(u32, u32)> {
    let mut fields = output.split_whitespace();
    let behind = match fields.next()?.parse::<u64>() {
        Ok(value) => value,
        Err(_) => return None,
    };
    let ahead = match fields.next()?.parse::<u64>() {
        Ok(value) => value,
        Err(_) => return None,
    };
    Some((
        ahead.min(u64::from(cap)) as u32,
        behind.min(u64::from(cap)) as u32,
    ))
}

pub(crate) fn dirty_count(status_len: usize, cap: u32) -> Option<u32> {
    Some(status_len.min(cap as usize) as u32)
}

pub(crate) fn from_parts(
    ahead_behind: Option<(u32, u32)>,
    log: (i64, String, String),
    dirty_files: Option<u32>,
) -> BranchMeta {
    BranchMeta {
        ahead_base: ahead_behind.map(|(ahead, _)| ahead),
        behind_base: ahead_behind.map(|(_, behind)| behind),
        dirty_files,
        last_commit_unix: Some(log.0),
        last_author: Some(log.1),
        last_subject: Some(log.2),
        stash: None,
    }
}

pub(crate) fn gix(loc: &GitLoc, base: Option<&str>) -> Result<BranchMeta> {
    let repo = super::native_diff::open(loc.path())?;
    let head = repo
        .rev_parse_single("HEAD")
        .context("gix rev-parse HEAD")?;
    let base = base
        .map(|base| {
            repo.rev_parse_single(base)
                .with_context(|| format!("gix rev-parse {base}"))
        })
        .transpose()?;
    let head_hex = head.to_string();
    let base_hex = base.as_ref().map(ToString::to_string).unwrap_or_default();
    let detached = repo.head_name().context("gix head_name")?.is_none();
    let commit_meta = memoized((PathBuf::from(loc.path()), head_hex, base_hex), || {
        let commit = repo
            .find_commit(head.detach())
            .context("gix find HEAD commit")?;
        let (ahead_base, behind_base) = if detached {
            (None, None)
        } else if let Some(base) = base {
            (
                Some(count_excluding(
                    &repo,
                    head.detach(),
                    base.detach(),
                    META_COUNT_CAP,
                )?),
                Some(count_excluding(
                    &repo,
                    base.detach(),
                    head.detach(),
                    META_COUNT_CAP,
                )?),
            )
        } else {
            (None, None)
        };
        let last_commit_unix = commit
            .committer()
            .context("gix commit committer")?
            .seconds();
        let author = commit.author().context("gix commit author")?;
        Ok(CommitMeta {
            ahead_base,
            behind_base,
            last_commit_unix: Some(last_commit_unix),
            last_author: Some(sanitized_author(author.name.as_ref())),
            last_subject: Some(sanitized_subject(commit.message_raw_sloppy())),
        })
    })?;
    let stash = stash_from_reflog(repo.common_dir(), META_COUNT_CAP);
    Ok(commit_meta.with_dirty(None).with_stash(stash))
}

fn count_excluding(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    hidden: gix::ObjectId,
    cap: u32,
) -> Result<u32> {
    let walk = repo
        .rev_walk([tip])
        .with_hidden([hidden])
        .all()
        .context("gix rev_walk")?;
    let mut count = 0u32;
    for info in walk {
        info.map_err(|error| anyhow::anyhow!("gix rev_walk item: {error}"))?;
        count = count.saturating_add(1);
        if count >= cap {
            return Ok(cap);
        }
    }
    Ok(count)
}

pub(crate) fn cli(loc: &GitLoc, base: Option<&str>, detached: bool) -> Result<BranchMeta> {
    let mut rev_parse_args = vec!["rev-parse", "HEAD"];
    if let Some(base) = base {
        rev_parse_args.push(base);
    }
    let (exit, output) = super::run_status(loc, &rev_parse_args)?;
    anyhow::ensure!(exit == 0, "git rev-parse for branch metadata failed");
    let mut ids = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let head = ids
        .next()
        .context("git rev-parse HEAD returned no object")?;
    let base_id = base.map(|_| ids.next().unwrap_or_default());
    let key = (
        PathBuf::from(loc.path()),
        head.to_string(),
        base_id.unwrap_or_default().to_string(),
    );
    let commit_meta = memoized(key, || {
        let ahead_behind = if detached {
            None
        } else if let Some(base) = base {
            let (exit, output) = super::run_status(
                loc,
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{base}...HEAD"),
                ],
            )?;
            (exit == 0)
                .then(|| parse_ahead_behind(&output, META_COUNT_CAP))
                .flatten()
        } else {
            None
        };
        let (exit, output) =
            super::run_status(loc, &["log", "-1", "--format=%ct%x00%an%x00%s", "HEAD"])?;
        anyhow::ensure!(exit == 0, "git log for branch metadata failed");
        let log = parse_log_record(output.as_bytes())?;
        Ok(CommitMeta {
            ahead_base: ahead_behind.map(|(ahead, _)| ahead),
            behind_base: ahead_behind.map(|(_, behind)| behind),
            last_commit_unix: Some(log.0),
            last_author: Some(log.1),
            last_subject: Some(log.2),
        })
    })?;
    let stash = if loc.is_remote() {
        None
    } else {
        super::native_diff::open(loc.path())
            .ok()
            .and_then(|repo| stash_from_reflog(repo.common_dir(), META_COUNT_CAP))
    };
    Ok(commit_meta.with_dirty(None).with_stash(stash))
}

pub(crate) fn with_dirty(
    meta: Result<BranchMeta>,
    dirty_files: Result<Option<u32>>,
) -> Result<BranchMeta> {
    let mut meta = meta?;
    meta.dirty_files = dirty_files?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::super::{CliGit, GitBackend, GixGit};
    use super::*;

    #[test]
    fn meta_caps_at_999() {
        for (cap, expected) in [(3, (3, 3)), (META_COUNT_CAP, (999, 999))] {
            assert_eq!(parse_ahead_behind("1000\t1001", cap), Some(expected));
        }
    }

    #[test]
    fn last_commit_subject_and_time_are_first_line_sanitised() {
        assert_eq!(
            parse_log_record(b"1700000000\0Ada\x01 Lovelace\0first\x01 line\nsecond\n").unwrap(),
            (
                1_700_000_000,
                "Ada Lovelace".to_string(),
                "first line".to_string()
            )
        );
        assert_eq!(
            parse_log_record(b"1700000000\0\0\n").unwrap(),
            (1_700_000_000, "".to_string(), "".to_string())
        );
    }

    #[test]
    fn bridged_batch_parsers() {
        assert_eq!(parse_ahead_behind("2\t5", META_COUNT_CAP), Some((5, 2)));
        assert_eq!(
            parse_log_record(b"42\0author\0subject\x1f\n").unwrap(),
            (42, "author".to_string(), "subject".to_string())
        );
    }

    #[test]
    fn author_is_control_free_and_capped_in_the_pure_parser() {
        let author = format!("{}\x01tail", "a".repeat(64));
        let output = format!("42\0{author}\0subject");
        let parsed = parse_log_record(output.as_bytes()).unwrap();
        assert_eq!(parsed.1, "a".repeat(64));
        assert!(!parsed.1.chars().any(char::is_control));
    }

    fn stash_fixture(contents: Option<&[u8]>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("logs").join("refs");
        std::fs::create_dir_all(&refs).unwrap();
        if let Some(contents) = contents {
            std::fs::write(refs.join("stash"), contents).unwrap();
        }
        dir
    }

    const STASH_LINE: &str = "0000000000000000000000000000000000000000 \
1111111111111111111111111111111111111111 A U Thor <a@example.com> 1700000000 +0000\tWIP on main: x";

    #[test]
    fn stash_reflog_counts_lines() {
        let one = format!("{STASH_LINE}\n");
        let three = format!("{STASH_LINE}\n{STASH_LINE}\n{STASH_LINE}\n");
        let three_no_trailing = format!("{STASH_LINE}\n{STASH_LINE}\n{STASH_LINE}");
        let crlf = format!("{STASH_LINE}\r\n{STASH_LINE}\r\n\r\n");
        let cases: [(&[u8], Option<u32>); 6] = [
            (&b""[..], Some(0)),
            (&b"\n\n"[..], Some(0)),
            (one.as_bytes(), Some(1)),
            (three.as_bytes(), Some(3)),
            (three_no_trailing.as_bytes(), Some(3)),
            (crlf.as_bytes(), Some(2)),
        ];
        for (contents, expected) in cases {
            let dir = stash_fixture(Some(contents));
            assert_eq!(
                stash_from_reflog(dir.path(), META_COUNT_CAP),
                expected,
                "{:?}",
                String::from_utf8_lossy(contents)
            );
        }
    }

    #[test]
    fn stash_reflog_caps_count_and_size() {
        let three = format!("{STASH_LINE}\n{STASH_LINE}\n{STASH_LINE}\n");
        let dir = stash_fixture(Some(three.as_bytes()));
        assert_eq!(stash_from_reflog(dir.path(), 2), Some(2));

        let oversized = vec![b'x'; STASH_REFLOG_READ_CAP as usize + 1];
        let dir = stash_fixture(Some(oversized.as_slice()));
        assert_eq!(stash_from_reflog(dir.path(), 7), Some(7));

        let exact = vec![b'x'; STASH_REFLOG_READ_CAP as usize];
        let dir = stash_fixture(Some(exact.as_slice()));
        assert_eq!(stash_from_reflog(dir.path(), 7), Some(1));
    }

    #[test]
    fn stash_reflog_missing_is_zero_and_unreadable_is_unknown() {
        let dir = stash_fixture(None);
        assert_eq!(stash_from_reflog(dir.path(), META_COUNT_CAP), Some(0));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(stash_from_reflog(empty.path(), META_COUNT_CAP), Some(0));

        // A plain (non-symlink) directory where the reflog file belongs is an
        // IO error other than NotFound: unknown, never a false zero.
        let dir = stash_fixture(None);
        std::fs::create_dir_all(dir.path().join("logs").join("refs").join("stash")).unwrap();
        assert_eq!(stash_from_reflog(dir.path(), META_COUNT_CAP), None);
    }

    #[test]
    fn stash_defaults_to_unknown_for_older_serialised_meta() {
        let meta: BranchMeta = serde_json::from_str(r#"{"ahead_base":1}"#).unwrap();
        assert_eq!(meta.stash, None);
        assert_eq!(
            from_parts(None, (1, String::new(), String::new()), None).stash,
            None
        );
    }

    #[test]
    fn stash_count_reaches_meta_on_both_backends_after_memo() {
        let repo = super::super::testutil::TestRepo::new("branch-meta-stash");
        repo.commit_file("tracked", "one", "subject");
        let loc = repo.loc();
        assert_eq!(gix(&loc, None).unwrap().stash, Some(0));
        assert_eq!(cli(&loc, None, false).unwrap().stash, Some(0));

        std::fs::write(repo.dir.join("tracked"), "two").unwrap();
        super::super::testutil::git_in(&repo.dir, &["stash", "push", "-q"]);
        // HEAD did not move, so the commit memo hits; the stash is still fresh.
        assert_eq!(gix(&loc, None).unwrap().stash, Some(1));
        assert_eq!(cli(&loc, None, false).unwrap().stash, Some(1));
        let gix_meta = GixGit::new().glyph_reads_with_submodules(&loc, false).meta;
        assert_eq!(gix_meta.unwrap().stash, Some(1));
        let cli_meta = CliGit.glyph_reads_with_submodules(&loc, false).meta;
        assert_eq!(cli_meta.unwrap().stash, Some(1));
    }

    #[test]
    fn dirty_count_counts_untracked_and_modified_and_caps() {
        assert_eq!(dirty_count(0, META_COUNT_CAP), Some(0));
        assert_eq!(dirty_count(2, META_COUNT_CAP), Some(2));
        assert_eq!(dirty_count(1_000, META_COUNT_CAP), Some(META_COUNT_CAP));

        let repo = super::super::testutil::TestRepo::new("branch-meta-dirty");
        repo.commit_file("a", "1", "a");
        repo.commit_file("b", "1", "b");
        std::fs::write(repo.dir.join("a"), "2").unwrap();
        std::fs::write(repo.dir.join("b"), "2").unwrap();
        super::super::testutil::git_in(&repo.dir, &["add", "b"]);
        std::fs::write(repo.dir.join("b"), "3").unwrap();
        std::fs::write(repo.dir.join("u"), "x").unwrap();
        assert_eq!(
            GixGit::new()
                .dirty_and_count(&repo.loc(), META_COUNT_CAP)
                .unwrap(),
            (true, Some(3))
        );
        assert_eq!(
            GixGit::new().dirty_and_count(&repo.loc(), 2).unwrap(),
            (true, Some(2))
        );
        assert_eq!(
            CliGit.dirty_and_count(&repo.loc(), META_COUNT_CAP).unwrap(),
            (true, Some(3))
        );
    }

    #[test]
    fn ahead_behind_base_counts_commits_since_merge_base() {
        let repo = super::super::testutil::TestRepo::new("branch-meta-base");
        repo.commit_file("main-1", "1", "main 1");
        repo.commit_file("main-2", "2", "main 2");
        super::super::testutil::git_in(&repo.dir, &["checkout", "-q", "-b", "feature"]);
        repo.commit_file("feature-1", "1", "feature 1");
        repo.commit_file("feature-2", "2", "feature 2");
        repo.commit_file("feature-3", "3", "feature 3");
        super::super::testutil::git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("main-3", "3", "main 3");
        super::super::testutil::git_in(&repo.dir, &["checkout", "-q", "feature"]);

        let meta = gix(&repo.loc(), Some("main")).unwrap();
        assert_eq!(meta.ahead_base, Some(3));
        assert_eq!(meta.behind_base, Some(1));

        let r = super::super::native_diff::open(&repo.dir).unwrap();
        let h = r.rev_parse_single("feature").unwrap().detach();
        let m = r.rev_parse_single("main").unwrap().detach();
        assert_eq!(count_excluding(&r, h, m, 2).unwrap(), 2);
    }

    #[test]
    fn cli_backend_meta_costs_one_spawn_on_memo_hit() {
        let repo = super::super::testutil::TestRepo::new("branch-meta-cli");
        repo.commit_file("tracked", "one", "subject");
        let loc = repo.loc();
        let before = super::super::git_spawn_count();
        cli(&loc, Some("main"), false).unwrap();
        let after_miss = super::super::git_spawn_count();
        cli(&loc, Some("main"), false).unwrap();
        let after_hit = super::super::git_spawn_count();
        assert!(after_miss > before);
        assert_eq!(after_hit - after_miss, 1);
    }

    #[test]
    fn glyph_reads_meta_memoised_and_spawn_free() {
        let repo = super::super::testutil::TestRepo::new("branch-meta-glyph");
        repo.commit_file("tracked", "one", "subject");
        let loc = repo.loc();
        let git = GixGit::new();
        let first = git.glyph_reads_with_submodules(&loc, false);
        assert!(first.meta.is_ok());
        let spawn_after_warm = super::super::git_spawn_count();
        let compute_after_warm = branch_meta_compute_count();
        for _ in 0..3 {
            assert!(git.glyph_reads_with_submodules(&loc, false).meta.is_ok());
        }
        assert_eq!(
            super::super::git_spawn_count() - spawn_after_warm,
            0,
            "cached gix metadata must not spawn git"
        );
        assert_eq!(branch_meta_compute_count(), compute_after_warm);

        repo.commit_file("tracked", "two", "new subject");
        assert!(git.glyph_reads_with_submodules(&loc, false).meta.is_ok());
        assert_eq!(branch_meta_compute_count(), compute_after_warm + 1);
    }
}
