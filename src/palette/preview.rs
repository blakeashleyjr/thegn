//! Builds the right-hand preview for the highlighted row. Kept deliberately
//! cheap (bounded reads, short git invocations) and memoized by the caller so it
//! only rebuilds when the selection changes — never per redraw.

use super::item::{Action, Row, RowKind};
use std::path::Path;
use std::process::Command;

/// Max lines a preview will render (the pane clips anyway; this bounds IO).
const MAX_LINES: usize = 240;

/// A stable identity for the selected row, so the caller can skip rebuilding the
/// preview while the selection is unchanged.
pub fn key(row: &Row) -> String {
    format!("{:?}|{}|{}", row.kind, row.label, row.detail)
}

/// Build preview lines for `row`. Returns plain text lines; the caller styles
/// them. Empty when there's nothing useful to show.
pub fn build(row: &Row) -> Vec<String> {
    match (&row.kind, &row.action, &row.preview_path) {
        (RowKind::Content, Action::OpenFileAt(p, line), _) => context(p, *line, 60),
        (_, _, Some(p)) if is_file(p) => head(p, MAX_LINES),
        (RowKind::Worktree, _, Some(p)) => git_status(p),
        (RowKind::Repo, _, Some(p)) => git_log(p),
        (RowKind::Branch, Action::Checkout(b), _) => vec![format!("switch to branch '{b}'")],
        _ => Vec::new(),
    }
}

fn is_file(p: &Path) -> bool {
    p.is_file()
}

/// First `max` lines of a text file (skips obviously-binary content).
fn head(path: &Path, max: usize) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return vec![format!("(could not read {})", path.display())];
    };
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return vec!["(binary file)".into()];
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .take(max)
        .map(|l| l.to_string())
        .collect()
}

/// A window of lines centered on `line` (1-based), for a content-search hit.
fn context(path: &Path, line: usize, window: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![format!("(could not read {})", path.display())];
    };
    let start = line.saturating_sub(window / 2).max(1);
    text.lines()
        .enumerate()
        .skip(start - 1)
        .take(window)
        .map(|(i, l)| {
            let n = i + 1;
            let marker = if n == line { "▸" } else { " " };
            format!("{marker}{n:>5} {l}")
        })
        .collect()
}

fn git_status(worktree: &Path) -> Vec<String> {
    let mut out = git(worktree, &["status", "-sb"]);
    if out.is_empty() {
        out.push("(clean working tree)".into());
    }
    out
}

fn git_log(repo: &Path) -> Vec<String> {
    git(repo, &["log", "--oneline", "--graph", "--decorate", "-20"])
}

fn git(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut full = vec!["-C", dir.to_str().unwrap_or(".")];
    full.extend_from_slice(args);
    Command::new("git")
        .args(&full)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .take(MAX_LINES)
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::item::{Action, Row, RowKind};
    use crate::palette::testutil;
    use std::path::PathBuf;

    fn row(kind: RowKind, action: Action, path: Option<PathBuf>) -> Row {
        Row {
            glyph: "x".into(),
            hue: crate::theme::TEAL,
            label: "lbl".into(),
            detail: "d".into(),
            haystack: "lbl".into(),
            kind,
            action,
            frecency_key: None,
            preview_path: path,
        }
    }

    #[test]
    fn file_head_reads_text() {
        let dir = testutil::sandbox().join("prev");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let lines = build(&row(RowKind::File, Action::OpenFile(f.clone()), Some(f)));
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[test]
    fn binary_file_is_reported_not_dumped() {
        let dir = testutil::sandbox().join("prev");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("bin");
        std::fs::write(&f, [0u8, 1, 2, 0, 3]).unwrap();
        let lines = build(&row(RowKind::File, Action::OpenFile(f.clone()), Some(f)));
        assert_eq!(lines, vec!["(binary file)"]);
    }

    #[test]
    fn content_context_marks_the_hit_line() {
        let dir = testutil::sandbox().join("prev");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("c.txt");
        std::fs::write(&f, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let lines = build(&row(
            RowKind::Content,
            Action::OpenFileAt(f.clone(), 3),
            Some(f),
        ));
        assert!(lines.iter().any(|l| l.contains("▸") && l.contains("l3")));
    }

    #[test]
    fn missing_file_yields_empty_preview() {
        let p = PathBuf::from("/no/such/file/here.xyz");
        let lines = build(&row(RowKind::File, Action::OpenFile(p.clone()), Some(p)));
        assert!(lines.is_empty());
    }

    #[test]
    fn branch_preview_is_descriptive() {
        let lines = build(&row(RowKind::Branch, Action::Checkout("feat".into()), None));
        assert_eq!(lines, vec!["switch to branch 'feat'"]);
    }

    #[test]
    fn worktree_and_repo_previews_use_git() {
        let repo = testutil::temp_git_repo("prev-repo");
        let wt = build(&row(
            RowKind::Worktree,
            Action::Dashboard,
            Some(repo.clone()),
        ));
        // Clean tree -> status -sb prints the branch header (## main).
        assert!(wt.iter().any(|l| l.contains("main")) || wt == vec!["(clean working tree)"]);
        let log = build(&row(RowKind::Repo, Action::Dashboard, Some(repo)));
        assert!(log.iter().any(|l| l.contains("init")));
    }

    #[test]
    fn key_is_stable_for_same_row() {
        let r = row(RowKind::File, Action::Dashboard, None);
        assert_eq!(key(&r), key(&r));
    }
}
