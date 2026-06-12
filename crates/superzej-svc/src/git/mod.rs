//! Git backend seam. `gix` for reads (status/diff/refs/log — the hot panel-poll
//! path); the `git` CLI for porcelain writes gix can't yet cover (worktree add
//! with checkout, push, merge, rebase) and for `GitLoc::Remote` (gix is local
//! only). Native impl lands in Phase 2; the CLI fallback wraps superzej-core's
//! existing `worktree`/`repo`/`util::git_*` code.

use anyhow::{Context, Result};
use std::path::Path;
use superzej_core::gitrefs::{BranchInfo, Commit, StashEntry};
use superzej_core::reflog::ReflogEntry;
use superzej_core::remote::GitLoc;

mod bisect;
mod branch;
mod cherry;
mod commit;
mod custom;
mod patch;
mod rebase;
mod stage;
mod stash;
mod undo;

pub use bisect::BisectOps;
pub use branch::{BranchOps, ForceMode};
pub use cherry::CherryOps;
pub use commit::{CommitOps, ResetMode};
pub use custom::CustomOps;
pub use patch::PatchOps;
pub use rebase::{PauseReason, RebaseOps, RebaseOpts, RebaseOutcome, RebaseStatus};
pub use stage::StageOps;
pub use stash::StashOps;
pub use undo::UndoOps;

/// Diff flags for any output that will later be fed to `git apply`: user
/// config (`diff.noprefix`), external diff drivers, and rename headers all
/// produce patches `git apply` rejects, so the panel's stageable diffs pin
/// them off. `-c` must precede the subcommand, so this replaces the leading
/// `"diff"` arg rather than following it.
pub(crate) const SANITIZED_DIFF: &[&str] = &[
    "-c",
    "diff.noprefix=false",
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-renames",
    "-U3",
];

/// A changed file in `git status` terms (porcelain XY + path).
#[derive(Debug, Clone)]
pub struct FileStatus {
    pub path: String,
    pub staged: char,
    pub unstaged: char,
}

/// One entry of a diff against a base ref (added/deleted line counts).
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

#[derive(Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub is_head: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
}

/// What kind of multi-step operation the repo is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl MergeKind {
    /// The statusbar/header chip label.
    pub fn label(self) -> &'static str {
        match self {
            MergeKind::Merge => "MERGING",
            MergeKind::Rebase => "REBASING",
            MergeKind::CherryPick => "CHERRY-PICK",
            MergeKind::Revert => "REVERTING",
        }
    }
}

/// A merge/rebase/cherry-pick in progress.
#[derive(Debug, Clone)]
pub struct MergeInfo {
    pub kind: MergeKind,
    /// Best-effort name of what is being merged/applied (e.g.
    /// "origin/main"). Empty when unresolvable.
    pub onto: String,
}

/// One `git log --graph` row: the graph glyph prefix plus (for commit rows)
/// sha/subject/refs. Pure connector rows carry an empty sha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub graph: String,
    pub sha: String,
    pub subject: String,
    /// Decorations (`%D`), e.g. "HEAD -> main, origin/main".
    pub refs: String,
}

impl LogRow {
    pub fn is_head(&self) -> bool {
        self.refs.split(',').any(|r| {
            let r = r.trim();
            r == "HEAD" || r.starts_with("HEAD ->")
        })
    }
}

/// One hunk of a unified diff, capped for inline preview rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` range header.
    pub header: String,
    /// The function context after the header (may be empty).
    pub func: String,
    /// `(origin, text)` rows: origin is '+', '-', or ' '.
    pub lines: Vec<(char, String)>,
    /// True when lines were dropped to fit the preview cap.
    pub truncated: bool,
}

/// Count files in conflict (porcelain XY in the unmerged set: DD AU UD UA
/// DU AA UU).
pub fn conflict_count(status: &[FileStatus]) -> usize {
    status.iter().filter(|f| is_conflict(f)).count()
}

/// Whether a porcelain entry is an unmerged (conflicted) path.
pub fn is_conflict(f: &FileStatus) -> bool {
    matches!(
        (f.staged, f.unstaged),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

/// Parse `git log --graph --format=%x1f%h%x1f%s%x1f%D` output. Lines without
/// the unit separator are pure graph connectors.
pub fn parse_log_graph(out: &str) -> Vec<LogRow> {
    out.lines()
        .map(|line| match line.split_once('\u{1f}') {
            Some((graph, rest)) => {
                let mut it = rest.split('\u{1f}');
                LogRow {
                    graph: graph.trim_end().to_string(),
                    sha: it.next().unwrap_or_default().to_string(),
                    subject: it.next().unwrap_or_default().to_string(),
                    refs: it
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .to_string(),
                }
            }
            None => LogRow {
                graph: line.trim_end().to_string(),
                sha: String::new(),
                subject: String::new(),
                refs: String::new(),
            },
        })
        .collect()
}

/// Parse unified-diff output (`git diff -U3 -- <path>`) into hunks, capping
/// each hunk's content at `max_lines` rows.
pub fn parse_unified_hunks(diff: &str, max_lines: usize) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            // "@@ -a,b +c,d @@ func" — the header ends at the second "@@".
            let (range, func) = match rest.split_once("@@") {
                Some((r, f)) => (r.trim(), f.trim()),
                None => (rest.trim(), ""),
            };
            hunks.push(Hunk {
                header: format!("@@ {range} @@"),
                func: func.to_string(),
                lines: Vec::new(),
                truncated: false,
            });
            continue;
        }
        let Some(h) = hunks.last_mut() else { continue };
        let mut chars = line.chars();
        // Only +/-/space rows are hunk content ("\ No newline at end of
        // file" and friends fall through). File headers ("+++ b/…",
        // "--- a/…") also start with +/- but only appear before the first @@.
        if let Some(origin @ ('+' | '-' | ' ')) = chars.next() {
            if h.lines.len() >= max_lines {
                h.truncated = true;
            } else {
                h.lines.push((origin, chars.collect()));
            }
        }
    }
    hunks
}

/// Reads go native (gix) for local locs; writes and remote stay CLI.
pub trait GitBackend: Send + Sync {
    fn status(&self, loc: &GitLoc) -> Result<Vec<FileStatus>>;
    /// Whether the worktree has any local changes — staged, unstaged, or
    /// untracked — i.e. `git status --porcelain` non-emptiness. The boolean
    /// the sidebar's dirty glyph needs, without paying for the full file list.
    fn is_dirty(&self, loc: &GitLoc) -> Result<bool> {
        Ok(!self.status(loc)?.is_empty())
    }
    fn diff_files(&self, loc: &GitLoc, base: &str) -> Result<Vec<DiffEntry>>;
    fn branches(&self, loc: &GitLoc) -> Result<Vec<Branch>>;
    fn current_branch(&self, loc: &GitLoc) -> Result<String>;
    /// Commits the current branch is `(ahead, behind)` its upstream tracking
    /// branch. `None` when the branch has no configured upstream (or HEAD is
    /// detached) — the sidebar simply omits the ↑/↓ glyphs in that case.
    fn ahead_behind(&self, loc: &GitLoc) -> Result<Option<(usize, usize)>>;
    fn worktrees(&self, root: &Path) -> Result<Vec<WorktreeInfo>>;
    fn add_worktree(&self, root: &Path, branch: &str, base: &str, path: &Path) -> Result<()>;
    fn remove_worktree(&self, root: &Path, path: &Path, delete_branch: bool) -> Result<()>;

    /// A merge/rebase/cherry-pick in progress, or `None`. CLI everywhere —
    /// probed via `rev-parse --verify` so it works for remote locs too.
    fn merge_state(&self, loc: &GitLoc) -> Result<Option<MergeInfo>> {
        let exists = |what: &str| -> bool {
            loc.git_command(&["rev-parse", "-q", "--verify", what])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let name_of = |what: &str| -> String {
            loc.git_command(&["name-rev", "--name-only", "--always", what])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let kind = if exists("MERGE_HEAD") {
            Some((MergeKind::Merge, "MERGE_HEAD"))
        } else if exists("REBASE_HEAD") {
            Some((MergeKind::Rebase, "REBASE_HEAD"))
        } else if exists("CHERRY_PICK_HEAD") {
            Some((MergeKind::CherryPick, "CHERRY_PICK_HEAD"))
        } else if exists("REVERT_HEAD") {
            Some((MergeKind::Revert, "REVERT_HEAD"))
        } else {
            None
        };
        Ok(kind.map(|(kind, head)| MergeInfo {
            kind,
            onto: name_of(head),
        }))
    }

    /// The last `n` commits as graph rows (glyph prefix + sha/subject/refs).
    fn log_graph(&self, loc: &GitLoc, n: usize) -> Result<Vec<LogRow>> {
        let n = n.to_string();
        let out = run(
            loc,
            &["log", "--graph", "--format=%x1f%h%x1f%s%x1f%D", "-n", &n],
        )?;
        Ok(parse_log_graph(&out))
    }

    /// Commit timestamps (epoch seconds) for the last `weeks` weeks — the
    /// commit-calendar feed. Cheap: one line per commit.
    fn commit_times(&self, loc: &GitLoc, weeks: usize) -> Result<Vec<i64>> {
        let since = format!("{weeks}.weeks");
        let out = run(loc, &["log", "--since", &since, "--format=%ct"])?;
        Ok(out.lines().filter_map(|l| l.trim().parse().ok()).collect())
    }

    /// The unified-diff hunks of one file against `base`, capped at
    /// `max_lines` content rows per hunk (inline preview).
    fn diff_hunks(
        &self,
        loc: &GitLoc,
        base: &str,
        path: &str,
        max_lines: usize,
    ) -> Result<Vec<Hunk>> {
        let out = run(loc, &["diff", "--no-color", "-U3", base, "--", path])?;
        Ok(parse_unified_hunks(&out, max_lines))
    }

    /// Stash entry count (0 when the stash is empty or absent).
    fn stash_count(&self, loc: &GitLoc) -> Result<usize> {
        let out = match loc.git_command(&["stash", "list", "--format=%h"]).output() {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return Ok(0),
        };
        Ok(out.lines().filter(|l| !l.trim().is_empty()).count())
    }

    /// The last `n` commits as structured records (parents included — the
    /// commit-graph / commits-view feed).
    fn log_commits(&self, loc: &GitLoc, n: usize) -> Result<Vec<Commit>> {
        let n = n.to_string();
        let out = run(
            loc,
            &[
                "log",
                "--format=%x1f%H%x1f%h%x1f%an%x1f%ae%x1f%ct%x1f%P%x1f%D%x1f%s",
                "-n",
                &n,
            ],
        )?;
        Ok(superzej_core::gitrefs::parse_commits(&out))
    }

    /// All local branches with upstream/divergence detail, newest first.
    fn branches_full(&self, loc: &GitLoc) -> Result<Vec<BranchInfo>> {
        let out = run(
            loc,
            &[
                "for-each-ref",
                "refs/heads",
                "--sort=-committerdate",
                "--format=%(HEAD)%x1f%(refname:short)%x1f%(upstream:short)%x1f%(upstream:track)%x1f%(objectname)%x1f%(committerdate:unix)%x1f%(contents:subject)",
            ],
        )?;
        Ok(superzej_core::gitrefs::parse_branches(&out))
    }

    /// The stash as structured entries (empty on a stash-less repo).
    fn stash_list(&self, loc: &GitLoc) -> Result<Vec<StashEntry>> {
        let out = match loc
            .git_command(&["stash", "list", "--format=%gd\u{1f}%H\u{1f}%ct\u{1f}%gs"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return Ok(Vec::new()),
        };
        Ok(superzej_core::gitrefs::parse_stashes(&out))
    }

    /// The last `n` HEAD reflog entries (the undo planner's feed).
    fn reflog(&self, loc: &GitLoc, n: usize) -> Result<Vec<ReflogEntry>> {
        let n = n.to_string();
        let out = run(
            loc,
            &["reflog", "--format=%H%x1f%gd%x1f%ct%x1f%gs", "-n", &n],
        )?;
        Ok(superzej_core::reflog::parse_reflog(&out))
    }

    /// Diffstat between two arbitrary refs (`diff_files` with an `A..B`
    /// base) — named for discoverability.
    fn diff_refs(&self, loc: &GitLoc, from: &str, to: &str) -> Result<Vec<DiffEntry>> {
        self.diff_files(loc, &format!("{from}..{to}"))
    }

    /// One file's worktree-vs-index diff, sanitized for `git apply`
    /// round-trips (see [`SANITIZED_DIFF`]). Empty string when unchanged.
    fn unstaged_diff(&self, loc: &GitLoc, path: &str) -> Result<String> {
        let mut args = SANITIZED_DIFF.to_vec();
        args.extend(["--", path]);
        run(loc, &args)
    }

    /// One file's index-vs-HEAD diff, sanitized for `git apply` round-trips.
    fn staged_diff(&self, loc: &GitLoc, path: &str) -> Result<String> {
        let mut args = SANITIZED_DIFF.to_vec();
        args.extend(["--cached", "--", path]);
        run(loc, &args)
    }

    /// A commit's patch (optionally narrowed to one path), sanitized for
    /// `git apply` round-trips — the custom-patch builder's feed.
    fn commit_diff(&self, loc: &GitLoc, sha: &str, path: Option<&str>) -> Result<String> {
        let mut args = vec![
            "-c",
            "diff.noprefix=false",
            "show",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "-U3",
            "--format=",
            sha,
        ];
        if let Some(p) = path {
            args.extend(["--", p]);
        }
        run(loc, &args)
    }

    /// Stage one path (`git add -- <path>`).
    fn stage(&self, loc: &GitLoc, path: &str) -> Result<()> {
        run(loc, &["add", "--", path]).map(|_| ())
    }

    /// Unstage one path (`git reset -q HEAD -- <path>` — works on every git).
    fn unstage(&self, loc: &GitLoc, path: &str) -> Result<()> {
        run(loc, &["reset", "-q", "HEAD", "--", path]).map(|_| ())
    }
}

/// The permanent fallback: every op via the `git` CLI (through `GitLoc`, so it
/// works for both local and remote locs). The native impl composes over this.
pub struct CliGit;

fn run(loc: &GitLoc, args: &[&str]) -> Result<String> {
    let out = loc
        .git_command(args)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The write-op runner: like [`run`] but with extra env, a null stdin, and
/// `GIT_TERMINAL_PROMPT=0` — a credential/gpg prompt must fail fast, never
/// hang the background thread that mutations run on.
pub(crate) fn run_w(loc: &GitLoc, envs: &[(&str, &str)], args: &[&str]) -> Result<String> {
    let mut env: Vec<(&str, &str)> = vec![("GIT_TERMINAL_PROMPT", "0")];
    env.extend_from_slice(envs);
    let out = loc
        .git_command_env(&env, args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// [`run_w`] with bytes piped to stdin (`git apply -`, `git commit -F -`).
pub(crate) fn run_stdin(
    loc: &GitLoc,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: &[u8],
) -> Result<String> {
    let mut env: Vec<(&str, &str)> = vec![("GIT_TERMINAL_PROMPT", "0")];
    env.extend_from_slice(envs);
    let out = loc
        .git_with_stdin(&env, args, stdin)
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `-c` flags disabling gpg signing for history rewrites when the user set
/// `[git] override_gpg = true` (a passphrase prompt would stall the op).
pub(crate) fn gpg_args(override_gpg: bool) -> &'static [&'static str] {
    if override_gpg {
        &["-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"]
    } else {
        &[]
    }
}

fn run_root(root: &Path, args: &[&str]) -> Result<()> {
    if superzej_core::util::git_ok(root, args) {
        Ok(())
    } else {
        anyhow::bail!("git {} failed in {}", args.join(" "), root.display())
    }
}

impl GitBackend for CliGit {
    fn status(&self, loc: &GitLoc) -> Result<Vec<FileStatus>> {
        let out = run(loc, &["status", "--porcelain=v1", "-z"])?;
        let mut v = Vec::new();
        for entry in out.split('\0').filter(|s| s.len() >= 3) {
            let bytes = entry.as_bytes();
            v.push(FileStatus {
                staged: bytes[0] as char,
                unstaged: bytes[1] as char,
                path: entry[3..].to_string(),
            });
        }
        Ok(v)
    }

    fn diff_files(&self, loc: &GitLoc, base: &str) -> Result<Vec<DiffEntry>> {
        let out = run(loc, &["diff", "--numstat", base])?;
        let mut v = Vec::new();
        for line in out.lines() {
            let mut it = line.splitn(3, '\t');
            let (a, d, p) = (it.next(), it.next(), it.next());
            if let (Some(a), Some(d), Some(p)) = (a, d, p) {
                v.push(DiffEntry {
                    added: a.parse().unwrap_or(0),
                    deleted: d.parse().unwrap_or(0),
                    path: p.to_string(),
                });
            }
        }
        Ok(v)
    }

    fn branches(&self, loc: &GitLoc) -> Result<Vec<Branch>> {
        let out = run(loc, &["branch", "--format=%(HEAD)\t%(refname:short)"])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (mark, name) = l.split_once('\t')?;
                Some(Branch {
                    name: name.to_string(),
                    is_head: mark.trim() == "*",
                })
            })
            .collect())
    }

    fn current_branch(&self, loc: &GitLoc) -> Result<String> {
        Ok(run(loc, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string())
    }

    fn ahead_behind(&self, loc: &GitLoc) -> Result<Option<(usize, usize)>> {
        // `@{u}` resolves the upstream; the command fails when none is set, so a
        // non-zero exit is treated as "no upstream" rather than an error.
        let out = match loc
            .git_command(&["rev-list", "--left-right", "--count", "@{u}...HEAD"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return Ok(None),
        };
        // Output is "<behind>\t<ahead>": left side is @{u} (commits we lack),
        // right side is HEAD (commits ahead).
        let mut it = out.split_whitespace();
        let behind = it.next().and_then(|s| s.parse().ok());
        let ahead = it.next().and_then(|s| s.parse().ok());
        match (ahead, behind) {
            (Some(a), Some(b)) => Ok(Some((a, b))),
            _ => Ok(None),
        }
    }

    fn worktrees(&self, root: &Path) -> Result<Vec<WorktreeInfo>> {
        let loc = GitLoc::for_worktree(root);
        let out = run(&loc, &["worktree", "list", "--porcelain"])?;
        let mut v = Vec::new();
        let mut cur: Option<WorktreeInfo> = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                if let Some(w) = cur.take() {
                    v.push(w);
                }
                cur = Some(WorktreeInfo {
                    path: p.to_string(),
                    branch: None,
                });
            } else if let Some(b) = line.strip_prefix("branch ")
                && let Some(w) = cur.as_mut()
            {
                w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
            }
        }
        if let Some(w) = cur.take() {
            v.push(w);
        }
        Ok(v)
    }

    fn add_worktree(&self, root: &Path, branch: &str, base: &str, path: &Path) -> Result<()> {
        let p = path.to_string_lossy();
        run_root(root, &["worktree", "add", "-b", branch, &p, base])
    }

    fn remove_worktree(&self, root: &Path, path: &Path, delete_branch: bool) -> Result<()> {
        let p = path.to_string_lossy();
        run_root(root, &["worktree", "remove", "--force", &p])?;
        if delete_branch {
            // Best-effort: the branch may still be checked out elsewhere.
            let _ = run_root(root, &["branch", "-D", &p]);
        }
        Ok(())
    }
}

/// The native backend: gix for the clean read wins (current branch, branch list)
/// on local locs; everything else — status/diff (gix status semantics can
/// diverge), worktree enumeration, all writes, and every remote loc — delegates
/// to the CLI fallback. This is the plan's "gix is a read engine, not a write
/// engine" stance made concrete.
pub struct GixGit {
    fallback: CliGit,
}

impl Default for GixGit {
    fn default() -> Self {
        Self { fallback: CliGit }
    }
}

impl GixGit {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GitBackend for GixGit {
    fn is_dirty(&self, loc: &GitLoc) -> Result<bool> {
        if loc.is_remote() {
            return self.fallback.is_dirty(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        // The full status iterator (HEAD↔index, index↔worktree, untracked
        // dirwalk), early-exiting on the first entry. Deliberately NOT gix's
        // `Repository::is_dirty()`: that disables the dirwalk and would miss
        // untracked-only worktrees that the CLI fallback (`git status
        // --porcelain`) reports — a silent sidebar-glyph semantics change.
        let mut iter = repo
            .status(gix::progress::Discard)
            .context("gix status")?
            .into_iter(Vec::<gix::bstr::BString>::new())
            .context("gix status iter")?;
        Ok(iter
            .next()
            .transpose()
            .context("gix status item")?
            .is_some())
    }

    fn current_branch(&self, loc: &GitLoc) -> Result<String> {
        if loc.is_remote() {
            return self.fallback.current_branch(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        match repo.head_name().context("gix head_name")? {
            Some(name) => Ok(name.shorten().to_string()),
            None => Ok("HEAD".to_string()), // detached
        }
    }

    fn branches(&self, loc: &GitLoc) -> Result<Vec<Branch>> {
        if loc.is_remote() {
            return self.fallback.branches(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        let head = repo
            .head_name()
            .context("gix head_name")?
            .map(|n| n.shorten().to_string());
        let platform = repo.references().context("gix references")?;
        let mut v = Vec::new();
        for r in platform.local_branches().context("gix local_branches")? {
            let r = r.map_err(|e| anyhow::anyhow!("gix branch ref: {e}"))?;
            let name = r.name().shorten().to_string();
            let is_head = head.as_deref() == Some(name.as_str());
            v.push(Branch { name, is_head });
        }
        Ok(v)
    }

    fn ahead_behind(&self, loc: &GitLoc) -> Result<Option<(usize, usize)>> {
        if loc.is_remote() {
            return self.fallback.ahead_behind(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        // The current branch reference; bail to None when detached / unborn.
        let Some(head_ref) = repo.head_ref().context("gix head_ref")? else {
            return Ok(None);
        };
        // Its upstream tracking ref (e.g. refs/remotes/origin/main). Absent =>
        // no upstream configured => no counts.
        let Some(upstream_name) = head_ref.remote_tracking_ref_name(gix::remote::Direction::Fetch)
        else {
            return Ok(None);
        };
        let upstream_name = match upstream_name {
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        let Ok(mut upstream) = repo.find_reference(upstream_name.as_ref()) else {
            return Ok(None);
        };

        let head_id = head_ref.id();
        let upstream_id = upstream.peel_to_id().context("peel upstream ref")?;

        // ahead = commits reachable from HEAD but not from upstream; behind is
        // the symmetric reverse. `with_hidden` paints the opposite tip's
        // ancestry as unwanted so the walk yields exactly the difference.
        let count_excluding = |tip: gix::ObjectId, hidden: gix::ObjectId| -> Result<usize> {
            let walk = repo
                .rev_walk([tip])
                .with_hidden([hidden])
                .all()
                .context("gix rev_walk")?;
            let mut n = 0usize;
            for info in walk {
                info.map_err(|e| anyhow::anyhow!("gix rev_walk item: {e}"))?;
                n += 1;
            }
            Ok(n)
        };

        let ahead = count_excluding(head_id.detach(), upstream_id.detach())?;
        let behind = count_excluding(upstream_id.detach(), head_id.detach())?;
        Ok(Some((ahead, behind)))
    }

    // --- delegated to the CLI fallback ---
    fn status(&self, loc: &GitLoc) -> Result<Vec<FileStatus>> {
        self.fallback.status(loc)
    }
    fn diff_files(&self, loc: &GitLoc, base: &str) -> Result<Vec<DiffEntry>> {
        self.fallback.diff_files(loc, base)
    }
    fn worktrees(&self, root: &Path) -> Result<Vec<WorktreeInfo>> {
        self.fallback.worktrees(root)
    }
    fn add_worktree(&self, root: &Path, branch: &str, base: &str, path: &Path) -> Result<()> {
        self.fallback.add_worktree(root, branch, base, path)
    }
    fn remove_worktree(&self, root: &Path, path: &Path, delete_branch: bool) -> Result<()> {
        self.fallback.remove_worktree(root, path, delete_branch)
    }
}

/// Shared real-repo fixture helpers for the ops modules' integration tests.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    /// A throwaway repo under /tmp, removed on drop. `git init -b main` done.
    pub(crate) struct TestRepo {
        pub dir: PathBuf,
    }

    impl TestRepo {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sz-git-{tag}-{}-{:x}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            git_in(&dir, &["init", "-q", "-b", "main"]);
            TestRepo { dir }
        }

        pub fn loc(&self) -> superzej_core::remote::GitLoc {
            superzej_core::remote::GitLoc::for_worktree(&self.dir)
        }

        /// Write a file and `git add` + commit it.
        pub fn commit_file(&self, path: &str, content: &str, msg: &str) {
            std::fs::write(self.dir.join(path), content).unwrap();
            git_in(&self.dir, &["add", path]);
            git_in(&self.dir, &["commit", "-q", "-m", msg]);
        }

        /// Trimmed stdout of a git command (panics on failure).
        pub fn out(&self, args: &[&str]) -> String {
            let out = git_cmd(&self.dir, args).output().unwrap();
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        /// Commit subjects, newest first.
        pub fn subjects(&self) -> Vec<String> {
            self.out(&["log", "--format=%s"])
                .lines()
                .map(str::to_string)
                .collect()
        }

        pub fn head(&self) -> String {
            self.out(&["rev-parse", "HEAD"])
        }

        /// Sha of a commit by subject (panics if absent/ambiguous).
        pub fn sha_of(&self, subject: &str) -> String {
            let out = self.out(&["log", "--format=%H %s"]);
            let mut hits = out.lines().filter(|l| {
                l.split_once(' ')
                    .map(|(_, s)| s == subject)
                    .unwrap_or(false)
            });
            let sha = hits
                .next()
                .unwrap_or_else(|| panic!("no commit with subject {subject:?}"))
                .split(' ')
                .next()
                .unwrap()
                .to_string();
            assert!(hits.next().is_none(), "ambiguous subject {subject:?}");
            sha
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Command {
        let mut c = std::process::Command::new("git");
        c.args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        c
    }

    /// Run `git` in `dir`, panicking on failure.
    pub(crate) fn git_in(dir: &Path, args: &[&str]) {
        let out = git_cmd(dir, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub(crate) fn commit_empty(dir: &Path, msg: &str) {
        git_in(dir, &["commit", "--allow-empty", "-q", "-m", msg]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> std::path::PathBuf {
        // The svc crate dir is <root>/crates/superzej-svc; the repo root is two up.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn cli_and_gix_agree_on_current_branch_and_branches() {
        let loc = GitLoc::for_worktree(&repo_root());
        let cli = CliGit;
        let gix = GixGit::new();

        let cb_cli = cli.current_branch(&loc).unwrap();
        let cb_gix = gix.current_branch(&loc).unwrap();
        assert_eq!(cb_cli, cb_gix, "gix and git disagree on current branch");
        assert!(!cb_cli.is_empty());

        let mut b_cli: Vec<String> = cli
            .branches(&loc)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        let mut b_gix: Vec<String> = gix
            .branches(&loc)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        b_cli.sort();
        b_gix.sort();
        assert_eq!(b_cli, b_gix, "gix and git disagree on the branch set");
        // exactly one head in each
        assert_eq!(
            gix.branches(&loc)
                .unwrap()
                .iter()
                .filter(|b| b.is_head)
                .count(),
            1
        );
    }

    #[test]
    fn cli_worktree_enumeration_includes_the_root() {
        let root = repo_root();
        let wts = CliGit.worktrees(&root).unwrap();
        assert!(!wts.is_empty());
        // The canonical root path should appear among the worktrees.
        let canon = std::fs::canonicalize(&root).unwrap();
        assert!(wts.iter().any(|w| {
            std::fs::canonicalize(&w.path)
                .map(|p| p == canon)
                .unwrap_or(false)
        }));
    }

    #[test]
    fn status_and_diff_run_without_error() {
        let loc = GitLoc::for_worktree(&repo_root());
        // Should not error on a normal repo (content is environment-dependent).
        let _ = CliGit.status(&loc).unwrap();
        let _ = CliGit.diff_files(&loc, "HEAD").unwrap();
    }

    /// Run `git` in `dir`, panicking on failure (test setup helper).
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    fn commit_empty(dir: &std::path::Path, msg: &str) {
        git_in(dir, &["commit", "--allow-empty", "-q", "-m", msg]);
    }

    #[test]
    fn gix_and_cli_agree_on_is_dirty() {
        let base = std::env::temp_dir().join(format!("sz-dirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        git_in(&base, &["init", "-q", "-b", "main"]);
        commit_empty(&base, "c0");

        let loc = GitLoc::for_worktree(&base);
        let gix = GixGit::new();
        let cli = CliGit;

        // Clean repo: both clean.
        assert!(!cli.is_dirty(&loc).unwrap());
        assert!(!gix.is_dirty(&loc).unwrap());

        // Untracked-only: porcelain reports `??`; gix must agree (this is the
        // case gix's own `Repository::is_dirty()` would miss).
        std::fs::write(base.join("new.txt"), "hello").unwrap();
        assert!(cli.is_dirty(&loc).unwrap());
        assert!(gix.is_dirty(&loc).unwrap());

        // Staged change: both dirty.
        git_in(&base, &["add", "new.txt"]);
        assert!(cli.is_dirty(&loc).unwrap());
        assert!(gix.is_dirty(&loc).unwrap());

        // Committed: clean again.
        git_in(&base, &["commit", "-q", "-m", "c1"]);
        assert!(!cli.is_dirty(&loc).unwrap());
        assert!(!gix.is_dirty(&loc).unwrap());

        // Unstaged modification of a tracked file: both dirty.
        std::fs::write(base.join("new.txt"), "changed").unwrap();
        assert!(cli.is_dirty(&loc).unwrap());
        assert!(gix.is_dirty(&loc).unwrap());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_log_graph_splits_commit_and_connector_rows() {
        let out = "* \u{1f}a3f81c2\u{1f}merge origin/main\u{1f}HEAD -> main, origin/main\n\
                   |\\  \n\
                   | * \u{1f}9c2d11e\u{1f}panel: focus follows zoom\u{1f}\n\
                   * | \u{1f}f44a09b\u{1f}tabs: dedup titles\u{1f}tag: v0.1\n\
                   |/  \n\
                   * \u{1f}77a1f2c\u{1f}host: panel split math\u{1f}\n";
        let rows = parse_log_graph(out);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].graph, "*");
        assert_eq!(rows[0].sha, "a3f81c2");
        assert_eq!(rows[0].subject, "merge origin/main");
        assert!(rows[0].is_head());
        assert_eq!(rows[1].graph, "|\\");
        assert!(rows[1].sha.is_empty(), "connector row");
        assert_eq!(rows[2].graph, "| *");
        assert!(!rows[2].is_head());
        assert_eq!(rows[3].refs, "tag: v0.1");
        assert_eq!(parse_log_graph(""), Vec::<LogRow>::new());
    }

    #[test]
    fn is_head_matches_head_decorations_only() {
        let row = |refs: &str| LogRow {
            graph: String::new(),
            sha: "x".into(),
            subject: String::new(),
            refs: refs.into(),
        };
        assert!(row("HEAD -> main, origin/main").is_head());
        assert!(row("HEAD").is_head());
        assert!(row("main, HEAD -> feat").is_head());
        assert!(!row("origin/HEAD-ish, main").is_head());
        assert!(!row("").is_head());
    }

    #[test]
    fn parse_unified_hunks_extracts_headers_lines_and_caps() {
        // Built line-by-line: `\`-continued string literals would strip the
        // leading space that marks a context row.
        let diff = [
            "diff --git a/src/tabs.rs b/src/tabs.rs",
            "index 1111111..2222222 100644",
            "--- a/src/tabs.rs",
            "+++ b/src/tabs.rs",
            "@@ -180,6 +180,14 @@ fn handle_key()",
            " context line",
            "+let idx = self.order.iter()",
            "+    .position(|t| t.id == id)?;",
            "-self.tabs.swap(a, b);",
            "@@ -512,3 +518,9 @@",
            "+filtered",
            "\\ No newline at end of file",
        ]
        .join("\n");
        let diff = diff.as_str();
        let hunks = parse_unified_hunks(diff, 100);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header, "@@ -180,6 +180,14 @@");
        assert_eq!(hunks[0].func, "fn handle_key()");
        assert_eq!(
            hunks[0].lines,
            vec![
                (' ', "context line".to_string()),
                ('+', "let idx = self.order.iter()".to_string()),
                ('+', "    .position(|t| t.id == id)?;".to_string()),
                ('-', "self.tabs.swap(a, b);".to_string()),
            ]
        );
        assert!(!hunks[0].truncated);
        assert_eq!(hunks[1].func, "");
        assert_eq!(hunks[1].lines.len(), 1);

        // The cap truncates and flags.
        let capped = parse_unified_hunks(diff, 2);
        assert_eq!(capped[0].lines.len(), 2);
        assert!(capped[0].truncated);

        // File headers before the first @@ never leak into hunks; garbage is
        // tolerated.
        assert!(parse_unified_hunks("not a diff\n+++ b/x\n--- a/x\n", 10).is_empty());
    }

    #[test]
    fn conflict_count_matches_unmerged_xy_codes() {
        let f = |s: char, u: char| FileStatus {
            path: "x".into(),
            staged: s,
            unstaged: u,
        };
        let status = vec![
            f('M', ' '),
            f('U', 'U'),
            f('A', 'A'),
            f('D', 'D'),
            f('A', 'U'),
            f('U', 'D'),
            f('U', 'A'),
            f('D', 'U'),
            f('?', '?'),
            f('A', ' '),
        ];
        assert_eq!(conflict_count(&status), 7);
        assert_eq!(conflict_count(&[]), 0);
    }

    #[test]
    fn merge_state_detects_a_live_merge_and_clears_after_abort() {
        let base = std::env::temp_dir().join(format!("sz-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        git_in(&base, &["init", "-q", "-b", "main"]);
        std::fs::write(base.join("f.txt"), "base\n").unwrap();
        git_in(&base, &["add", "f.txt"]);
        git_in(&base, &["commit", "-q", "-m", "c0"]);
        git_in(&base, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(base.join("f.txt"), "feat\n").unwrap();
        git_in(&base, &["commit", "-q", "-am", "feat change"]);
        git_in(&base, &["checkout", "-q", "main"]);
        std::fs::write(base.join("f.txt"), "main\n").unwrap();
        git_in(&base, &["commit", "-q", "-am", "main change"]);

        let loc = GitLoc::for_worktree(&base);
        assert!(CliGit.merge_state(&loc).unwrap().is_none());

        // A conflicting merge leaves MERGE_HEAD behind (merge itself fails).
        let _ = std::process::Command::new("git")
            .args(["merge", "feat"])
            .current_dir(&base)
            .output();
        let st = CliGit.merge_state(&loc).unwrap().expect("merge detected");
        assert_eq!(st.kind, MergeKind::Merge);
        assert_eq!(st.kind.label(), "MERGING");
        assert!(st.onto.contains("feat"), "onto was {:?}", st.onto);
        // The porcelain conflict set agrees.
        let status = CliGit.status(&loc).unwrap();
        assert_eq!(conflict_count(&status), 1);

        git_in(&base, &["merge", "--abort"]);
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn log_graph_stash_and_stage_roundtrip_on_a_fixture_repo() {
        let base = std::env::temp_dir().join(format!("sz-ops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        git_in(&base, &["init", "-q", "-b", "main"]);
        commit_empty(&base, "c0");
        commit_empty(&base, "c1");

        let loc = GitLoc::for_worktree(&base);
        let rows = CliGit.log_graph(&loc, 6).unwrap();
        let commits: Vec<&LogRow> = rows.iter().filter(|r| !r.sha.is_empty()).collect();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "c1");
        assert!(commits[0].is_head());

        // commit_times sees both commits (they are < 1 week old).
        let times = CliGit.commit_times(&loc, 4).unwrap();
        assert_eq!(times.len(), 2);
        assert!(times.iter().all(|&t| t > 0));

        // stash count: zero, then one.
        assert_eq!(CliGit.stash_count(&loc).unwrap(), 0);
        std::fs::write(base.join("w.txt"), "x").unwrap();
        git_in(&base, &["add", "w.txt"]);
        git_in(&base, &["stash", "-q"]);
        assert_eq!(CliGit.stash_count(&loc).unwrap(), 1);
        git_in(&base, &["stash", "drop", "-q"]);

        // stage/unstage move a file between the porcelain columns.
        std::fs::write(base.join("s.txt"), "y").unwrap();
        let untracked = |st: &[FileStatus]| st.iter().any(|f| f.staged == '?' && f.path == "s.txt");
        assert!(untracked(&CliGit.status(&loc).unwrap()));
        CliGit.stage(&loc, "s.txt").unwrap();
        let st = CliGit.status(&loc).unwrap();
        assert!(
            st.iter().any(|f| f.staged == 'A' && f.path == "s.txt"),
            "{st:?}"
        );
        CliGit.unstage(&loc, "s.txt").unwrap();
        assert!(untracked(&CliGit.status(&loc).unwrap()));

        // diff_hunks on a tracked modification.
        std::fs::write(base.join("h.txt"), "one\n").unwrap();
        git_in(&base, &["add", "h.txt"]);
        git_in(&base, &["commit", "-q", "-m", "h0"]);
        std::fs::write(base.join("h.txt"), "two\n").unwrap();
        let hunks = CliGit.diff_hunks(&loc, "HEAD", "h.txt", 16).unwrap();
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].lines.iter().any(|(o, t)| *o == '+' && t == "two"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ahead_behind_counts_divergence_and_is_none_without_upstream() {
        let base = std::env::temp_dir().join(format!("sz-ab-{}-{:p}", std::process::id(), &0u8));
        let _ = std::fs::remove_dir_all(&base);
        let remote = base.join("remote.git");
        let clone = base.join("clone");
        std::fs::create_dir_all(&base).unwrap();

        // A bare "remote" with one commit, cloned locally so the clone's branch
        // has a tracking upstream.
        git_in(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git_in(&seed, &["init", "-q", "-b", "main"]);
        commit_empty(&seed, "c0");
        git_in(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_in(&seed, &["push", "-q", "origin", "main"]);
        git_in(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );

        let loc = GitLoc::for_worktree(&clone);
        let gix = GixGit::new();
        let cli = CliGit;

        // Freshly cloned, in sync with upstream.
        assert_eq!(gix.ahead_behind(&loc).unwrap(), Some((0, 0)));
        assert_eq!(cli.ahead_behind(&loc).unwrap(), Some((0, 0)));

        // Two local commits ahead.
        commit_empty(&clone, "a1");
        commit_empty(&clone, "a2");
        assert_eq!(gix.ahead_behind(&loc).unwrap(), Some((2, 0)));
        assert_eq!(cli.ahead_behind(&loc).unwrap(), Some((2, 0)));

        // Advance the remote by one and refetch: now also 1 behind.
        commit_empty(&seed, "r1");
        git_in(&seed, &["push", "-q", "origin", "main"]);
        git_in(&clone, &["fetch", "-q", "origin"]);
        assert_eq!(gix.ahead_behind(&loc).unwrap(), Some((2, 1)));
        assert_eq!(cli.ahead_behind(&loc).unwrap(), Some((2, 1)));

        // A repo with no upstream reports None.
        let solo = base.join("solo");
        std::fs::create_dir_all(&solo).unwrap();
        git_in(&solo, &["init", "-q", "-b", "main"]);
        commit_empty(&solo, "s0");
        let solo_loc = GitLoc::for_worktree(&solo);
        assert_eq!(gix.ahead_behind(&solo_loc).unwrap(), None);
        assert_eq!(cli.ahead_behind(&solo_loc).unwrap(), None);

        let _ = std::fs::remove_dir_all(&base);
    }
}
