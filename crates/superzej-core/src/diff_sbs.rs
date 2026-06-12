//! Unified-diff → side-by-side pairing for the diff overlay: each hunk's
//! lines become aligned `(old, new)` row pairs — context on both sides,
//! consecutive `-`/`+` runs zipped as changed lines, leftovers one-sided.
//!
//! Pure parsing; the host fetches `git diff --no-color` text off-thread and
//! renders the result. Tolerant of malformed input: unparseable headers skip
//! their hunk, trailing noise is ignored.

/// What a cell holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Context,
    Removed,
    Added,
}

/// One side of a row: the source line number and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbsCell {
    pub line_no: u32,
    pub text: String,
    pub kind: CellKind,
}

/// One aligned row; `None` renders as a blank cell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SbsRow {
    pub old: Option<SbsCell>,
    pub new: Option<SbsCell>,
}

/// One hunk of aligned rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbsHunk {
    pub old_start: u32,
    pub new_start: u32,
    /// The function context from the `@@ … @@ func` header (may be empty).
    pub func: String,
    pub rows: Vec<SbsRow>,
}

/// The parsed file: hunks + total diffstat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SbsFile {
    pub hunks: Vec<SbsHunk>,
    pub added: u32,
    pub deleted: u32,
}

/// Parse a `@@ -a[,b] +c[,d] @@ func` header into (old_start, new_start, func).
fn parse_hunk_header(line: &str) -> Option<(u32, u32, String)> {
    let rest = line.strip_prefix("@@")?;
    let (range, func) = match rest.split_once("@@") {
        Some((r, f)) => (r.trim(), f.trim()),
        None => return None,
    };
    let mut old_start = None;
    let mut new_start = None;
    for part in range.split_whitespace() {
        let (sign, nums) = part.split_at(1);
        let start = nums.split(',').next()?.parse::<u32>().ok()?;
        match sign {
            "-" => old_start = Some(start),
            "+" => new_start = Some(start),
            _ => return None,
        }
    }
    Some((old_start?, new_start?, func.to_string()))
}

/// Zip a buffered run of removed/added lines into aligned rows: the i-th
/// removal pairs with the i-th addition; leftovers go one-sided.
fn flush_run(rows: &mut Vec<SbsRow>, removed: &mut Vec<SbsCell>, added: &mut Vec<SbsCell>) {
    let n = removed.len().max(added.len());
    let mut rem = removed.drain(..);
    let mut add = added.drain(..);
    for _ in 0..n {
        rows.push(SbsRow {
            old: rem.next(),
            new: add.next(),
        });
    }
}

/// Parse a single-file unified diff (the output of
/// `git diff --no-color [-U<n>] <base> -- <path>`).
pub fn parse_unified(diff: &str) -> SbsFile {
    let mut file = SbsFile::default();
    let mut cur: Option<SbsHunk> = None;
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut removed: Vec<SbsCell> = Vec::new();
    let mut added: Vec<SbsCell> = Vec::new();

    let mut finish =
        |cur: &mut Option<SbsHunk>, removed: &mut Vec<SbsCell>, added: &mut Vec<SbsCell>| {
            if let Some(mut h) = cur.take() {
                flush_run(&mut h.rows, removed, added);
                file.hunks.push(h);
            }
        };

    for line in diff.lines() {
        if line.starts_with("@@") {
            match parse_hunk_header(line) {
                Some((o, n, func)) => {
                    finish(&mut cur, &mut removed, &mut added);
                    cur = Some(SbsHunk {
                        old_start: o,
                        new_start: n,
                        func,
                        rows: Vec::new(),
                    });
                    old_no = o;
                    new_no = n;
                }
                None => {
                    // Malformed header: drop the current hunk context so its
                    // lines can't misattribute.
                    finish(&mut cur, &mut removed, &mut added);
                }
            }
            continue;
        }
        let Some(h) = cur.as_mut() else { continue };
        let mut chars = line.chars();
        match chars.next() {
            Some(' ') => {
                flush_run(&mut h.rows, &mut removed, &mut added);
                let text: String = chars.collect();
                h.rows.push(SbsRow {
                    old: Some(SbsCell {
                        line_no: old_no,
                        text: text.clone(),
                        kind: CellKind::Context,
                    }),
                    new: Some(SbsCell {
                        line_no: new_no,
                        text,
                        kind: CellKind::Context,
                    }),
                });
                old_no += 1;
                new_no += 1;
            }
            Some('-') => {
                removed.push(SbsCell {
                    line_no: old_no,
                    text: chars.collect(),
                    kind: CellKind::Removed,
                });
                old_no += 1;
                file.deleted += 1;
            }
            Some('+') => {
                added.push(SbsCell {
                    line_no: new_no,
                    text: chars.collect(),
                    kind: CellKind::Added,
                });
                new_no += 1;
                file.added += 1;
            }
            // "\ No newline at end of file", file headers inside a hunk
            // region (can't happen in well-formed output), blank lines.
            _ => {}
        }
    }
    finish(&mut cur, &mut removed, &mut added);
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn balanced_change_pairs_old_with_new() {
        let f = parse_unified(&diff(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -10,3 +10,3 @@ fn demo()",
            " ctx",
            "-old line",
            "+new line",
            " tail",
        ]));
        assert_eq!(f.hunks.len(), 1);
        assert_eq!((f.added, f.deleted), (1, 1));
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.new_start), (10, 10));
        assert_eq!(h.func, "fn demo()");
        assert_eq!(h.rows.len(), 3);
        // Row 0: context on both sides with matching numbering.
        let r0 = &h.rows[0];
        assert_eq!(r0.old.as_ref().unwrap().line_no, 10);
        assert_eq!(r0.new.as_ref().unwrap().line_no, 10);
        assert_eq!(r0.old.as_ref().unwrap().kind, CellKind::Context);
        // Row 1: the paired change.
        let r1 = &h.rows[1];
        assert_eq!(r1.old.as_ref().unwrap().text, "old line");
        assert_eq!(r1.old.as_ref().unwrap().kind, CellKind::Removed);
        assert_eq!(r1.new.as_ref().unwrap().text, "new line");
        assert_eq!(r1.new.as_ref().unwrap().kind, CellKind::Added);
        // Row 2: trailing context advances both numbers past the change
        // (line 11 was consumed by the removal/addition pair).
        let r2 = &h.rows[2];
        assert_eq!(r2.old.as_ref().unwrap().line_no, 12);
        assert_eq!(r2.new.as_ref().unwrap().line_no, 12);
    }

    #[test]
    fn unbalanced_runs_leave_one_sided_rows() {
        let f = parse_unified(&diff(&[
            "@@ -1,2 +1,4 @@",
            "-only removal",
            "+first add",
            "+second add",
            "+third add",
        ]));
        let h = &f.hunks[0];
        assert_eq!(h.rows.len(), 3);
        assert!(h.rows[0].old.is_some() && h.rows[0].new.is_some());
        assert!(h.rows[1].old.is_none() && h.rows[1].new.is_some());
        assert!(h.rows[2].old.is_none());
        assert_eq!(h.rows[2].new.as_ref().unwrap().text, "third add");
        assert_eq!((f.added, f.deleted), (3, 1));
    }

    #[test]
    fn pure_addition_and_pure_deletion() {
        let add = parse_unified(&diff(&["@@ -0,0 +1,2 @@", "+a", "+b"]));
        assert_eq!(add.hunks[0].rows.len(), 2);
        assert!(add.hunks[0].rows.iter().all(|r| r.old.is_none()));
        assert_eq!(add.hunks[0].rows[1].new.as_ref().unwrap().line_no, 2);

        let del = parse_unified(&diff(&["@@ -5,2 +4,0 @@", "-x", "-y"]));
        assert!(del.hunks[0].rows.iter().all(|r| r.new.is_none()));
        assert_eq!(del.hunks[0].rows[0].old.as_ref().unwrap().line_no, 5);
        assert_eq!((del.added, del.deleted), (0, 2));
    }

    #[test]
    fn multi_hunk_numbering_resets_per_hunk() {
        let f = parse_unified(&diff(&[
            "@@ -1,1 +1,1 @@",
            "-a",
            "+A",
            "@@ -100,2 +100,1 @@ second()",
            " keep",
            "-drop",
        ]));
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(f.hunks[1].old_start, 100);
        assert_eq!(f.hunks[1].func, "second()");
        assert_eq!(f.hunks[1].rows[1].old.as_ref().unwrap().line_no, 101);
        assert!(f.hunks[1].rows[1].new.is_none());
    }

    #[test]
    fn single_count_headers_and_no_newline_marker() {
        // "-5 +5" (no comma counts) parses; the `\` marker is ignored.
        let f = parse_unified(&diff(&[
            "@@ -5 +5 @@",
            "-old",
            "+new",
            "\\ No newline at end of file",
        ]));
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].rows.len(), 1);
    }

    #[test]
    fn garbage_is_tolerated() {
        assert_eq!(parse_unified(""), SbsFile::default());
        assert!(
            parse_unified("not a diff at all\njust text\n")
                .hunks
                .is_empty()
        );
        // A malformed header drops cleanly; later good hunks still parse.
        let f = parse_unified(&diff(&[
            "@@ broken header",
            "-stray",
            "@@ -1,1 +1,1 @@",
            "+kept",
        ]));
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].rows[0].new.as_ref().unwrap().text, "kept");
        // Lines before any hunk header are ignored.
        let f = parse_unified("+++ b/x\n--- a/x\n");
        assert!(f.hunks.is_empty());
        assert_eq!((f.added, f.deleted), (0, 0));
    }

    #[test]
    fn crlf_input_keeps_carriage_return_out_of_semantics() {
        // .lines() strips \n but keeps \r — content equality is on the text
        // as-is; structure must still parse.
        let f = parse_unified("@@ -1,1 +1,1 @@\r\n-a\r\n+b\r\n");
        assert_eq!(f.hunks.len(), 1);
        assert_eq!((f.added, f.deleted), (1, 1));
    }
}
