//! Git ref/log output → structured data for the lazygit-style panels:
//! commits, rich branches, stashes, tags, bisect state, git version.
//!
//! Pure parsing; the svc layer runs the commands and hands the stdout here.
//! Commit and stash records use NUL-delimited fields, which cannot collide
//! with Git identity or message data. Ref records use unit separators because
//! Git ref names cannot contain control bytes. Parsers reject malformed rows
//! as a whole rather than silently returning partial repository state.

/// Whole-output byte cap applied by every parser before it allocates.
pub const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Per-field byte cap (NUL- or unit-separator-delimited fields).
pub const MAX_GIT_FIELD_BYTES: usize = 1024 * 1024;

/// Reject output above `max_output` bytes, or containing any NUL- or
/// unit-separator-delimited field above `max_field` bytes. One pass, no
/// allocation. The result is a whole-output error, never a truncation.
pub fn check_git_output(out: &str, max_output: usize, max_field: usize) -> anyhow::Result<()> {
    check_limits(out, max_output, max_field, false)
}

/// [`check_git_output`] for line-framed output, where `\n` also ends a field.
fn check_limits(
    out: &str,
    max_output: usize,
    max_field: usize,
    newline_delim: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        out.len() <= max_output,
        "git output is {} bytes, over the {max_output}-byte whole-output cap",
        out.len()
    );
    let mut run = 0usize;
    for b in out.bytes() {
        if b == 0 || b == 0x1f || (newline_delim && b == b'\n') {
            run = 0;
            continue;
        }
        run += 1;
        anyhow::ensure!(
            run <= max_field,
            "git output has a field over the {max_field}-byte per-field cap"
        );
    }
    Ok(())
}

/// One commit from `git log` (see [`parse_commits`] for the format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub email: String,
    /// Committer date, epoch seconds (`%ct`).
    pub date: i64,
    pub subject: String,
    /// Parent shas (`%P` split on whitespace) — empty for a root commit.
    pub parents: Vec<String>,
    /// Ref decorations (`%D`, e.g. "HEAD -> main, origin/main"); may be empty.
    pub refs: String,
}

/// Parse the eight NUL-delimited fields emitted by
/// `git log -z --format=%H%x00%h%x00%an%x00%ae%x00%ct%x00%P%x00%D%x00%s`.
/// Every record, including the last, must end with NUL; output that does not
/// is truncated and is rejected as a whole.
pub fn parse_commits(out: &str) -> anyhow::Result<Vec<Commit>> {
    parse_commits_with_limits(out, MAX_GIT_OUTPUT_BYTES, MAX_GIT_FIELD_BYTES)
}

fn parse_commits_with_limits(
    out: &str,
    max_output: usize,
    max_field: usize,
) -> anyhow::Result<Vec<Commit>> {
    check_limits(out, max_output, max_field, false)?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        out.ends_with('\0'),
        "malformed commit record: missing final NUL terminator"
    );
    let mut fields: Vec<&str> = out.split('\0').collect();
    if fields.last() == Some(&"") {
        fields.pop();
    }
    anyhow::ensure!(
        fields.len().is_multiple_of(8),
        "malformed commit record field count"
    );
    let mut commits = Vec::with_capacity(fields.len() / 8);
    let mut seen = std::collections::HashSet::new();
    for row in fields.chunks_exact(8) {
        let sha = row[0];
        let short = row[1];
        anyhow::ensure!(valid_oid(sha), "malformed commit object id");
        anyhow::ensure!(seen.insert(sha), "duplicate commit object id");
        anyhow::ensure!(
            !short.is_empty()
                && short.len() <= sha.len()
                && short.bytes().all(|b| b.is_ascii_hexdigit())
                && sha.starts_with(short),
            "malformed abbreviated commit object id"
        );
        let date = parse_epoch_seconds(row[4])
            .ok_or_else(|| anyhow::anyhow!("malformed commit timestamp"))?;
        let parents = row[5]
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            parents.iter().all(|parent| valid_oid(parent)),
            "malformed parent object id"
        );
        commits.push(Commit {
            sha: sha.to_owned(),
            short: short.to_owned(),
            author: row[2].to_owned(),
            email: row[3].to_owned(),
            date,
            parents,
            refs: row[6].to_owned(),
            subject: row[7].to_owned(),
        });
    }
    Ok(commits)
}

/// Parse a numeric `%ct` / `%(committerdate:unix)` field. Git emits plain
/// decimal digits; a sign (`+100`, `-5`) or any other byte is corruption, which
/// `i64::from_str` alone would accept. Mirrors the reflog timestamp check.
fn parse_epoch_seconds(field: &str) -> Option<i64> {
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    field.parse().ok()
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One local branch with tracking info (see [`parse_branches`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    /// Upstream short name (e.g. "origin/main"); `None` when untracked.
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    /// The configured upstream ref no longer exists (`[gone]`).
    pub upstream_gone: bool,
    pub sha: String,
    /// Committer date, epoch seconds.
    pub date: i64,
    pub subject: String,
}

/// Decompose an `%(upstream:track)` field — `""` | `"[ahead N]"` |
/// `"[behind M]"` | `"[ahead N, behind M]"` | `"[gone]"` — into
/// `(ahead, behind, gone)`.
fn parse_track(track: &str) -> anyhow::Result<(usize, usize, bool)> {
    if track.is_empty() {
        return Ok((0, 0, false));
    }
    let inner = track
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| anyhow::anyhow!("malformed upstream tracking field"))?;
    if inner == "gone" {
        return Ok((0, 0, true));
    }
    let (mut ahead, mut behind) = (0, 0);
    let (mut saw_ahead, mut saw_behind) = (false, false);
    for part in inner.split(',') {
        let mut words = part.split_whitespace();
        let field = words
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty upstream tracking count"))?;
        let count = words
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing upstream tracking count"))?
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("invalid upstream tracking count"))?;
        anyhow::ensure!(words.next().is_none(), "extra upstream tracking field data");
        match field {
            "ahead" if !saw_ahead => {
                ahead = count;
                saw_ahead = true;
            }
            "behind" if !saw_behind => {
                behind = count;
                saw_behind = true;
            }
            _ => anyhow::bail!("unknown or duplicate upstream tracking count"),
        }
    }
    anyhow::ensure!(saw_ahead || saw_behind, "empty upstream tracking field");
    Ok((ahead, behind, false))
}

/// Parse `git for-each-ref refs/heads --sort=-committerdate --format=
/// %(HEAD)%x1f%(refname:short)%x1f%(upstream:short)%x1f%(upstream:track)%x1f%(objectname)%x1f%(committerdate:unix)%x1f%(contents:subject)`
/// output. `%(HEAD)` is "*" on the current branch (detached HEAD simply emits
/// no starred row). An empty `%(upstream:short)` means no upstream. Non-empty
/// output must end with `\n` (git terminates every record); otherwise it is
/// rejected as truncated.
pub fn parse_branches(out: &str) -> anyhow::Result<Vec<BranchInfo>> {
    parse_branches_with_limits(out, MAX_GIT_OUTPUT_BYTES, MAX_GIT_FIELD_BYTES)
}

fn parse_branches_with_limits(
    out: &str,
    max_output: usize,
    max_field: usize,
) -> anyhow::Result<Vec<BranchInfo>> {
    check_limits(out, max_output, max_field, true)?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        out.ends_with('\n'),
        "malformed branch record: missing final newline terminator"
    );
    let mut names = std::collections::HashSet::new();
    out.split_terminator('\n')
        .enumerate()
        .map(|(line_no, line)| {
            let malformed = || anyhow::anyhow!("malformed branch row {}", line_no + 1);
            let mut it = line.splitn(7, '\u{1f}');
            let head = it.next().ok_or_else(malformed)?;
            anyhow::ensure!(
                head == "*" || head == " ",
                "malformed branch marker on row {}",
                line_no + 1
            );
            let name = it.next().ok_or_else(malformed)?.to_string();
            anyhow::ensure!(!name.is_empty(), "empty branch name on row {}", line_no + 1);
            anyhow::ensure!(
                names.insert(name.clone()),
                "duplicate branch name on row {}",
                line_no + 1
            );
            let upstream = Some(it.next().ok_or_else(malformed)?)
                .filter(|u| !u.is_empty())
                .map(String::from);
            let (ahead, behind, upstream_gone) = parse_track(it.next().ok_or_else(malformed)?)?;
            let sha = it.next().ok_or_else(malformed)?.to_string();
            anyhow::ensure!(
                valid_oid(&sha),
                "malformed branch object id on row {}",
                line_no + 1
            );
            let date =
                parse_epoch_seconds(it.next().ok_or_else(malformed)?).ok_or_else(malformed)?;
            let subject = it.next().ok_or_else(malformed)?.to_string();
            Ok(BranchInfo {
                name,
                is_head: head == "*",
                upstream,
                ahead,
                behind,
                upstream_gone,
                sha,
                date,
                subject,
            })
        })
        .collect()
}

/// One stash entry (see [`parse_stashes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// The N of "stash@{N}".
    pub index: usize,
    pub sha: String,
    /// Commit date, epoch seconds.
    pub date: i64,
    /// The reflog subject, e.g. "WIP on main: abc123 subject".
    pub message: String,
}

/// Parse the four NUL-delimited fields emitted by
/// `git stash list -z --format=%gd%x00%H%x00%ct%x00%gs`. `%gd` is the
/// selector "stash@{N}"; every record, including the last, must end with NUL;
/// output that does not is truncated and is rejected as a whole. Any
/// malformed row makes the complete parse fail.
pub fn parse_stashes(out: &str) -> anyhow::Result<Vec<StashEntry>> {
    parse_stashes_with_limits(out, MAX_GIT_OUTPUT_BYTES, MAX_GIT_FIELD_BYTES)
}

fn parse_stashes_with_limits(
    out: &str,
    max_output: usize,
    max_field: usize,
) -> anyhow::Result<Vec<StashEntry>> {
    check_limits(out, max_output, max_field, false)?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        out.ends_with('\0'),
        "malformed stash record: missing final NUL terminator"
    );
    let mut fields: Vec<&str> = out.split('\0').collect();
    if fields.last() == Some(&"") {
        fields.pop();
    }
    anyhow::ensure!(
        fields.len().is_multiple_of(4),
        "malformed stash record field count"
    );
    let mut indexes = std::collections::HashSet::new();
    fields
        .chunks_exact(4)
        .enumerate()
        .map(|(line_no, row)| {
            let malformed = || anyhow::anyhow!("malformed stash row {}", line_no + 1);
            let selector = row[0];
            let index = selector
                .strip_prefix("stash@{")
                .and_then(|v| v.strip_suffix('}'))
                .ok_or_else(malformed)?
                .parse()
                .map_err(|_| malformed())?;
            anyhow::ensure!(
                indexes.insert(index),
                "duplicate stash selector on row {}",
                line_no + 1
            );
            let sha = row[1].to_string();
            anyhow::ensure!(
                valid_oid(&sha),
                "malformed stash object id on row {}",
                line_no + 1
            );
            let date = parse_epoch_seconds(row[2]).ok_or_else(malformed)?;
            let message = row[3].to_string();
            Ok(StashEntry {
                index,
                sha,
                date,
                message,
            })
        })
        .collect()
}

/// One tag (see [`parse_tags`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInfo {
    pub name: String,
    /// The ref target: the tag object's sha when annotated, the commit sha
    /// when lightweight.
    pub sha: String,
    pub annotated: bool,
}

/// Parse `git for-each-ref refs/tags
/// --format=%(refname:short)%x1f%(objectname)%x1f%(objecttype)` output.
/// Object type "tag" means annotated; "commit" (or anything else a tag can
/// point at) means lightweight. Non-empty output must end with `\n`;
/// otherwise it is rejected as truncated.
pub fn parse_tags(out: &str) -> anyhow::Result<Vec<TagInfo>> {
    parse_tags_with_limits(out, MAX_GIT_OUTPUT_BYTES, MAX_GIT_FIELD_BYTES)
}

fn parse_tags_with_limits(
    out: &str,
    max_output: usize,
    max_field: usize,
) -> anyhow::Result<Vec<TagInfo>> {
    check_limits(out, max_output, max_field, true)?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        out.ends_with('\n'),
        "malformed tag record: missing final newline terminator"
    );
    let mut names = std::collections::HashSet::new();
    out.split_terminator('\n')
        .enumerate()
        .map(|(line_no, line)| {
            let malformed = || anyhow::anyhow!("malformed tag row {}", line_no + 1);
            let mut it = line.splitn(3, '\u{1f}');
            let name = it.next().ok_or_else(malformed)?.to_string();
            let sha = it.next().ok_or_else(malformed)?.to_string();
            let kind = it.next().ok_or_else(malformed)?;
            anyhow::ensure!(
                !name.is_empty() && !sha.is_empty(),
                "empty tag field on row {}",
                line_no + 1
            );
            anyhow::ensure!(
                valid_oid(&sha),
                "malformed tag object id on row {}",
                line_no + 1
            );
            anyhow::ensure!(
                matches!(kind, "tag" | "commit" | "tree" | "blob"),
                "unknown tag object type on row {}",
                line_no + 1
            );
            anyhow::ensure!(
                names.insert(name.clone()),
                "duplicate tag name on row {}",
                line_no + 1
            );
            Ok(TagInfo {
                name,
                sha,
                annotated: kind == "tag",
            })
        })
        .collect()
}

/// An in-progress bisect (see [`parse_bisect`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectState {
    /// The "bad" term — line 1 of BISECT_TERMS (git writes bad first, good
    /// second; defaults to "bad" when the file is absent).
    pub bad_term: String,
    /// The "good" term — line 2 of BISECT_TERMS (default "good").
    pub good_term: String,
    /// The sha marked bad, when one has been marked.
    pub bad: Option<String>,
    /// Shas marked good.
    pub good: Vec<String>,
    /// Shas marked skip.
    pub skipped: Vec<String>,
    /// The HEAD sha while bisecting (passed in by the caller).
    pub current: String,
    /// The first bad commit, once found — filled from [`find_culprit`].
    pub culprit: Option<String>,
}

/// Build bisect state from `git for-each-ref refs/bisect
/// --format=%(refname:short)%x1f%(objectname)` output, the BISECT_TERMS file
/// content (`None` → default terms "bad"/"good"), and the current HEAD sha.
/// Malformed terms, object ids, or ref rows are errors; no partial state is
/// returned. Non-empty `refs_out` must end with `\n`; otherwise it is
/// rejected as truncated.
///
/// Under refs/bisect the ref names are `<bad-term>`, `<good-term>-<sha>` and
/// `skip-<sha>` (skip is always "skip", even with custom terms). Depending on
/// ambiguity `%(refname:short)` may keep a `bisect/` or `refs/bisect/` prefix,
/// so both are stripped before matching.
pub fn parse_bisect(
    refs_out: &str,
    terms: Option<&str>,
    head: &str,
) -> anyhow::Result<BisectState> {
    // BISECT_TERMS is two lines: the bad term first, the good term second
    // (write_terms() in git's bisect.c writes them in that order).
    let (bad_term, good_term) = if let Some(terms) = terms {
        let lines = terms.lines().collect::<Vec<_>>();
        anyhow::ensure!(
            lines.len() == 2 && !lines[0].trim().is_empty() && !lines[1].trim().is_empty(),
            "malformed BISECT_TERMS"
        );
        (lines[0].trim().to_owned(), lines[1].trim().to_owned())
    } else {
        ("bad".to_owned(), "good".to_owned())
    };
    anyhow::ensure!(valid_oid(head), "malformed bisect HEAD");
    anyhow::ensure!(
        refs_out.is_empty() || refs_out.ends_with('\n'),
        "malformed bisect ref record: missing final newline terminator"
    );

    let mut state = BisectState {
        bad_term: bad_term.clone(),
        good_term: good_term.clone(),
        bad: None,
        good: Vec::new(),
        skipped: Vec::new(),
        current: head.to_string(),
        culprit: None,
    };
    let good_prefix = format!("{good_term}-");
    for (index, line) in refs_out.split_terminator('\n').enumerate() {
        let row = index + 1;
        let Some((name, sha)) = line.split_once('\u{1f}') else {
            anyhow::bail!("malformed bisect ref row {row}");
        };
        anyhow::ensure!(
            !name.is_empty() && !sha.contains('\u{1f}') && valid_oid(sha),
            "malformed bisect ref row {row}"
        );
        let name = name
            .trim_start_matches("refs/")
            .trim_start_matches("bisect/");
        if name == bad_term {
            anyhow::ensure!(
                state.bad.is_none(),
                "duplicate bisect bad mark on row {row}"
            );
            state.bad = Some(sha.to_owned());
        } else if name.strip_prefix("skip-").is_some() {
            state.skipped.push(sha.to_owned());
        } else if name.strip_prefix(good_prefix.as_str()).is_some() {
            state.good.push(sha.to_owned());
        } else {
            anyhow::bail!("unknown bisect ref on row {row}");
        }
    }
    Ok(state)
}

/// Scan bisect command stdout for the culprit line, anchored to the start of a
/// line with a 40-hex sha (so a commit subject quoting the phrase mid-line can't
/// fool it). git prints `"<full-sha> is the first <term> commit"`, where older
/// git used the bare term (`bad`) and git >= 2.55 quotes it (`'bad'`); custom
/// bisect terms flow through the same way (`'broken'`). We accept a single
/// space-free token between the fixed `is the first` / `commit` anchors, which
/// matches every form without being fooled by a subject that keeps going.
pub fn find_culprit(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (sha, rest) = line.split_at_checked(40)?;
        if !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let term = rest
            .trim_end()
            .strip_prefix(" is the first ")?
            .strip_suffix(" commit")?;
        (!term.is_empty() && !term.contains(' ')).then(|| sha.to_string())
    })
}

/// Parse `git version` output ("git version 2.43.0"; vendor suffixes like
/// "2.39.2.windows.1" only contribute their first three components) into
/// (major, minor, patch). A missing/non-numeric patch is 0; anything without
/// numeric major.minor is `None`.
pub fn parse_git_version(out: &str) -> Option<(u32, u32, u32)> {
    let mut it = out.trim().strip_prefix("git version ")?.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// True when `git stash push --staged` exists: (major, minor) >= (2, 35).
pub fn supports_stash_staged(v: (u32, u32, u32)) -> bool {
    (v.0, v.1) >= (2, 35)
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: char = '\u{1f}';

    /// Join fields into one NUL-delimited commit record.
    fn rec(fields: &[&str]) -> String {
        fields.join("\0")
    }

    fn stash_row(selector: &str, sha: &str, date: &str, message: &str) -> String {
        format!("{selector}\0{sha}\0{date}\0{message}")
    }

    // --- parse_commits ---

    #[test]
    fn commits_empty_input() {
        assert_eq!(parse_commits("").unwrap(), Vec::<Commit>::new());
    }

    #[test]
    fn commits_root_merge_and_octopus_parents() {
        let mut out = [
            rec(&[
                "a".repeat(40).as_str(),
                "aaaaaaa",
                "Ann",
                "a@e",
                "100",
                "",
                "",
                "root",
            ]),
            rec(&[
                "b".repeat(40).as_str(),
                "bbbbbbb",
                "Bob",
                "b@e",
                "200",
                &format!("{} {}", "d".repeat(40), "e".repeat(40)),
                "HEAD -> main, origin/main",
                "merge two",
            ]),
            rec(&[
                "c".repeat(40).as_str(),
                "ccccccc",
                "Cy",
                "c@e",
                "300",
                &format!("{} {} {}", "f".repeat(40), "1".repeat(40), "2".repeat(40)),
                "",
                "octopus",
            ]),
        ]
        .join("\0");
        out.push('\0');
        let v = parse_commits(&out).unwrap();
        assert_eq!(v.len(), 3);
        assert!(v[0].parents.is_empty());
        assert_eq!(v[0].refs, "");
        assert_eq!(v[0].date, 100);
        assert_eq!(v[1].parents, vec!["d".repeat(40), "e".repeat(40)]);
        assert_eq!(v[1].refs, "HEAD -> main, origin/main");
        assert_eq!(v[1].author, "Bob");
        assert_eq!(v[1].email, "b@e");
        assert_eq!(v[1].short, "bbbbbbb");
        assert_eq!(v[2].parents.len(), 3);
        assert_eq!(v[2].sha, "c".repeat(40));
    }

    #[test]
    fn commits_subject_keeps_unicode_and_edge_whitespace() {
        let out = format!(
            "{}\0",
            rec(&[
                &"a".repeat(40),
                "a",
                "Ann",
                "a@e",
                "1",
                "",
                "",
                "  héllo → wörld  ",
            ])
        );
        let v = parse_commits(&out).unwrap();
        assert_eq!(v[0].subject, "  héllo → wörld  ");
    }

    #[test]
    fn commits_control_bytes_round_trip_and_malformed_records_fail_closed() {
        let first = rec(&[
            &"a".repeat(40),
            "a",
            "A\u{1f}nn",
            "a@e",
            "7",
            "",
            "",
            "first",
        ]);
        let second = rec(&[&"b".repeat(40), "b", "Bob", "b@e", "8", "", "", "second"]);
        let parsed = parse_commits(&format!("{first}\0{second}\0")).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].author, "A\u{1f}nn");
        assert_eq!(parsed[1].email, "b@e");
        for malformed in [
            "\0".to_string(),
            format!(
                "{}\0x",
                rec(&[&"a".repeat(40), "a", "A", "a@e", "7", "", "", "s"])
            ),
            rec(&["not-a-hash", "a", "A", "a@e", "7", "", "", "s"]),
            rec(&[&"a".repeat(40), "z", "A", "a@e", "7", "", "", "s"]),
            rec(&[&"a".repeat(40), "a", "A", "a@e", "bad", "", "", "s"]),
            rec(&[&"a".repeat(40), "a", "A", "a@e", "7", "not-an-oid", "", "s"]),
            format!(
                "{}\0{}",
                rec(&[&"a".repeat(40), "a", "A", "a@e", "7", "", "", "s"]),
                rec(&[&"a".repeat(40), "a", "A", "a@e", "7", "", "", "s"])
            ),
        ] {
            assert!(parse_commits(&malformed).is_err());
        }
    }

    // --- parse_branches ---

    fn branch_row(head: &str, name: &str, upstream: &str, track: &str) -> String {
        [
            head,
            name,
            upstream,
            track,
            &"d".repeat(40),
            "42",
            "subject",
        ]
        .join(&US.to_string())
    }

    #[test]
    fn branches_every_track_form() {
        let mut out = [
            branch_row("*", "main", "origin/main", ""),
            branch_row(" ", "feat", "origin/feat", "[ahead 1]"),
            branch_row(" ", "fix", "origin/fix", "[behind 2]"),
            branch_row(" ", "both", "origin/both", "[ahead 1, behind 2]"),
            branch_row(" ", "old", "origin/old", "[gone]"),
        ]
        .join("\n");
        out.push('\n');
        let v = parse_branches(&out).unwrap();
        assert_eq!(v.len(), 5);
        assert!(v[0].is_head);
        assert_eq!((v[0].ahead, v[0].behind, v[0].upstream_gone), (0, 0, false));
        assert_eq!(v[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(v[0].sha, "d".repeat(40));
        assert_eq!(v[0].date, 42);
        assert_eq!(v[0].subject, "subject");
        assert_eq!((v[1].ahead, v[1].behind), (1, 0));
        assert_eq!((v[2].ahead, v[2].behind), (0, 2));
        assert_eq!((v[3].ahead, v[3].behind), (1, 2));
        assert!(!v[3].upstream_gone);
        assert!(v[4].upstream_gone);
        assert_eq!((v[4].ahead, v[4].behind), (0, 0));
        assert!(!v[1].is_head);
    }

    #[test]
    fn branches_no_upstream_and_weird_names() {
        let mut out = [
            branch_row(" ", "feature/deep/dotted.name", "", ""),
            branch_row("*", "release-1.2.x", "origin/release-1.2.x", "[ahead 3]"),
        ]
        .join("\n");
        out.push('\n');
        let v = parse_branches(&out).unwrap();
        assert_eq!(v[0].name, "feature/deep/dotted.name");
        assert_eq!(v[0].upstream, None);
        assert!(!v[0].upstream_gone);
        assert_eq!(v[1].name, "release-1.2.x");
        assert!(v[1].is_head);
        assert_eq!(v[1].ahead, 3);
    }

    #[test]
    fn branches_empty_and_malformed() {
        assert_eq!(parse_branches("").unwrap(), Vec::<BranchInfo>::new());
        // Too few fields / bad date fail the entire parse.
        let out = format!("*{US}only-two\n{}\n", branch_row(" ", "x", "", "")).replace("42", "NaN");
        assert!(parse_branches(&out).is_err());
        assert!(
            parse_branches(&format!(
                "{}\n",
                branch_row(" ", "x", "", "").replace("42", "NaN")
            ))
            .is_err()
        );
        assert!(
            parse_branches(&format!("{}\n", branch_row(" ", "x", "", "[ahead many]"))).is_err()
        );
        assert!(
            parse_branches(&format!(
                "{}\n{}\n",
                branch_row(" ", "same", "", ""),
                branch_row(" ", "same", "", "")
            ))
            .is_err()
        );
    }

    // --- parse_stashes ---

    #[test]
    fn stashes_indices_and_messy_messages() {
        let mut out = [
            stash_row(
                "stash@{0}",
                &"a".repeat(40),
                "10",
                "WIP on main: abc123 fix: thing {x}",
            ),
            stash_row(
                "stash@{1}",
                &"b".repeat(40),
                "20",
                "On feat: msg: with: colons",
            ),
            stash_row("stash@{2}", &"c".repeat(40), "30", "plain"),
        ]
        .join("\0");
        out.push('\0');
        let v = parse_stashes(&out).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].index, 0);
        assert_eq!(v[0].message, "WIP on main: abc123 fix: thing {x}");
        assert_eq!(v[1].index, 1);
        assert_eq!(v[1].message, "On feat: msg: with: colons");
        assert_eq!((v[2].index, v[2].date), (2, 30));
        assert_eq!(v[2].sha, "c".repeat(40));
    }

    #[test]
    fn stash_message_unit_separator_round_trips() {
        let message = "On main: a\u{1f}b";
        let out = format!(
            "{}\0",
            stash_row("stash@{0}", &"a".repeat(40), "10", message)
        );
        let parsed = parse_stashes(&out).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].message, message);
    }

    #[test]
    fn stashes_empty_and_malformed() {
        assert_eq!(parse_stashes("").unwrap(), Vec::<StashEntry>::new());
        let out = [
            stash_row("not-a-selector", &"a".repeat(40), "1", "m"),
            stash_row("stash@{x}", &"b".repeat(40), "1", "m"),
        ]
        .join("\0");
        assert!(parse_stashes(&out).is_err());
        assert!(parse_stashes(&format!("stash@{{0}}\0{}\0bad\0m", "a".repeat(40))).is_err());
        assert!(
            parse_stashes(&format!(
                "{}\0{}",
                stash_row("stash@{0}", &"a".repeat(40), "1", "a"),
                stash_row("stash@{0}", &"b".repeat(40), "2", "b")
            ))
            .is_err()
        );
        assert!(
            parse_stashes(&format!(
                "{}\0{}",
                stash_row("stash@{0}", "bad", "1", "a"),
                stash_row("stash@{1}", &"b".repeat(40), "2", "b")
            ))
            .is_err()
        );
    }

    #[test]
    fn malformed_gitref_fields_are_rejected() {
        let sha = "a".repeat(40);
        let cases = [
            ("track missing bracket", parse_track("[ahead 1").map(|_| ())),
            ("track missing count", parse_track("[ahead]").map(|_| ())),
            ("track extra data", parse_track("[ahead 1 x]").map(|_| ())),
            (
                "track duplicate field",
                parse_track("[ahead 1, ahead 2]").map(|_| ()),
            ),
            (
                "track unknown field",
                parse_track("[sideways 1]").map(|_| ()),
            ),
            ("track empty field", parse_track("[]").map(|_| ())),
            (
                "tag unknown object type",
                parse_tags(&format!("v1{US}{sha}{US}unknown\n")).map(|_| ()),
            ),
            (
                "tag empty name",
                parse_tags(&format!("{US}{sha}{US}commit\n")).map(|_| ()),
            ),
            (
                "stash extra field",
                parse_stashes(&format!("stash@{{0}}\0{sha}\01\0message\0extra")).map(|_| ()),
            ),
        ];
        for (case, result) in cases {
            assert!(result.is_err(), "{case} should be rejected");
        }
    }

    // --- parse_tags ---

    #[test]
    fn tags_annotated_vs_lightweight() {
        let out = format!(
            "v1.0{US}{}{US}tag\nv1.1{US}{}{US}commit\n",
            "a".repeat(40),
            "b".repeat(40)
        );
        let v = parse_tags(&out).unwrap();
        assert_eq!(v.len(), 2);
        assert!(v[0].annotated);
        assert_eq!(
            (v[0].name.as_str(), v[0].sha.as_str()),
            ("v1.0", "a".repeat(40).as_str())
        );
        assert!(!v[1].annotated);
        assert_eq!(v[1].name, "v1.1");
        assert_eq!(parse_tags("").unwrap(), Vec::<TagInfo>::new());
        assert!(parse_tags(&format!("{out}bad-line\n")).is_err());
        assert!(
            parse_tags(&format!(
                "v1{US}{}{US}commit\nv1{US}{}{US}commit\n",
                "a".repeat(40),
                "b".repeat(40)
            ))
            .is_err()
        );
        assert!(parse_tags(&format!("v1{US}a{US}commit\nv1{US}b{US}commit\n")).is_err());
    }

    // --- parse_bisect / find_culprit ---

    #[test]
    fn bisect_default_terms_with_marks() {
        let bad = "b".repeat(40);
        let good_one = "1".repeat(40);
        let good_two = "2".repeat(40);
        let skipped = "3".repeat(40);
        let head = "f".repeat(40);
        let refs = format!(
            "bad{US}{bad}\ngood-{good_one}{US}{good_one}\ngood-{good_two}{US}{good_two}\nskip-{skipped}{US}{skipped}\n",
        );
        let st = parse_bisect(&refs, None, &head).unwrap();
        assert_eq!(st.bad_term, "bad");
        assert_eq!(st.good_term, "good");
        assert_eq!(st.bad.as_deref(), Some(bad.as_str()));
        assert_eq!(st.good, vec![good_one, good_two]);
        assert_eq!(st.skipped, vec![skipped]);
        assert_eq!(st.current, head);
        assert_eq!(st.culprit, None);
    }

    #[test]
    fn bisect_custom_terms() {
        // BISECT_TERMS: bad term on line 1, good term on line 2.
        let bad = "b".repeat(40);
        let good = "1".repeat(40);
        let skipped = "2".repeat(40);
        let head = "f".repeat(40);
        let refs =
            format!("broken{US}{bad}\nfixed-{good}{US}{good}\nskip-{skipped}{US}{skipped}\n");
        let st = parse_bisect(&refs, Some("broken\nfixed\n"), &head).unwrap();
        assert_eq!(
            (st.bad_term.as_str(), st.good_term.as_str()),
            ("broken", "fixed")
        );
        assert_eq!(st.bad.as_deref(), Some(bad.as_str()));
        assert_eq!(st.good, vec![good]);
        assert_eq!(st.skipped, vec![skipped]);
    }

    #[test]
    fn bisect_nothing_marked_and_prefixed_refnames() {
        let head = "f".repeat(40);
        let bad = "b".repeat(40);
        let good = "1".repeat(40);
        let st = parse_bisect("", None, &head).unwrap();
        assert_eq!(st.bad, None);
        assert!(st.good.is_empty() && st.skipped.is_empty());
        // refname:short may keep a bisect/ or refs/bisect/ prefix.
        let refs = format!("bisect/bad{US}{bad}\nrefs/bisect/good-{good}{US}{good}\n");
        let st = parse_bisect(&refs, None, &head).unwrap();
        assert_eq!(st.bad.as_deref(), Some(bad.as_str()));
        assert_eq!(st.good, vec![good]);
    }

    #[test]
    fn bisect_rows_are_all_or_error() {
        let sha = "a".repeat(40);
        let other_sha = "b".repeat(40);
        let head = "f".repeat(40);
        let cases = [
            ("separator-less row", "bad".to_owned()),
            ("two separators", format!("bad{US}{sha}{US}{other_sha}")),
            ("empty name", format!("{US}{sha}")),
            ("short object id", format!("bad{US}abc")),
            ("unknown ref", format!("other{US}{sha}")),
            (
                "duplicate bad mark",
                format!("bad{US}{sha}\nbad{US}{other_sha}"),
            ),
        ];
        for (case, refs) in cases {
            assert!(
                parse_bisect(&format!("{refs}\n"), None, &head).is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn bisect_terms_and_head_are_validated() {
        let head = "f".repeat(40);
        for (case, terms) in [
            ("empty terms", ""),
            ("one term", "broken\n"),
            ("three terms", "a\nb\nc\n"),
            ("blank bad term", " \nfixed\n"),
            ("blank good term", "broken\n \n"),
        ] {
            assert!(parse_bisect("", Some(terms), &head).is_err(), "{case}");
        }
        assert!(parse_bisect("", None, "H").is_err(), "invalid HEAD");

        let st = parse_bisect("", Some("broken\nfixed\n"), &head).unwrap();
        assert_eq!(st.bad_term, "broken");
        assert_eq!(st.good_term, "fixed");
    }

    #[test]
    fn ref_rows_without_the_final_newline_are_rejected() {
        let head = "f".repeat(40);
        let branch = branch_row(" ", "main", "", "");
        let tag = format!("v1{US}{}{US}commit", "a".repeat(40));
        let bisect = format!("bad{US}{}", "b".repeat(40));

        let err = parse_branches(&branch).unwrap_err().to_string();
        assert!(err.contains("final newline"), "{err}");
        assert_eq!(parse_branches(&format!("{branch}\n")).unwrap().len(), 1);

        let err = parse_tags(&tag).unwrap_err().to_string();
        assert!(err.contains("final newline"), "{err}");
        assert_eq!(parse_tags(&format!("{tag}\n")).unwrap().len(), 1);

        let err = parse_bisect(&bisect, None, &head).unwrap_err().to_string();
        assert!(err.contains("final newline"), "{err}");
        assert!(parse_bisect(&format!("{bisect}\n"), None, &head).is_ok());
    }

    #[test]
    fn ref_parsers_accept_empty_output_and_truncated_last_field_is_not_silently_kept() {
        let head = "f".repeat(40);
        assert!(parse_branches("").unwrap().is_empty());
        assert!(parse_tags("").unwrap().is_empty());
        assert_eq!(parse_bisect("", None, &head).unwrap().bad, None);

        // Output cut off inside the free-text subject of the last record.
        let sha = "d".repeat(40);
        let truncated = format!(
            "{}\n*{US}b{US}{US}{US}{sha}{US}1{US}trunc",
            branch_row(" ", "a", "", "")
        );
        assert!(parse_branches(&truncated).is_err());
    }

    #[test]
    fn culprit_anchored_to_line_start_full_sha() {
        let sha = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let out = format!("bisecting...\n{sha} is the first bad commit\nAuthor: x");
        assert_eq!(find_culprit(&out).as_deref(), Some(sha));
        // Negative: no such line.
        assert_eq!(find_culprit("still bisecting\n"), None);
        assert_eq!(find_culprit(""), None);
        // Not fooled by the phrase mid-line in a subject, a short sha, or a
        // non-hex prefix.
        assert_eq!(
            find_culprit(&format!("    say {sha} is the first bad commit")),
            None
        );
        assert_eq!(find_culprit("abc123 is the first bad commit"), None);
        assert_eq!(
            find_culprit(&format!("{} is the first bad commit", "z".repeat(40))),
            None
        );
        // The sha must be followed by exactly the phrase.
        assert_eq!(
            find_culprit(&format!("{sha} is the first bad commit, maybe")),
            None
        );
        // Trailing whitespace (CRLF) is fine.
        assert_eq!(
            find_culprit(&format!("{sha} is the first bad commit\r\n")).as_deref(),
            Some(sha)
        );
        // git >= 2.55 quotes the term; custom terms flow through the same shape.
        assert_eq!(
            find_culprit(&format!("{sha} is the first 'bad' commit")).as_deref(),
            Some(sha)
        );
        assert_eq!(
            find_culprit(&format!("{sha} is the first 'broken' commit\r\n")).as_deref(),
            Some(sha)
        );
        // A multi-word tail between the anchors is still rejected.
        assert_eq!(
            find_culprit(&format!("{sha} is the first really bad commit")),
            None
        );
    }

    // --- version ---

    #[test]
    fn version_parses_and_rejects() {
        assert_eq!(parse_git_version("git version 2.43.0\n"), Some((2, 43, 0)));
        assert_eq!(parse_git_version("git version 2.30.1"), Some((2, 30, 1)));
        assert_eq!(
            parse_git_version("git version 2.39.2.windows.1"),
            Some((2, 39, 2))
        );
        // Two-component versions get patch 0.
        assert_eq!(parse_git_version("git version 2.30"), Some((2, 30, 0)));
        assert_eq!(parse_git_version("garbage"), None);
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("git version x.y.z"), None);
        assert_eq!(parse_git_version("git version 2.nope.0"), None);
    }

    #[test]
    fn stash_staged_gate_boundary() {
        assert!(!supports_stash_staged((2, 34, 9)));
        assert!(supports_stash_staged((2, 35, 0)));
        assert!(supports_stash_staged((2, 36, 1)));
        assert!(supports_stash_staged((3, 0, 0)));
    }

    #[test]
    fn commits_missing_final_terminator_is_rejected_not_truncated() {
        let record =
            |sha: &str, subject: &str| rec(&[sha, "aaaaaaa", "Ann", "a@e", "1", "", "", subject]);
        let first = record(&"a".repeat(40), "a complete subject");
        let parsed = parse_commits(&format!("{first}\0")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].subject, "a complete subject");

        let error = parse_commits(&first).unwrap_err();
        assert!(error.to_string().contains("terminator"), "{error}");

        let second_cut = record(&"b".repeat(40), "a cut sub");
        assert!(parse_commits(&format!("{first}\0{second_cut}")).is_err());
    }

    #[test]
    fn stashes_missing_final_terminator_is_rejected_not_truncated() {
        let row = stash_row(
            "stash@{0}",
            &"a".repeat(40),
            "10",
            "WIP on main: complete message",
        );
        let parsed = parse_stashes(&format!("{row}\0")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].message, "WIP on main: complete message");

        let error = parse_stashes(&row).unwrap_err();
        assert!(error.to_string().contains("terminator"), "{error}");
    }

    #[test]
    fn terminated_empty_chunk_only_input_still_reports_empty_record() {
        assert!(parse_commits("\0").is_err());
        assert!(parse_stashes("\0").is_err());
    }

    // --- truncation / mutation / shape tables (THE-383) ---

    /// Subject holding tab, newline, a non-ASCII char and an embedded `\r`.
    const HOSTILE: &str = "tab\there\nnew line \u{e9}\rcr";
    /// Same, minus the newline (line-oriented parsers cannot carry one).
    const LINE_HOSTILE: &str = "tab\there \u{e9}\rcr";

    fn oid(n: usize) -> String {
        format!("{n:040x}")
    }

    /// Cumulative byte offsets of every record boundary, starting at 0.
    fn boundaries(records: &[String]) -> Vec<usize> {
        let mut out = vec![0usize];
        let mut total = 0usize;
        for record in records {
            total += record.len();
            out.push(total);
        }
        out
    }

    /// Cut `input` at every char boundary: `Ok` only on a record boundary
    /// (with the whole-record count), `Err` everywhere else.
    fn assert_truncation_table<T>(
        input: &str,
        bounds: &[usize],
        parse: impl Fn(&str) -> anyhow::Result<Vec<T>>,
    ) {
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            match (bounds.iter().position(|b| *b == cut), parse(&input[..cut])) {
                (Some(records), Ok(rows)) => assert_eq!(rows.len(), records, "cut {cut}"),
                (Some(_), Err(error)) => {
                    panic!("cut {cut} on a record boundary must parse: {error}")
                }
                (None, Ok(rows)) => {
                    panic!(
                        "cut {cut} mid-record must be rejected, got {} rows",
                        rows.len()
                    )
                }
                (None, Err(_)) => {}
            }
        }
    }

    fn assert_rejects<T>(
        table: &[(&str, String, &str)],
        parse: impl Fn(&str) -> anyhow::Result<Vec<T>>,
    ) {
        for (label, input, expect) in table {
            match parse(input) {
                Ok(rows) => panic!(
                    "{label}: expected error containing {expect:?}, parsed {} rows",
                    rows.len()
                ),
                Err(error) => assert!(error.to_string().contains(expect), "{label}: {error}"),
            }
        }
    }

    fn commit_fields(n: usize, subject: &str) -> Vec<String> {
        let sha = oid(n);
        let parent = if n == 0 { String::new() } else { oid(n - 1) };
        vec![
            sha.clone(),
            sha[..7].to_string(),
            "Ann \u{d6}".to_string(),
            "a@e".to_string(),
            format!("{}", 100 + n),
            parent,
            "HEAD -> main".to_string(),
            subject.to_string(),
        ]
    }

    fn nul_record(fields: &[String]) -> String {
        let mut out = fields.join("\0");
        out.push('\0');
        out
    }

    fn commit_records(count: usize) -> Vec<String> {
        let mut records = Vec::new();
        for n in 0..count {
            records.push(nul_record(&commit_fields(n, HOSTILE)));
        }
        records
    }

    fn commit_with(index: usize, value: &str) -> String {
        let mut fields = commit_fields(1, "subject");
        fields[index] = value.to_string();
        nul_record(&fields)
    }

    fn stash_fields(n: usize) -> Vec<String> {
        vec![
            format!("stash@{{{n}}}"),
            oid(n + 1),
            format!("{}", 200 + n),
            format!("WIP on main: {HOSTILE}"),
        ]
    }

    fn stash_records(count: usize) -> Vec<String> {
        let mut records = Vec::new();
        for n in 0..count {
            records.push(nul_record(&stash_fields(n)));
        }
        records
    }

    fn table_branch_row(n: usize) -> String {
        let (head, upstream, track) = match n % 3 {
            0 => ("*", "origin/main", "[ahead 1, behind 2]"),
            1 => (" ", "", ""),
            _ => (" ", "origin/gone", "[gone]"),
        };
        format!(
            "{head}{US}feat/\u{e9}-{n}{US}{upstream}{US}{track}{US}{}{US}{}{US}{LINE_HOSTILE}\n",
            oid(n + 1),
            300 + n
        )
    }

    fn branch_rows(count: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for n in 0..count {
            rows.push(table_branch_row(n));
        }
        rows
    }

    fn tag_row(n: usize) -> String {
        let kind = if n.is_multiple_of(2) { "tag" } else { "commit" };
        format!("rel/\u{e9}-{n}{US}{}{US}{kind}\n", oid(n + 1))
    }

    fn tag_rows(count: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for n in 0..count {
            rows.push(tag_row(n));
        }
        rows
    }

    fn bisect_head() -> String {
        "f".repeat(40)
    }

    /// Row count of a parsed bisect state: bad mark + good + skip refs.
    fn bisect_rows(refs: &str) -> anyhow::Result<Vec<()>> {
        let state = parse_bisect(refs, None, &bisect_head())?;
        let count = usize::from(state.bad.is_some()) + state.good.len() + state.skipped.len();
        Ok(vec![(); count])
    }

    fn bisect_row_list(count: usize) -> Vec<String> {
        let mut rows = vec![format!("bad{US}{}\n", oid(1))];
        for n in 0..count.saturating_sub(1) {
            if n.is_multiple_of(2) {
                rows.push(format!("good-{}{US}{}\n", oid(n + 10), oid(n + 10)));
            } else {
                rows.push(format!("skip-{}{US}{}\n", oid(n + 10), oid(n + 10)));
            }
        }
        rows.truncate(count);
        rows
    }

    /// Every control byte Git can store in a text field (0x00 frames records).
    fn control_bytes() -> Vec<u8> {
        let mut bytes: Vec<u8> = (0x01u8..=0x1f).collect();
        bytes.push(0x7f);
        bytes
    }

    #[test]
    fn commits_every_control_byte_round_trips_without_shifting_fields() {
        for b in control_bytes() {
            let c = char::from(b);
            let author = format!("A{c}nn");
            let email = format!("a{c}@e");
            let subject = format!("sub{c}ject{c}");
            let mut mid = commit_fields(1, &subject);
            mid[2] = author.clone();
            mid[3] = email.clone();
            let records = [
                nul_record(&commit_fields(0, "first")),
                nul_record(&mid),
                nul_record(&commit_fields(2, "third")),
            ];
            let parsed = parse_commits(&records.concat())
                .unwrap_or_else(|e| panic!("byte {b:#04x} must parse: {e}"));
            assert_eq!(parsed.len(), 3, "byte {b:#04x}");
            let expect_first = parse_commits(&records[0]).unwrap();
            let expect_third = parse_commits(&records[2]).unwrap();
            assert_eq!(parsed[0], expect_first[0], "byte {b:#04x}");
            assert_eq!(parsed[2], expect_third[0], "byte {b:#04x}");
            assert_eq!(parsed[1].author, author, "byte {b:#04x}");
            assert_eq!(parsed[1].email, email, "byte {b:#04x}");
            assert_eq!(parsed[1].subject, subject, "byte {b:#04x}");
            assert_eq!(parsed[1].sha, oid(1), "byte {b:#04x}");
            assert_eq!(parsed[1].date, 101, "byte {b:#04x}");
        }
    }

    #[test]
    fn stashes_every_control_byte_round_trips_without_shifting_fields() {
        for b in control_bytes() {
            let c = char::from(b);
            let message = format!("WIP{c}on{c}main");
            let mut mid = stash_fields(1);
            mid[3] = message.clone();
            let records = [
                nul_record(&stash_fields(0)),
                nul_record(&mid),
                nul_record(&stash_fields(2)),
            ];
            let parsed = parse_stashes(&records.concat())
                .unwrap_or_else(|e| panic!("byte {b:#04x} must parse: {e}"));
            assert_eq!(parsed.len(), 3, "byte {b:#04x}");
            assert_eq!(parsed[0], parse_stashes(&records[0]).unwrap()[0]);
            assert_eq!(parsed[2], parse_stashes(&records[2]).unwrap()[0]);
            assert_eq!(parsed[1].message, message, "byte {b:#04x}");
            assert_eq!(parsed[1].index, 1, "byte {b:#04x}");
            assert_eq!(parsed[1].sha, oid(2), "byte {b:#04x}");
        }
    }

    #[test]
    fn commit_and_stash_truncation_prefixes_equal_the_leading_records() {
        let records = commit_records(3);
        let input: String = records.concat();
        let bounds = boundaries(&records);
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            if let Ok(rows) = parse_commits(&input[..cut]) {
                let k = bounds.iter().position(|b| *b == cut).expect("boundary");
                let expect = parse_commits(&records[..k].concat()).unwrap();
                assert_eq!(rows, expect, "cut {cut}");
            }
        }
        let records = stash_records(3);
        let input: String = records.concat();
        let bounds = boundaries(&records);
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            if let Ok(rows) = parse_stashes(&input[..cut]) {
                let k = bounds.iter().position(|b| *b == cut).expect("boundary");
                let expect = parse_stashes(&records[..k].concat()).unwrap();
                assert_eq!(rows, expect, "cut {cut}");
            }
        }
    }

    #[test]
    fn commits_truncation_table_every_byte_offset() {
        let records = commit_records(4);
        let input: String = records.concat();
        assert_truncation_table(&input, &boundaries(&records), parse_commits);
    }

    #[test]
    fn commits_hostile_subject_round_trips() {
        let records = commit_records(2);
        let parsed = parse_commits(&records.concat()).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].subject, HOSTILE);
        assert_eq!(parsed[1].author, "Ann \u{d6}");
        assert_eq!(parsed[1].parents, vec![oid(0)]);
    }

    #[test]
    fn commits_mutation_table() {
        let good = commit_with(7, "subject");
        let dup = format!("{good}{good}");
        let mut nine = commit_fields(1, "subject");
        nine.push("extra".to_string());
        let mut missing = commit_fields(1, "subject");
        missing.remove(6);
        let doubled = good.replacen('\0', "\0\0", 1);
        let table = vec![
            (
                "non-hex oid",
                commit_with(0, &"z".repeat(40)),
                "malformed commit object id",
            ),
            (
                "39-char oid",
                commit_with(0, &"a".repeat(39)),
                "malformed commit object id",
            ),
            (
                "41-char oid",
                commit_with(0, &"a".repeat(41)),
                "malformed commit object id",
            ),
            (
                "empty oid",
                commit_with(0, ""),
                "malformed commit object id",
            ),
            (
                "non-hex short",
                commit_with(1, "zzzzzzz"),
                "malformed abbreviated",
            ),
            ("empty short", commit_with(1, ""), "malformed abbreviated"),
            (
                "short not a prefix",
                commit_with(1, "1234567"),
                "malformed abbreviated",
            ),
            (
                "letters in date",
                commit_with(4, "12ab"),
                "malformed commit timestamp",
            ),
            (
                "empty date",
                commit_with(4, ""),
                "malformed commit timestamp",
            ),
            (
                "fractional date",
                commit_with(4, "1.5"),
                "malformed commit timestamp",
            ),
            (
                "overflow date",
                commit_with(4, "99999999999999999999"),
                "malformed commit timestamp",
            ),
            (
                "non-hex parent",
                commit_with(5, &"z".repeat(40)),
                "malformed parent object id",
            ),
            (
                "short parent",
                commit_with(5, "abc"),
                "malformed parent object id",
            ),
            ("duplicate record", dup, "duplicate commit object id"),
            ("extra trailing NUL", format!("{good}\0"), "field count"),
            ("doubled NUL", doubled, "field count"),
            ("missing field", nul_record(&missing), "field count"),
            ("extra field", nul_record(&nine), "field count"),
            (
                "no final NUL",
                good.trim_end_matches('\0').to_string(),
                "final NUL",
            ),
            ("lone NUL", "\0".to_string(), "malformed commit record"),
        ];
        assert_rejects(&table, parse_commits);
    }

    #[test]
    fn commits_signed_timestamp_is_rejected() {
        // `%ct` is plain decimal digits; a sign is corruption, as in reflog.
        let table = [
            (
                "plus sign",
                commit_with(4, "+100"),
                "malformed commit timestamp",
            ),
            (
                "minus sign",
                commit_with(4, "-5"),
                "malformed commit timestamp",
            ),
        ];
        assert_rejects(&table, parse_commits);
    }

    #[test]
    fn commits_minimal_and_thousand_record_inputs() {
        let one = commit_records(1);
        assert_eq!(parse_commits(&one.concat()).unwrap().len(), 1);
        let records = commit_records(1000);
        let parsed = parse_commits(&records.concat()).unwrap();
        assert_eq!(parsed.len(), 1000);
        assert_eq!(parsed[999].sha, oid(999));
    }

    #[test]
    fn commits_five_thousand_records_parse() {
        let records = commit_records(5000);
        assert_eq!(parse_commits(&records.concat()).unwrap().len(), 5000);
    }

    #[test]
    fn stashes_truncation_table_every_byte_offset() {
        let records = stash_records(4);
        let input: String = records.concat();
        assert_truncation_table(&input, &boundaries(&records), parse_stashes);
    }

    #[test]
    fn stashes_mutation_table() {
        let with = |index: usize, value: &str| {
            let mut fields = stash_fields(1);
            fields[index] = value.to_string();
            nul_record(&fields)
        };
        let good = nul_record(&stash_fields(1));
        let mut missing = stash_fields(1);
        missing.remove(2);
        let table = vec![
            ("empty selector", with(0, ""), "malformed stash row 1"),
            (
                "non-numeric selector",
                with(0, "stash@{x}"),
                "malformed stash row 1",
            ),
            (
                "negative selector",
                with(0, "stash@{-1}"),
                "malformed stash row 1",
            ),
            ("bare selector", with(0, "stash"), "malformed stash row 1"),
            (
                "non-hex oid",
                with(1, &"z".repeat(40)),
                "malformed stash object id",
            ),
            (
                "short oid",
                with(1, &"a".repeat(39)),
                "malformed stash object id",
            ),
            ("letters in date", with(2, "12ab"), "malformed stash row 1"),
            ("empty date", with(2, ""), "malformed stash row 1"),
            ("plus-signed date", with(2, "+100"), "malformed stash row 1"),
            ("minus-signed date", with(2, "-5"), "malformed stash row 1"),
            (
                "duplicate selector",
                format!("{good}{good}"),
                "duplicate stash selector",
            ),
            ("extra trailing NUL", format!("{good}\0"), "field count"),
            ("doubled NUL", good.replacen('\0', "\0\0", 1), "field count"),
            ("missing field", nul_record(&missing), "field count"),
            (
                "no final NUL",
                good.trim_end_matches('\0').to_string(),
                "final NUL",
            ),
        ];
        assert_rejects(&table, parse_stashes);
    }

    #[test]
    fn stashes_duplicate_sha_with_distinct_selectors_is_legal() {
        let mut second = stash_fields(1);
        second[1] = oid(1);
        let input = format!("{}{}", nul_record(&stash_fields(0)), nul_record(&second));
        let parsed = parse_stashes(&input).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].sha, parsed[1].sha);
        assert_eq!(parsed[0].message, format!("WIP on main: {HOSTILE}"));
    }

    #[test]
    fn stashes_minimal_thousand_and_five_thousand() {
        assert_eq!(parse_stashes(&stash_records(1).concat()).unwrap().len(), 1);
        assert_eq!(
            parse_stashes(&stash_records(1000).concat()).unwrap().len(),
            1000
        );
        let parsed = parse_stashes(&stash_records(5000).concat()).unwrap();
        assert_eq!(parsed.len(), 5000);
        assert_eq!(parsed[4999].index, 4999);
    }

    #[test]
    fn branches_truncation_table_every_byte_offset() {
        let rows = branch_rows(4);
        let input: String = rows.concat();
        assert_truncation_table(&input, &boundaries(&rows), parse_branches);
    }

    #[test]
    fn branches_mutation_table() {
        let row_with = |index: usize, value: &str| {
            let mut fields: [String; 7] = [
                "*".to_string(),
                "main".to_string(),
                "origin/main".to_string(),
                "[ahead 1, behind 2]".to_string(),
                oid(1),
                "300".to_string(),
                "subject".to_string(),
            ];
            fields[index] = value.to_string();
            format!("{}\n", fields.join("\u{1f}"))
        };
        let good = row_with(1, "main");
        let short = good.replacen("\u{1f}subject", "", 1);
        let doubled = good.replacen("main\u{1f}", "main\u{1f}\u{1f}", 1);
        let table = vec![
            (
                "bad head marker",
                row_with(0, "x"),
                "malformed branch marker on row 1",
            ),
            ("empty name", row_with(1, ""), "empty branch name on row 1"),
            (
                "bad track",
                row_with(3, "[ahead x]"),
                "invalid upstream tracking count",
            ),
            (
                "unbracketed track",
                row_with(3, "ahead 1"),
                "malformed upstream tracking field",
            ),
            (
                "empty bracket",
                row_with(3, "[]"),
                "empty upstream tracking",
            ),
            (
                "non-hex oid",
                row_with(4, &"z".repeat(40)),
                "malformed branch object id",
            ),
            (
                "short oid",
                row_with(4, &"a".repeat(39)),
                "malformed branch object id",
            ),
            ("empty oid", row_with(4, ""), "malformed branch object id"),
            (
                "letters in date",
                row_with(5, "12ab"),
                "malformed branch row 1",
            ),
            ("empty date", row_with(5, ""), "malformed branch row 1"),
            ("missing field", short, "malformed branch row 1"),
            (
                "doubled separator",
                doubled,
                "malformed upstream tracking field",
            ),
            (
                "duplicate name",
                format!("{good}{good}"),
                "duplicate branch name on row 2",
            ),
            (
                "extra blank line",
                format!("{good}\n"),
                "malformed branch marker on row 2",
            ),
            (
                "no final newline",
                good.trim_end_matches('\n').to_string(),
                "final newline",
            ),
        ];
        assert_rejects(&table, parse_branches);
    }

    #[test]
    fn branches_signed_date_is_rejected_and_extra_separator_fields_are_accepted() {
        let row = |date: &str| format!("*{US}main{US}{US}{US}{}{US}{date}{US}a{US}b\n", oid(1));
        let table = [
            ("plus-signed date", row("+5"), "malformed branch row 1"),
            ("minus-signed date", row("-5"), "malformed branch row 1"),
        ];
        assert_rejects(&table, parse_branches);
        let parsed = parse_branches(&row("5")).unwrap();
        assert_eq!(parsed[0].date, 5);
        assert_eq!(parsed[0].subject, format!("a{US}b"));
    }

    #[test]
    fn branches_minimal_thousand_and_five_thousand() {
        assert_eq!(parse_branches(&branch_rows(1).concat()).unwrap().len(), 1);
        assert_eq!(
            parse_branches(&branch_rows(1000).concat()).unwrap().len(),
            1000
        );
        let parsed = parse_branches(&branch_rows(5000).concat()).unwrap();
        assert_eq!(parsed.len(), 5000);
        assert_eq!(parsed[4999].subject, LINE_HOSTILE);
    }

    #[test]
    fn tags_truncation_table_every_byte_offset() {
        let rows = tag_rows(4);
        let input: String = rows.concat();
        assert_truncation_table(&input, &boundaries(&rows), parse_tags);
    }

    #[test]
    fn tags_mutation_table() {
        let row_with = |index: usize, value: &str| {
            let mut fields = ["v1".to_string(), oid(1), "tag".to_string()];
            fields[index] = value.to_string();
            format!("{}\n", fields.join("\u{1f}"))
        };
        let good = row_with(0, "v1");
        let table = vec![
            ("empty name", row_with(0, ""), "empty tag field on row 1"),
            ("empty oid", row_with(1, ""), "empty tag field on row 1"),
            (
                "non-hex oid",
                row_with(1, &"z".repeat(40)),
                "malformed tag object id",
            ),
            (
                "short oid",
                row_with(1, &"a".repeat(39)),
                "malformed tag object id",
            ),
            (
                "unknown type",
                row_with(2, "banana"),
                "unknown tag object type",
            ),
            ("empty type", row_with(2, ""), "unknown tag object type"),
            (
                "extra field",
                format!("v1{US}{}{US}tag{US}x\n", oid(1)),
                "unknown tag object type",
            ),
            (
                "missing field",
                format!("v1{US}{}\n", oid(1)),
                "malformed tag row 1",
            ),
            (
                "duplicate name",
                format!("{good}{good}"),
                "duplicate tag name on row 2",
            ),
            (
                "extra blank line",
                format!("{good}\n"),
                "malformed tag row 2",
            ),
            (
                "no final newline",
                good.trim_end_matches('\n').to_string(),
                "final newline",
            ),
        ];
        assert_rejects(&table, parse_tags);
    }

    #[test]
    fn tags_minimal_thousand_and_five_thousand() {
        assert_eq!(parse_tags(&tag_rows(1).concat()).unwrap().len(), 1);
        assert_eq!(parse_tags(&tag_rows(1000).concat()).unwrap().len(), 1000);
        let parsed = parse_tags(&tag_rows(5000).concat()).unwrap();
        assert_eq!(parsed.len(), 5000);
        assert!(parsed[0].annotated && !parsed[1].annotated);
    }

    #[test]
    fn bisect_truncation_table_every_byte_offset() {
        let rows = bisect_row_list(5);
        let input: String = rows.concat();
        assert_truncation_table(&input, &boundaries(&rows), bisect_rows);
    }

    #[test]
    fn bisect_mutation_table() {
        let head = bisect_head();
        let bad = format!("bad{US}{}\n", oid(1));
        let refs_table = vec![
            (
                "no final newline",
                bad.trim_end_matches('\n').to_string(),
                "final newline",
            ),
            (
                "no separator",
                "bad\n".to_string(),
                "malformed bisect ref row 1",
            ),
            (
                "empty name",
                format!("{US}{}\n", oid(1)),
                "malformed bisect ref row 1",
            ),
            (
                "non-hex oid",
                format!("bad{US}{}\n", "z".repeat(40)),
                "malformed bisect ref row 1",
            ),
            (
                "short oid",
                format!("bad{US}{}\n", "a".repeat(39)),
                "malformed bisect ref row 1",
            ),
            (
                "empty oid",
                format!("bad{US}\n"),
                "malformed bisect ref row 1",
            ),
            (
                "extra separator",
                format!("bad{US}{}{US}x\n", oid(1)),
                "malformed bisect ref row 1",
            ),
            (
                "duplicate bad",
                format!("{bad}{bad}"),
                "duplicate bisect bad mark on row 2",
            ),
            (
                "unknown ref",
                format!("other{US}{}\n", oid(1)),
                "unknown bisect ref on row 1",
            ),
            (
                "extra blank line",
                format!("{bad}\n"),
                "malformed bisect ref row 2",
            ),
        ];
        for (label, input, expect) in &refs_table {
            let error = parse_bisect(input, None, &head).expect_err(label);
            assert!(error.to_string().contains(expect), "{label}: {error}");
        }
        let head_and_terms = [
            (
                "bad head",
                parse_bisect("", None, &"z".repeat(40)),
                "malformed bisect HEAD",
            ),
            (
                "short head",
                parse_bisect("", None, "abc"),
                "malformed bisect HEAD",
            ),
            (
                "one-line terms",
                parse_bisect("", Some("bad\n"), &head),
                "malformed BISECT_TERMS",
            ),
            (
                "three-line terms",
                parse_bisect("", Some("a\nb\nc\n"), &head),
                "malformed BISECT_TERMS",
            ),
            (
                "blank term",
                parse_bisect("", Some("bad\n\n"), &head),
                "malformed BISECT_TERMS",
            ),
        ];
        for (label, result, expect) in head_and_terms {
            let error = result.expect_err(label);
            assert!(error.to_string().contains(expect), "{label}: {error}");
        }
    }

    #[test]
    fn bisect_duplicate_good_marks_are_legal() {
        let row = format!("good-{}{US}{}\n", oid(5), oid(5));
        let state = parse_bisect(&format!("{row}{row}"), None, &bisect_head()).unwrap();
        assert_eq!(state.good, vec![oid(5), oid(5)]);
    }

    #[test]
    fn bisect_minimal_thousand_and_five_thousand() {
        assert_eq!(bisect_rows(&bisect_row_list(1).concat()).unwrap().len(), 1);
        assert_eq!(
            bisect_rows(&bisect_row_list(1000).concat()).unwrap().len(),
            1000
        );
        assert_eq!(
            bisect_rows(&bisect_row_list(5000).concat()).unwrap().len(),
            5000
        );
    }

    // --- parser byte caps (THE-383) ---

    /// Exactly-at-cap passes, one byte over fails, for both limits.
    fn assert_caps_edges<T: std::fmt::Debug>(
        input: &str,
        newline_delim: bool,
        parse: impl Fn(&str, usize, usize) -> anyhow::Result<Vec<T>>,
    ) {
        let total = input.len();
        let longest = input
            .split(|c: char| c == '\0' || c == US || (newline_delim && c == '\n'))
            .map(str::len)
            .max()
            .unwrap();
        parse(input, total, longest).unwrap();
        parse(input, usize::MAX, usize::MAX).unwrap();
        let over = parse(input, total - 1, longest).unwrap_err().to_string();
        assert!(over.contains("whole-output cap"), "{over}");
        let field = parse(input, total, longest - 1).unwrap_err().to_string();
        assert!(field.contains("per-field cap"), "{field}");
    }

    fn assert_field_over_cap<T: std::fmt::Debug>(
        input: &str,
        parse: impl Fn(&str, usize, usize) -> anyhow::Result<Vec<T>>,
    ) {
        parse(input, usize::MAX, usize::MAX).unwrap();
        let error = parse(input, usize::MAX, 100).unwrap_err().to_string();
        assert!(error.contains("per-field cap"), "{error}");
    }

    #[test]
    fn caps_check_git_output_delimiters() {
        assert!(check_git_output("", 0, 0).is_ok());
        assert!(check_git_output("aaa\0bb\u{1f}c", 100, 3).is_ok());
        assert!(check_git_output("aaa\0bb\u{1f}c", 100, 2).is_err());
        // A newline is not a delimiter for the NUL/US form.
        assert!(check_git_output("aa\naa", 100, 4).is_err());
        assert!(check_limits("aa\naa", 100, 2, true).is_ok());
        assert!(check_git_output("abcdef", 5, 100).is_err());
    }

    #[test]
    fn caps_commits_exact_and_over() {
        let many = commit_records(3).concat();
        assert_caps_edges(&many, false, parse_commits_with_limits);
        let one = commit_records(1).concat();
        assert_caps_edges(&one, false, parse_commits_with_limits);
    }

    #[test]
    fn caps_commits_field_in_middle_fails_whole_parse() {
        let records = commit_records(3);
        let long = "x".repeat(200);
        let input = format!("{}{}{}", records[0], commit_with(7, &long), records[2]);
        assert_field_over_cap(&input, parse_commits_with_limits);
    }

    #[test]
    fn caps_branches_exact_and_over() {
        let many = branch_rows(3).concat();
        assert_caps_edges(&many, true, parse_branches_with_limits);
        let one = branch_rows(1).concat();
        assert_caps_edges(&one, true, parse_branches_with_limits);
        // Minified without the trailing newline is rejected as truncated, not by a cap.
        let minified = one.trim_end_matches('\n');
        let error = parse_branches(minified).unwrap_err().to_string();
        assert!(error.contains("missing final newline"), "{error}");
    }

    #[test]
    fn caps_branches_field_in_middle_fails_whole_parse() {
        let rows = branch_rows(3);
        let mid = table_branch_row(1).replace(LINE_HOSTILE, &"x".repeat(200));
        let input = format!("{}{}{}", rows[0], mid, rows[2]);
        assert_field_over_cap(&input, parse_branches_with_limits);
    }

    #[test]
    fn caps_stashes_exact_and_over() {
        let many = stash_records(3).concat();
        assert_caps_edges(&many, false, parse_stashes_with_limits);
        let one = stash_records(1).concat();
        assert_caps_edges(&one, false, parse_stashes_with_limits);
    }

    #[test]
    fn caps_stashes_field_in_middle_fails_whole_parse() {
        let records = stash_records(3);
        let mut fields = stash_fields(1);
        fields[3] = "x".repeat(200);
        let input = format!("{}{}{}", records[0], nul_record(&fields), records[2]);
        assert_field_over_cap(&input, parse_stashes_with_limits);
    }

    #[test]
    fn caps_tags_exact_and_over() {
        let many = tag_rows(3).concat();
        assert_caps_edges(&many, true, parse_tags_with_limits);
        let one = tag_rows(1).concat();
        assert_caps_edges(&one, true, parse_tags_with_limits);
        let minified = one.trim_end_matches('\n');
        let error = parse_tags(minified).unwrap_err().to_string();
        assert!(error.contains("missing final newline"), "{error}");
    }

    #[test]
    fn caps_tags_field_in_middle_fails_whole_parse() {
        let rows = tag_rows(3);
        let mid = tag_row(1).replace("rel/\u{e9}-1", &"x".repeat(200));
        let input = format!("{}{}{}", rows[0], mid, rows[2]);
        assert_field_over_cap(&input, parse_tags_with_limits);
    }

    #[test]
    fn caps_thousand_records_stay_ok_under_real_caps() {
        assert_eq!(
            parse_commits(&commit_records(1000).concat()).unwrap().len(),
            1000
        );
        assert_eq!(
            parse_stashes(&stash_records(1000).concat()).unwrap().len(),
            1000
        );
        assert_eq!(
            parse_branches(&branch_rows(1000).concat()).unwrap().len(),
            1000
        );
        assert_eq!(parse_tags(&tag_rows(1000).concat()).unwrap().len(), 1000);
        assert_eq!(MAX_GIT_OUTPUT_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_GIT_FIELD_BYTES, 1024 * 1024);
    }

    /// NUL-terminate every field and decode the bytes the way the svc runner
    /// does (`String::from_utf8_lossy`), so invalid UTF-8 reaches the parser.
    fn lossy_z(fields: &[&[u8]]) -> String {
        let mut bytes = Vec::new();
        for field in fields {
            bytes.extend_from_slice(field);
            bytes.push(0);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn lossy_invalid_utf8_in_commit_display_fields_keeps_alignment() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let out = lossy_z(&[
            a.as_bytes(),
            b"aaaaaaa",
            b"Ann \xff\xfe",
            b"ann\xff\xfe@example.invalid",
            b"1700000000",
            b.as_bytes(),
            b"HEAD -> main",
            b"first \xff\xfe subject",
            b.as_bytes(),
            b"bbbbbbb",
            b"Bob",
            b"bob@example.invalid",
            b"1600000000",
            b"",
            b"",
            b"\xff\xfe",
        ]);
        let commits = parse_commits(&out).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, a);
        assert_eq!(commits[0].short, "aaaaaaa");
        assert_eq!(commits[0].date, 1_700_000_000);
        assert_eq!(commits[0].parents, vec![b.clone()]);
        assert_eq!(commits[0].refs, "HEAD -> main");
        assert!(commits[0].author.contains('\u{FFFD}'));
        assert!(commits[0].email.contains('\u{FFFD}'));
        assert!(commits[0].subject.contains('\u{FFFD}'));
        assert_eq!(commits[1].sha, b);
        assert_eq!(commits[1].date, 1_600_000_000);
        assert_eq!(commits[1].author, "Bob");
        assert!(commits[1].parents.is_empty());
        assert!(commits[1].subject.contains('\u{FFFD}'));
    }

    #[test]
    fn lossy_invalid_utf8_in_stash_message_keeps_alignment() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let out = lossy_z(&[
            b"stash@{0}",
            a.as_bytes(),
            b"1700000000",
            b"WIP on main: \xff\xfe",
            b"stash@{1}",
            b.as_bytes(),
            b"1600000000",
            b"On main: plain",
        ]);
        let stashes = parse_stashes(&out).unwrap();
        assert_eq!(stashes.len(), 2);
        assert_eq!((stashes[0].index, stashes[0].sha.as_str()), (0, a.as_str()));
        assert_eq!(stashes[0].date, 1_700_000_000);
        assert!(stashes[0].message.contains('\u{FFFD}'));
        assert_eq!((stashes[1].index, stashes[1].sha.as_str()), (1, b.as_str()));
        assert_eq!(stashes[1].date, 1_600_000_000);
        assert_eq!(stashes[1].message, "On main: plain");
    }

    #[test]
    fn lossy_invalid_utf8_in_a_fixed_field_is_an_error() {
        let a = "a".repeat(40);
        let mut bad_sha = a.clone().into_bytes();
        bad_sha[10] = 0xff;
        let commit = |sha: &[u8], date: &[u8]| {
            lossy_z(&[
                sha,
                b"aaaaaaa",
                b"Ann",
                b"ann@example.invalid",
                date,
                b"",
                b"",
                b"subject",
            ])
        };
        assert!(parse_commits(&commit(&bad_sha, b"1700000000")).is_err());
        assert!(parse_commits(&commit(a.as_bytes(), b"17000\xff0000")).is_err());
        let stash =
            |sha: &[u8], date: &[u8]| lossy_z(&[b"stash@{0}", sha, date, b"WIP on main: x"]);
        assert!(parse_stashes(&stash(&bad_sha, b"1700000000")).is_err());
        assert!(parse_stashes(&stash(a.as_bytes(), b"17000\xff0000")).is_err());
    }
}
