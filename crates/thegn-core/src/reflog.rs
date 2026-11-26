//! Undo/redo planning over the HEAD reflog, lazygit-style: classify each
//! entry as a user action, then walk newest→oldest with a bracket counter so
//! resets *we* issued cancel out instead of being undone themselves.
//!
//! Pure — the host runs `git reflog -z --no-show-signature
//! --format=%H%x00%gd%x00%ct%x00%gs -n 100`
//! off-thread and persists the target of every reset it performs (undo *and*
//! redo) in [`OurMarks`] (SQLite at the call site). Lazygit tags its resets
//! with distinct `[lazygit undo]`/`[lazygit redo]` reflog markers; our marks
//! are an undirected sha set, so polarity is reconstructed structurally
//! first: walking oldest→newest with a stack of still-open undos, one of our
//! resets is a *redo* iff it targets the state the top open undo moved away
//! from (the sha of the entry just below that undo) — otherwise it opens a
//! new undo.
//!
//! # The counter
//!
//! With polarity assigned, both planners walk newest→oldest. `Other` entries
//! and checkouts that didn't move are transparent. Then:
//!
//! - undo mark → `counter += 1` (it already cancelled one user action below);
//! - redo mark → `counter -= 1` (it re-applied one);
//! - user action, [`plan_undo`]: if `counter == 0` this is the action to
//!   invert (checkout back to `from`, or hard-reset to the next-older entry's
//!   sha — the pre-state), else `counter -= 1`;
//! - user action, [`plan_redo`]: `counter -= 1` first; `0` → re-apply this
//!   action (hard-reset to its own sha / checkout its `to`), negative → a
//!   user action intervened since the last open undo, so nothing to redo.
//!
//! Worked example — `commit one` (HEAD=A), `commit two` (HEAD=B), undo, undo
//! leaves marks `{A, P}` and this reflog (newest first, P = pre-`one` state):
//!
//! ```text
//! HEAD@{0}  P  reset: moving to P   undo mark   counter 0 → 1
//! HEAD@{1}  A  reset: moving to A   undo mark   counter 1 → 2
//! HEAD@{2}  B  commit: two          user        redo: 2 → 1; undo: 2 ≠ 0 → 1
//! HEAD@{3}  A  commit: one          user        redo: 1 → 0 ✓; undo: 1 ≠ 0 → 0
//! HEAD@{4}  P  …                    noise
//! ```
//!
//! so `plan_redo` re-applies `commit: one` (`HardResetTo A`) and `plan_undo`
//! exhausts the walk (`Nothing` — both commits are already undone). After
//! that redo runs, its entry (`reset: moving to A`, matching the open undo at
//! `HEAD@{1}`'s pre-state) counts `-1`, which is exactly what makes the next
//! redo land on `commit: two` and a fresh undo land back on it too.

/// What a reflog entry did, parsed from its `%gs` subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflogAction {
    /// `commit: …`, `commit (initial): …`, `commit (merge): …`.
    Commit,
    /// `commit (amend): …`.
    Amend,
    /// `rebase (start|pick|finish|…): …` and the `rebase -i (…)` forms.
    Rebase,
    /// `merge <ref>: …`.
    Merge,
    /// `cherry-pick: …`.
    CherryPick,
    /// `revert: …`.
    Revert,
    /// `reset: moving to <target>`.
    Reset {
        /// The reset target verbatim (a sha for resets we issue).
        to: String,
    },
    /// `checkout: moving from <from> to <to>`.
    Checkout {
        /// The ref/sha HEAD moved away from.
        from: String,
        /// The ref/sha HEAD moved to.
        to: String,
    },
    /// Anything else — transparent to the planners.
    Other,
}

/// One parsed reflog row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    /// `%H` — the commit HEAD pointed at *after* the action.
    pub sha: String,
    /// `%gd`, e.g. `HEAD@{0}`.
    pub selector: String,
    /// `%ct` — commit timestamp (unix seconds).
    pub date: i64,
    /// The classified action.
    pub action: ReflogAction,
    /// `%gs` verbatim, for UI display.
    pub raw: String,
}

/// Classify a `%gs` subject. Order matters: `commit (amend)` before the
/// `commit` catch-all (which also covers `(initial)`/`(merge)`).
fn classify(raw: &str) -> ReflogAction {
    if raw.starts_with("commit (amend)") {
        return ReflogAction::Amend;
    }
    if raw.starts_with("commit") {
        return ReflogAction::Commit;
    }
    if raw.starts_with("rebase") {
        return ReflogAction::Rebase;
    }
    if raw.starts_with("cherry-pick") {
        return ReflogAction::CherryPick;
    }
    if raw.starts_with("revert") {
        return ReflogAction::Revert;
    }
    if let Some(to) = raw.strip_prefix("reset: moving to ") {
        return ReflogAction::Reset {
            to: to.trim().to_string(),
        };
    }
    if let Some(rest) = raw.strip_prefix("checkout: moving from ")
        && let Some((from, to)) = rest.split_once(" to ")
    {
        return ReflogAction::Checkout {
            from: from.to_string(),
            to: to.trim().to_string(),
        };
    }
    if raw.starts_with("merge ") {
        return ReflogAction::Merge;
    }
    ReflogAction::Other
}

/// Parse `git reflog -z --format=%H%x00%gd%x00%ct%x00%gs -n 100` output
/// (newest first). The NUL framing makes each field unambiguous, including
/// subjects containing other control characters. Malformed output is rejected
/// as a whole so positional undo planning can never use a shortened history.
pub fn parse_reflog(out: &str) -> anyhow::Result<Vec<ReflogEntry>> {
    parse_reflog_with_limits(
        out,
        crate::gitrefs::MAX_GIT_OUTPUT_BYTES,
        crate::gitrefs::MAX_GIT_FIELD_BYTES,
    )
}

fn parse_reflog_with_limits(
    out: &str,
    max_output: usize,
    max_field: usize,
) -> anyhow::Result<Vec<ReflogEntry>> {
    crate::gitrefs::check_git_output(out, max_output, max_field)?;
    if out.is_empty() {
        return Ok(Vec::new());
    }

    let record_count = out.bytes().filter(|byte| *byte == b'\0').count() / 4;
    anyhow::ensure!(
        out.ends_with('\0'),
        "reflog record {record_count}: missing final NUL terminator"
    );

    let fields: Vec<&str> = out[..out.len() - 1].split('\0').collect();
    anyhow::ensure!(
        fields.len().is_multiple_of(4),
        "reflog trailing record {} has {} of 4 fields",
        fields.len() / 4,
        fields.len() % 4
    );

    let mut entries = Vec::with_capacity(fields.len() / 4);
    for (record_index, record) in fields.as_chunks::<4>().0.iter().enumerate() {
        let sha = record[0];
        let selector = record[1];
        let timestamp = record[2];
        let raw = record[3];
        anyhow::ensure!(
            (sha.len() == 40 || sha.len() == 64)
                && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "reflog record {record_index}: object id must be 40 or 64 hexadecimal characters"
        );
        anyhow::ensure!(
            !selector.is_empty(),
            "reflog record {record_index}: selector is empty"
        );
        anyhow::ensure!(
            !timestamp.is_empty() && timestamp.bytes().all(|byte| byte.is_ascii_digit()),
            "reflog record {record_index}: timestamp must contain decimal digits only"
        );
        let date = timestamp.parse::<i64>().map_err(|error| {
            anyhow::anyhow!("reflog record {record_index}: invalid timestamp: {error}")
        })?;
        entries.push(ReflogEntry {
            sha: sha.to_string(),
            selector: selector.to_string(),
            date,
            action: classify(raw),
            raw: raw.to_string(),
        });
    }
    Ok(entries)
}

/// Full-or-prefix sha equality, either direction — reflog targets may be
/// abbreviated while marks hold full `%H` hashes, or vice versa. Empty
/// strings never match.
fn sha_matches(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// Targets of `reset: moving to <sha>` entries that *we* created (undo and
/// redo resets), persisted by the caller. Order-insensitive set semantics;
/// membership matches on full sha or prefix in either direction.
#[derive(Debug, Clone, Default)]
pub struct OurMarks {
    shas: Vec<String>,
}

impl OurMarks {
    /// Build the set from persisted sha strings.
    pub fn new(shas: impl IntoIterator<Item = String>) -> Self {
        Self {
            shas: shas.into_iter().collect(),
        }
    }

    /// Whether `sha` matches any stored mark (full or prefix, either way).
    pub fn contains(&self, sha: &str) -> bool {
        self.shas.iter().any(|m| sha_matches(m, sha))
    }
}

/// What the caller should run to undo (or redo) the last user action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoPlan {
    /// `git reset --hard <sha>` (dirty-worktree autostash guard at the call
    /// site). The caller must add `sha` to [`OurMarks`] after running it.
    HardResetTo {
        /// The reset target.
        sha: String,
        /// Raw `%gs` of the action being un/re-done, for the confirm dialog.
        undoing: String,
    },
    /// `git checkout <branch>` — inverse (or replay) of a checkout entry.
    Checkout {
        /// The ref to check out.
        branch: String,
        /// Raw `%gs` of the action being un/re-done, for the confirm dialog.
        undoing: String,
    },
    /// Nothing to do.
    Nothing,
}

/// How an entry participates in the counter walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Transparent: `Other`, or a checkout that didn't move.
    Noise,
    /// One of our resets that cancelled a user action below it (an undo).
    UndoMark,
    /// One of our resets that re-applied one (a redo).
    RedoMark,
    /// A real user action — a candidate target.
    User,
}

/// Classify every entry, reconstructing undo/redo polarity for our resets:
/// walking oldest→newest with a stack of still-open undos (each recording
/// the sha HEAD moved away from — its next-older neighbour), one of our
/// resets is a redo iff it targets the top open undo's pre-state; otherwise
/// it opens a new undo. An open undo with no older neighbour records no
/// pre-state and can never be matched.
fn classify_steps(entries: &[ReflogEntry], marks: &OurMarks) -> Vec<Step> {
    let mut steps = vec![Step::Noise; entries.len()];
    let mut open: Vec<Option<&str>> = Vec::new();
    for i in (0..entries.len()).rev() {
        steps[i] = match &entries[i].action {
            ReflogAction::Other => Step::Noise,
            ReflogAction::Checkout { from, to } if from == to => Step::Noise,
            ReflogAction::Reset { to } if marks.contains(to) => {
                if matches!(open.last(), Some(Some(pre)) if sha_matches(to, pre)) {
                    open.pop();
                    Step::RedoMark
                } else {
                    open.push(entries.get(i + 1).map(|p| p.sha.as_str()));
                    Step::UndoMark
                }
            }
            _ => Step::User,
        };
    }
    steps
}

/// Plan the inverse of the most recent user action not already cancelled by
/// one of our undo resets (see the module docs for the counter). The target's
/// pre-state is the next-older entry's sha; a user action with no older
/// entry — or a reflog that is all marks/noise — yields [`UndoPlan::Nothing`].
pub fn plan_undo(entries: &[ReflogEntry], marks: &OurMarks) -> UndoPlan {
    let steps = classify_steps(entries, marks);
    let mut counter: i32 = 0;
    for (i, entry) in entries.iter().enumerate() {
        match steps[i] {
            Step::Noise => {}
            Step::UndoMark => counter += 1,
            Step::RedoMark => counter -= 1,
            Step::User if counter != 0 => counter -= 1,
            Step::User => {
                return match (&entry.action, entries.get(i + 1)) {
                    (ReflogAction::Checkout { from, .. }, _) => UndoPlan::Checkout {
                        branch: from.clone(),
                        undoing: entry.raw.clone(),
                    },
                    (_, Some(prev)) => UndoPlan::HardResetTo {
                        sha: prev.sha.clone(),
                        undoing: entry.raw.clone(),
                    },
                    (_, None) => UndoPlan::Nothing,
                };
            }
        }
    }
    UndoPlan::Nothing
}

/// Plan the re-application of the most recently undone user action. A user
/// action newer than every open undo invalidates redo ([`UndoPlan::Nothing`]);
/// otherwise the target is the user action whose decrement brings the counter
/// back to zero, restored via its own entry sha (the post-action state).
pub fn plan_redo(entries: &[ReflogEntry], marks: &OurMarks) -> UndoPlan {
    let steps = classify_steps(entries, marks);
    let mut counter: i32 = 0;
    for (i, entry) in entries.iter().enumerate() {
        match steps[i] {
            Step::Noise => {}
            Step::UndoMark => counter += 1,
            Step::RedoMark => counter -= 1,
            Step::User => {
                counter -= 1;
                if counter < 0 {
                    return UndoPlan::Nothing;
                }
                if counter == 0 {
                    return match &entry.action {
                        ReflogAction::Checkout { to, .. } => UndoPlan::Checkout {
                            branch: to.clone(),
                            undoing: entry.raw.clone(),
                        },
                        _ => UndoPlan::HardResetTo {
                            sha: entry.sha.clone(),
                            undoing: entry.raw.clone(),
                        },
                    };
                }
            }
        }
    }
    UndoPlan::Nothing
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry with the action classified from `raw`, like [`parse_reflog`]
    /// would build it.
    fn entry(sha: &str, raw: &str) -> ReflogEntry {
        ReflogEntry {
            sha: sha.to_string(),
            selector: String::new(),
            date: 0,
            action: classify(raw),
            raw: raw.to_string(),
        }
    }

    fn marks(shas: &[&str]) -> OurMarks {
        OurMarks::new(shas.iter().map(|s| s.to_string()))
    }

    /// The base history: `commit one` (HEAD=aaa1) on top of state ppp0, then
    /// `commit two` (HEAD=bbb2). Newest first.
    fn base() -> Vec<ReflogEntry> {
        vec![
            entry("bbb2", "commit: two"),
            entry("aaa1", "commit: one"),
            entry("ppp0", "clone: from somewhere"),
        ]
    }

    fn reset_to(sha: &str) -> ReflogEntry {
        entry(sha, &format!("reset: moving to {sha}"))
    }

    fn hard_reset(sha: &str, undoing: &str) -> UndoPlan {
        UndoPlan::HardResetTo {
            sha: sha.to_string(),
            undoing: undoing.to_string(),
        }
    }

    // ── parse_reflog ────────────────────────────────────────────────────

    #[test]
    fn parse_classifies_each_subject_form() {
        let cases = [
            ("commit: msg", ReflogAction::Commit),
            ("commit (initial): msg", ReflogAction::Commit),
            ("commit (merge): msg", ReflogAction::Commit),
            ("commit (amend): msg", ReflogAction::Amend),
            ("rebase (start): checkout main", ReflogAction::Rebase),
            ("rebase (pick): msg", ReflogAction::Rebase),
            (
                "rebase (finish): returning to refs/heads/x",
                ReflogAction::Rebase,
            ),
            ("rebase -i (start): checkout HEAD~3", ReflogAction::Rebase),
            ("merge feature/x: Fast-forward", ReflogAction::Merge),
            ("cherry-pick: msg", ReflogAction::CherryPick),
            ("revert: msg", ReflogAction::Revert),
            (
                "reset: moving to abc123",
                ReflogAction::Reset {
                    to: "abc123".into(),
                },
            ),
            (
                "checkout: moving from feature/a/b to fix/c-d",
                ReflogAction::Checkout {
                    from: "feature/a/b".into(),
                    to: "fix/c-d".into(),
                },
            ),
            ("reset: something else", ReflogAction::Other),
            ("checkout: moving from nowhere", ReflogAction::Other),
            ("pull: Fast-forward", ReflogAction::Other),
            ("total junk", ReflogAction::Other),
        ];
        for (gs, want) in cases {
            let out = format!("{}\0HEAD@{{0}}\01700000000\0{gs}\0", "a".repeat(40));
            let parsed = parse_reflog(&out).unwrap();
            assert_eq!(parsed.len(), 1, "one row for {gs:?}");
            assert_eq!(parsed[0].action, want, "classification of {gs:?}");
            assert_eq!(parsed[0].raw, gs, "raw preserved verbatim");
        }
    }

    #[test]
    fn parse_preserves_fields() {
        let out = format!(
            "{}\0HEAD@{{3}}\01712345678\0commit: hello\0",
            "a".repeat(40)
        );
        let parsed = parse_reflog(&out).unwrap();
        assert_eq!(
            parsed,
            vec![ReflogEntry {
                sha: "a".repeat(40),
                selector: "HEAD@{3}".into(),
                date: 1_712_345_678,
                action: ReflogAction::Commit,
                raw: "commit: hello".into(),
            }]
        );
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse_reflog("").unwrap().is_empty());
        assert!(parse_reflog("\n\n").is_err());
    }

    #[test]
    fn parse_rejects_any_malformed_record() {
        let good_sha = "a".repeat(40);
        let good = format!("{good_sha}\0HEAD@{{0}}\01700000000\0commit: good\0");
        let bad_records = [
            format!("{good_sha}\0HEAD@{{0}}\01700000000\0commit: no final terminator"),
            format!("{good_sha}\0HEAD@{{0}}\01700000000\0"),
            format!(
                "{}\0HEAD@{{0}}\01700000000\0commit: short\0",
                "a".repeat(39)
            ),
            format!(
                "{}g\0HEAD@{{0}}\01700000000\0commit: non-hex\0",
                "a".repeat(39)
            ),
            format!("{good_sha}\0\01700000000\0commit: empty selector\0"),
            format!("{good_sha}\0HEAD@{{0}}\0not-a-date\0commit: bad timestamp\0"),
            format!(
                "{good_sha}\0HEAD@{{0}}\0{}\0commit: overflowing timestamp\0",
                "9".repeat(30)
            ),
            format!(
                "{good}{short}\0HEAD@{{1}}\01700000000\0",
                short = "b".repeat(39)
            ),
        ];

        let appended_bad_error = parse_reflog(bad_records.last().unwrap())
            .expect_err("a later malformed row must reject the complete history");
        assert_eq!(
            appended_bad_error.to_string(),
            "reflog trailing record 1 has 3 of 4 fields"
        );

        for out in bad_records.iter().take(bad_records.len() - 1) {
            let error =
                parse_reflog(out).expect_err("malformed input must reject the whole reflog");
            let message = error.to_string();
            assert!(
                message.starts_with("reflog record")
                    || message.starts_with("reflog trailing record"),
                "{message}"
            );
        }
    }

    #[test]
    fn parse_keeps_unit_separator_in_subject() {
        let sha = "b".repeat(40);
        let subject = "commit: a\u{1f}b";
        let out = format!("{sha}\0HEAD@{{0}}\01700000000\0{subject}\0");

        let parsed = parse_reflog(&out).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].raw, subject);
    }

    #[test]
    fn parse_accepts_sha256_ids() {
        let sha = "c".repeat(64);
        let out = format!("{sha}\0HEAD@{{0}}\01700000000\0commit: sha256\0");

        let parsed = parse_reflog(&out).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].sha, sha);
    }

    // ── OurMarks ────────────────────────────────────────────────────────

    #[test]
    fn marks_match_full_and_prefix_both_ways() {
        let m = marks(&["abcdef012345"]);
        assert!(m.contains("abcdef012345"), "exact");
        assert!(m.contains("abcdef0"), "needle is a prefix of the mark");
        assert!(m.contains("abcdef0123456789"), "mark is a prefix");
        assert!(!m.contains("abce"), "diverging sha");
        assert!(!m.contains(""), "empty needle never matches");
        assert!(!marks(&[""]).contains("abc"), "empty mark never matches");
        assert!(!OurMarks::default().contains("abc"), "empty set");
    }

    // ── plan_undo ───────────────────────────────────────────────────────

    #[test]
    fn undo_simple_commit_targets_next_older_sha() {
        assert_eq!(
            plan_undo(&base(), &OurMarks::default()),
            hard_reset("aaa1", "commit: two")
        );
    }

    #[test]
    fn undo_skips_our_own_reset() {
        // After one undo (reset to aaa1) the next undo brackets past it and
        // targets `commit: one` → reset to the pre-`one` state.
        let mut entries = base();
        entries.insert(0, reset_to("aaa1"));
        assert_eq!(
            plan_undo(&entries, &marks(&["aaa1"])),
            hard_reset("ppp0", "commit: one")
        );
    }

    #[test]
    fn undo_two_user_actions_one_ours() {
        // commit three (on the undone state) is the newest user action; our
        // older reset is irrelevant to it.
        let mut entries = base();
        entries.insert(0, reset_to("aaa1"));
        entries.insert(0, entry("ccc3", "commit: three"));
        assert_eq!(
            plan_undo(&entries, &marks(&["aaa1"])),
            hard_reset("aaa1", "commit: three")
        );
    }

    #[test]
    fn undo_inverts_checkout() {
        let mut entries = base();
        entries.insert(0, entry("bbb2", "checkout: moving from main to feature/x"));
        assert_eq!(
            plan_undo(&entries, &OurMarks::default()),
            UndoPlan::Checkout {
                branch: "main".into(),
                undoing: "checkout: moving from main to feature/x".into(),
            }
        );
    }

    #[test]
    fn undo_ignores_noop_checkout() {
        let mut entries = base();
        entries.insert(0, entry("bbb2", "checkout: moving from main to main"));
        assert_eq!(
            plan_undo(&entries, &OurMarks::default()),
            hard_reset("aaa1", "commit: two")
        );
    }

    #[test]
    fn undo_targets_foreign_reset() {
        // A reset we did NOT issue is a user action like any other.
        let mut entries = base();
        entries.insert(0, entry("xyz9", "reset: moving to xyz9"));
        assert_eq!(
            plan_undo(&entries, &OurMarks::default()),
            hard_reset("bbb2", "reset: moving to xyz9")
        );
    }

    #[test]
    fn undo_oldest_entry_is_nothing() {
        let entries = vec![entry("aaa1", "commit: one")];
        assert_eq!(plan_undo(&entries, &OurMarks::default()), UndoPlan::Nothing);
    }

    #[test]
    fn undo_all_ours_is_nothing() {
        let entries = vec![reset_to("aaa1")];
        assert_eq!(plan_undo(&entries, &marks(&["aaa1"])), UndoPlan::Nothing);
    }

    #[test]
    fn undo_empty_is_nothing() {
        assert_eq!(plan_undo(&[], &OurMarks::default()), UndoPlan::Nothing);
    }

    // ── plan_redo ───────────────────────────────────────────────────────

    #[test]
    fn redo_after_single_undo() {
        // Sequence 1: commit one, commit two, undo → redo restores the
        // `commit: two` state.
        let mut entries = base();
        entries.insert(0, reset_to("aaa1"));
        assert_eq!(
            plan_redo(&entries, &marks(&["aaa1"])),
            hard_reset("bbb2", "commit: two")
        );
    }

    #[test]
    fn redo_invalidated_by_new_commit() {
        // Sequence 2: a user action after the undo consumes it.
        let mut entries = base();
        entries.insert(0, reset_to("aaa1"));
        entries.insert(0, entry("ccc3", "commit: three"));
        assert_eq!(plan_redo(&entries, &marks(&["aaa1"])), UndoPlan::Nothing);
    }

    #[test]
    fn redo_without_undo_is_nothing() {
        assert_eq!(plan_redo(&base(), &OurMarks::default()), UndoPlan::Nothing);
        assert_eq!(plan_redo(&[], &OurMarks::default()), UndoPlan::Nothing);
    }

    #[test]
    fn redo_full_cycle_undo_undo_redo_redo() {
        // Sequence 3 + the full cycle: undo, undo, redo (→ `one`'s state A),
        // redo (→ `two`'s state B), then redo again exhausts.
        let mut entries = base();
        let mut m = marks(&[]);

        // undo #1 → reset to aaa1.
        assert_eq!(plan_undo(&entries, &m), hard_reset("aaa1", "commit: two"));
        entries.insert(0, reset_to("aaa1"));
        m = marks(&["aaa1"]);

        // undo #2 brackets past our reset → reset to ppp0.
        assert_eq!(plan_undo(&entries, &m), hard_reset("ppp0", "commit: one"));
        entries.insert(0, reset_to("ppp0"));
        m = marks(&["aaa1", "ppp0"]);

        // Nothing left to undo; redo #1 restores `commit: one`'s state.
        assert_eq!(plan_undo(&entries, &m), UndoPlan::Nothing);
        assert_eq!(plan_redo(&entries, &m), hard_reset("aaa1", "commit: one"));
        entries.insert(0, reset_to("aaa1"));

        // redo #2 restores `commit: two`'s state.
        assert_eq!(plan_redo(&entries, &m), hard_reset("bbb2", "commit: two"));
        entries.insert(0, reset_to("bbb2"));
        m = marks(&["aaa1", "ppp0", "bbb2"]);

        // Fully redone: nothing further to redo, and undo targets `two` again.
        assert_eq!(plan_redo(&entries, &m), UndoPlan::Nothing);
        assert_eq!(plan_undo(&entries, &m), hard_reset("aaa1", "commit: two"));
    }

    #[test]
    fn redo_reapplies_checkout() {
        // An open undo above a checkout entry: redo replays the checkout's
        // destination.
        let mut entries = base();
        entries.insert(0, entry("bbb2", "checkout: moving from main to feature/x"));
        entries.insert(0, reset_to("sss7"));
        assert_eq!(
            plan_redo(&entries, &marks(&["sss7"])),
            UndoPlan::Checkout {
                branch: "feature/x".into(),
                undoing: "checkout: moving from main to feature/x".into(),
            }
        );
    }

    // ── combined narrative ──────────────────────────────────────────────

    #[test]
    fn narrative_undo_redo_undo() {
        // commit one → commit two → undo → redo → undo, with the caller
        // appending reflog rows and marking every reset it runs.
        let mut entries = base();
        let mut m = marks(&[]);

        // undo: drop `commit: two`.
        assert_eq!(plan_undo(&entries, &m), hard_reset("aaa1", "commit: two"));
        entries.insert(0, reset_to("aaa1"));
        m = marks(&["aaa1"]);

        // redo: bring it back.
        assert_eq!(plan_redo(&entries, &m), hard_reset("bbb2", "commit: two"));
        entries.insert(0, reset_to("bbb2"));
        m = marks(&["aaa1", "bbb2"]);

        // undo again: the redo reset is recognised as a redo mark (it
        // re-targets the state our undo moved away from), so the pair cancels
        // and `commit: two` is the target once more.
        assert_eq!(plan_undo(&entries, &m), hard_reset("aaa1", "commit: two"));
        // …and redo is spent.
        assert_eq!(plan_redo(&entries, &m), UndoPlan::Nothing);
    }

    // --- truncation / mutation / shape tables (THE-383) ---

    /// Subject holding tab, newline, a non-ASCII char and an embedded `\r`.
    const HOSTILE: &str = "commit: tab\there\nnew line \u{e9}\rcr";

    fn reflog_oid(n: usize) -> String {
        format!("{n:040x}")
    }

    fn reflog_fields(n: usize) -> Vec<String> {
        vec![
            reflog_oid(n + 1),
            format!("HEAD@{{{n}}}"),
            format!("{}", 1_700_000_000 + n),
            HOSTILE.to_string(),
        ]
    }

    fn reflog_record(fields: &[String]) -> String {
        let mut out = fields.join("\0");
        out.push('\0');
        out
    }

    fn reflog_records(count: usize) -> Vec<String> {
        let mut records = Vec::new();
        for n in 0..count {
            records.push(reflog_record(&reflog_fields(n)));
        }
        records
    }

    fn reflog_with(index: usize, value: &str) -> String {
        let mut fields = reflog_fields(1);
        fields[index] = value.to_string();
        reflog_record(&fields)
    }

    #[test]
    fn reflog_truncation_table_every_byte_offset() {
        let records = reflog_records(4);
        let input: String = records.concat();
        let mut bounds = vec![0usize];
        let mut total = 0usize;
        for record in &records {
            total += record.len();
            bounds.push(total);
        }
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            let parsed = parse_reflog(&input[..cut]);
            match bounds.iter().position(|b| *b == cut) {
                Some(count) => {
                    let rows = parsed.unwrap_or_else(|e| panic!("cut {cut} must parse: {e}"));
                    assert_eq!(rows.len(), count, "cut {cut}");
                }
                None => assert!(parsed.is_err(), "cut {cut} mid-record must be rejected"),
            }
        }
    }

    #[test]
    fn reflog_every_control_byte_round_trips_without_shifting_fields() {
        let controls: Vec<u8> = (0x01u8..=0x1f).chain([0x7f]).collect();
        for b in controls {
            let c = char::from(b);
            let subject = format!("commit: a{c}b{c}");
            let mut mid = reflog_fields(1);
            mid[3] = subject.clone();
            let records = [
                reflog_record(&reflog_fields(0)),
                reflog_record(&mid),
                reflog_record(&reflog_fields(2)),
            ];
            let parsed = parse_reflog(&records.concat())
                .unwrap_or_else(|e| panic!("byte {b:#04x} must parse: {e}"));
            assert_eq!(parsed.len(), 3, "byte {b:#04x}");
            assert_eq!(parsed[0], parse_reflog(&records[0]).unwrap()[0]);
            assert_eq!(parsed[2], parse_reflog(&records[2]).unwrap()[0]);
            assert_eq!(parsed[1].raw, subject, "byte {b:#04x}");
            assert_eq!(parsed[1].sha, reflog_oid(2), "byte {b:#04x}");
            assert_eq!(parsed[1].selector, "HEAD@{1}", "byte {b:#04x}");
            assert_eq!(parsed[1].date, 1_700_000_001, "byte {b:#04x}");
        }
    }

    #[test]
    fn reflog_truncation_prefixes_equal_the_leading_records() {
        let records = reflog_records(3);
        let input: String = records.concat();
        let mut bounds = vec![0usize];
        let mut total = 0usize;
        for record in &records {
            total += record.len();
            bounds.push(total);
        }
        for cut in 0..=input.len() {
            if !input.is_char_boundary(cut) {
                continue;
            }
            if let Ok(rows) = parse_reflog(&input[..cut]) {
                let k = bounds.iter().position(|b| *b == cut).expect("boundary");
                let expect = parse_reflog(&records[..k].concat()).unwrap();
                assert_eq!(rows, expect, "cut {cut}");
            }
        }
    }

    #[test]
    fn reflog_hostile_subject_round_trips() {
        let parsed = parse_reflog(&reflog_records(2).concat()).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].raw, HOSTILE);
        assert_eq!(parsed[1].action, ReflogAction::Commit);
        assert_eq!(parsed[1].date, 1_700_000_001);
    }

    #[test]
    fn reflog_mutation_table() {
        let good = reflog_with(1, "HEAD@{1}");
        let mut missing = reflog_fields(1);
        missing.remove(2);
        let mut five = reflog_fields(1);
        five.push("extra".to_string());
        let sha_msg = "object id must be 40 or 64 hexadecimal characters";
        let ts_msg = "timestamp must contain decimal digits only";
        let table = vec![
            ("non-hex oid", reflog_with(0, &"z".repeat(40)), sha_msg),
            ("39-char oid", reflog_with(0, &"a".repeat(39)), sha_msg),
            ("41-char oid", reflog_with(0, &"a".repeat(41)), sha_msg),
            ("empty oid", reflog_with(0, ""), sha_msg),
            ("empty selector", reflog_with(1, ""), "selector is empty"),
            ("empty timestamp", reflog_with(2, ""), ts_msg),
            ("signed timestamp", reflog_with(2, "+5"), ts_msg),
            ("negative timestamp", reflog_with(2, "-5"), ts_msg),
            ("letters in timestamp", reflog_with(2, "12ab"), ts_msg),
            (
                "overflow timestamp",
                reflog_with(2, "99999999999999999999"),
                "invalid timestamp",
            ),
            (
                "no final NUL",
                good.trim_end_matches('\0').to_string(),
                "missing final NUL",
            ),
            ("extra trailing NUL", format!("{good}\0"), "of 4 fields"),
            ("doubled NUL", good.replacen('\0', "\0\0", 1), "of 4 fields"),
            ("missing field", reflog_record(&missing), "of 4 fields"),
            ("extra field", reflog_record(&five), "of 4 fields"),
        ];
        for (label, input, expect) in &table {
            let error = parse_reflog(input).expect_err(label);
            assert!(error.to_string().contains(expect), "{label}: {error}");
        }
    }

    #[test]
    fn reflog_duplicate_records_are_legal() {
        let one = reflog_record(&reflog_fields(0));
        let parsed = parse_reflog(&format!("{one}{one}")).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], parsed[1]);
    }

    #[test]
    fn reflog_sha256_oid_is_accepted() {
        let parsed = parse_reflog(&reflog_with(0, &"b".repeat(64))).unwrap();
        assert_eq!(parsed[0].sha.len(), 64);
    }

    #[test]
    fn reflog_minimal_thousand_and_five_thousand() {
        assert_eq!(parse_reflog(&reflog_records(1).concat()).unwrap().len(), 1);
        assert_eq!(
            parse_reflog(&reflog_records(1000).concat()).unwrap().len(),
            1000
        );
        let parsed = parse_reflog(&reflog_records(5000).concat()).unwrap();
        assert_eq!(parsed.len(), 5000);
        assert_eq!(parsed[4999].selector, "HEAD@{4999}");
    }

    // --- parser byte caps (THE-383) ---

    #[test]
    fn reflog_caps_exact_and_over() {
        for count in [1usize, 3] {
            let input = reflog_records(count).concat();
            let total = input.len();
            let longest = input.split('\0').map(str::len).max().unwrap();
            assert_eq!(
                parse_reflog_with_limits(&input, total, longest)
                    .unwrap()
                    .len(),
                count
            );
            let over = parse_reflog_with_limits(&input, total - 1, longest)
                .unwrap_err()
                .to_string();
            assert!(over.contains("whole-output cap"), "{over}");
            let field = parse_reflog_with_limits(&input, total, longest - 1)
                .unwrap_err()
                .to_string();
            assert!(field.contains("per-field cap"), "{field}");
        }
    }

    #[test]
    fn reflog_caps_field_in_middle_fails_whole_parse() {
        let records = reflog_records(3);
        let input = format!(
            "{}{}{}",
            records[0],
            reflog_with(3, &"x".repeat(200)),
            records[2]
        );
        assert_eq!(
            parse_reflog_with_limits(&input, usize::MAX, usize::MAX)
                .unwrap()
                .len(),
            3
        );
        let error = parse_reflog_with_limits(&input, usize::MAX, 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("per-field cap"), "{error}");
    }

    #[test]
    fn reflog_caps_thousand_records_stay_ok_under_real_caps() {
        let input = reflog_records(1000).concat();
        assert_eq!(parse_reflog(&input).unwrap().len(), 1000);
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
    fn lossy_invalid_utf8_in_reflog_message_keeps_alignment() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let out = lossy_z(&[
            a.as_bytes(),
            b"HEAD@{0}",
            b"1700000000",
            b"commit: \xff\xfe subject",
            b.as_bytes(),
            b"HEAD@{1}",
            b"1600000000",
            b"reset: moving to main",
        ]);
        let entries = parse_reflog(&out).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sha, a);
        assert_eq!(entries[0].selector, "HEAD@{0}");
        assert_eq!(entries[0].date, 1_700_000_000);
        assert_eq!(entries[0].action, ReflogAction::Commit);
        assert!(entries[0].raw.contains('\u{FFFD}'));
        assert_eq!(entries[1].sha, b);
        assert_eq!(entries[1].selector, "HEAD@{1}");
        assert_eq!(entries[1].date, 1_600_000_000);
        assert_eq!(entries[1].action, ReflogAction::Reset { to: "main".into() });
    }

    #[test]
    fn lossy_invalid_utf8_in_a_reflog_fixed_field_is_an_error() {
        let a = "a".repeat(40);
        let mut bad_sha = a.clone().into_bytes();
        bad_sha[10] = 0xff;
        let record = |sha: &[u8], date: &[u8]| lossy_z(&[sha, b"HEAD@{0}", date, b"commit: x"]);
        assert!(parse_reflog(&record(&bad_sha, b"1700000000")).is_err());
        assert!(parse_reflog(&record(a.as_bytes(), b"17000\xff0000")).is_err());
    }
}
