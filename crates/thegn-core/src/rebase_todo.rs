//! Interactive-rebase TODO model: parse/serialize `git-rebase-todo` files and
//! apply the lazygit-style mutations (retag, reorder, fixup placement). Pure —
//! the svc layer fetches `git log` output off-thread, injects serialized todos
//! via `GIT_SEQUENCE_EDITOR`, and rewrites `.git/rebase-merge/git-rebase-todo`
//! mid-rebase.
//!
//! Round-trip contract: `serialize(parse(x))` preserves every semantically
//! meaningful line of `x`. Comment (`#…`) and blank lines are regenerable
//! noise — skipped on parse, never emitted. Any other line we cannot parse is
//! kept verbatim as [`TodoAction::Unknown`] so rewriting a live todo can never
//! silently drop sequencer instructions. `fixup -c` is folded into
//! [`TodoAction::FixupC`] (`fixup -C`): both replace the squashed message with
//! the fixup commit's, and the open-editor nuance of `-c` is irrelevant to a
//! todo rewriter.
//!
//! Reordering ([`move_entry`]) swaps a commit with the *adjacent commit
//! entry*, jumping over inert lines (`exec`/`break`/`noop`) but refusing to
//! cross structural ones (`label`/`reset`/`merge`/`update-ref`/unknown) whose
//! relative order encodes rebase-merges topology.

use std::collections::HashSet;
use std::fmt::Write as _;

/// One sequencer instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoAction {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    /// `fixup -C` (and `-c`) — use the fixup commit's message.
    FixupC,
    Drop,
    Break,
    /// `exec <cmd>` — the full command line.
    Exec(String),
    Label(String),
    Reset(String),
    /// `merge …` pass-through: rebase-merges todos we don't author but must
    /// not corrupt (the argument is everything after the action word).
    Merge(String),
    /// `update-ref refs/…` — git ≥ 2.38 emits these with `rebase.updateRefs`.
    UpdateRef(String),
    Noop,
    /// A non-blank, non-comment line we couldn't parse, preserved verbatim
    /// (the whole line) so a rewrite never corrupts a live todo.
    Unknown(String),
}

impl TodoAction {
    /// Whether this action targets a commit (carries a sha + subject).
    pub fn is_commit(&self) -> bool {
        matches!(
            self,
            TodoAction::Pick
                | TodoAction::Reword
                | TodoAction::Edit
                | TodoAction::Squash
                | TodoAction::Fixup
                | TodoAction::FixupC
                | TodoAction::Drop
        )
    }

    /// Whether crossing this line on reorder would corrupt rebase-merges
    /// topology (labels, resets, merges, ref updates, anything unparsed).
    fn is_structural(&self) -> bool {
        matches!(
            self,
            TodoAction::Label(_)
                | TodoAction::Reset(_)
                | TodoAction::Merge(_)
                | TodoAction::UpdateRef(_)
                | TodoAction::Unknown(_)
        )
    }
}

/// One todo line. For non-commit actions (`Exec`/`Label`/`Reset`/`Break`/
/// `Noop`/`UpdateRef`/`Merge`/`Unknown`) `sha` and `subject` are empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoEntry {
    pub action: TodoAction,
    pub sha: String,
    pub subject: String,
}

/// Errors from the todo mutators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoError {
    /// No commit entry matched the given sha (by mutual prefix).
    ShaNotFound(String),
    /// The requested move/placement would corrupt the todo.
    CannotMove(&'static str),
}

impl std::fmt::Display for TodoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoError::ShaNotFound(sha) => write!(f, "no todo entry matches sha {sha}"),
            TodoError::CannotMove(why) => write!(f, "cannot move entry: {why}"),
        }
    }
}

impl std::error::Error for TodoError {}

/// Sha equality the way git resolves abbreviations: either side may be a
/// prefix of the other (entries hold full or abbreviated shas depending on
/// whether they came from `git log` or a live todo file). Empty never matches.
fn sha_matches(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

fn entry(action: TodoAction) -> TodoEntry {
    TodoEntry {
        action,
        sha: String::new(),
        subject: String::new(),
    }
}

/// Parse `<sha> [subject]` into a commit entry (subject may be empty).
fn commit_entry(action: TodoAction, rest: &str) -> Option<TodoEntry> {
    if rest.is_empty() {
        return None;
    }
    let (sha, subject) = match rest.split_once(char::is_whitespace) {
        Some((sha, subject)) => (sha, subject.trim_start()),
        None => (rest, ""),
    };
    Some(TodoEntry {
        action,
        sha: sha.to_string(),
        subject: subject.to_string(),
    })
}

/// Parse one trimmed, non-blank, non-comment todo line; `None` = unknown.
fn parse_line(line: &str) -> Option<TodoEntry> {
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (line, ""),
    };
    match word {
        "pick" | "p" => commit_entry(TodoAction::Pick, rest),
        "reword" | "r" => commit_entry(TodoAction::Reword, rest),
        "edit" | "e" => commit_entry(TodoAction::Edit, rest),
        "squash" | "s" => commit_entry(TodoAction::Squash, rest),
        "drop" | "d" => commit_entry(TodoAction::Drop, rest),
        "fixup" | "f" => match rest
            .strip_prefix("-C ")
            .or_else(|| rest.strip_prefix("-c "))
        {
            Some(rest) => commit_entry(TodoAction::FixupC, rest.trim_start()),
            // A flag with no sha (`-C`, `-c`, or a cut-short `-`) or any
            // other flag-looking token is not a sha: it is malformed, so let
            // it fall to Unknown like every other malformed line.
            None if rest.starts_with('-') => None,
            None => commit_entry(TodoAction::Fixup, rest),
        },
        "exec" | "x" if !rest.is_empty() => Some(entry(TodoAction::Exec(rest.to_string()))),
        "label" | "l" if !rest.is_empty() => Some(entry(TodoAction::Label(rest.to_string()))),
        "reset" | "t" if !rest.is_empty() => Some(entry(TodoAction::Reset(rest.to_string()))),
        "merge" | "m" if !rest.is_empty() => Some(entry(TodoAction::Merge(rest.to_string()))),
        "update-ref" | "u" if !rest.is_empty() => {
            Some(entry(TodoAction::UpdateRef(rest.to_string())))
        }
        "break" | "b" if rest.is_empty() => Some(entry(TodoAction::Break)),
        "noop" if rest.is_empty() => Some(entry(TodoAction::Noop)),
        _ => None,
    }
}

/// Parse a `git-rebase-todo` file. Comment (`#…`) and blank lines are skipped
/// (git regenerates them); both long (`pick`) and short (`p`) action forms are
/// accepted; any other non-blank line is preserved verbatim as
/// [`TodoAction::Unknown`].
pub fn parse_todo(text: &str) -> Vec<TodoEntry> {
    let mut entries = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        entries.push(
            parse_line(trimmed).unwrap_or_else(|| entry(TodoAction::Unknown(line.to_string()))),
        );
    }
    entries
}

/// Serialize entries to todo-file text: long action forms, one per line,
/// trailing newline. `parse(serialize(e)) == e` for entries we author.
pub fn serialize_todo(entries: &[TodoEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        match &e.action {
            TodoAction::Pick => push_commit(&mut out, "pick", e),
            TodoAction::Reword => push_commit(&mut out, "reword", e),
            TodoAction::Edit => push_commit(&mut out, "edit", e),
            TodoAction::Squash => push_commit(&mut out, "squash", e),
            TodoAction::Fixup => push_commit(&mut out, "fixup", e),
            TodoAction::FixupC => push_commit(&mut out, "fixup -C", e),
            TodoAction::Drop => push_commit(&mut out, "drop", e),
            TodoAction::Exec(cmd) => push_arg(&mut out, "exec", cmd),
            TodoAction::Label(name) => push_arg(&mut out, "label", name),
            TodoAction::Reset(name) => push_arg(&mut out, "reset", name),
            TodoAction::Merge(rest) => push_arg(&mut out, "merge", rest),
            TodoAction::UpdateRef(name) => push_arg(&mut out, "update-ref", name),
            TodoAction::Break => out.push_str("break"),
            TodoAction::Noop => out.push_str("noop"),
            TodoAction::Unknown(line) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

fn push_commit(out: &mut String, word: &str, e: &TodoEntry) {
    out.push_str(word);
    out.push(' ');
    out.push_str(&e.sha);
    if !e.subject.is_empty() {
        out.push(' ');
        out.push_str(&e.subject);
    }
}

fn push_arg(out: &mut String, word: &str, arg: &str) {
    out.push_str(word);
    out.push(' ');
    out.push_str(arg);
}

/// Build the base todo for `<base>..HEAD` from
/// `git log --reverse --format=%x1f%H%x1f%s` output (one unit-separator-
/// prefixed record per line, oldest first): every commit becomes `Pick`.
/// Subjects occupy the remainder of their line and may contain unit separators.
/// Any malformed or duplicate record rejects the entire result.
pub fn todo_from_log(log_out: &str) -> anyhow::Result<Vec<TodoEntry>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for (index, raw_line) in log_out.split_terminator('\n').enumerate() {
        let line_number = index + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        let Some(record) = line.strip_prefix('\u{1f}') else {
            anyhow::bail!("rebase log row {line_number}: missing record separator");
        };
        let Some((sha, subject)) = record.split_once('\u{1f}') else {
            anyhow::bail!("rebase log row {line_number}: expected sha and subject");
        };
        if !matches!(sha.len(), 40 | 64)
            || !sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            anyhow::bail!("rebase log row {line_number}: invalid full sha");
        }
        if !seen.insert(sha) {
            anyhow::bail!("rebase log row {line_number}: duplicate commit {sha}");
        }
        entries.push(TodoEntry {
            action: TodoAction::Pick,
            sha: sha.to_string(),
            subject: subject.trim_end_matches('\r').to_string(),
        });
    }
    Ok(entries)
}

/// Index of the first commit entry matching `sha` by mutual prefix.
fn find_commit(entries: &[TodoEntry], sha: &str) -> Result<usize, TodoError> {
    entries
        .iter()
        .position(|e| e.action.is_commit() && sha_matches(&e.sha, sha))
        .ok_or_else(|| TodoError::ShaNotFound(sha.to_string()))
}

/// Set the action of every commit entry whose sha matches one of `targets`
/// (mutual-prefix matching). Errors if any target matches nothing.
pub fn retag(
    entries: &[TodoEntry],
    targets: &[&str],
    action: TodoAction,
) -> Result<Vec<TodoEntry>, TodoError> {
    let mut out = entries.to_vec();
    for target in targets {
        let mut hit = false;
        for e in &mut out {
            if e.action.is_commit() && sha_matches(&e.sha, target) {
                e.action = action.clone();
                hit = true;
            }
        }
        if !hit {
            return Err(TodoError::ShaNotFound((*target).to_string()));
        }
    }
    Ok(out)
}

/// Move the commit entry matching `sha` one position up (towards the top of
/// the file, i.e. earlier in the rebase) or down: it swaps with the adjacent
/// *commit* entry, jumping over inert lines (`exec`/`break`/`noop`) but
/// refusing to cross structural ones (`label`/`reset`/`merge`/`update-ref`/
/// unknown) whose order encodes rebase-merges topology. Errors at the
/// boundaries (no commit beyond the target in that direction).
pub fn move_entry(entries: &[TodoEntry], sha: &str, up: bool) -> Result<Vec<TodoEntry>, TodoError> {
    let from = find_commit(entries, sha)?;
    let mut to = from;
    loop {
        to = if up {
            match to.checked_sub(1) {
                Some(t) => t,
                None => return Err(TodoError::CannotMove("already first")),
            }
        } else {
            if to + 1 >= entries.len() {
                return Err(TodoError::CannotMove("already last"));
            }
            to + 1
        };
        let action = &entries[to].action;
        if action.is_commit() {
            break;
        }
        if action.is_structural() {
            return Err(TodoError::CannotMove(
                "would cross a structural rebase line",
            ));
        }
    }
    let mut out = entries.to_vec();
    out.swap(from, to);
    Ok(out)
}

/// Place a fixup commit directly after its target with action `Fixup` — the
/// amend-old-commit flow: `git commit --fixup=<target>` puts the fixup at the
/// end of the todo, then this moves it into position. Identifying both ends
/// by sha (not subject) is deliberate: `--autosquash` matches by subject and
/// mis-targets when two commits share one.
pub fn place_fixup(
    entries: &[TodoEntry],
    fixup_sha: &str,
    target_sha: &str,
) -> Result<Vec<TodoEntry>, TodoError> {
    let from = find_commit(entries, fixup_sha)?;
    let target = find_commit(entries, target_sha)?;
    if from == target {
        return Err(TodoError::CannotMove(
            "fixup and target are the same commit",
        ));
    }
    let mut out = entries.to_vec();
    let mut fix = out.remove(from);
    fix.action = TodoAction::Fixup;
    let target = if from < target { target - 1 } else { target };
    out.insert(target + 1, fix);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(action: TodoAction, sha: &str, subject: &str) -> TodoEntry {
        TodoEntry {
            action,
            sha: sha.to_string(),
            subject: subject.to_string(),
        }
    }

    fn bare(action: TodoAction) -> TodoEntry {
        entry(action)
    }

    fn picks(shas: &[&str]) -> Vec<TodoEntry> {
        shas.iter()
            .map(|s| commit(TodoAction::Pick, s, &format!("subject {s}")))
            .collect()
    }

    // ---- parsing ----

    #[test]
    fn parses_long_forms() {
        let text = "pick aaa1 first\nreword bbb2 second\nedit ccc3 third\n\
                    squash ddd4 fourth\nfixup eee5 fifth\ndrop fff6 sixth\n";
        let got = parse_todo(text);
        assert_eq!(
            got,
            vec![
                commit(TodoAction::Pick, "aaa1", "first"),
                commit(TodoAction::Reword, "bbb2", "second"),
                commit(TodoAction::Edit, "ccc3", "third"),
                commit(TodoAction::Squash, "ddd4", "fourth"),
                commit(TodoAction::Fixup, "eee5", "fifth"),
                commit(TodoAction::Drop, "fff6", "sixth"),
            ]
        );
    }

    #[test]
    fn parses_short_forms() {
        let text = "p aaa1 first\nr bbb2 second\ne ccc3 third\ns ddd4 fourth\n\
                    f eee5 fifth\nd fff6 sixth\nx echo hi\nl onto\nt onto\n\
                    m -C abc topic\nu refs/heads/wip\nb\n";
        let got = parse_todo(text);
        assert_eq!(
            got,
            vec![
                commit(TodoAction::Pick, "aaa1", "first"),
                commit(TodoAction::Reword, "bbb2", "second"),
                commit(TodoAction::Edit, "ccc3", "third"),
                commit(TodoAction::Squash, "ddd4", "fourth"),
                commit(TodoAction::Fixup, "eee5", "fifth"),
                commit(TodoAction::Drop, "fff6", "sixth"),
                bare(TodoAction::Exec("echo hi".into())),
                bare(TodoAction::Label("onto".into())),
                bare(TodoAction::Reset("onto".into())),
                bare(TodoAction::Merge("-C abc topic".into())),
                bare(TodoAction::UpdateRef("refs/heads/wip".into())),
                bare(TodoAction::Break),
            ]
        );
    }

    #[test]
    fn parses_fixup_flag_variants() {
        let got =
            parse_todo("fixup -C aaa1 keep my message\nfixup -c bbb2 also mine\nf -C ccc3 short\n");
        assert_eq!(
            got,
            vec![
                commit(TodoAction::FixupC, "aaa1", "keep my message"),
                commit(TodoAction::FixupC, "bbb2", "also mine"),
                commit(TodoAction::FixupC, "ccc3", "short"),
            ]
        );
    }

    #[test]
    fn parses_exec_with_spaced_args() {
        let got = parse_todo("exec cargo test -p thegn-core --lib rebase_todo\n");
        assert_eq!(
            got,
            vec![bare(TodoAction::Exec(
                "cargo test -p thegn-core --lib rebase_todo".into()
            ))]
        );
    }

    #[test]
    fn parses_break_and_noop() {
        assert_eq!(parse_todo("break\n"), vec![bare(TodoAction::Break)]);
        assert_eq!(parse_todo("noop\n"), vec![bare(TodoAction::Noop)]);
        // Trailing junk on an argless action is malformed: preserve verbatim.
        assert_eq!(
            parse_todo("break now\n"),
            vec![bare(TodoAction::Unknown("break now".into()))]
        );
        assert_eq!(
            parse_todo("noop x\n"),
            vec![bare(TodoAction::Unknown("noop x".into()))]
        );
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let text = "# Rebase abc..def onto abc (3 commands)\n\npick aaa1 one\n\n# Commands:\n#  p, pick <commit> = use commit\n   \npick bbb2 two\n";
        assert_eq!(
            parse_todo(text),
            vec![
                commit(TodoAction::Pick, "aaa1", "one"),
                commit(TodoAction::Pick, "bbb2", "two"),
            ]
        );
    }

    #[test]
    fn preserves_unknown_lines_verbatim() {
        let text = "pick aaa1 one\nfrobnicate the widget\npick bbb2 two\n";
        let got = parse_todo(text);
        assert_eq!(
            got[1],
            bare(TodoAction::Unknown("frobnicate the widget".into()))
        );
        // The unknown line survives a parse → serialize rewrite untouched.
        assert_eq!(serialize_todo(&got), text);
        // Commit actions missing their sha are unknown too (corruption guard).
        assert_eq!(
            parse_todo("pick\n"),
            vec![bare(TodoAction::Unknown("pick".into()))]
        );
        assert_eq!(
            parse_todo("exec\n"),
            vec![bare(TodoAction::Unknown("exec".into()))]
        );
        assert_eq!(
            parse_todo("fixup -C\n"),
            vec![bare(TodoAction::Unknown("fixup -C".into()))]
        );
    }

    #[test]
    fn parses_missing_subject_and_crlf() {
        assert_eq!(
            parse_todo("pick aaa1\r\nreword bbb2 two\r\n"),
            vec![
                commit(TodoAction::Pick, "aaa1", ""),
                commit(TodoAction::Reword, "bbb2", "two"),
            ]
        );
    }

    // ---- serialization & round-trips ----

    #[test]
    fn serializes_long_forms_with_trailing_newline() {
        let entries = vec![
            commit(TodoAction::Pick, "aaa1", "one"),
            commit(TodoAction::FixupC, "bbb2", "two"),
            commit(TodoAction::Drop, "ccc3", ""),
            bare(TodoAction::Exec("make check".into())),
            bare(TodoAction::Label("onto".into())),
            bare(TodoAction::Reset("onto".into())),
            bare(TodoAction::Merge("-C abc topic # merge topic".into())),
            bare(TodoAction::UpdateRef("refs/heads/wip".into())),
            bare(TodoAction::Break),
            bare(TodoAction::Noop),
        ];
        assert_eq!(
            serialize_todo(&entries),
            "pick aaa1 one\nfixup -C bbb2 two\ndrop ccc3\nexec make check\n\
             label onto\nreset onto\nmerge -C abc topic # merge topic\n\
             update-ref refs/heads/wip\nbreak\nnoop\n"
        );
    }

    #[test]
    fn round_trips_authored_entries() {
        let entries = vec![
            commit(TodoAction::Pick, "aaa1", "one"),
            commit(TodoAction::Reword, "bbb2", "two words"),
            commit(TodoAction::Edit, "ccc3", ""),
            commit(TodoAction::Squash, "ddd4", "four"),
            commit(TodoAction::Fixup, "eee5", "five"),
            commit(TodoAction::FixupC, "fff6", "six"),
            commit(TodoAction::Drop, "0007", "seven"),
            bare(TodoAction::Exec("sh -c 'echo a b'".into())),
            bare(TodoAction::Label("base".into())),
            bare(TodoAction::Reset("base".into())),
            bare(TodoAction::Merge("-C abc topic".into())),
            bare(TodoAction::UpdateRef("refs/heads/x".into())),
            bare(TodoAction::Break),
            bare(TodoAction::Noop),
            bare(TodoAction::Unknown("???".into())),
        ];
        assert_eq!(parse_todo(&serialize_todo(&entries)), entries);
    }

    #[test]
    fn round_trips_short_form_file_to_canonical_text() {
        let parsed = parse_todo("p aaa1 one\nf -c bbb2 two\nx echo hi\nb\n");
        let text = serialize_todo(&parsed);
        assert_eq!(
            text,
            "pick aaa1 one\nfixup -C bbb2 two\nexec echo hi\nbreak\n"
        );
        assert_eq!(parse_todo(&text), parsed);
    }

    // ---- todo_from_log ----

    #[test]
    fn todo_from_log_handles_empty_input() {
        assert_eq!(todo_from_log("").unwrap(), vec![]);
        assert_eq!(todo_from_log("\n").unwrap(), vec![]);
        assert_eq!(todo_from_log("\r\n").unwrap(), vec![]);
    }

    #[test]
    fn todo_from_log_single_commit() {
        let sha = "a".repeat(40);
        let log = format!("\u{1f}{sha}\u{1f}fix: the thing\n");
        let got = todo_from_log(&log).unwrap();
        assert_eq!(got, vec![commit(TodoAction::Pick, &sha, "fix: the thing")]);
    }

    #[test]
    fn todo_from_log_three_commits_with_unicode_subjects() {
        let first = "a".repeat(40);
        let second = "b".repeat(40);
        let third = "c".repeat(40);
        let log = format!(
            "\u{1f}{first}\u{1f}feat: add naïve café parser\n\
             \u{1f}{second}\u{1f}fix: 修复 the 🐛 bug\n\
             \u{1f}{third}\u{1f}\n"
        );
        let got = todo_from_log(&log).unwrap();
        assert_eq!(
            got,
            vec![
                commit(TodoAction::Pick, &first, "feat: add naïve café parser"),
                commit(TodoAction::Pick, &second, "fix: 修复 the 🐛 bug"),
                commit(TodoAction::Pick, &third, ""),
            ]
        );
    }

    #[test]
    fn todo_from_log_unit_separator_in_subject_does_not_shift_commits() {
        let first = "a".repeat(40);
        let second = "b".repeat(40);
        let log = format!("\u{1f}{first}\u{1f}first\u{1f}subject\n\u{1f}{second}\u{1f}second\n");
        let entries = todo_from_log(&log).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sha, first);
        assert_eq!(entries[0].subject, "first\u{1f}subject");
        assert_eq!(entries[1].sha, second);
        assert_eq!(entries[1].subject, "second");
    }

    #[test]
    fn todo_from_log_rejects_malformed_lines() {
        let sha = "a".repeat(40);
        let cases = [
            "\u{1f}deadbeef\u{1f}short sha\n".to_string(),
            format!("{sha}\u{1f}missing leading separator\n"),
            format!("\u{1f}{sha}\n"),
            format!("\u{1f}{sha}\u{1f}first\n\u{1f}{sha}\u{1f}duplicate\n"),
        ];
        for log in cases {
            assert!(
                todo_from_log(&log).is_err(),
                "accepted malformed log {log:?}"
            );
        }
        assert_eq!(todo_from_log("").unwrap(), vec![]);
        assert_eq!(todo_from_log("\n").unwrap(), vec![]);
    }

    // ---- sha matching ----

    #[test]
    fn sha_matches_by_prefix_in_either_direction() {
        assert!(sha_matches("deadbeefcafe", "deadbeef"));
        assert!(sha_matches("deadbeef", "deadbeefcafe"));
        assert!(sha_matches("deadbeef", "deadbeef"));
        assert!(!sha_matches("deadbeef", "beefdead"));
        assert!(!sha_matches("", "deadbeef"));
        assert!(!sha_matches("deadbeef", ""));
        assert!(!sha_matches("", ""));
    }

    // ---- retag ----

    #[test]
    fn retag_single_target() {
        let got = retag(
            &picks(&["aaa1", "bbb2", "ccc3"]),
            &["bbb2"],
            TodoAction::Drop,
        )
        .unwrap();
        assert_eq!(got[0].action, TodoAction::Pick);
        assert_eq!(got[1].action, TodoAction::Drop);
        assert_eq!(got[2].action, TodoAction::Pick);
    }

    #[test]
    fn retag_multiple_targets_and_prefixes() {
        let entries = vec![
            commit(TodoAction::Pick, "deadbeefcafe", "one"),
            commit(TodoAction::Pick, "bbb2", "two"),
            commit(TodoAction::Pick, "ccc3", "three"),
        ];
        // First target abbreviated, second longer than the stored sha.
        let got = retag(&entries, &["deadbeef", "bbb2000"], TodoAction::Squash).unwrap();
        assert_eq!(got[0].action, TodoAction::Squash);
        assert_eq!(got[1].action, TodoAction::Squash);
        assert_eq!(got[2].action, TodoAction::Pick);
    }

    #[test]
    fn retag_missing_sha_errors() {
        let err = retag(&picks(&["aaa1"]), &["zzz9"], TodoAction::Edit).unwrap_err();
        assert_eq!(err, TodoError::ShaNotFound("zzz9".into()));
    }

    #[test]
    fn retag_ignores_non_commit_entries() {
        let entries = vec![bare(TodoAction::Exec("true".into()))];
        let err = retag(&entries, &["true"], TodoAction::Drop).unwrap_err();
        assert_eq!(err, TodoError::ShaNotFound("true".into()));
    }

    // ---- move_entry ----

    #[test]
    fn move_up_and_down_in_the_middle() {
        let entries = picks(&["aaa1", "bbb2", "ccc3"]);
        let up = move_entry(&entries, "bbb2", true).unwrap();
        assert_eq!(up[0].sha, "bbb2");
        assert_eq!(up[1].sha, "aaa1");
        let down = move_entry(&entries, "bbb2", false).unwrap();
        assert_eq!(down[1].sha, "ccc3");
        assert_eq!(down[2].sha, "bbb2");
    }

    #[test]
    fn move_refuses_at_boundaries() {
        let entries = picks(&["aaa1", "bbb2"]);
        assert_eq!(
            move_entry(&entries, "aaa1", true).unwrap_err(),
            TodoError::CannotMove("already first")
        );
        assert_eq!(
            move_entry(&entries, "bbb2", false).unwrap_err(),
            TodoError::CannotMove("already last")
        );
    }

    #[test]
    fn move_jumps_over_inert_lines() {
        let entries = vec![
            commit(TodoAction::Pick, "aaa1", "one"),
            bare(TodoAction::Exec("make test".into())),
            bare(TodoAction::Break),
            commit(TodoAction::Pick, "bbb2", "two"),
        ];
        let up = move_entry(&entries, "bbb2", true).unwrap();
        assert_eq!(up[0].sha, "bbb2");
        assert_eq!(up[1].action, TodoAction::Exec("make test".into()));
        assert_eq!(up[3].sha, "aaa1");
        let down = move_entry(&entries, "aaa1", false).unwrap();
        assert_eq!(down[0].sha, "bbb2");
        assert_eq!(down[3].sha, "aaa1");
    }

    #[test]
    fn move_refuses_to_cross_structural_lines() {
        for structural in [
            TodoAction::Label("base".into()),
            TodoAction::Reset("base".into()),
            TodoAction::Merge("-C abc topic".into()),
            TodoAction::UpdateRef("refs/heads/x".into()),
            TodoAction::Unknown("???".into()),
        ] {
            let entries = vec![
                commit(TodoAction::Pick, "aaa1", "one"),
                bare(structural),
                commit(TodoAction::Pick, "bbb2", "two"),
            ];
            assert_eq!(
                move_entry(&entries, "bbb2", true).unwrap_err(),
                TodoError::CannotMove("would cross a structural rebase line")
            );
            assert_eq!(
                move_entry(&entries, "aaa1", false).unwrap_err(),
                TodoError::CannotMove("would cross a structural rebase line")
            );
        }
    }

    #[test]
    fn move_refuses_when_only_inert_lines_remain() {
        let entries = vec![
            bare(TodoAction::Exec("true".into())),
            commit(TodoAction::Pick, "aaa1", "one"),
            bare(TodoAction::Exec("false".into())),
        ];
        assert_eq!(
            move_entry(&entries, "aaa1", true).unwrap_err(),
            TodoError::CannotMove("already first")
        );
        assert_eq!(
            move_entry(&entries, "aaa1", false).unwrap_err(),
            TodoError::CannotMove("already last")
        );
    }

    #[test]
    fn move_missing_sha_errors() {
        assert_eq!(
            move_entry(&picks(&["aaa1"]), "zzz9", true).unwrap_err(),
            TodoError::ShaNotFound("zzz9".into())
        );
    }

    // ---- place_fixup ----

    #[test]
    fn place_fixup_lands_after_the_target() {
        // Fixup at the end (the `git commit --fixup` flow), target mid-list.
        let entries = picks(&["aaa1", "bbb2", "ccc3", "ffff"]);
        let got = place_fixup(&entries, "ffff", "bbb2").unwrap();
        let shas: Vec<&str> = got.iter().map(|e| e.sha.as_str()).collect();
        assert_eq!(shas, ["aaa1", "bbb2", "ffff", "ccc3"]);
        assert_eq!(got[2].action, TodoAction::Fixup);
        assert_eq!(got[1].action, TodoAction::Pick);
    }

    #[test]
    fn place_fixup_target_is_last_entry() {
        let entries = picks(&["aaa1", "bbb2", "ffff"]);
        // Fixup earlier than the target also works (index shift covered).
        let got = place_fixup(&entries, "aaa1", "ffff").unwrap();
        let shas: Vec<&str> = got.iter().map(|e| e.sha.as_str()).collect();
        assert_eq!(shas, ["bbb2", "ffff", "aaa1"]);
        assert_eq!(got[2].action, TodoAction::Fixup);
    }

    #[test]
    fn place_fixup_missing_shas_error() {
        let entries = picks(&["aaa1", "bbb2"]);
        assert_eq!(
            place_fixup(&entries, "zzz9", "aaa1").unwrap_err(),
            TodoError::ShaNotFound("zzz9".into())
        );
        assert_eq!(
            place_fixup(&entries, "bbb2", "zzz9").unwrap_err(),
            TodoError::ShaNotFound("zzz9".into())
        );
        assert_eq!(
            place_fixup(&entries, "aaa1", "aaa1").unwrap_err(),
            TodoError::CannotMove("fixup and target are the same commit")
        );
    }

    #[test]
    fn place_fixup_with_identical_subjects_targets_by_sha() {
        // `--autosquash` would mis-target here; sha matching must not.
        let entries = vec![
            commit(TodoAction::Pick, "aaa1", "fix typo"),
            commit(TodoAction::Pick, "bbb2", "fix typo"),
            commit(TodoAction::Pick, "ffff", "fixup! fix typo"),
        ];
        let got = place_fixup(&entries, "ffff", "bbb2").unwrap();
        let shas: Vec<&str> = got.iter().map(|e| e.sha.as_str()).collect();
        assert_eq!(shas, ["aaa1", "bbb2", "ffff"]);
        assert_eq!(got[2].action, TodoAction::Fixup);
    }

    // ---- misc ----

    #[test]
    fn is_commit_classifies_actions() {
        assert!(TodoAction::Pick.is_commit());
        assert!(TodoAction::FixupC.is_commit());
        assert!(!TodoAction::Break.is_commit());
        assert!(!TodoAction::Exec("x".into()).is_commit());
        assert!(!TodoAction::Unknown("x".into()).is_commit());
    }

    #[test]
    fn errors_display_and_implement_error() {
        let e: &dyn std::error::Error = &TodoError::ShaNotFound("abc".into());
        assert_eq!(e.to_string(), "no todo entry matches sha abc");
        assert_eq!(
            TodoError::CannotMove("already first").to_string(),
            "cannot move entry: already first"
        );
    }

    // --- truncation / mutation / shape tables (THE-383) ---

    const US: char = '\u{1f}';
    /// Subject holding tab, a non-ASCII char and an embedded `\r` (a newline
    /// cannot occur in a line-oriented format).
    const LINE_HOSTILE: &str = "subj\twith tab \u{e9}\rmid";

    fn full_sha(n: usize) -> String {
        format!("{n:040x}")
    }

    fn log_row(n: usize) -> String {
        format!("{US}{}{US}{LINE_HOSTILE} {n}\n", full_sha(n + 1))
    }

    fn log_rows(count: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for n in 0..count {
            rows.push(log_row(n));
        }
        rows
    }

    fn assert_log_error(input: &str, expect: &str, label: &str) {
        match todo_from_log(input) {
            Ok(rows) => panic!(
                "{label}: expected error {expect:?}, parsed {} rows",
                rows.len()
            ),
            Err(error) => assert!(error.to_string().contains(expect), "{label}: {error}"),
        }
    }

    #[test]
    fn todo_from_log_truncation_table_every_byte_offset() {
        let rows = log_rows(4);
        let input: String = rows.concat();
        let mut starts = vec![0usize];
        let mut total = 0usize;
        for row in &rows {
            total += row.len();
            starts.push(total);
        }
        let full = todo_from_log(&input).unwrap();
        assert_eq!(full.len(), 4);
        // One unit separator, a 40-char sha and the second separator.
        let header = 1 + 40 + 1;
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            let whole = starts.iter().rposition(|s| *s <= cut).unwrap();
            let partial = cut - starts[whole];
            let parsed = todo_from_log(&input[..cut]);
            if partial == 0 {
                let got = parsed.unwrap_or_else(|e| panic!("cut {cut} must parse: {e}"));
                assert_eq!(got, full[..whole].to_vec(), "cut {cut}");
            } else if partial < header {
                assert!(
                    parsed.is_err(),
                    "cut {cut} inside the header must be rejected"
                );
            } else {
                // Tolerant by line shape: a truncated subject is kept short.
                let got = parsed.unwrap_or_else(|e| panic!("cut {cut} must parse: {e}"));
                assert_eq!(got.len(), whole + 1, "cut {cut}");
                assert_eq!(got[..whole].to_vec(), full[..whole].to_vec(), "cut {cut}");
                let last = &got[whole];
                assert_eq!(last.sha, full[whole].sha, "cut {cut}: sha never truncated");
                assert!(
                    full[whole].subject.starts_with(&last.subject),
                    "cut {cut}: subject {:?} is not a prefix",
                    last.subject
                );
            }
        }
    }

    #[test]
    fn todo_from_log_mutation_table() {
        let sha = full_sha(1);
        let table = vec![
            (
                "missing record separator",
                format!("{sha}{US}s\n"),
                "missing record separator",
            ),
            (
                "missing second separator",
                format!("{US}{sha}\n"),
                "expected sha and subject",
            ),
            (
                "only a separator",
                format!("{US}\n"),
                "expected sha and subject",
            ),
            (
                "uppercase sha",
                format!("{US}{}{US}s\n", sha.to_uppercase().replace('0', "A")),
                "invalid full sha",
            ),
            (
                "non-hex sha",
                format!("{US}{}{US}s\n", "z".repeat(40)),
                "invalid full sha",
            ),
            (
                "39-char sha",
                format!("{US}{}{US}s\n", "a".repeat(39)),
                "invalid full sha",
            ),
            (
                "41-char sha",
                format!("{US}{}{US}s\n", "a".repeat(41)),
                "invalid full sha",
            ),
            ("empty sha", format!("{US}{US}s\n"), "invalid full sha"),
            (
                "doubled separator",
                format!("{US}{US}{sha}{US}s\n"),
                "invalid full sha",
            ),
            (
                "sha with NUL",
                format!("{US}{}\0{US}s\n", &sha[..39]),
                "invalid full sha",
            ),
            (
                "duplicate commit",
                format!("{US}{sha}{US}a\n{US}{sha}{US}b\n"),
                "duplicate commit",
            ),
            ("bad second row", format!("{US}{sha}{US}a\nnope\n"), "row 2"),
        ];
        for (label, input, expect) in &table {
            assert_log_error(input, expect, label);
        }
    }

    #[test]
    fn todo_from_log_subject_may_hold_separators_and_empty() {
        let sha = full_sha(1);
        let got =
            todo_from_log(&format!("{US}{sha}{US}a{US}b\n{US}{}{US}\n", full_sha(2))).unwrap();
        assert_eq!(got[0].subject, format!("a{US}b"));
        assert_eq!(got[1].subject, "");
    }

    #[test]
    fn todo_from_log_line_ending_variants_agree() {
        let rows = log_rows(3);
        let lf: String = rows.concat();
        let want = todo_from_log(&lf).unwrap();
        assert_eq!(want.len(), 3);
        assert_eq!(want[0].subject, format!("{LINE_HOSTILE} 0"));

        let crlf = lf.replace('\n', "\r\n");
        let no_trailing = lf.trim_end_matches('\n').to_string();
        let crlf_no_trailing = crlf.trim_end_matches("\r\n").to_string();
        let blank_tail = format!("{lf}\n\n");
        let crlf_blank_tail = format!("{crlf}\r\n\r\n");
        let lone_cr_tail = format!("{lf}\r\n");
        for (label, input) in [
            ("crlf", crlf),
            ("no trailing newline", no_trailing),
            ("crlf without trailing newline", crlf_no_trailing),
            ("trailing blank lines", blank_tail),
            ("crlf trailing blank lines", crlf_blank_tail),
            ("trailing lone CRLF", lone_cr_tail),
        ] {
            assert_eq!(todo_from_log(&input).unwrap(), want, "{label}");
        }
    }

    #[test]
    fn todo_from_log_minimal_thousand_and_five_thousand() {
        assert_eq!(todo_from_log(&log_rows(1).concat()).unwrap().len(), 1);
        assert_eq!(todo_from_log("").unwrap().len(), 0);
        assert_eq!(todo_from_log(&log_rows(1000).concat()).unwrap().len(), 1000);
        let parsed = todo_from_log(&log_rows(5000).concat()).unwrap();
        assert_eq!(parsed.len(), 5000);
        assert_eq!(parsed[4999].sha, full_sha(5000));
    }

    fn todo_text() -> String {
        let mut text = String::new();
        write!(text, "pick {} {LINE_HOSTILE}\n", full_sha(1))
            .expect("writing to String cannot fail");
        write!(text, "reword {} second \u{e9}\n", full_sha(2))
            .expect("writing to String cannot fail");
        text.push_str("# a comment line\n");
        text.push_str("exec make test\n");
        text.push_str("break\n");
        write!(text, "drop {} last\n", full_sha(3)).expect("writing to String cannot fail");
        text
    }

    #[test]
    fn parse_todo_truncation_never_panics_and_never_corrupts_a_sha() {
        let input = todo_text();
        let full = parse_todo(&input);
        assert_eq!(full.len(), 5);
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            let prefix = &input[..cut];
            let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
            let whole = parse_todo(&prefix[..line_start]);
            let n = whole.len();
            assert_eq!(
                whole,
                full[..n].to_vec(),
                "cut {cut}: whole lines unchanged"
            );
            let got = parse_todo(prefix);
            assert!(got.len() <= n + 1, "cut {cut}");
            assert_eq!(got[..n].to_vec(), whole, "cut {cut}");
            if got.len() == n {
                continue; // cut on a line start, or inside a comment / blank
            }
            let partial = &got[n];
            let want = &full[n];
            let full_line = input[line_start..].lines().next().unwrap_or("");
            match &partial.action {
                TodoAction::Unknown(text) => {
                    assert!(full_line.starts_with(text.as_str()), "cut {cut}: {text:?}");
                }
                TodoAction::Exec(arg) => {
                    let TodoAction::Exec(whole_arg) = &want.action else {
                        panic!("cut {cut}: exec partial of a non-exec line");
                    };
                    assert!(whole_arg.starts_with(arg.as_str()), "cut {cut}");
                }
                action if action.is_commit() => {
                    assert_eq!(*action, want.action, "cut {cut}");
                    assert!(
                        want.sha.starts_with(&partial.sha),
                        "cut {cut}: sha {:?}",
                        partial.sha
                    );
                    assert!(want.subject.starts_with(&partial.subject), "cut {cut}");
                }
                other => assert_eq!(*other, want.action, "cut {cut}"),
            }
        }
    }

    #[test]
    fn parse_todo_truncated_fixup_flag_is_preserved_as_unknown() {
        // Tolerant (never an error), but a todo cut right after `fixup -` or
        // a flag with no sha never becomes a commit entry with a flag "sha".
        for line in ["fixup -", "f -", "fixup -C", "fixup -c", "fixup -x"] {
            assert_eq!(
                parse_todo(line),
                vec![entry(TodoAction::Unknown(line.to_string()))],
                "{line}"
            );
        }
    }

    #[test]
    fn parse_todo_garbage_is_preserved_never_an_error() {
        let got = parse_todo("\u{0}\u{1f}weird\npi\n\t \nexec\n");
        assert_eq!(got.len(), 3);
        assert!(
            got.iter()
                .all(|e| matches!(e.action, TodoAction::Unknown(_)))
        );
    }

    #[test]
    fn parse_todo_line_ending_variants_agree() {
        let lf = todo_text();
        let want = parse_todo(&lf);
        assert_eq!(want.len(), 5);
        assert_eq!(want[0].subject, LINE_HOSTILE);
        let crlf = lf.replace('\n', "\r\n");
        let no_trailing = lf.trim_end_matches('\n').to_string();
        let crlf_no_trailing = crlf.trim_end_matches("\r\n").to_string();
        let blank_tail = format!("{lf}\n\n");
        let crlf_blank_tail = format!("{crlf}\r\n\r\n");
        let blank_head = format!("\n\n{lf}");
        for (label, input) in [
            ("crlf", crlf),
            ("no trailing newline", no_trailing),
            ("crlf without trailing newline", crlf_no_trailing),
            ("trailing blank lines", blank_tail),
            ("crlf trailing blank lines", crlf_blank_tail),
            ("leading blank lines", blank_head),
        ] {
            assert_eq!(parse_todo(&input), want, "{label}");
        }
    }

    #[test]
    fn parse_todo_minimal_thousand_and_five_thousand() {
        assert!(parse_todo("").is_empty());
        assert_eq!(parse_todo("pick abc\n").len(), 1);
        let mut text = String::new();
        for n in 0..5000 {
            write!(text, "pick {} {LINE_HOSTILE}\n", full_sha(n))
                .expect("writing to String cannot fail");
        }
        let parsed = parse_todo(&text);
        assert_eq!(parsed.len(), 5000);
        assert_eq!(parsed[4999].sha, full_sha(4999));
        let thousand: String = text.lines().take(1000).map(|l| format!("{l}\n")).collect();
        assert_eq!(parse_todo(&thousand).len(), 1000);
    }
}
