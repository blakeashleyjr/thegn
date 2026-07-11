//! Undo/redo planning over the HEAD reflog, lazygit-style: classify each
//! entry as a user action, then walk newest→oldest with a bracket counter so
//! resets *we* issued cancel out instead of being undone themselves.
//!
//! Pure — the host runs `git reflog --format=%H%x1f%gd%x1f%ct%x1f%gs -n 100`
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

/// Parse `git reflog --format=%H%x1f%gd%x1f%ct%x1f%gs -n 100` output (newest
/// first). Tolerant of malformed input: rows missing fields, with an empty
/// sha, or with a non-numeric timestamp are skipped.
pub fn parse_reflog(out: &str) -> Vec<ReflogEntry> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\u{1f}');
            let sha = fields.next()?;
            let selector = fields.next()?;
            let date = fields.next()?.parse::<i64>().ok()?;
            let raw = fields.next()?;
            if sha.is_empty() {
                return None;
            }
            Some(ReflogEntry {
                sha: sha.to_string(),
                selector: selector.to_string(),
                date,
                action: classify(raw),
                raw: raw.to_string(),
            })
        })
        .collect()
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
            let out = format!("abc\u{1f}HEAD@{{0}}\u{1f}1700000000\u{1f}{gs}\n");
            let parsed = parse_reflog(&out);
            assert_eq!(parsed.len(), 1, "one row for {gs:?}");
            assert_eq!(parsed[0].action, want, "classification of {gs:?}");
            assert_eq!(parsed[0].raw, gs, "raw preserved verbatim");
        }
    }

    #[test]
    fn parse_preserves_fields() {
        let out = "deadbeef\u{1f}HEAD@{3}\u{1f}1712345678\u{1f}commit: hello\n";
        let parsed = parse_reflog(out);
        assert_eq!(
            parsed,
            vec![ReflogEntry {
                sha: "deadbeef".into(),
                selector: "HEAD@{3}".into(),
                date: 1_712_345_678,
                action: ReflogAction::Commit,
                raw: "commit: hello".into(),
            }]
        );
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse_reflog("").is_empty());
        assert!(parse_reflog("\n\n").is_empty());
    }

    #[test]
    fn parse_skips_malformed_rows() {
        let out = concat!(
            "onlysha\n",                                           // missing fields
            "sha\u{1f}HEAD@{0}\u{1f}notadate\u{1f}commit: x\n",    // bad timestamp
            "\u{1f}HEAD@{1}\u{1f}1700000000\u{1f}commit: y\n",     // empty sha
            "good\u{1f}HEAD@{2}\u{1f}1700000000\u{1f}commit: z\n"  // survives
        );
        let parsed = parse_reflog(out);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].sha, "good");
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
}
