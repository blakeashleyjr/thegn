//! Shared host-side rows for PR review feedback.
//!
//! The PR modal and the full-screen PR diff deliberately consume the same
//! projection.  In particular, this module is the only place that inserts
//! review rows after a new-side diff line; the local worktree diff never calls
//! it.

use thegn_core::forge::model::{DiffFile, DiffLine, ReviewThread};
use thegn_core::review::AnchoredReview;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewRow {
    Hunk(String),
    Diff(DiffLine),
    Thread(ReviewThread),
    Outdated(ReviewThread),
    General(ReviewThread),
}

/// Build one file's selectable/renderable rows in PR diff order.
pub(crate) fn file_rows(
    file: &DiffFile,
    review: Option<&AnchoredReview>,
    include_resolved: bool,
) -> Vec<ReviewRow> {
    let mut rows = Vec::new();
    let anchored = review.and_then(|r| r.files.iter().find(|f| f.path == file.path));
    for hunk in &file.hunks {
        rows.push(ReviewRow::Hunk(hunk.header.clone()));
        for line in &hunk.lines {
            rows.push(ReviewRow::Diff(line.clone()));
            if let Some(file_review) = anchored {
                for thread in file_review.threads.iter().filter(|t| {
                    Some(t.line) == line.new_lineno && (include_resolved || !t.thread.resolved)
                }) {
                    rows.push(ReviewRow::Thread(thread.thread.clone()));
                }
            }
        }
    }
    rows
}

/// Build the explicit, selectable feedback block that follows a PR diff.
pub(crate) fn feedback_rows(review: &AnchoredReview, include_resolved: bool) -> Vec<ReviewRow> {
    review
        .files
        .iter()
        .flat_map(|file| file.outdated.iter())
        .filter(|thread| include_resolved || !thread.resolved)
        .cloned()
        .map(ReviewRow::Outdated)
        .chain(
            review
                .general
                .iter()
                .filter(|thread| include_resolved || !thread.resolved)
                .cloned()
                .map(ReviewRow::General),
        )
        .collect()
}
