//! Git ref/log output → structured data for the lazygit-style panels:
//! commits, rich branches, stashes, tags, bisect state, git version.
//!
//! Pure parsing; the svc layer runs the commands and hands the stdout here.
//! Record formats lean on the `%x1f` unit-separator idiom (see
//! `parse_log_graph` in superzej-svc): fields can hold anything but `\u{1f}`
//! and `\n`, so splitting is unambiguous. Tolerant of malformed input:
//! unparseable records are dropped, never panicked on.

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

/// Parse `git log --format=%x1f%H%x1f%h%x1f%an%x1f%ae%x1f%ct%x1f%P%x1f%D%x1f%s`
/// output: each record starts with the 0x1f unit separator, fields are
/// 0x1f-separated, records are newline-separated. The subject rides last so
/// it can hold anything but `\n`. Lines that don't begin with the separator
/// or are missing fields are skipped.
pub fn parse_commits(out: &str) -> Vec<Commit> {
    out.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('\u{1f}')?;
            let mut it = rest.splitn(8, '\u{1f}');
            let sha = it.next()?.to_string();
            let short = it.next()?.to_string();
            let author = it.next()?.to_string();
            let email = it.next()?.to_string();
            let date = it.next()?.parse().ok()?;
            let parents = it.next()?.split_whitespace().map(String::from).collect();
            let refs = it.next()?.to_string();
            let subject = it.next()?.to_string();
            Some(Commit {
                sha,
                short,
                author,
                email,
                date,
                subject,
                parents,
                refs,
            })
        })
        .collect()
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

/// Decompose an `%(upstream:track)` field — "" | "[ahead N]" | "[behind M]"
/// | "[ahead N, behind M]" | "[gone]" — into (ahead, behind, gone).
fn parse_track(track: &str) -> (usize, usize, bool) {
    let inner = track.trim().trim_start_matches('[').trim_end_matches(']');
    if inner == "gone" {
        return (0, 0, true);
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in inner.split(',') {
        let mut words = part.split_whitespace();
        match (words.next(), words.next().and_then(|n| n.parse().ok())) {
            (Some("ahead"), Some(n)) => ahead = n,
            (Some("behind"), Some(n)) => behind = n,
            _ => {}
        }
    }
    (ahead, behind, false)
}

/// Parse `git for-each-ref refs/heads --sort=-committerdate --format=
/// %(HEAD)%x1f%(refname:short)%x1f%(upstream:short)%x1f%(upstream:track)%x1f%(objectname)%x1f%(committerdate:unix)%x1f%(contents:subject)`
/// output. `%(HEAD)` is "*" on the current branch (detached HEAD simply emits
/// no starred row). An empty `%(upstream:short)` means no upstream.
pub fn parse_branches(out: &str) -> Vec<BranchInfo> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.splitn(7, '\u{1f}');
            let is_head = it.next()? == "*";
            let name = it.next()?.to_string();
            let upstream = Some(it.next()?).filter(|u| !u.is_empty()).map(String::from);
            let (ahead, behind, upstream_gone) = parse_track(it.next()?);
            let sha = it.next()?.to_string();
            let date = it.next()?.parse().ok()?;
            let subject = it.next()?.to_string();
            Some(BranchInfo {
                name,
                is_head,
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

/// Parse `git stash list --format=%gd%x1f%H%x1f%ct%x1f%gs` output. `%gd` is
/// the selector "stash@{N}"; rows whose selector doesn't fit that shape are
/// skipped.
pub fn parse_stashes(out: &str) -> Vec<StashEntry> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.splitn(4, '\u{1f}');
            let index = it
                .next()?
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            let sha = it.next()?.to_string();
            let date = it.next()?.parse().ok()?;
            let message = it.next()?.to_string();
            Some(StashEntry {
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
pub fn parse_tags(out: &str) -> Vec<TagInfo> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.splitn(3, '\u{1f}');
            let name = it.next()?.to_string();
            let sha = it.next()?.to_string();
            let annotated = it.next()? == "tag";
            Some(TagInfo {
                name,
                sha,
                annotated,
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

/// Scan bisect command stdout for `"<full-sha> is the first bad commit"`,
/// anchored to the start of a line with a 40-hex sha (so a commit subject
/// quoting the phrase mid-line can't fool it).
pub fn find_culprit(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (sha, rest) = line.split_at_checked(40)?;
        (sha.bytes().all(|b| b.is_ascii_hexdigit())
            && rest.trim_end() == " is the first bad commit")
            .then(|| sha.to_string())
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

    /// Join fields into one `%x1f`-prefixed log record.
    fn rec(fields: &[&str]) -> String {
        format!("{US}{}", fields.join(&US.to_string()))
    }

    // --- parse_commits ---

    #[test]
    fn commits_empty_input() {
        assert_eq!(parse_commits(""), Vec::<Commit>::new());
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
                "p1 p2",
                "HEAD -> main, origin/main",
                "merge two",
            ]),
            rec(&[
                "c".repeat(40).as_str(),
                "ccccccc",
                "Cy",
                "c@e",
                "300",
                "p1 p2 p3",
                "",
                "octopus",
            ]),
        ]
        .join("\n");
        let v = parse_commits(&out);
        assert_eq!(v.len(), 3);
        assert!(v[0].parents.is_empty());
        assert_eq!(v[0].refs, "");
        assert_eq!(v[0].date, 100);
        assert_eq!(v[1].parents, vec!["p1", "p2"]);
        assert_eq!(v[1].refs, "HEAD -> main, origin/main");
        assert_eq!(v[1].author, "Bob");
        assert_eq!(v[1].email, "b@e");
        assert_eq!(v[1].short, "bbbbbbb");
        assert_eq!(v[2].parents.len(), 3);
        assert_eq!(v[2].sha, "c".repeat(40));
    }

    #[test]
    fn commits_subject_keeps_unicode_and_edge_whitespace() {
        let out = rec(&["s", "sh", "Ann", "a@e", "1", "", "", "  héllo → wörld  "]);
        let v = parse_commits(&out);
        assert_eq!(v[0].subject, "  héllo → wörld  ");
    }

    #[test]
    fn commits_malformed_rows_are_dropped() {
        // No leading separator, too few fields, non-numeric date — all skipped;
        // the good row survives.
        let out = [
            "garbage without separator".to_string(),
            rec(&["sha", "sh", "Ann"]),
            rec(&["sha", "sh", "Ann", "a@e", "not-a-date", "", "", "subj"]),
            rec(&["sha", "sh", "Ann", "a@e", "7", "", "", "kept"]),
        ]
        .join("\n");
        let v = parse_commits(&out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].subject, "kept");
    }

    // --- parse_branches ---

    fn branch_row(head: &str, name: &str, upstream: &str, track: &str) -> String {
        [head, name, upstream, track, "deadbeef", "42", "subject"].join(&US.to_string())
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
        let v = parse_branches(&out);
        assert_eq!(v.len(), 5);
        assert!(v[0].is_head);
        assert_eq!((v[0].ahead, v[0].behind, v[0].upstream_gone), (0, 0, false));
        assert_eq!(v[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(v[0].sha, "deadbeef");
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
        let v = parse_branches(&out);
        assert_eq!(v[0].name, "feature/deep/dotted.name");
        assert_eq!(v[0].upstream, None);
        assert!(!v[0].upstream_gone);
        assert_eq!(v[1].name, "release-1.2.x");
        assert!(v[1].is_head);
        assert_eq!(v[1].ahead, 3);
    }

    #[test]
    fn branches_empty_and_malformed() {
        assert_eq!(parse_branches(""), Vec::<BranchInfo>::new());
        // Too few fields / bad date → dropped.
        let out = format!("*{US}only-two\n{}", branch_row(" ", "x", "", "")).replace("42", "NaN");
        assert_eq!(parse_branches(&out), Vec::<BranchInfo>::new());
    }

    // --- parse_stashes ---

    #[test]
    fn stashes_indices_and_messy_messages() {
        let out = [
            format!("stash@{{0}}{US}s0{US}10{US}WIP on main: abc123 fix: thing {{x}}"),
            format!("stash@{{1}}{US}s1{US}20{US}On feat: msg: with: colons"),
            format!("stash@{{2}}{US}s2{US}30{US}plain"),
        ]
        .join("\n");
        let v = parse_stashes(&out);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].index, 0);
        assert_eq!(v[0].message, "WIP on main: abc123 fix: thing {x}");
        assert_eq!(v[1].index, 1);
        assert_eq!(v[1].message, "On feat: msg: with: colons");
        assert_eq!((v[2].index, v[2].date), (2, 30));
        assert_eq!(v[2].sha, "s2");
    }

    #[test]
    fn stashes_empty_and_malformed() {
        assert_eq!(parse_stashes(""), Vec::<StashEntry>::new());
        let out = format!("not-a-selector{US}s{US}1{US}m\nstash@{{x}}{US}s{US}1{US}m");
        assert_eq!(parse_stashes(&out), Vec::<StashEntry>::new());
    }

    // --- parse_tags ---

    #[test]
    fn tags_annotated_vs_lightweight() {
        let out = format!("v1.0{US}t1{US}tag\nv1.1{US}c1{US}commit\nbad-line");
        let v = parse_tags(&out);
        assert_eq!(v.len(), 2);
        assert!(v[0].annotated);
        assert_eq!((v[0].name.as_str(), v[0].sha.as_str()), ("v1.0", "t1"));
        assert!(!v[1].annotated);
        assert_eq!(v[1].name, "v1.1");
        assert_eq!(parse_tags(""), Vec::<TagInfo>::new());
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
