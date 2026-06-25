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
            // A flag with no sha is malformed; let it fall to Unknown.
            None if rest == "-C" || rest == "-c" => None,
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
/// `git log --reverse --format=%x1f%H%x1f%s` output (unit-separator-prefixed
/// records, oldest first): every commit becomes `Pick`. The unit separator
/// cannot appear in a sha or single-line subject, so splitting on it is safe.
pub fn todo_from_log(log_out: &str) -> Vec<TodoEntry> {
    let mut fields = log_out.split('\x1f');
    fields.next(); // Anything before the first record separator.
    let mut entries = Vec::new();
    while let (Some(sha), Some(subject)) = (fields.next(), fields.next()) {
        let sha = sha.trim();
        if sha.is_empty() {
            continue;
        }
        entries.push(TodoEntry {
            action: TodoAction::Pick,
            sha: sha.to_string(),
            subject: subject.trim_end_matches(['\n', '\r']).to_string(),
        });
    }
    entries
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
        let got = parse_todo("exec cargo test -p superzej-core --lib rebase_todo\n");
        assert_eq!(
            got,
            vec![bare(TodoAction::Exec(
                "cargo test -p superzej-core --lib rebase_todo".into()
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
        assert_eq!(todo_from_log(""), vec![]);
        assert_eq!(todo_from_log("\n"), vec![]);
    }

    #[test]
    fn todo_from_log_single_commit() {
        let got = todo_from_log("\u{1f}deadbeef\u{1f}fix: the thing\n");
        assert_eq!(
            got,
            vec![commit(TodoAction::Pick, "deadbeef", "fix: the thing")]
        );
    }

    #[test]
    fn todo_from_log_three_commits_with_unicode_subjects() {
        let log = "\u{1f}aaa1\u{1f}feat: add naïve café parser\n\
                   \u{1f}bbb2\u{1f}fix: 修复 the 🐛 bug\n\
                   \u{1f}ccc3\u{1f}\n";
        let got = todo_from_log(log);
        assert_eq!(
            got,
            vec![
                commit(TodoAction::Pick, "aaa1", "feat: add naïve café parser"),
                commit(TodoAction::Pick, "bbb2", "fix: 修复 the 🐛 bug"),
                commit(TodoAction::Pick, "ccc3", ""),
            ]
        );
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
}
