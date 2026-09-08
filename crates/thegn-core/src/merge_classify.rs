//! Classify merge conflicts into the two kinds that need different advice —
//! pure text policy, no I/O (the crate-boundary gate and the 95% coverage gate
//! both apply).
//!
//! # Why this exists
//!
//! A reconcile chunk written as generic prose is worse than none. The one handed
//! to the THE-32 reconcile said "default to keeping both sides… expect volume
//! rather than difficulty". That was correct for 9 of its 34 hunks and **wrong
//! for 25**: `main` had restructured the diff-row model and the worktree
//! lifecycle hooks, where keeping both sides does not compile and, in one case,
//! would have silently dropped main's diff colouring. Replacing the prose with
//! per-conflict decisions produced a correct merge on the first attempt.
//!
//! Writing those decisions by hand cost ~15 minutes of reading conflicts. This
//! module does the mechanical half — which hunks are safe and which need a
//! human's decision — so the Lead spends that time only where it counts.
//!
//! # The rule, and why it holds
//!
//! Reading diff3-style markers (`merge.conflictStyle = diff3`/`zdiff3`, which
//! this repo uses — the base section is what makes the call possible):
//!
//! ```text
//! <<<<<<< HEAD
//! ours
//! ||||||| base
//! base            <-- THIS section decides
//! =======
//! theirs
//! >>>>>>> main
//! ```
//!
//! - **Base empty** ⇒ [`HunkClass::ConcurrentAdd`]. Both sides inserted at
//!   the same point. That does not prove they compose: duplicate declarations
//!   and duplicate configuration keys are common counterexamples.
//! - **Base non-empty** ⇒ [`HunkClass::Restructure`]. There was code here and
//!   both sides changed it, so one of them rewrote what the other also touched.
//!   "Keep both" is meaningless; somebody must decide what the merged behaviour
//!   is.
//!
//! Checked against the real THE-32 merge: the 9 `config*.rs` hunks classify as
//! concurrent additions, and the `pr_view.rs` / `diff_view.rs` /
//! lifecycle-hook hunks classify as restructures. Both remain review items.

use std::{collections::BTreeMap, fmt};

/// What kind of decision a conflict hunk needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkClass {
    /// Both sides inserted content where the base had none. Needs review.
    ConcurrentAdd,
    /// Both sides changed content that existed. Needs a named decision.
    Restructure,
}

impl HunkClass {
    pub fn as_str(self) -> &'static str {
        match self {
            HunkClass::ConcurrentAdd => "concurrent-add",
            HunkClass::Restructure => "restructure",
        }
    }
}

impl fmt::Display for HunkClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One conflict hunk, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based line of the `<<<<<<<` marker in the file.
    pub line: usize,
    pub class: HunkClass,
    /// The first non-blank line of "our" side, trimmed and clipped — enough for
    /// a human to recognise the hunk without opening the file.
    pub ours_hint: String,
    /// The same for "their" side.
    pub theirs_hint: String,
}

/// A file's conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileConflicts {
    pub path: String,
    pub hunks: Vec<Hunk>,
    /// Why this conflict could not be classified from textual markers. Such a
    /// file still needs an explicit human decision; it must never look clean.
    pub inspection_error: Option<String>,
}

impl FileConflicts {
    pub fn concurrent_add(&self) -> usize {
        self.hunks
            .iter()
            .filter(|h| h.class == HunkClass::ConcurrentAdd)
            .count()
    }
    pub fn restructure(&self) -> usize {
        self.hunks
            .iter()
            .filter(|h| h.class == HunkClass::Restructure)
            .count()
    }
}

const HINT_MAX: usize = 72;

fn hint(section: &[&str]) -> String {
    let line = section
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= HINT_MAX {
        line.to_string()
    } else {
        let clipped: String = line.chars().take(HINT_MAX).collect();
        format!("{clipped}…")
    }
}

/// Parse and classify every conflict hunk in one file's contents.
///
/// Returns an empty vec for a file with no markers. A malformed/truncated hunk
/// (a `<<<<<<<` with no `>>>>>>>`) is skipped rather than guessed at: this
/// output drives advice, and inventing a class for a hunk we could not read
/// would be worse than staying quiet about it.
///
/// Without a base section — i.e. `merge.conflictStyle` left at the default
/// `merge` — every hunk reads as [`HunkClass::Restructure`], the conservative
/// answer: "someone has to look at this" is never actively wrong, whereas a
/// false safe classification tells a worker to keep both sides of a rewrite.
pub fn classify_file(contents: &str) -> Vec<Hunk> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if !lines[i].starts_with("<<<<<<<") {
            i += 1;
            continue;
        }
        let start = i;
        let mut base_start: Option<usize> = None;
        let mut sep: Option<usize> = None;
        let mut end: Option<usize> = None;
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            if l.starts_with("|||||||") && base_start.is_none() && sep.is_none() {
                base_start = Some(j);
            } else if l.starts_with("=======") && sep.is_none() {
                sep = Some(j);
            } else if l.starts_with(">>>>>>>") {
                end = Some(j);
                break;
            } else if l.starts_with("<<<<<<<") {
                // A new hunk opened before this one closed: the first is
                // malformed. Re-scan from here rather than pairing markers
                // across hunks.
                break;
            }
            j += 1;
        }
        let (Some(sep_i), Some(end_i)) = (sep, end) else {
            i = start + 1;
            continue;
        };
        let ours_end = base_start.unwrap_or(sep_i);
        let ours = &lines[start + 1..ours_end];
        let theirs = &lines[sep_i + 1..end_i];
        let class = match base_start {
            // diff3: the base section decides.
            Some(b) => {
                let base = &lines[b + 1..sep_i];
                if base.iter().all(|l| l.trim().is_empty()) {
                    HunkClass::ConcurrentAdd
                } else {
                    HunkClass::Restructure
                }
            }
            // No base recorded — cannot tell, so assume the answer that makes a
            // human look.
            None => HunkClass::Restructure,
        };
        out.push(Hunk {
            line: start + 1,
            class,
            ours_hint: hint(ours),
            theirs_hint: hint(theirs),
        });
        i = end_i + 1;
    }
    out
}

/// Roll a set of classified files into the chunk-spec skeleton a Lead
/// annotates. Deliberately a STARTING POINT, not a finished chunk: the
/// mechanical split is computable, the restructure decisions are not, and a
/// generated file that pretended otherwise would reproduce exactly the generic
/// advice this exists to replace.
pub fn render_chunk_skeleton(issue: &str, files: &[FileConflicts]) -> String {
    render_chunk(issue, files, &BTreeMap::new())
}

/// Render a reconciliation chunk, filling review decisions supplied as
/// `path:line=decision` for textual hunks and `path=decision` for unclassified
/// files. The readiness footer becomes dispatchable only when every item has a
/// non-empty decision.
pub fn render_chunk(
    issue: &str,
    files: &[FileConflicts],
    decisions: &BTreeMap<String, String>,
) -> String {
    let total: usize = files.iter().map(|f| f.hunks.len()).sum();
    let concurrent_add: usize = files.iter().map(FileConflicts::concurrent_add).sum();
    let restructure: usize = files.iter().map(FileConflicts::restructure).sum();
    let unclassified = files
        .iter()
        .filter(|f| f.inspection_error.is_some())
        .count();
    let mut s = String::new();
    s.push_str(&format!(
        "# {:?} reconcile — merge current main into the lane\n\n",
        issue
    ));
    s.push_str("## Files to touch (exact paths)\n\n");
    for f in files {
        s.push_str(&format!("- {:?}\n", f.path));
    }
    s.push_str(&format!(
        "\n## State you are landing in\n\n\
         `git merge main` is ALREADY IN PROGRESS and left {} file(s) conflicted — \
         {total} hunk(s) total. Do not abort or restart it. Resolve, then `git commit` \
         the merge.\n\n\
         {concurrent_add} concurrent insertion(s) and {restructure} rewrite(s) need a decision, \
         and {unclassified} file(s) could not be classified automatically. \
         **Do not apply a blanket \"keep both sides\" rule** — see below.\n",
        files.len()
    ));

    if concurrent_add > 0 {
        s.push_str(
            "\n## Concurrent additions — NEEDS A DECISION\n\n\
             The base had nothing here, but that proves only that both sides inserted. \
             Keeping both may duplicate a declaration or configuration key. Decide each \
             hunk explicitly.\n\n",
        );
        for f in files.iter().filter(|f| f.concurrent_add() > 0) {
            s.push_str(&format!("### {:?}\n\n", f.path));
            for h in f
                .hunks
                .iter()
                .filter(|h| h.class == HunkClass::ConcurrentAdd)
            {
                let key = format!("{}:{}", f.path, h.line);
                let decision = decisions.get(&key).filter(|text| !text.trim().is_empty());
                s.push_str(&format!(
                    "- **line {}** — ours: {:?} / theirs: {:?}\n  - DECISION: {}\n",
                    h.line,
                    h.ours_hint,
                    h.theirs_hint,
                    decision.map_or_else(|| "_(state it)_".to_string(), |text| format!("{text:?}"))
                ));
            }
            s.push('\n');
        }
    }

    if unclassified > 0 {
        s.push_str(
            "\n## Unclassified conflicts — INSPECT BEFORE DISPATCHING\n\n\
             These Git conflicts could not be represented as textual marker hunks. \
             Decide them with `git status` and `git ls-files -u`; do not infer that \
             zero parsed hunks means the file is resolved.\n\n",
        );
        for f in files.iter().filter(|f| f.inspection_error.is_some()) {
            let decision = decisions
                .get(&f.path)
                .filter(|text| !text.trim().is_empty());
            s.push_str(&format!(
                "- {:?} — {:?}\n  - DECISION: {}\n",
                f.path,
                f.inspection_error.as_deref().unwrap_or("inspection failed"),
                decision.map_or_else(|| "_(state it)_".to_string(), |text| format!("{text:?}"))
            ));
        }
    }

    if restructure > 0 {
        s.push_str(
            "\n## Restructure — NEEDS A DECISION (fill these in before dispatching)\n\n\
             Code existed here and both sides changed it: one side rewrote what the other \
             also touched. \"Keep both\" does not compile, and taking one side wholesale \
             can silently drop the other's behaviour. For each of these, state which \
             structure wins and which behaviour is ported onto it.\n\n",
        );
        for f in files.iter().filter(|f| f.restructure() > 0) {
            s.push_str(&format!("### {:?}\n\n", f.path));
            for h in f.hunks.iter().filter(|h| h.class == HunkClass::Restructure) {
                let key = format!("{}:{}", f.path, h.line);
                let decision = decisions.get(&key).filter(|text| !text.trim().is_empty());
                s.push_str(&format!(
                    "- **line {}** — ours: {:?} / theirs: {:?}\n  - DECISION: {}\n",
                    h.line,
                    h.ours_hint,
                    h.theirs_hint,
                    decision.map_or_else(|| "_(state it)_".to_string(), |text| format!("{text:?}"))
                ));
            }
            s.push('\n');
        }
    }
    let complete = files.iter().all(|file| {
        let unclassified_done = file.inspection_error.is_none()
            || decisions
                .get(&file.path)
                .is_some_and(|d| !d.trim().is_empty());
        unclassified_done
            && file.hunks.iter().all(|hunk| {
                decisions
                    .get(&format!("{}:{}", file.path, hunk.line))
                    .is_some_and(|d| !d.trim().is_empty())
            })
    });
    if complete {
        s.push_str(
            "\n## Dispatch readiness\n\n**READY — every conflict has an explicit decision.**\n",
        );
    } else {
        s.push_str(
            "\n## Dispatch readiness\n\n\
             **DRAFT ONLY — do not dispatch this chunk yet.** Supply every `DECISION` \
             with `--decision 'path:line=text'` (or `path=text` for an unclassified file).\n",
        );
    }
    s
}

#[cfg(test)]
#[path = "merge_classify_tests.rs"]
mod merge_classify_tests;
