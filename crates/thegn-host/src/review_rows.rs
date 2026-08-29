//! Shared host-side rows for PR review feedback.
//!
//! The PR modal and the full-screen PR diff deliberately consume the same
//! projection.  In particular, this module is the only place that inserts
//! review rows after a new-side diff line; the local worktree diff never calls
//! it.

use thegn_core::forge::model::{DiffFile, DiffLine, PrComment, ReviewThread};
use thegn_core::review::{AnchoredReview, AnchoredThread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewRow {
    Hunk(String),
    Diff(DiffLine),
    Thread(ReviewThread),
    Comment(PrComment),
    Notice(String),
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
                    t.line == line.new_lineno && (include_resolved || !t.thread.resolved)
                }) {
                    rows.push(ReviewRow::Thread(thread.thread.clone()));
                }
            }
        }
    }
    if let Some(file_review) = anchored {
        for thread in file_review
            .outdated
            .iter()
            .filter(|t| include_resolved || !t.resolved)
        {
            rows.push(ReviewRow::Thread(thread.clone()));
        }
    }
    rows
}

/// Return the comments that should be shown in the explicit outdated block.
pub(crate) fn outdated_rows(review: &AnchoredReview, include_resolved: bool) -> Vec<ReviewThread> {
    review
        .files
        .iter()
        .flat_map(|file| file.outdated.iter())
        .filter(|thread| include_resolved || !thread.resolved)
        .cloned()
        .chain(
            review
                .general
                .iter()
                .filter(|thread| include_resolved || !thread.resolved)
                .cloned(),
        )
        .collect()
}

/// Find the anchored row for a thread without making a nearest-line guess.
pub(crate) fn anchored_thread<'a>(
    review: &'a AnchoredReview,
    thread: &ReviewThread,
) -> Option<&'a AnchoredThread> {
    review
        .files
        .iter()
        .flat_map(|file| file.threads.iter())
        .find(|candidate| candidate.thread.id == thread.id)
}
