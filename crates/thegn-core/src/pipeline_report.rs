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
}
