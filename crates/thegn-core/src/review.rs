//! Pure PR review data projection.
//!
//! This module deliberately knows nothing about a forge client, SQLite, or a
//! renderer. A review snapshot is a cache wire value; [`anchor_threads`] and
//! [`format_review_feedback`] are the shared projections used by the host and
//! the agent handoff path.

use crate::forge::model::{PrComment, PrConversation, PrDiff, PrReview, ReviewThread};
use serde::{Deserialize, Serialize};

/// A complete, identity-bearing review snapshot for one worktree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrReviewSnapshot {
    pub worktree_key: String,
    pub branch: String,
    pub pr_number: u64,
    pub head_oid: String,
    pub fetched_at: i64,
    #[serde(default)]
    pub conversation: PrConversation,
    #[serde(default)]
    pub diff: PrDiff,
}

/// The result of projecting conversation threads onto the PR's new-side diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchoredReview {
    /// Files retain PR diff order. Threads in each file retain diff-line order,
    /// then source order for multiple threads on one line.
    pub files: Vec<AnchoredReviewFile>,
    /// Threads without a path are not associated with a file.
    pub general: Vec<ReviewThread>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchoredReviewFile {
    pub path: String,
    pub threads: Vec<AnchoredThread>,
    /// A path-bearing thread that has no exact new-side line anchor.
    pub outdated: Vec<ReviewThread>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchoredThread {
    pub thread: ReviewThread,
    pub line: u64,
}

/// Maximum size of text handed to a live or headless agent.
pub const MAX_REVIEW_FEEDBACK_CHARS: usize = 32 * 1024;
const MAX_REMOTE_FIELD_CHARS: usize = 8 * 1024;
const MAX_AUTHOR_CHARS: usize = 256;

/// Attach a thread only when its path and new-side line exactly match a PR
/// diff line. Deleted-side, missing, renamed-path, and otherwise stale anchors
/// are never guessed onto a nearby line.
pub fn anchor_threads(diff: &PrDiff, threads: &[ReviewThread]) -> AnchoredReview {
    let mut result = AnchoredReview {
        files: diff
            .files
            .iter()
            .map(|file| AnchoredReviewFile {
                path: file.path.clone(),
                ..AnchoredReviewFile::default()
            })
            .collect(),
        general: Vec::new(),
    };

    let mut anchored = vec![false; threads.len()];
    for (file_index, file) in diff.files.iter().enumerate() {
        for hunk in &file.hunks {
            for line in &hunk.lines {
                let Some(new_lineno) = line.new_lineno else {
                    continue;
                };
                for (thread_index, thread) in threads.iter().enumerate() {
                    if anchored[thread_index]
                        || thread.path != file.path
                        || thread.line != Some(new_lineno)
                    {
                        continue;
                    }
                    result.files[file_index].threads.push(AnchoredThread {
                        thread: thread.clone(),
                        line: new_lineno,
                    });
                    anchored[thread_index] = true;
                }
            }
        }
    }

    for (index, thread) in threads.iter().enumerate() {
        if anchored[index] {
            continue;
        }
        if thread.path.is_empty() {
            result.general.push(thread.clone());
            continue;
        }
        if let Some(file) = result
            .files
            .iter_mut()
            .find(|file| file.path == thread.path)
        {
            file.outdated.push(thread.clone());
        } else {
            // Preserve a renamed/missing path as an explicit feedback bucket;
            // inventing a matching file would make the anchor look current.
            result.files.push(AnchoredReviewFile {
                path: thread.path.clone(),
                outdated: vec![thread.clone()],
                ..AnchoredReviewFile::default()
            });
        }
    }
    result
}

/// Return visible threads in stable PR-diff order. Resolved threads are
/// omitted unless the view-local resolved toggle is enabled.
pub fn visible_threads(review: &AnchoredReview, include_resolved: bool) -> Vec<ReviewThread> {
    let mut visible = Vec::new();
    for file in &review.files {
        for thread in &file.threads {
            if include_resolved || !thread.thread.resolved {
                visible.push(thread.thread.clone());
            }
        }
        for thread in &file.outdated {
            if include_resolved || !thread.resolved {
                visible.push(thread.clone());
            }
        }
    }
    for thread in &review.general {
        if include_resolved || !thread.resolved {
            visible.push(thread.clone());
        }
    }
    visible
}

/// Format one selected thread, or all unresolved threads when `selected` is
/// `None`. Remote bodies are explicitly framed as data and are bounded and
/// stripped of terminal controls. The result never ends in a newline.
pub fn format_review_feedback(
    snapshot: &PrReviewSnapshot,
    selected: Option<&ReviewThread>,
) -> String {
    let threads: Vec<&ReviewThread> = match selected {
        Some(thread) => vec![thread],
        None => snapshot
            .conversation
            .threads
            .iter()
            .filter(|thread| !thread.resolved)
            .collect(),
    };
    let all_feedback = selected.is_none();
    let mut out = String::new();
    push_line(
        &mut out,
        &format!(
            "PR #{} on branch `{}` (head {})",
            snapshot.pr_number,
            clean_bounded(&snapshot.branch, MAX_REMOTE_FIELD_CHARS),
            clean_bounded(&snapshot.head_oid, MAX_REMOTE_FIELD_CHARS)
        ),
    );
    if all_feedback {
        push_line(&mut out, "Top-level feedback:");
        for comment in &snapshot.conversation.comments {
            push_remote_comment(&mut out, comment);
        }
        for review in &snapshot.conversation.reviews {
            if !review.body.trim().is_empty() {
                push_remote_review(&mut out, review);
            }
        }
    }
    for thread in threads {
        push_line(&mut out, "Review thread:");
        let location = match thread.line {
            Some(line) if !thread.path.is_empty() => format!(
                "{}:{}",
                clean_bounded(&thread.path, MAX_REMOTE_FIELD_CHARS),
                line
            ),
            _ if !thread.path.is_empty() => {
                format!(
                    "{} (outdated/no exact diff anchor)",
                    clean_bounded(&thread.path, MAX_REMOTE_FIELD_CHARS)
                )
            }
            _ => "no diff anchor".to_string(),
        };
        push_line(&mut out, &format!("Location: {location}"));
        if !thread.diff_hunk.is_empty() {
            push_line(&mut out, "Diff hunk:");
            push_remote_block(&mut out, &thread.diff_hunk);
        }
        for comment in &thread.comments {
            push_remote_comment(&mut out, comment);
        }
    }
    truncate_feedback(&mut out);
    out
}

fn push_remote_comment(out: &mut String, comment: &PrComment) {
    push_line(out, "<review-comment data>");
    push_line(
        out,
        &format!(
            "author: {}",
            clean_bounded(&comment.author, MAX_AUTHOR_CHARS)
        ),
    );
    push_remote_block(out, &comment.body);
    push_line(out, "</review-comment data>");
}

fn push_remote_review(out: &mut String, review: &PrReview) {
    push_line(out, "<submitted-review data>");
    push_line(
        out,
        &format!(
            "author: {} state: {}",
            clean_bounded(&review.author, MAX_AUTHOR_CHARS),
            clean_bounded(&review.state, MAX_AUTHOR_CHARS)
        ),
    );
    push_remote_block(out, &review.body);
    push_line(out, "</submitted-review data>");
}

fn push_remote_block(out: &mut String, text: &str) {
    push_line(out, "<remote-body>");
    for line in clean_bounded(text, MAX_REMOTE_FIELD_CHARS).split('\n') {
        push_line(out, line);
    }
    push_line(out, "</remote-body>");
}

fn clean_bounded(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|ch| !is_terminal_control(*ch))
        .take(max_chars)
        .collect()
}

fn is_terminal_control(ch: char) -> bool {
    matches!(ch as u32, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f)
}

fn push_line(out: &mut String, line: &str) {
    let remaining = MAX_REVIEW_FEEDBACK_CHARS.saturating_sub(out.len());
    if remaining <= 1 {
        return;
    }
    let mut room = remaining - 1;
    for ch in line.chars() {
        if ch.len_utf8() > room {
            break;
        }
        out.push(ch);
        room -= ch.len_utf8();
    }
    out.push('\n');
}

fn truncate_feedback(out: &mut String) {
    while out.ends_with('\n') {
        out.pop();
    }
    if out.chars().count() <= MAX_REVIEW_FEEDBACK_CHARS {
        return;
    }
    let suffix = "…[truncated]";
    let keep = MAX_REVIEW_FEEDBACK_CHARS - suffix.chars().count();
    let prefix: String = out.chars().take(keep).collect();
    *out = prefix;
    out.push_str(suffix);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::model::{DiffFile, DiffHunk, DiffLine, DiffLineKind};

    fn diff(path: &str, lines: &[(DiffLineKind, Option<u64>)]) -> PrDiff {
        PrDiff {
            files: vec![DiffFile {
                path: path.into(),
                old_path: None,
                hunks: vec![DiffHunk {
                    header: "@@".into(),
                    lines: lines
                        .iter()
                        .map(|(kind, line)| DiffLine {
                            kind: *kind,
                            text: "code".into(),
                            new_lineno: *line,
                            old_lineno: None,
                        })
                        .collect(),
                }],
            }],
        }
    }

    fn thread(id: &str, path: &str, line: Option<u64>, resolved: bool) -> ReviewThread {
        ReviewThread {
            id: id.into(),
            path: path.into(),
            line,
            resolved,
            comments: vec![PrComment {
                author: "alice".into(),
                body: format!("comment {id}"),
                ..PrComment::default()
            }],
            diff_hunk: "@@ -1 +1 @@".into(),
        }
    }

    #[test]
    fn anchors_exact_new_side_and_preserves_duplicate_source_order() {
        let threads = vec![
            thread("first", "src/lib.rs", Some(10), false),
            thread("second", "src/lib.rs", Some(10), false),
        ];
        let review = anchor_threads(
            &diff(
                "src/lib.rs",
                &[(DiffLineKind::Del, None), (DiffLineKind::Add, Some(10))],
            ),
            &threads,
        );
        assert_eq!(review.files[0].threads.len(), 2);
        assert_eq!(review.files[0].threads[0].thread.id, "first");
        assert_eq!(review.files[0].threads[1].thread.id, "second");
        assert!(review.files[0].outdated.is_empty());
    }

    #[test]
    fn misses_deleted_lines_and_renamed_paths_without_nearest_guess() {
        let threads = vec![
            thread("deleted", "src/lib.rs", Some(9), false),
            thread("renamed", "src/old.rs", Some(10), false),
            thread("general", "", None, false),
        ];
        let review = anchor_threads(
            &diff(
                "src/lib.rs",
                &[(DiffLineKind::Del, None), (DiffLineKind::Add, Some(10))],
            ),
            &threads,
        );
        assert_eq!(review.files[0].outdated[0].id, "deleted");
        assert_eq!(review.files[1].path, "src/old.rs");
        assert_eq!(review.files[1].outdated[0].id, "renamed");
        assert_eq!(review.general[0].id, "general");
    }

    #[test]
    fn visible_threads_filters_resolved_locally() {
        let review = anchor_threads(
            &diff("src/lib.rs", &[(DiffLineKind::Add, Some(10))]),
            &[
                thread("open", "src/lib.rs", Some(10), false),
                thread("done", "src/lib.rs", Some(10), true),
            ],
        );
        assert_eq!(
            visible_threads(&review, false)
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["open"]
        );
        assert_eq!(visible_threads(&review, true).len(), 2);
    }

    #[test]
    fn formatter_includes_identity_hunk_comments_and_top_level_only_for_all() {
        let mut snapshot = PrReviewSnapshot {
            worktree_key: "/wt".into(),
            branch: "feature".into(),
            pr_number: 27,
            head_oid: "abc123".into(),
            fetched_at: 1,
            conversation: PrConversation {
                comments: vec![PrComment {
                    author: "top".into(),
                    body: "summary".into(),
                    ..PrComment::default()
                }],
                ..PrConversation::default()
            },
            diff: PrDiff::default(),
        };
        snapshot
            .conversation
            .threads
            .push(thread("t", "src/lib.rs", Some(10), false));
        let selected = format_review_feedback(&snapshot, Some(&snapshot.conversation.threads[0]));
        assert!(selected.contains("PR #27"));
        assert!(selected.contains("src/lib.rs:10"));
        assert!(selected.contains("@@ -1 +1 @@"));
        assert!(!selected.contains("summary"));
        let all = format_review_feedback(&snapshot, None);
        assert!(all.contains("summary"));
        assert!(!all.ends_with('\n'));
    }

    #[test]
    fn formatter_strips_controls_and_is_bounded() {
        let mut snapshot = PrReviewSnapshot {
            pr_number: 27,
            conversation: PrConversation {
                threads: vec![ReviewThread {
                    comments: vec![PrComment {
                        author: "a\x1b[31m".into(),
                        body: "x\0\u{9b}\n\t".repeat(20_000),
                        ..PrComment::default()
                    }],
                    ..thread("hostile", "src/lib.rs", Some(1), false)
                }],
                ..PrConversation::default()
            },
            ..PrReviewSnapshot::default()
        };
        let out = format_review_feedback(&snapshot, None);
        assert!(out.chars().count() <= MAX_REVIEW_FEEDBACK_CHARS);
        assert!(!out.chars().any(is_terminal_control));
        assert!(!out.ends_with('\n'));
        snapshot.conversation.threads[0].resolved = true;
        let resolved = format_review_feedback(&snapshot, None);
        assert!(!resolved.contains("hostile"));
    }
}
