//! Git ref/log output → structured data for the lazygit-style panels:
//! commits, rich branches, stashes, tags, bisect state, git version.
//!
//! Pure parsing; the svc layer runs the commands and hands the stdout here.
//! Commit and stash records use NUL-delimited fields, which cannot collide
//! with Git identity or message data. Ref records use unit separators because
//! Git ref names cannot contain control bytes. Parsers reject malformed rows
//! as a whole rather than silently returning partial repository state.

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
/// Exactly one final empty chunk is accepted as the record terminator.
pub fn parse_commits(out: &str) -> anyhow::Result<Vec<Commit>> {
    if out.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields: Vec<&str> = out.split('\0').collect();
    if fields.last() == Some(&"") {
        fields.pop();
    }
    anyhow::ensure!(!fields.is_empty(), "empty commit record terminator");
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
        let date = row[4]
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("malformed commit timestamp"))?;
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
/// no starred row). An empty `%(upstream:short)` means no upstream.
pub fn parse_branches(out: &str) -> anyhow::Result<Vec<BranchInfo>> {
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
            let date = it
                .next()
                .ok_or_else(malformed)?
                .parse()
                .map_err(|_| malformed())?;
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
/// selector "stash@{N}"; exactly one final empty chunk is accepted as the
/// record terminator. Any malformed row makes the complete parse fail.
pub fn parse_stashes(out: &str) -> anyhow::Result<Vec<StashEntry>> {
    if out.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields: Vec<&str> = out.split('\0').collect();
    if fields.last() == Some(&"") {
        fields.pop();
    }
    anyhow::ensure!(!fields.is_empty(), "empty stash record terminator");
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
            let date = row[2].parse().map_err(|_| malformed())?;
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
/// point at) means lightweight.
pub fn parse_tags(out: &str) -> anyhow::Result<Vec<TagInfo>> {
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
                it.next().is_none(),
                "extra tag fields on row {}",
                line_no + 1
            );
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
///
/// Under refs/bisect the ref names are `<bad-term>`, `<good-term>-<sha>` and
/// `skip-<sha>` (skip is always "skip", even with custom terms). Depending on
/// ambiguity `%(refname:short)` may keep a `bisect/` or `refs/bisect/` prefix,
/// so both are stripped before matching.
pub fn parse_bisect(refs_out: &str, terms: Option<&str>, head: &str) -> BisectState {
    // BISECT_TERMS is two lines: the bad term first, the good term second
    // (write_terms() in git's bisect.c writes them in that order).
    let mut lines = terms.unwrap_or("").lines();
    let bad_term = lines.next().filter(|t| !t.is_empty()).unwrap_or("bad");
    let good_term = lines.next().filter(|t| !t.is_empty()).unwrap_or("good");

    let mut state = BisectState {
        bad_term: bad_term.to_string(),
        good_term: good_term.to_string(),
        bad: None,
        good: Vec::new(),
        skipped: Vec::new(),
        current: head.to_string(),
        culprit: None,
    };
    let good_prefix = format!("{good_term}-");
    for line in refs_out.lines() {
        let Some((name, sha)) = line.split_once('\u{1f}') else {
            continue;
        };
        let name = name
            .trim_start_matches("refs/")
            .trim_start_matches("bisect/");
        if name == bad_term {
            state.bad = Some(sha.to_string());
        } else if name.strip_prefix("skip-").is_some() {
            state.skipped.push(sha.to_string());
        } else if name.strip_prefix(good_prefix.as_str()).is_some() {
            state.good.push(sha.to_string());
        }
    }
    state
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
        let out = [
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
        let out = rec(&[
            &"a".repeat(40),
            "a",
            "Ann",
            "a@e",
            "1",
            "",
            "",
            "  héllo → wörld  ",
        ]);
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
        let out = [
            branch_row("*", "main", "origin/main", ""),
            branch_row(" ", "feat", "origin/feat", "[ahead 1]"),
            branch_row(" ", "fix", "origin/fix", "[behind 2]"),
            branch_row(" ", "both", "origin/both", "[ahead 1, behind 2]"),
            branch_row(" ", "old", "origin/old", "[gone]"),
        ]
        .join("\n");
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
        let out = [
            branch_row(" ", "feature/deep/dotted.name", "", ""),
            branch_row("*", "release-1.2.x", "origin/release-1.2.x", "[ahead 3]"),
        ]
        .join("\n");
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
        let out = format!("*{US}only-two\n{}", branch_row(" ", "x", "", "")).replace("42", "NaN");
        assert!(parse_branches(&out).is_err());
        assert!(parse_branches(&branch_row(" ", "x", "", "").replace("42", "NaN")).is_err());
        assert!(parse_branches(&branch_row(" ", "x", "", "[ahead many]")).is_err());
        assert!(
            parse_branches(&format!(
                "{}\n{}",
                branch_row(" ", "same", "", ""),
                branch_row(" ", "same", "", "")
            ))
            .is_err()
        );
    }

    // --- parse_stashes ---

    #[test]
    fn stashes_indices_and_messy_messages() {
        let out = [
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
                parse_tags(&format!("v1{US}{sha}{US}unknown")).map(|_| ()),
            ),
            (
                "tag empty name",
                parse_tags(&format!("{US}{sha}{US}commit")).map(|_| ()),
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
            "v1.0{US}{}{US}tag\nv1.1{US}{}{US}commit",
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
        assert!(parse_tags(&format!("{out}\nbad-line")).is_err());
        assert!(
            parse_tags(&format!(
                "v1{US}{}{US}commit\nv1{US}{}{US}commit",
                "a".repeat(40),
                "b".repeat(40)
            ))
            .is_err()
        );
        assert!(parse_tags(&format!("v1{US}a{US}commit\nv1{US}b{US}commit")).is_err());
    }

    // --- parse_bisect / find_culprit ---

    #[test]
    fn bisect_default_terms_with_marks() {
        let refs = format!(
            "bad{US}B\ngood-{}{US}G1\ngood-{}{US}G2\nskip-{}{US}S1",
            "1".repeat(40),
            "2".repeat(40),
            "3".repeat(40),
        );
        let st = parse_bisect(&refs, None, "HEADSHA");
        assert_eq!(st.bad_term, "bad");
        assert_eq!(st.good_term, "good");
        assert_eq!(st.bad.as_deref(), Some("B"));
        assert_eq!(st.good, vec!["G1", "G2"]);
        assert_eq!(st.skipped, vec!["S1"]);
        assert_eq!(st.current, "HEADSHA");
        assert_eq!(st.culprit, None);
    }

    #[test]
    fn bisect_custom_terms() {
        // BISECT_TERMS: bad term on line 1, good term on line 2.
        let refs = format!("broken{US}B\nfixed-aaa{US}G\nskip-bbb{US}S");
        let st = parse_bisect(&refs, Some("broken\nfixed\n"), "H");
        assert_eq!(
            (st.bad_term.as_str(), st.good_term.as_str()),
            ("broken", "fixed")
        );
        assert_eq!(st.bad.as_deref(), Some("B"));
        assert_eq!(st.good, vec!["G"]);
        assert_eq!(st.skipped, vec!["S"]);
    }

    #[test]
    fn bisect_nothing_marked_and_prefixed_refnames() {
        let st = parse_bisect("", None, "H");
        assert_eq!(st.bad, None);
        assert!(st.good.is_empty() && st.skipped.is_empty());
        // refname:short may keep a bisect/ or refs/bisect/ prefix; an
        // unrelated name and a separator-less line are ignored.
        let refs = format!("bisect/bad{US}B\nrefs/bisect/good-x{US}G\nother{US}X\nnoseparator");
        let st = parse_bisect(&refs, Some(""), "H");
        assert_eq!(st.bad.as_deref(), Some("B"));
        assert_eq!(st.good, vec!["G"]);
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
}
