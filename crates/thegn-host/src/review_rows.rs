//! Shared host-side rows for PR review feedback.
//!
//! The PR modal and the full-screen PR diff deliberately consume the same
//! projection.  In particular, this module is the only place that inserts
//! review rows after a new-side diff line; the local worktree diff never calls
//! it.

use crate::chrome::S;
use crate::seg::{Line, Tok, seg, sp};
use thegn_core::forge::model::{DiffFile, DiffLine, DiffLineKind, PrConversation, ReviewThread};
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

/// Build one expanded PR file's rows. Outdated feedback is scoped to the
/// open file and general feedback is included once after that file's diff;
/// feedback belonging to other files stays in the file-list block.
pub(crate) fn expanded_file_rows(
    file: &DiffFile,
    review: Option<&AnchoredReview>,
    include_resolved: bool,
) -> Vec<ReviewRow> {
    let mut rows = file_rows(file, review, include_resolved);
    if let Some(review) = review {
        if let Some(file_review) = review.files.iter().find(|f| f.path == file.path) {
            rows.extend(
                file_review
                    .outdated
                    .iter()
                    .filter(|thread| include_resolved || !thread.resolved)
                    .cloned()
                    .map(ReviewRow::Outdated),
            );
        }
        rows.extend(
            review
                .general
                .iter()
                .filter(|thread| include_resolved || !thread.resolved)
                .cloned()
                .map(ReviewRow::General),
        );
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

pub(crate) fn row_thread(row: &ReviewRow) -> Option<&ReviewThread> {
    match row {
        ReviewRow::Thread(thread) | ReviewRow::Outdated(thread) | ReviewRow::General(thread) => {
            Some(thread)
        }
        ReviewRow::Hunk(_) | ReviewRow::Diff(_) => None,
    }
}

pub(crate) fn render_review_row(row: &ReviewRow, selected: bool, cols: usize) -> Vec<(Line, bool)> {
    match row {
        ReviewRow::Hunk(header) => vec![(
            Line::segs(vec![seg(
                Tok::Hue(thegn_core::theme::Hue::Teal),
                trunc(header, cols),
            )]),
            false,
        )],
        ReviewRow::Diff(line) => vec![(diff_line(line, selected, cols), selected)],
        ReviewRow::Thread(thread) => review_thread_lines(thread, selected, cols),
        ReviewRow::Outdated(thread) => review_feedback_lines(thread, "OUTDATED", selected, cols),
        ReviewRow::General(thread) => review_feedback_lines(thread, "GENERAL", selected, cols),
    }
}

pub(crate) fn sel_marker(selected: bool) -> String {
    if selected {
        format!("{} ", crate::caps::active_glyphs().chevron)
    } else {
        "  ".into()
    }
}

pub(crate) fn file_stat(f: &DiffFile) -> (usize, usize) {
    let mut adds = 0;
    let mut dels = 0;
    for h in &f.hunks {
        for l in &h.lines {
            match l.kind {
                DiffLineKind::Add => adds += 1,
                DiffLineKind::Del => dels += 1,
                DiffLineKind::Context => {}
            }
        }
    }
    (adds, dels)
}

pub(crate) fn diff_line(dl: &DiffLine, selected: bool, cols: usize) -> Line {
    let (marker, tone) = match dl.kind {
        DiffLineKind::Add => ("+", Tok::Hue(thegn_core::theme::Hue::Green)),
        DiffLineKind::Del => ("-", Tok::Hue(thegn_core::theme::Hue::Red)),
        DiffLineKind::Context => (" ", Tok::Slot(S::Dim)),
    };
    let no = dl
        .new_lineno
        .map(|n| format!("{n:>5} "))
        .unwrap_or_else(|| "      ".into());
    let body = trunc(&dl.text, cols.saturating_sub(9));
    Line::segs(vec![
        seg(Tok::Slot(S::Faint), sel_marker(selected)),
        seg(Tok::Slot(S::Ghost3), no),
        seg(tone, format!("{marker}{body}")),
    ])
}

pub(crate) fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let ellipsis = crate::caps::active_glyphs().ellipsis;
        let ellipsis_len = ellipsis.chars().count();
        if max <= ellipsis_len {
            ellipsis.chars().take(max).collect()
        } else {
            s.chars()
                .take(max - ellipsis_len)
                .chain(ellipsis.chars())
                .collect()
        }
    }
}

pub(crate) fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        }
        while line.chars().count() > width {
            let head: String = line.chars().take(width).collect();
            out.push(head);
            line = line.chars().skip(width).collect();
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub(crate) fn push_wrapped_body_lines(out: &mut Vec<(Line, bool)>, body: &str, cols: usize) {
    let width = cols.saturating_sub(4).max(8);
    for raw in body.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        for chunk in wrap(raw, width) {
            out.push((
                Line::segs(vec![sp(4), seg(Tok::Slot(S::Dim), chunk)]),
                false,
            ));
        }
    }
}

/// Render one inline review thread. Only the header is selectable; every
/// comment remains visible below it using the Conversation tab's wrapping.
pub(crate) fn review_thread_lines(
    thread: &ReviewThread,
    selected: bool,
    cols: usize,
) -> Vec<(Line, bool)> {
    let mark = if thread.resolved {
        crate::caps::active_glyphs().check
    } else {
        crate::caps::active_glyphs().warn
    };
    let location = format!(
        "{}:{}{}",
        thread.path,
        thread.line.map(|line| line.to_string()).unwrap_or_default(),
        if thread.resolved { " (resolved)" } else { "" }
    );
    let author = thread
        .comments
        .first()
        .map(|comment| comment.author.as_str())
        .unwrap_or("reviewer");
    let mut out = vec![(
        Line::segs(vec![
            seg(Tok::Slot(S::Faint), sel_marker(selected)),
            seg(Tok::Slot(S::Accent), mark),
            seg(
                Tok::Slot(S::Text),
                format!(
                    "{author} {} {location}",
                    crate::caps::active_glyphs().middot
                ),
            ),
        ]),
        selected,
    )];
    if thread.comments.is_empty() {
        push_wrapped_body_lines(&mut out, "(no comment)", cols);
    } else {
        for comment in &thread.comments {
            out.push((
                Line::segs(vec![
                    seg(Tok::Slot(S::Faint), sel_marker(false)),
                    seg(Tok::Slot(S::Accent), "  -> "),
                    seg(Tok::Slot(S::Text), comment.author.clone()).bold(),
                ]),
                false,
            ));
            push_wrapped_body_lines(&mut out, &comment.body, cols);
        }
    }
    out
}

pub(crate) fn review_feedback_lines(
    thread: &ReviewThread,
    label: &str,
    selected: bool,
    cols: usize,
) -> Vec<(Line, bool)> {
    let location = format!(
        "{}:{}{}",
        thread.path,
        thread.line.map(|line| line.to_string()).unwrap_or_default(),
        if thread.resolved { " (resolved)" } else { "" }
    );
    let mut out = vec![(
        Line::segs(vec![
            seg(Tok::Slot(S::Faint), sel_marker(selected)),
            seg(
                Tok::Slot(S::Accent),
                format!("{label} {} ", crate::caps::active_glyphs().middot),
            ),
            seg(Tok::Slot(S::Dim), location),
        ]),
        selected,
    )];
    for comment in &thread.comments {
        out.push((
            Line::segs(vec![
                seg(Tok::Slot(S::Faint), sel_marker(false)),
                seg(Tok::Slot(S::Accent), "  -> "),
                seg(Tok::Slot(S::Text), comment.author.clone()).bold(),
            ]),
            false,
        ));
        push_wrapped_body_lines(&mut out, &comment.body, cols);
    }
    out
}

/// Render the PR-level part of a review snapshot. This deliberately remains
/// separate from anchored rows because it has no diff-row selection target.
pub(crate) fn top_level_feedback_lines(
    conversation: &PrConversation,
    cols: usize,
) -> Vec<(Line, bool)> {
    let mut out = Vec::new();
    for comment in &conversation.comments {
        out.push((
            Line::segs(vec![
                seg(
                    Tok::Slot(S::Accent),
                    format!("TOP-LEVEL {} ", crate::caps::active_glyphs().middot),
                ),
                seg(Tok::Slot(S::Text), comment.author.clone()).bold(),
            ]),
            false,
        ));
        push_wrapped_body_lines(&mut out, &comment.body, cols);
    }
    for review in &conversation.reviews {
        if review.body.trim().is_empty() {
            continue;
        }
        let (glyph, tone) = review_state_marker(&review.state);
        out.push((
            Line::segs(vec![
                seg(tone, format!("{} ", glyph)),
                seg(Tok::Slot(S::Text), review.author.clone()).bold(),
                seg(tone, format!("  {}", review.state)),
            ]),
            false,
        ));
        push_wrapped_body_lines(&mut out, &review.body, cols);
    }
    out
}

pub(crate) fn review_state_marker(state: &str) -> (&'static str, Tok) {
    match state.to_uppercase().as_str() {
        "APPROVED" => (
            crate::caps::active_glyphs().check,
            Tok::Hue(thegn_core::theme::Hue::Green),
        ),
        "CHANGES_REQUESTED" => (
            crate::caps::active_glyphs().cross,
            Tok::Hue(thegn_core::theme::Hue::Red),
        ),
        _ => (crate::caps::active_glyphs().mail, Tok::Slot(S::Dim)),
    }
}
