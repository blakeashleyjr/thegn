//! Structured report and progress-note validation + status digest — pure policy,
//! no I/O, no substrates (the crate-boundary gate and the 95% coverage gate both
//! apply — test every function).

use crate::issue::{AgentDispatch, DispatchNote};
use std::collections::HashMap;
use std::fmt;

// --- text validation --------------------------------------------------------

/// The hard cap on a worker's structured report, in chars. Anything bigger
/// belongs in the artifact file (git is the source of truth).
pub const REPORT_MAX_CHARS: usize = 16_384;

/// The hard cap on a single progress note.
pub const NOTE_MAX_CHARS: usize = 4_096;

/// A report that cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    Empty,
    TooLong { len: usize },
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::Empty => f.write_str("report must not be empty"),
            ReportError::TooLong { len } => write!(
                f,
                "report is {len} chars, max {REPORT_MAX_CHARS} — put the full artifact \
                 in git and keep the report under the cap"
            ),
        }
    }
}

/// A progress note that cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteError {
    Empty,
    TooLong { len: usize },
}

impl fmt::Display for NoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoteError::Empty => f.write_str("note must not be empty"),
            NoteError::TooLong { len } => write!(f, "note is {len} chars, max {NOTE_MAX_CHARS}"),
        }
    }
}

/// Trim and validate a report. Returns the trimmed text, or an error on
/// empty / over the cap.
pub fn report_text(text: &str) -> Result<String, ReportError> {
    // Reports are printed by the CLI and may be copied into a supervisor
    // prompt. Preserve LF because the handoff format is intentionally
    // multiline, but remove every other control character (including ESC,
    // BEL, CR, and C1 controls) before storing it.
    let t: String = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    let t = t.trim();
    if t.is_empty() {
        return Err(ReportError::Empty);
    }
    let len = t.chars().count();
    if len > REPORT_MAX_CHARS {
        return Err(ReportError::TooLong { len });
    }
    Ok(t.to_string())
}

/// Trim and validate a progress note. Returns the trimmed text, or an error
/// on empty / over the cap.
pub fn note_text(text: &str) -> Result<String, NoteError> {
    // Notes are rendered as one roster line / bullet. Remove all control
    // characters rather than allowing a worker note to emit terminal escape
    // sequences or forge additional output lines.
    let t: String = text.chars().filter(|c| !c.is_control()).collect();
    let t = t.trim();
    if t.is_empty() {
        return Err(NoteError::Empty);
    }
    let len = t.chars().count();
    if len > NOTE_MAX_CHARS {
        return Err(NoteError::TooLong { len });
    }
    Ok(t.to_string())
}

// --- verdict ----------------------------------------------------------------

/// What a stage worker's report *concluded*, as distinct from what happened to
/// its row.
///
/// `status` and verdict are independent axes that the roster collapsed into
/// one field, and both became unreadable as a result. Measured over a 469-row
/// run: three rows sat at `failed` whose reports read "PASS; ready for the
/// merge queue", and sixteen sat at `done` carrying a REVISE. So "the worker
/// broke", "the reviewer asked for changes" and "the bookkeeping lost the row"
/// were indistinguishable without opening the artifact — which is the read the
/// report exists to save.
///
/// Deliberately **derived, never stored.** A column would need a schema bump,
/// and the enum-ladder collision that costs is the single most repeated merge
/// conflict in this repo's queue; worse, a stored copy can disagree with the
/// report it came from. The report is the source of truth and this is a pure
/// read of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Ready to advance — the stage says its work stands.
    Pass,
    /// The stage worked and is asking for changes. NOT a failure: a REVISE is
    /// a review doing its job.
    Revise,
    /// The stage ran and concluded the work is not sound.
    Fail,
    /// The stage could not run at all (environment, not code).
    Blocked,
    /// No report, or a report that states no verdict.
    None,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Revise => "revise",
            Verdict::Fail => "fail",
            Verdict::Blocked => "blocked",
            Verdict::None => "none",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Read the verdict out of a report's opening words.
///
/// Workers are told to lead with the verdict and overwhelmingly do, but the
/// shapes vary in practice — `PASS`, `PASS (ready for the merge queue)`,
/// `verdict: done`, `done; commits: …`, `REVISE; …`. Only the FIRST line is
/// considered, and only its leading token: a report body that merely discusses
/// a failure ("no remaining blocking defect was found") must never read as one.
pub fn verdict_of(report: Option<&str>) -> Verdict {
    let Some(line) = report
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .and_then(|r| r.lines().find(|l| !l.trim().is_empty()))
    else {
        return Verdict::None;
    };
    // Strip a `verdict:` lead-in, then take the first word, unpunctuated.
    let line = line.trim();
    let rest = line
        .strip_prefix("verdict:")
        .or_else(|| line.strip_prefix("Verdict:"))
        .or_else(|| line.strip_prefix("VERDICT:"))
        .unwrap_or(line)
        .trim_start();
    let token: String = rest
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    match token.as_str() {
        "pass" | "approved" | "done" | "complete" => Verdict::Pass,
        "revise" => Verdict::Revise,
        "fail" | "failed" => Verdict::Fail,
        "blocked" => Verdict::Blocked,
        _ => Verdict::None,
    }
}

/// Does this row's recorded outcome contradict what its own report says?
///
/// The reconciliation a supervisor otherwise performs by eye. `true` means the
/// pair is worth a human's attention: a row parked at `failed` whose report
/// says PASS is either a bookkeeping loss (the work is fine and the row lies)
/// or a worker overclaiming — both matter, and neither is visible from `status`
/// alone.
pub fn verdict_disagrees_with_status(
    status: crate::issue::AgentDispatchStatus,
    v: Verdict,
) -> bool {
    use crate::issue::AgentDispatchStatus as S;
    matches!(
        (status, v),
        // A closed-as-broken row whose worker said the work stands.
        (S::Failed | S::Abandoned, Verdict::Pass)
            // ...and the inverse: a row closed as good over its own objection.
            | (S::Done, Verdict::Fail | Verdict::Blocked)
    )
}

// --- status digest ----------------------------------------------------------

/// One row's status snapshot — report, note count, and the most recent note
/// (if any within the since window). Designed so the CLI in chunk-2 prints or
/// JSONs it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatusDigest {
    pub id: i64,
    pub status: String,
    pub stage: Option<String>,
    pub issue_id: String,
    pub report: Option<String>,
    pub note_count: usize,
    pub latest_note: Option<(i64, String)>,
}

/// Build one [`StatusDigest`] per row (roster order), carrying note counts
/// and latest notes computed over the `since_ms`-filtered notes map.
///
/// A note for a pruned row (one not in `rows`) is silently ignored — it
/// must not panic.
pub fn digest(
    rows: &[AgentDispatch],
    notes: &HashMap<i64, Vec<DispatchNote>>,
    since_ms: Option<i64>,
) -> Vec<StatusDigest> {
    rows.iter()
        .map(|r| {
            let row_notes = notes.get(&r.id);
            // Filter by since if requested, then sort by created_at
            let filtered: Vec<&DispatchNote> = if let Some(since) = since_ms {
                row_notes
                    .iter()
                    .flat_map(|ns| ns.iter().filter(move |n| n.created_at_ms > since))
                    .collect()
            } else {
                row_notes.iter().flat_map(|ns| ns.iter()).collect()
            };
            let note_count = filtered.len();
            let latest_note = filtered
                .into_iter()
                .max_by_key(|n| (n.created_at_ms, n.id))
                .map(|n| (n.id, n.text.clone()));
            StatusDigest {
                id: r.id,
                status: r.status.as_str().to_string(),
                stage: r.stage.clone(),
                issue_id: r.issue_id.clone(),
                report: r.report.clone(),
                note_count,
                latest_note,
            }
        })
        .collect()
}

// --- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: i64,
        issue: &str,
        stage: Option<&str>,
        status: &str,
        report: Option<&str>,
    ) -> AgentDispatch {
        use crate::issue::AgentDispatchStatus;
        AgentDispatch {
            id,
            issue_id: issue.to_string(),
            worktree_path: format!("/wt/{id}"),
            agent_name: "claude".to_string(),
            dispatched_at_ms: id,
            status: AgentDispatchStatus::parse(status),
            stage: stage.map(str::to_string),
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: report.map(str::to_string),
            exit_code: None,
            exited_at_ms: None,
        }
    }

    fn note(id: i64, dispatch_id: i64, created_at_ms: i64, text: &str) -> DispatchNote {
        DispatchNote {
            id,
            dispatch_id,
            created_at_ms,
            text: text.to_string(),
        }
    }

    // --- text validation ----------------------------------------------------

    #[test]
    fn report_text_trims_and_rejects_empty() {
        assert_eq!(report_text("  hi  ").unwrap(), "hi");
        assert_eq!(report_text("  "), Err(ReportError::Empty));
        assert_eq!(report_text(""), Err(ReportError::Empty));
        assert_eq!(
            report_text("\x1b[31mhi\x07\nthere\r"),
            Ok("[31mhi\nthere".into())
        );
    }

    #[test]
    fn report_text_rejects_over_cap() {
        let big = "x".repeat(16_385);
        assert_eq!(report_text(&big), Err(ReportError::TooLong { len: 16_385 }));
        // Exact cap is OK
        let exact = "x".repeat(16_384);
        assert_eq!(report_text(&exact).unwrap(), exact);
    }

    #[test]
    fn note_text_trims_and_rejects_empty() {
        assert_eq!(note_text("  hi  ").unwrap(), "hi");
        assert_eq!(note_text("  "), Err(NoteError::Empty));
        assert_eq!(note_text(""), Err(NoteError::Empty));
        assert_eq!(
            note_text("first\nsecond\x1b[2J"),
            Ok("firstsecond[2J".into())
        );
    }

    #[test]
    fn note_text_rejects_over_cap() {
        let big = "x".repeat(4_097);
        assert_eq!(note_text(&big), Err(NoteError::TooLong { len: 4_097 }));
        // Exact cap is OK
        let exact = "x".repeat(4_096);
        assert_eq!(note_text(&exact).unwrap(), exact);
    }

    // --- digest -------------------------------------------------------------

    #[test]
    fn digest_empty_inputs() {
        let got = digest(&[], &HashMap::new(), None);
        assert!(got.is_empty());
    }

    #[test]
    fn digest_roster_order_with_no_notes() {
        let rows = vec![
            row(1, "linear:A", Some("code"), "running", None),
            row(2, "linear:B", Some("review"), "queued", None),
        ];
        let notes: HashMap<i64, Vec<DispatchNote>> = HashMap::new();
        let got = digest(&rows, &notes, None);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, 1);
        assert_eq!(got[0].note_count, 0);
        assert!(got[0].latest_note.is_none());
        assert_eq!(got[1].id, 2);
        assert_eq!(got[1].report, None);
    }

    #[test]
    fn digest_counts_and_picks_latest_note() {
        let rows = vec![row(1, "linear:A", None, "done", Some("all good"))];
        let mut notes: HashMap<i64, Vec<DispatchNote>> = HashMap::new();
        notes.insert(
            1,
            vec![
                note(10, 1, 100, "started"),
                note(11, 1, 200, "linting"),
                note(12, 1, 300, "done"),
            ],
        );
        let got = digest(&rows, &notes, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].note_count, 3);
        assert_eq!(got[0].latest_note, Some((12, "done".to_string())));
        assert_eq!(got[0].report.as_deref(), Some("all good"));
    }

    #[test]
    fn digest_filters_by_since() {
        let rows = vec![row(1, "linear:A", None, "running", None)];
        let mut notes: HashMap<i64, Vec<DispatchNote>> = HashMap::new();
        notes.insert(1, vec![note(10, 1, 100, "old"), note(11, 1, 200, "new")]);
        let got = digest(&rows, &notes, Some(150));
        assert_eq!(got[0].note_count, 1);
        assert_eq!(got[0].latest_note, Some((11, "new".to_string())));
    }

    #[test]
    fn digest_ignores_unknown_row_ids_in_notes_map() {
        let rows = vec![row(1, "linear:A", None, "done", None)];
        let mut notes: HashMap<i64, Vec<DispatchNote>> = HashMap::new();
        notes.insert(2, vec![note(20, 2, 100, "orphan")]);
        notes.insert(3, vec![note(30, 3, 200, "another")]);
        // These must not panic — a note for a pruned row is silently ignored.
        let got = digest(&rows, &notes, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].note_count, 0);
    }

    #[test]
    fn digest_passes_report_through() {
        let rows = vec![
            row(
                1,
                "linear:A",
                Some("code"),
                "done",
                Some("verdict: done\nnext: review"),
            ),
            row(2, "linear:B", Some("review"), "queued", None),
        ];
        let got = digest(&rows, &HashMap::new(), None);
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0].report.as_deref(),
            Some("verdict: done\nnext: review")
        );
        assert_eq!(got[1].report, None);
    }

    // --- verdict ------------------------------------------------------------

    #[test]
    fn verdict_reads_the_shapes_workers_actually_wrote() {
        // Every string here is a real report opening from the 469-row roster.
        for (report, want) in [
            ("PASS", Verdict::Pass),
            ("PASS (ready for the merge queue)", Verdict::Pass),
            (
                "PASS; ready for the merge queue. Corrected observer authority",
                Verdict::Pass,
            ),
            (
                "verdict: done; commits: 57bd6e34 feat(the-32)",
                Verdict::Pass,
            ),
            (
                "done; commits: b8f133b0 (merge main into THE-59 lane)",
                Verdict::Pass,
            ),
            (
                "REVISE; reviewed THE-48 from base caef2f0e",
                Verdict::Revise,
            ),
            ("APPROVED with notes", Verdict::Pass),
            ("BLOCKED — the gate could not run", Verdict::Blocked),
            ("FAIL: two ratchet violations", Verdict::Fail),
        ] {
            assert_eq!(verdict_of(Some(report)), want, "report: {report:?}");
        }
    }

    #[test]
    fn verdict_only_reads_the_opening_token() {
        // The dangerous direction: prose that MENTIONS a failure must not be
        // read as one. This body is from a real PASS verdict.
        assert_eq!(
            verdict_of(Some(
                "PASS\n\nNo remaining blocking injection, path, permission, swallowed-error \
                 or concurrency defect was found. Earlier rounds did FAIL."
            )),
            Verdict::Pass
        );
        // ... and a verdict is never inferred from a later line.
        assert_eq!(
            verdict_of(Some("Reviewed the diff.\nPASS")),
            Verdict::None,
            "only the first non-empty line carries the verdict"
        );
    }

    #[test]
    fn verdict_is_none_without_a_report() {
        assert_eq!(verdict_of(None), Verdict::None);
        assert_eq!(verdict_of(Some("")), Verdict::None);
        assert_eq!(verdict_of(Some("   \n  ")), Verdict::None);
        assert_eq!(verdict_of(Some("looked at it, seems fine")), Verdict::None);
    }

    #[test]
    fn disagreement_flags_the_pairs_that_actually_occurred() {
        use crate::issue::AgentDispatchStatus as S;
        // Roster rows 324/443/298: `failed` carrying a PASS/done report.
        assert!(verdict_disagrees_with_status(S::Failed, Verdict::Pass));
        assert!(verdict_disagrees_with_status(S::Abandoned, Verdict::Pass));
        // A `done` row carrying FAIL/BLOCKED is the inverse overclaim.
        assert!(verdict_disagrees_with_status(S::Done, Verdict::Fail));
        assert!(verdict_disagrees_with_status(S::Done, Verdict::Blocked));
        // A REVISE on a done row is NOT a disagreement: the stage worked and
        // asked for changes, which is a review doing its job. Sixteen rows in
        // the corpus are exactly this and none of them are defects.
        assert!(!verdict_disagrees_with_status(S::Done, Verdict::Revise));
        assert!(!verdict_disagrees_with_status(S::Done, Verdict::Pass));
        assert!(!verdict_disagrees_with_status(S::Running, Verdict::None));
    }
}
