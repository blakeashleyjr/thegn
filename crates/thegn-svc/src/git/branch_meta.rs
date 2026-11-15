use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use thegn_core::remote::GitLoc;

pub const META_COUNT_CAP: u32 = 999;
const META_MEMO_CAP: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BranchMeta {
    pub ahead_base: Option<u32>,
    pub behind_base: Option<u32>,
    pub dirty_files: Option<u32>,
    pub last_commit_unix: Option<i64>,
    pub last_subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitMeta {
    ahead_base: Option<u32>,
    behind_base: Option<u32>,
    last_commit_unix: Option<i64>,
    last_subject: Option<String>,
}

impl CommitMeta {
    fn with_dirty(self, dirty_files: Option<u32>) -> BranchMeta {
        BranchMeta {
            ahead_base: self.ahead_base,
            behind_base: self.behind_base,
            dirty_files,
            last_commit_unix: self.last_commit_unix,
            last_subject: self.last_subject,
        }
    }
}

static META_MEMO: LazyLock<Mutex<HashMap<(PathBuf, String, String), CommitMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static META_COMPUTES: AtomicU64 = AtomicU64::new(0);

pub fn branch_meta_compute_count() -> u64 {
    META_COMPUTES.load(Ordering::Relaxed)
}

fn memoized(
    key: (PathBuf, String, String),
    compute: impl FnOnce() -> Result<CommitMeta>,
) -> Result<CommitMeta> {
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

fn sanitized_subject(message: &[u8]) -> String {
    let first_line = message
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let mut subject = String::new();
    for character in String::from_utf8_lossy(first_line).chars() {
        if character.is_control() {
            continue;
        }
        if subject.len() + character.len_utf8() > 200 {
            break;
        }
        subject.push(character);
    }
    subject
}

pub(crate) fn parse_log_record(output: &[u8]) -> Result<(i64, String)> {
    let (timestamp, subject) = output
        .split_once(|byte| *byte == b'\0')
        .context("git log metadata missing NUL separator")?;
    let timestamp = std::str::from_utf8(timestamp)
        .context("git log metadata timestamp is not UTF-8")?
        .trim()
        .parse::<i64>()
        .context("git log metadata timestamp is not an integer")?;
    Ok((timestamp, sanitized_subject(subject)))
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
    log: (i64, String),
    dirty_files: Option<u32>,
) -> BranchMeta {
    BranchMeta {
        ahead_base: ahead_behind.map(|(ahead, _)| ahead),
        behind_base: ahead_behind.map(|(_, behind)| behind),
        dirty_files,
        last_commit_unix: Some(log.0),
        last_subject: Some(log.1),
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
        Ok(CommitMeta {
            ahead_base,
            behind_base,
            last_commit_unix: Some(last_commit_unix),
            last_subject: Some(sanitized_subject(commit.message_raw_sloppy())),
        })
    })?;
    Ok(commit_meta.with_dirty(None))
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
        base_id.clone().unwrap_or_default(),
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
        let (exit, output) = super::run_status(loc, &["log", "-1", "--format=%ct%x00%s", "HEAD"])?;
        anyhow::ensure!(exit == 0, "git log for branch metadata failed");
        let log = parse_log_record(output.as_bytes())?;
        Ok(CommitMeta {
            ahead_base: ahead_behind.map(|(ahead, _)| ahead),
            behind_base: ahead_behind.map(|(_, behind)| behind),
            last_commit_unix: Some(log.0),
            last_subject: Some(log.1),
        })
    })?;
    Ok(commit_meta.with_dirty(None))
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
            parse_log_record(b"1700000000\0first\x01 line\nsecond\n").unwrap(),
            (1_700_000_000, "first line".to_string())
        );
        assert_eq!(parse_log_record(b"1700000000\0\n").unwrap().1, "");
    }

    #[test]
    fn bridged_batch_parsers() {
        assert_eq!(parse_ahead_behind("2\t5", META_COUNT_CAP), Some((5, 2)));
        assert_eq!(
            parse_log_record(b"42\0subject\x1f\n").unwrap(),
            (42, "subject".to_string())
        );
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
