//! Line-staging patch engine for the diff panel: parse `git diff` output into
//! addressable lines, let the UI select individual ones, and rebuild a valid
//! partial patch for `git apply --cached [--reverse]`. Semantics mirror
//! lazygit's `pkg/commands/patch` (verified against real `git apply`).
//!
//! Pure string-domain logic; the host shells out for the diff and the apply.
//! Byte fidelity is a hard contract: `parse_patch` → [`render_patch`] is
//! byte-identical for well-formed git output (hunk headers are kept verbatim,
//! `\r` survives so CRLF files round-trip, `\ No newline at end of file`
//! markers are preserved). Tolerant of malformed input — junk lines end the
//! current hunk and are dropped; nothing panics.

use std::collections::{BTreeSet, HashMap};

/// What a body line of a hunk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged line (leading `' '`).
    Context,
    /// Added line (leading `+`).
    Add,
    /// Deleted line (leading `-`).
    Del,
    /// `\ No newline at end of file` bound to the preceding `-` line. Also
    /// the representation chosen for a marker after a *context* line, where
    /// it applies to both sides of the diff.
    NoNewlineOld,
    /// `\ No newline at end of file` bound to the preceding `+` line.
    NoNewlineNew,
}

/// One body line of a hunk: the text WITHOUT the leading `+`/`-`/`' '`/`\`
/// marker, but WITH any trailing `\r` preserved (CRLF round-trips).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub kind: LineKind,
    pub text: String,
}

/// One `@@`-delimited hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// Trailing text after the second `@@` (function context; may be empty).
    pub heading: String,
    /// The verbatim header line as parsed — [`render_patch`] emits it as-is
    /// so count-omitted forms like `@@ -1 +1 @@` round-trip byte-identically.
    /// Empty for programmatically built hunks; render then synthesizes an
    /// explicit `@@ -a,b +c,d @@` header from the numeric fields.
    pub header_line: String,
    pub lines: Vec<PatchLine>,
}

/// What kind of change a file section describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Modified,
    Added,
    Deleted,
    Binary,
    ModeOnly,
}

/// One file section of a (possibly multi-file) diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFile {
    /// Verbatim pre-`@@` header lines (`diff --git`, `index`, `---`/`+++`,
    /// mode lines, binary payloads …) — quoted/spaced paths never corrupt.
    pub header: Vec<String>,
    /// Old path without the `a/` prefix; for [`FileKind::Added`] this is the
    /// new path (there is no old side).
    pub old_path: String,
    /// New path without the `b/` prefix; for [`FileKind::Deleted`] this is
    /// the old path.
    pub new_path: String,
    pub kind: FileKind,
    pub hunks: Vec<PatchHunk>,
}

/// Parse one side of a hunk range (`-a[,b]` / `+c[,d]`; count defaults to 1).
fn parse_range(part: &str, sign: char) -> Option<(u32, u32)> {
    let rest = part.strip_prefix(sign)?;
    match rest.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((rest.parse().ok()?, 1)),
    }
}

/// Parse `@@ -a[,b] +c[,d] @@[ heading]` → (old, new ranges, heading).
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32, String)> {
    let rest = line.strip_prefix("@@ ")?;
    let (ranges, tail) = rest.split_once(" @@")?;
    let mut parts = ranges.split(' ').filter(|p| !p.is_empty());
    let (old_start, old_count) = parse_range(parts.next()?, '-')?;
    let (new_start, new_count) = parse_range(parts.next()?, '+')?;
    if parts.next().is_some() {
        return None;
    }
    let heading = tail.strip_prefix(' ').unwrap_or(tail).to_string();
    Some((old_start, old_count, new_start, new_count, heading))
}

/// Clean a `---`/`+++` path: strip a trailing tab (git appends one for paths
/// containing spaces), surrounding quotes (kept escaped — header lines stay
/// verbatim, this is only the display/lookup form), and the `a/`/`b/` prefix.
/// `None` for `/dev/null`.
fn header_path(raw: &str) -> Option<String> {
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let raw = raw.strip_suffix('\t').unwrap_or(raw);
    if raw == "/dev/null" {
        return None;
    }
    let raw = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    Some(strip_ab(raw))
}

/// Strip a leading `a/` or `b/` prefix.
fn strip_ab(s: &str) -> String {
    s.strip_prefix("a/")
        .or_else(|| s.strip_prefix("b/"))
        .unwrap_or(s)
        .to_string()
}

/// Best-effort path pair from the text after `diff --git ` — the fallback
/// when `---`/`+++` lines are absent (binary, mode-only). Quoted pairs are
/// split on the quote boundary; unquoted ones use the equal-length midpoint
/// (paths are equal under `--no-renames`, so spaces stay unambiguous).
fn diff_line_paths(rest: &str) -> Option<(String, String)> {
    let rest = rest.strip_suffix('\r').unwrap_or(rest);
    if let Some(inner) = rest.strip_prefix('"') {
        let (first, rem) = inner.split_once('"')?;
        let second = rem
            .strip_prefix(' ')?
            .strip_prefix('"')?
            .strip_suffix('"')?;
        return Some((strip_ab(first), strip_ab(second)));
    }
    let mid = rest.len() / 2;
    if rest.len() >= 5
        && rest.len() % 2 == 1
        && rest.as_bytes()[mid] == b' '
        && let (Some(a), Some(b)) = (
            rest[..mid].strip_prefix("a/"),
            rest[mid + 1..].strip_prefix("b/"),
        )
        && a == b
    {
        return Some((a.to_string(), b.to_string()));
    }
    let (l, r) = rest.split_once(' ')?;
    Some((strip_ab(l), strip_ab(r)))
}

/// Accumulates one file section during parsing.
#[derive(Default)]
struct FileBuilder {
    header: Vec<String>,
    /// Text after `diff --git ` (path fallback for binary/mode-only files).
    diff_rest: String,
    /// `---` path: outer `None` = not seen, inner `None` = `/dev/null`.
    minus: Option<Option<String>>,
    /// `+++` path, same encoding.
    plus: Option<Option<String>>,
    added: bool,
    deleted: bool,
    binary: bool,
    hunks: Vec<PatchHunk>,
}

impl FileBuilder {
    fn new(line: &str, rest: &str) -> Self {
        Self {
            header: vec![line.to_string()],
            diff_rest: rest.to_string(),
            ..Self::default()
        }
    }

    /// Record a pre-hunk header line verbatim, sniffing paths and file kind.
    fn absorb_header(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("--- ") {
            self.minus = Some(header_path(rest));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            self.plus = Some(header_path(rest));
        } else if line.starts_with("new file mode") {
            self.added = true;
        } else if line.starts_with("deleted file mode") {
            self.deleted = true;
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            self.binary = true;
        }
        self.header.push(line.to_string());
    }

    fn finish(self) -> PatchFile {
        let guess = diff_line_paths(&self.diff_rest);
        let minus_devnull = matches!(self.minus, Some(None));
        let plus_devnull = matches!(self.plus, Some(None));
        let mut old_path = self
            .minus
            .flatten()
            .or_else(|| guess.clone().map(|g| g.0))
            .unwrap_or_default();
        let mut new_path = self
            .plus
            .flatten()
            .or_else(|| guess.map(|g| g.1))
            .unwrap_or_default();
        let kind = if self.binary {
            FileKind::Binary
        } else if self.added || minus_devnull {
            FileKind::Added
        } else if self.deleted || plus_devnull {
            FileKind::Deleted
        } else if self.hunks.is_empty() {
            FileKind::ModeOnly
        } else {
            FileKind::Modified
        };
        match kind {
            FileKind::Added => old_path = new_path.clone(),
            FileKind::Deleted => new_path = old_path.clone(),
            _ => {}
        }
        PatchFile {
            header: self.header,
            old_path,
            new_path,
            kind,
            hunks: self.hunks,
        }
    }
}

/// Parse a (possibly multi-file) unified diff as produced by
/// `git -c diff.noprefix=false diff --no-color --no-ext-diff --no-renames -U3`.
///
/// Tolerant of trailing noise and never panics on malformed input: lines
/// before the first `diff --git` are ignored, a junk line inside a hunk ends
/// the hunk, and a malformed `@@` header drops cleanly. Splits on `\n` only
/// (never [`str::lines`]) so a trailing `\r` stays in [`PatchLine::text`].
pub fn parse_patch(diff: &str) -> Vec<PatchFile> {
    let mut segments: Vec<&str> = diff.split('\n').collect();
    if segments.last() == Some(&"") {
        segments.pop(); // input ended with '\n' (or was empty)
    }

    let mut files: Vec<PatchFile> = Vec::new();
    let mut file: Option<FileBuilder> = None;
    let mut hunk: Option<PatchHunk> = None;
    let mut in_hunks = false;

    fn close_hunk(file: &mut Option<FileBuilder>, hunk: &mut Option<PatchHunk>) {
        if let Some(h) = hunk.take()
            && let Some(f) = file.as_mut()
        {
            f.hunks.push(h);
        }
    }

    for line in segments {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            close_hunk(&mut file, &mut hunk);
            if let Some(f) = file.take() {
                files.push(f.finish());
            }
            file = Some(FileBuilder::new(line, rest));
            in_hunks = false;
            continue;
        }
        if file.is_none() {
            continue; // noise before the first file section
        }
        if line.starts_with("@@") {
            close_hunk(&mut file, &mut hunk);
            // Even a malformed header ends the pre-hunk region: whatever
            // follows is never absorbed into `header`.
            in_hunks = true;
            if let Some((old_start, old_count, new_start, new_count, heading)) =
                parse_hunk_header(line)
            {
                hunk = Some(PatchHunk {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                    heading,
                    header_line: line.to_string(),
                    lines: Vec::new(),
                });
            }
            continue;
        }
        if let Some(h) = hunk.as_mut() {
            let kind = match line.as_bytes().first() {
                Some(b' ') => Some(LineKind::Context),
                Some(b'+') => Some(LineKind::Add),
                Some(b'-') => Some(LineKind::Del),
                Some(b'\\') => Some(match h.lines.last().map(|l| l.kind) {
                    Some(LineKind::Add) => LineKind::NoNewlineNew,
                    // After a `-` it is the old side's; after context it
                    // applies to both sides — represented as NoNewlineOld.
                    _ => LineKind::NoNewlineOld,
                }),
                _ => None,
            };
            match kind {
                Some(kind) => h.lines.push(PatchLine {
                    kind,
                    text: line[1..].to_string(),
                }),
                None => close_hunk(&mut file, &mut hunk), // junk ends the hunk
            }
            continue;
        }
        if !in_hunks && let Some(f) = file.as_mut() {
            f.absorb_header(line);
        }
        // else: noise between/after hunks — dropped.
    }
    close_hunk(&mut file, &mut hunk);
    if let Some(f) = file.take() {
        files.push(f.finish());
    }
    files
}

/// The leading marker character for a body line kind.
fn marker_char(kind: LineKind) -> char {
    match kind {
        LineKind::Context => ' ',
        LineKind::Add => '+',
        LineKind::Del => '-',
        LineKind::NoNewlineOld | LineKind::NoNewlineNew => '\\',
    }
}

/// Format an explicit `@@ -a,b +c,d @@[ heading]` header (counts never
/// omitted — `git apply` accepts both forms).
fn format_hunk_header(
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    heading: &str,
) -> String {
    let mut s = format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@");
    if !heading.is_empty() {
        s.push(' ');
        s.push_str(heading);
    }
    s
}

/// Append one hunk's body lines (each newline-terminated) to `out`.
fn push_body(out: &mut String, lines: &[PatchLine]) {
    for l in lines {
        out.push(marker_char(l.kind));
        out.push_str(&l.text);
        out.push('\n');
    }
}

/// Render files back to text. `parse_patch` → `render_patch` is
/// byte-identical for well-formed git output (incl. `\ No newline` markers,
/// CRLF content, count-omitted hunk headers — those re-emit verbatim via
/// [`PatchHunk::header_line`]).
pub fn render_patch(files: &[PatchFile]) -> String {
    let mut out = String::new();
    for f in files {
        for line in &f.header {
            out.push_str(line);
            out.push('\n');
        }
        for h in &f.hunks {
            if h.header_line.is_empty() {
                out.push_str(&format_hunk_header(
                    h.old_start,
                    h.old_count,
                    h.new_start,
                    h.new_count,
                    &h.heading,
                ));
            } else {
                out.push_str(&h.header_line);
            }
            out.push('\n');
            push_body(&mut out, &h.lines);
        }
    }
    out
}

/// A set of selected line addresses within ONE [`PatchFile`].
/// Address space: `(hunk_index, line_index within hunk.lines)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    set: BTreeSet<(usize, usize)>,
}

impl Selection {
    /// Select one line.
    pub fn insert(&mut self, hunk: usize, line: usize) {
        self.set.insert((hunk, line));
    }

    /// Deselect one line; returns whether it was selected.
    pub fn remove(&mut self, hunk: usize, line: usize) -> bool {
        self.set.remove(&(hunk, line))
    }

    /// Is this line selected?
    pub fn contains(&self, hunk: usize, line: usize) -> bool {
        self.set.contains(&(hunk, line))
    }

    /// Every line of `file.hunks[hunk]` (empty if out of range).
    pub fn whole_hunk(file: &PatchFile, hunk: usize) -> Selection {
        file.hunks
            .get(hunk)
            .map(|h| (0..h.lines.len()).map(|line| (hunk, line)).collect())
            .unwrap_or_default()
    }

    /// Every line of every hunk of `file`.
    pub fn whole_file(file: &PatchFile) -> Selection {
        (0..file.hunks.len())
            .flat_map(|hi| (0..file.hunks[hi].lines.len()).map(move |li| (hi, li)))
            .collect()
    }

    /// True when nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Number of selected lines.
    pub fn len(&self) -> usize {
        self.set.len()
    }
}

impl FromIterator<(usize, usize)> for Selection {
    fn from_iter<I: IntoIterator<Item = (usize, usize)>>(iter: I) -> Self {
        Selection {
            set: iter.into_iter().collect(),
        }
    }
}

/// Rewrite one hunk's body for a partial patch (lazygit's algorithm).
///
/// Forward: unselected `+` lines are dropped, unselected `-` lines become
/// context (the line is still in the pre-image). Reverse mirrors that:
/// unselected `+` → context, unselected `-` → dropped. Converted context is
/// buffered and flushed after the selected additions of the same change
/// block — unless an unselected new-side line was dropped earlier in the
/// block, in which case it flushes first to preserve relative order.
///
/// A `\ No newline` marker survives iff its preceding body line survives in
/// any form (kept, or `-`/`+` converted to context); if the preceding line
/// is dropped, the marker is dropped with it.
fn transform_hunk_lines(
    hunk: &PatchHunk,
    hunk_idx: usize,
    sel: &Selection,
    reverse: bool,
) -> Vec<PatchLine> {
    let mut out: Vec<PatchLine> = Vec::new();
    let mut pending: Vec<PatchLine> = Vec::new();
    let mut saw_dropped_new_side = false;
    let mut prev_dropped = false;

    for (li, line) in hunk.lines.iter().enumerate() {
        match line.kind {
            LineKind::Context => {
                out.append(&mut pending);
                saw_dropped_new_side = false;
                prev_dropped = false;
                out.push(line.clone());
            }
            LineKind::NoNewlineOld | LineKind::NoNewlineNew => {
                if !prev_dropped {
                    out.append(&mut pending);
                    out.push(line.clone());
                }
            }
            LineKind::Add | LineKind::Del => {
                // The side whose unselected lines turn into context: `-` when
                // staging, `+` when building a reverse (unstage) patch.
                let old_side = (line.kind == LineKind::Del) != reverse;
                if sel.contains(hunk_idx, li) {
                    if old_side || saw_dropped_new_side {
                        out.append(&mut pending);
                    }
                    out.push(line.clone());
                    prev_dropped = false;
                } else if old_side {
                    pending.push(PatchLine {
                        kind: LineKind::Context,
                        text: line.text.clone(),
                    });
                    prev_dropped = false;
                } else {
                    saw_dropped_new_side = true;
                    prev_dropped = true;
                }
            }
        }
    }
    out.append(&mut pending);
    out
}

/// Count body lines of the given kinds.
fn count_kinds(lines: &[PatchLine], a: LineKind, b: LineKind) -> i64 {
    lines.iter().filter(|l| l.kind == a || l.kind == b).count() as i64
}

/// Build a partial patch containing only the selected changes of `file`.
/// Returns `None` when no selected Add/Del lines exist (also for binary
/// files and mode-only sections, which carry no stageable lines).
///
/// `reverse=false` → patch meant for `git apply --cached` (stage selected);
/// `reverse=true` → patch meant for `git apply [--cached] --reverse`
/// (unstage/discard selected).
///
/// Per emitted hunk, counts are recomputed (`old = context+del`,
/// `new = context+add`) and `new_start = old_start + delta` where `delta`
/// accumulates `new_count − old_count` of previously emitted hunks. When a
/// side's count is 0 git records the line *before* the change
/// (`@@ -1,2 +0,0 @@`, `@@ -3,0 +4,2 @@` — verified against `git diff`),
/// hence the ±1 adjustment when a recounted side hits or leaves zero.
pub fn transform(file: &PatchFile, sel: &Selection, reverse: bool) -> Option<String> {
    if file.kind == FileKind::Binary {
        return None;
    }
    let mut body = String::new();
    let mut emitted = false;
    let mut delta: i64 = 0;
    for (hi, hunk) in file.hunks.iter().enumerate() {
        let lines = transform_hunk_lines(hunk, hi, sel, reverse);
        let old_count = count_kinds(&lines, LineKind::Context, LineKind::Del);
        let new_count = count_kinds(&lines, LineKind::Context, LineKind::Add);
        let has_changes = lines
            .iter()
            .any(|l| matches!(l.kind, LineKind::Add | LineKind::Del));
        if has_changes {
            let adjust = if old_count == 0 {
                1
            } else if new_count == 0 {
                -1
            } else {
                0
            };
            let new_start = (i64::from(hunk.old_start) + delta + adjust).max(0) as u32;
            body.push_str(&format_hunk_header(
                hunk.old_start,
                old_count.max(0) as u32,
                new_start,
                new_count.max(0) as u32,
                &hunk.heading,
            ));
            body.push('\n');
            push_body(&mut body, &lines);
            emitted = true;
        }
        // An omitted hunk contributes 0 (all its emitted-form lines are
        // context), so accumulating unconditionally matches "delta over
        // previously emitted hunks".
        delta += new_count - old_count;
    }
    if !emitted {
        return None;
    }
    let mut out = String::new();
    for line in &file.header {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&body);
    Some(out)
}

/// Multi-file variant for custom-patch building: per-file selections keyed
/// by `new_path`. Files without a selection (or whose selection yields no
/// changes) are skipped; `None` when nothing is emitted at all.
pub fn transform_all(
    files: &[PatchFile],
    sels: &HashMap<String, Selection>,
    reverse: bool,
) -> Option<String> {
    let mut out = String::new();
    for f in files {
        if let Some(sel) = sels.get(&f.new_path)
            && let Some(t) = transform(f, sel, reverse)
        {
            out.push_str(&t);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Join lines with `\n` and terminate with a trailing newline, the way
    /// git emits diffs.
    fn d(lines: &[&str]) -> String {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    /// Assert a byte-identical parse → render round-trip.
    fn rt(diff: &str) {
        assert_eq!(render_patch(&parse_patch(diff)), diff, "round-trip");
    }

    fn sel(pairs: &[(usize, usize)]) -> Selection {
        pairs.iter().copied().collect()
    }

    // A canonical single-file modify fixture.
    fn modify_fixture() -> String {
        d(&[
            "diff --git a/src/demo.rs b/src/demo.rs",
            "index 1111111..2222222 100644",
            "--- a/src/demo.rs",
            "+++ b/src/demo.rs",
            "@@ -1,5 +1,6 @@",
            " a1",
            "-a2",
            "+A2",
            "+A2b",
            " a3",
            " a4",
            " a5",
            "@@ -10,4 +11,3 @@ fn mid() {",
            " b1",
            "-b2",
            "-b3",
            "+B2",
            " b4",
            "@@ -20,3 +20,4 @@",
            " c1",
            " c2",
            "+C3",
            " c3",
            "\\ No newline at end of file",
        ])
    }

    // ---- parse → render round-trips -------------------------------------

    #[test]
    fn rt_simple_modify_and_multi_hunk() {
        rt(&modify_fixture());
    }

    #[test]
    fn rt_multi_file() {
        rt(&d(&[
            "diff --git a/one.txt b/one.txt",
            "index 1111111..2222222 100644",
            "--- a/one.txt",
            "+++ b/one.txt",
            "@@ -1,2 +1,2 @@",
            " keep",
            "-old",
            "+new",
            "diff --git a/two.txt b/two.txt",
            "index 3333333..4444444 100644",
            "--- a/two.txt",
            "+++ b/two.txt",
            "@@ -5,3 +5,2 @@ fn ctx()",
            " x",
            "-y",
            " z",
        ]));
    }

    #[test]
    fn rt_new_and_deleted_file() {
        rt(&d(&[
            "diff --git a/fresh.txt b/fresh.txt",
            "new file mode 100644",
            "index 0000000..2222222",
            "--- /dev/null",
            "+++ b/fresh.txt",
            "@@ -0,0 +1,2 @@",
            "+hello",
            "+world",
        ]));
        rt(&d(&[
            "diff --git a/gone.txt b/gone.txt",
            "deleted file mode 100644",
            "index 2222222..0000000",
            "--- a/gone.txt",
            "+++ /dev/null",
            "@@ -1,2 +0,0 @@",
            "-hello",
            "-world",
        ]));
    }

    #[test]
    fn rt_crlf_keeps_carriage_returns() {
        let diff = d(&[
            "diff --git a/dos.txt b/dos.txt",
            "index 1111111..2222222 100644",
            "--- a/dos.txt",
            "+++ b/dos.txt",
            "@@ -1,2 +1,2 @@",
            " keep\r",
            "-old\r",
            "+new\r",
        ]);
        rt(&diff);
        let files = parse_patch(&diff);
        assert_eq!(files[0].hunks[0].lines[1].text, "old\r");
        assert!(render_patch(&files).contains("+new\r\n"));
    }

    #[test]
    fn rt_no_newline_variants() {
        // Marker on the old side only.
        rt(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,1 +1,1 @@",
            "-old",
            "\\ No newline at end of file",
            "+new",
        ]));
        // Marker on the new side only.
        rt(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,1 +1,1 @@",
            "-old",
            "+new",
            "\\ No newline at end of file",
        ]));
        // Both sides.
        rt(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,2 +1,2 @@",
            " first",
            "-old",
            "\\ No newline at end of file",
            "+new",
            "\\ No newline at end of file",
        ]));
        // After a context line (applies to both sides).
        rt(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,2 +1,2 @@",
            "-old",
            "+new",
            " last",
            "\\ No newline at end of file",
        ]));
    }

    #[test]
    fn rt_count_omitted_header() {
        // git omits a count of exactly 1: `@@ -1 +1 @@`.
        rt(&d(&[
            "diff --git a/g.txt b/g.txt",
            "index 5626abf..e438487 100644",
            "--- a/g.txt",
            "+++ b/g.txt",
            "@@ -1 +1 @@",
            "-one",
            "+uno",
        ]));
    }

    #[test]
    fn rt_binary_and_mode_only() {
        rt(&d(&[
            "diff --git a/img.png b/img.png",
            "index 1111111..2222222 100644",
            "Binary files a/img.png and b/img.png differ",
        ]));
        rt(&d(&[
            "diff --git a/blob.bin b/blob.bin",
            "index 1111111..2222222 100644",
            "GIT binary patch",
            "literal 6",
            "Nc$~{i00ssr{Qv*}0RR91",
            "",
            "literal 4",
            "Lc$~{i00aO5017m_",
            "",
        ]));
        rt(&d(&[
            "diff --git a/script.sh b/script.sh",
            "old mode 100644",
            "new mode 100755",
        ]));
    }

    #[test]
    fn rt_quoted_and_spaced_paths() {
        rt(&d(&[
            "diff --git \"a/sp\\303\\244ce.txt\" \"b/sp\\303\\244ce.txt\"",
            "index 1111111..2222222 100644",
            "--- \"a/sp\\303\\244ce.txt\"",
            "+++ \"b/sp\\303\\244ce.txt\"",
            "@@ -1,1 +1,1 @@",
            "-a",
            "+b",
        ]));
        // Paths with plain spaces get a trailing tab on ---/+++ lines.
        rt(&d(&[
            "diff --git a/x y.txt b/x y.txt",
            "index 1111111..2222222 100644",
            "--- a/x y.txt\t",
            "+++ b/x y.txt\t",
            "@@ -1,1 +1,1 @@",
            "-a",
            "+b",
        ]));
    }

    // ---- parse facts ------------------------------------------------------

    #[test]
    fn parse_kinds_and_paths() {
        let diff = d(&[
            "diff --git a/m.txt b/m.txt",
            "index 1111111..2222222 100644",
            "--- a/m.txt",
            "+++ b/m.txt",
            "@@ -1,1 +1,1 @@",
            "-a",
            "+b",
            "diff --git a/new.txt b/new.txt",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/new.txt",
            "@@ -0,0 +1,1 @@",
            "+n",
            "diff --git a/del.txt b/del.txt",
            "deleted file mode 100644",
            "--- a/del.txt",
            "+++ /dev/null",
            "@@ -1,1 +0,0 @@",
            "-g",
            "diff --git a/img.png b/img.png",
            "Binary files a/img.png and b/img.png differ",
            "diff --git a/run.sh b/run.sh",
            "old mode 100644",
            "new mode 100755",
        ]);
        let files = parse_patch(&diff);
        assert_eq!(files.len(), 5);
        assert_eq!(files[0].kind, FileKind::Modified);
        assert_eq!(
            (files[0].old_path.as_str(), files[0].new_path.as_str()),
            ("m.txt", "m.txt")
        );
        assert_eq!(files[1].kind, FileKind::Added);
        // Added: old_path mirrors the +++ side.
        assert_eq!(files[1].old_path, "new.txt");
        assert_eq!(files[2].kind, FileKind::Deleted);
        assert_eq!(files[2].new_path, "del.txt");
        assert_eq!(files[3].kind, FileKind::Binary);
        assert!(files[3].hunks.is_empty());
        assert_eq!(files[3].new_path, "img.png");
        assert_eq!(files[4].kind, FileKind::ModeOnly);
        assert_eq!(files[4].new_path, "run.sh");
    }

    #[test]
    fn parse_hunk_fields_and_count_defaults() {
        let files = parse_patch(&modify_fixture());
        let h = &files[0].hunks[1];
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (10, 4, 11, 3)
        );
        assert_eq!(h.heading, "fn mid() {");
        assert_eq!(files[0].hunks[0].heading, "");

        let files = parse_patch(&d(&["diff --git a/g b/g", "@@ -5 +7 @@", "-x", "+y"]));
        let h = &files[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (5, 1, 7, 1)
        );
        assert_eq!(h.header_line, "@@ -5 +7 @@");
    }

    #[test]
    fn parse_marker_kinds_and_line_text() {
        let files = parse_patch(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,2 +1,2 @@",
            " first",
            "-old",
            "\\ No newline at end of file",
            "+new",
            "\\ No newline at end of file",
        ]));
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!(lines[0].text, "first"); // marker char stripped
        assert_eq!(lines[2].kind, LineKind::NoNewlineOld);
        assert_eq!(lines[2].text, " No newline at end of file");
        assert_eq!(lines[4].kind, LineKind::NoNewlineNew);

        // After a context line: represented as NoNewlineOld (both sides).
        let files = parse_patch(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,1 +1,2 @@",
            "+added",
            " last",
            "\\ No newline at end of file",
        ]));
        assert_eq!(files[0].hunks[0].lines[2].kind, LineKind::NoNewlineOld);
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse_patch("").is_empty());
        assert!(parse_patch("not a diff\njust text\n").is_empty());
        // Headers without a `diff --git` opener are ignored.
        assert!(parse_patch("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n").is_empty());
        // Malformed hunk headers drop, later good hunks still parse.
        let files = parse_patch(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ broken @@",
            "-stray",
            "@@ -1,1 +1,1 @@",
            "-a",
            "+b",
            "trailing noise after the hunk",
            "more noise",
        ]));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        // Noise after hunks must not leak into the header.
        assert_eq!(files[0].header.len(), 3);
        // A junk line ends the hunk; a later header starts a fresh one.
        let files = parse_patch(&d(&[
            "diff --git a/x b/x",
            "@@ -1,1 +1,1 @@",
            "-a",
            "",
            " orphaned context",
            "@@ -9,1 +9,1 @@",
            "+b",
        ]));
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[0].hunks[0].lines.len(), 1);
        assert_eq!(files[0].hunks[1].old_start, 9);
        // `@@ -1,1 +1,1 @@ x @@` style extra parts in ranges are rejected.
        assert!(parse_hunk_header("@@ -1,1 +1,1 extra @@").is_none());
        assert!(parse_hunk_header("@@ +1,1 -1,1 @@").is_none());
        assert!(parse_hunk_header("@@ -a,b +c,d @@").is_none());
    }

    #[test]
    fn parse_path_extraction_fallbacks() {
        // Binary file with spaces in the path: only the diff --git line to go on.
        let files = parse_patch(&d(&[
            "diff --git a/pix of cat.png b/pix of cat.png",
            "Binary files a/pix of cat.png and b/pix of cat.png differ",
        ]));
        assert_eq!(files[0].new_path, "pix of cat.png");
        assert_eq!(files[0].old_path, "pix of cat.png");
        // Quoted diff --git fallback.
        let files = parse_patch(&d(&[
            "diff --git \"a/we ird\" \"b/we ird\"",
            "old mode 100644",
            "new mode 100755",
        ]));
        assert_eq!(files[0].new_path, "we ird");
        // Quoted ---/+++ paths are unquoted for lookup (no unescaping).
        let files = parse_patch(&d(&[
            "diff --git \"a/q\" \"b/q\"",
            "--- \"a/q\"",
            "+++ \"b/q\"",
            "@@ -1,1 +1,1 @@",
            "-a",
            "+b",
        ]));
        assert_eq!(files[0].new_path, "q");
        // Unbalanced midpoint falls back to the first-space split.
        let files = parse_patch(&d(&["diff --git a/ab b/ab", "old mode 100644"]));
        assert_eq!(files[0].old_path, "ab");
        assert_eq!(files[0].new_path, "ab");
    }

    #[test]
    fn render_synthesizes_header_when_raw_missing() {
        let mut files = parse_patch(&modify_fixture());
        files[0].hunks[0].header_line.clear();
        let out = render_patch(&files);
        assert!(out.contains("@@ -1,5 +1,6 @@\n"));
        // Heading re-attaches when present.
        files[0].hunks[1].header_line.clear();
        let out = render_patch(&files);
        assert!(out.contains("@@ -10,4 +11,3 @@ fn mid() {\n"));
    }

    // ---- Selection ----------------------------------------------------------

    #[test]
    fn selection_basics() {
        let mut s = Selection::default();
        assert!(s.is_empty());
        s.insert(0, 1);
        s.insert(0, 1); // idempotent
        s.insert(2, 7);
        assert_eq!(s.len(), 2);
        assert!(s.contains(0, 1) && s.contains(2, 7));
        assert!(!s.contains(1, 1));
        assert!(s.remove(0, 1));
        assert!(!s.remove(0, 1));
        assert_eq!(s.len(), 1);

        let files = parse_patch(&modify_fixture());
        let wh = Selection::whole_hunk(&files[0], 0);
        assert_eq!(wh.len(), files[0].hunks[0].lines.len());
        assert!(wh.contains(0, 0) && wh.contains(0, 6));
        assert!(!wh.contains(1, 0));
        assert!(Selection::whole_hunk(&files[0], 99).is_empty());
        let all = Selection::whole_file(&files[0]);
        let total: usize = files[0].hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(all.len(), total);
    }

    // ---- transform: forward matrix -----------------------------------------

    #[test]
    fn forward_single_add_selected() {
        let files = parse_patch(&modify_fixture());
        // Hunk 0 lines: [ctx a1, -a2, +A2, +A2b, ctx a3, ctx a4, ctx a5].
        let out = transform(&files[0], &sel(&[(0, 2)]), false).unwrap();
        // Unselected del → context, unselected add dropped; the converted
        // context flushes after the selected addition of the same block.
        let expected = d(&[
            "diff --git a/src/demo.rs b/src/demo.rs",
            "index 1111111..2222222 100644",
            "--- a/src/demo.rs",
            "+++ b/src/demo.rs",
            "@@ -1,5 +1,6 @@",
            " a1",
            "+A2",
            " a2",
            " a3",
            " a4",
            " a5",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn forward_single_del_selected() {
        let files = parse_patch(&modify_fixture());
        let out = transform(&files[0], &sel(&[(0, 1)]), false).unwrap();
        let expected = d(&[
            "diff --git a/src/demo.rs b/src/demo.rs",
            "index 1111111..2222222 100644",
            "--- a/src/demo.rs",
            "+++ b/src/demo.rs",
            "@@ -1,5 +1,4 @@",
            " a1",
            "-a2",
            " a3",
            " a4",
            " a5",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn forward_mixed_adjacent_run_ordering() {
        // Block `-b2 -b3 +B2` (hunk 1, lines 1..=3): select -b2 and +B2.
        let files = parse_patch(&modify_fixture());
        let out = transform(&files[0], &sel(&[(1, 1), (1, 3)]), false).unwrap();
        // Order: selected del, selected add, then unselected-del context.
        let expected = d(&[
            "diff --git a/src/demo.rs b/src/demo.rs",
            "index 1111111..2222222 100644",
            "--- a/src/demo.rs",
            "+++ b/src/demo.rs",
            "@@ -10,4 +10,4 @@ fn mid() {",
            " b1",
            "-b2",
            "+B2",
            " b3",
            " b4",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn forward_dropped_add_before_selected_add_flushes_context_first() {
        // Block: -a(unsel) +A(unsel) +B(sel) — the dropped +A means +B sits
        // after it positionally, so the pending ` a` context flushes first.
        let files = parse_patch(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,1 +1,2 @@",
            "-a",
            "+A",
            "+B",
        ]));
        let out = transform(&files[0], &sel(&[(0, 2)]), false).unwrap();
        let expected = d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,1 +1,2 @@",
            " a",
            "+B",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn forward_selection_spanning_two_hunks_recounts_with_delta() {
        let files = parse_patch(&modify_fixture());
        // Whole hunk 0 (delta +1) plus the del of hunk 1.
        let mut s = Selection::whole_hunk(&files[0], 0);
        s.insert(1, 1); // -b2
        let out = transform(&files[0], &s, false).unwrap();
        assert!(out.contains("@@ -1,5 +1,6 @@\n"));
        // Hunk 1: old = 2 ctx + 1 del + 1 ctx(from -b3) = 4; new = 3;
        // new_start = 10 + 1.
        assert!(out.contains("@@ -10,4 +11,3 @@ fn mid() {\n"));
        assert!(out.contains("\n-b2\n b3\n"));
        // Hunk 2 untouched → omitted.
        assert!(!out.contains("C3"));
    }

    #[test]
    fn forward_middle_hunk_omitted_matches_git_verified_offsets() {
        // Mirrors a /tmp experiment against real git: seq 1..30 with three
        // changes; staging hunks 1 and 3 yields `@@ -22,7 +23,7 @@` for the
        // third (delta +1 from hunk 1), which `git apply --cached` accepted
        // and `git diff --cached` reproduced byte-for-byte.
        let fixture = d(&[
            "diff --git a/m.txt b/m.txt",
            "index e8823e1..1434d89 100644",
            "--- a/m.txt",
            "+++ b/m.txt",
            "@@ -2,7 +2,8 @@",
            " 2",
            " 3",
            " 4",
            "-5",
            "+five",
            "+five2",
            " 6",
            " 7",
            " 8",
            "@@ -12,7 +13,7 @@",
            " 12",
            " 13",
            " 14",
            "-15",
            "+fifteen",
            " 16",
            " 17",
            " 18",
            "@@ -22,7 +23,7 @@",
            " 22",
            " 23",
            " 24",
            "-25",
            "+twentyfive",
            " 26",
            " 27",
            " 28",
        ]);
        let files = parse_patch(&fixture);
        let mut s = Selection::whole_hunk(&files[0], 0);
        for li in 0..files[0].hunks[2].lines.len() {
            s.insert(2, li);
        }
        let out = transform(&files[0], &s, false).unwrap();
        assert!(out.contains("@@ -2,7 +2,8 @@\n"));
        assert!(!out.contains("fifteen"));
        assert!(out.contains("@@ -22,7 +23,7 @@\n"));
    }

    #[test]
    fn forward_select_all_equals_original() {
        // Selecting everything reproduces the original patch byte-for-byte
        // (fixture has explicit counts and consistent starts, like real git
        // output) — applying it stages the full change.
        let fixture = modify_fixture();
        let files = parse_patch(&fixture);
        let out = transform(&files[0], &Selection::whole_file(&files[0]), false).unwrap();
        assert_eq!(out, fixture);
    }

    #[test]
    fn forward_select_none_or_context_only_is_none() {
        let files = parse_patch(&modify_fixture());
        assert_eq!(transform(&files[0], &Selection::default(), false), None);
        // Context-only selection contributes no changes.
        assert_eq!(transform(&files[0], &sel(&[(0, 0), (0, 4)]), false), None);
    }

    #[test]
    fn binary_and_mode_only_transform_to_none() {
        let files = parse_patch(&d(&[
            "diff --git a/img.png b/img.png",
            "Binary files a/img.png and b/img.png differ",
            "diff --git a/run.sh b/run.sh",
            "old mode 100644",
            "new mode 100755",
        ]));
        let everything: Selection = (0..8).map(|i| (0, i)).collect();
        assert_eq!(transform(&files[0], &everything, false), None);
        assert_eq!(transform(&files[1], &everything, false), None);
    }

    #[test]
    fn pure_add_hunk_partially_selected() {
        // New-file hunk: keep old_start 0; new side starts at 1 (+1 adjust
        // for the zero old side) — `git diff` of a 1-line new file says
        // `@@ -0,0 +1 @@`.
        let files = parse_patch(&d(&[
            "diff --git a/fresh b/fresh",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/fresh",
            "@@ -0,0 +1,3 @@",
            "+a",
            "+b",
            "+c",
        ]));
        let out = transform(&files[0], &sel(&[(0, 1)]), false).unwrap();
        assert!(out.contains("@@ -0,0 +1,1 @@\n+b\n"));
        // Mid-file insertion (`git diff -U0` verified: `@@ -3,0 +4,2 @@`).
        let files = parse_patch(&d(&[
            "diff --git a/f.txt b/f.txt",
            "--- a/f.txt",
            "+++ b/f.txt",
            "@@ -3,0 +4,2 @@ c",
            "+X",
            "+Y",
        ]));
        let out = transform(&files[0], &sel(&[(0, 0)]), false).unwrap();
        assert!(out.contains("@@ -3,0 +4,1 @@ c\n+X\n"));
    }

    #[test]
    fn transform_yielding_zero_new_count_records_line_before() {
        // `git diff -U0` verified: deleting old lines 3-4 emits
        // `@@ -3,2 +2,0 @@` — a zero count records the line BEFORE.
        let files = parse_patch(&d(&[
            "diff --git a/f.txt b/f.txt",
            "--- a/f.txt",
            "+++ b/f.txt",
            "@@ -3,2 +3,2 @@",
            "-c",
            "-d",
            "+C",
            "+D",
            "@@ -9,1 +9,2 @@",
            " i",
            "+J",
        ]));
        let out = transform(&files[0], &sel(&[(0, 0), (0, 1), (1, 1)]), false).unwrap();
        assert!(out.contains("@@ -3,2 +2,0 @@\n-c\n-d\n"));
        // Later hunk shifts by the accumulated delta (-2).
        assert!(out.contains("@@ -9,1 +7,2 @@\n i\n+J\n"));
        // Deleting from line 1 bottoms out at 0 (`@@ -1,2 +0,0 @@`).
        let files = parse_patch(&d(&[
            "diff --git a/f.txt b/f.txt",
            "--- a/f.txt",
            "+++ b/f.txt",
            "@@ -1,2 +1,1 @@",
            "-a",
            "-b",
            "+ab",
        ]));
        let out = transform(&files[0], &sel(&[(0, 0), (0, 1)]), false).unwrap();
        assert!(out.contains("@@ -1,2 +0,0 @@\n-a\n-b\n"));
    }

    // ---- transform: reverse mirrors ----------------------------------------

    #[test]
    fn reverse_unselected_add_becomes_context_unselected_del_dropped() {
        let files = parse_patch(&modify_fixture());
        // Hunk 1 block `-b2 -b3 +B2`: select only -b2 in reverse mode.
        let out = transform(&files[0], &sel(&[(1, 1)]), true).unwrap();
        let expected_hunk = [
            // old = 2 ctx + 1 ctx(from +B2) + 1 del = 4; new = 3.
            "@@ -10,4 +10,3 @@ fn mid() {",
            " b1",
            "-b2",
            " B2",
            " b4",
        ]
        .join("\n");
        assert!(out.contains(&expected_hunk), "got:\n{out}");
        assert!(!out.contains("b3"), "unselected del must drop in reverse");
    }

    #[test]
    fn reverse_single_add_selected() {
        let files = parse_patch(&modify_fixture());
        // Hunk 2: select +C3 in reverse (pure unstage of one addition).
        let out = transform(&files[0], &sel(&[(2, 2)]), true).unwrap();
        let expected = d(&[
            "diff --git a/src/demo.rs b/src/demo.rs",
            "index 1111111..2222222 100644",
            "--- a/src/demo.rs",
            "+++ b/src/demo.rs",
            "@@ -20,3 +20,4 @@",
            " c1",
            " c2",
            "+C3",
            " c3",
            "\\ No newline at end of file",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn reverse_select_all_equals_original() {
        let fixture = modify_fixture();
        let files = parse_patch(&fixture);
        let out = transform(&files[0], &Selection::whole_file(&files[0]), true).unwrap();
        assert_eq!(out, fixture);
    }

    #[test]
    fn reverse_select_none_is_none() {
        let files = parse_patch(&modify_fixture());
        assert_eq!(transform(&files[0], &Selection::default(), true), None);
    }

    #[test]
    fn reverse_pending_context_orders_after_selected_dels() {
        // Reverse mirror of the run-ordering rule: unselected adds buffer as
        // context and flush after the selected old-side (del) lines.
        let files = parse_patch(&d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,2 +1,2 @@",
            "-a",
            "-b",
            "+A",
            "+B",
        ]));
        let out = transform(&files[0], &sel(&[(0, 0)]), true).unwrap();
        let expected = d(&[
            "diff --git a/x b/x",
            "--- a/x",
            "+++ b/x",
            "@@ -1,3 +1,2 @@",
            "-a",
            " A",
            " B",
        ]);
        assert_eq!(out, expected);
    }

    // ---- no-newline transform matrix ----------------------------------------

    fn no_eol_fixture() -> String {
        // git-verified shape: `first\nold⍉` → `first\nnew⍉` (⍉ = no newline).
        d(&[
            "diff --git a/n.txt b/n.txt",
            "--- a/n.txt",
            "+++ b/n.txt",
            "@@ -1,2 +1,2 @@",
            " first",
            "-old",
            "\\ No newline at end of file",
            "+new",
            "\\ No newline at end of file",
        ])
    }

    #[test]
    fn no_eol_marker_follows_selected_del() {
        let files = parse_patch(&no_eol_fixture());
        let out = transform(&files[0], &sel(&[(0, 1)]), false).unwrap();
        // Del kept → its marker kept; unselected add dropped → its marker too.
        let expected = d(&[
            "diff --git a/n.txt b/n.txt",
            "--- a/n.txt",
            "+++ b/n.txt",
            "@@ -1,2 +1,1 @@",
            " first",
            "-old",
            "\\ No newline at end of file",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn no_eol_marker_dropped_with_unselected_add() {
        let files = parse_patch(&no_eol_fixture());
        // Forward, select only the add: the del converts to context and its
        // marker survives attached to the kept line (git-verified: this exact
        // patch is accepted by `git apply --cached`).
        let out = transform(&files[0], &sel(&[(0, 3)]), false).unwrap();
        let expected = d(&[
            "diff --git a/n.txt b/n.txt",
            "--- a/n.txt",
            "+++ b/n.txt",
            "@@ -1,2 +1,3 @@",
            " first",
            " old",
            "\\ No newline at end of file",
            "+new",
            "\\ No newline at end of file",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn no_eol_reverse_marker_dropped_with_unselected_del() {
        let files = parse_patch(&no_eol_fixture());
        // Reverse, select only the add: the unselected del drops, and so does
        // its marker (git-verified accept of this exact output).
        let out = transform(&files[0], &sel(&[(0, 3)]), true).unwrap();
        let expected = d(&[
            "diff --git a/n.txt b/n.txt",
            "--- a/n.txt",
            "+++ b/n.txt",
            "@@ -1,1 +1,2 @@",
            " first",
            "+new",
            "\\ No newline at end of file",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn no_eol_reverse_add_converted_to_context_keeps_marker() {
        let files = parse_patch(&no_eol_fixture());
        // Reverse, select only the del: the add converts to context, its
        // marker stays bound to it (git-verified accept).
        let out = transform(&files[0], &sel(&[(0, 1)]), true).unwrap();
        let expected = d(&[
            "diff --git a/n.txt b/n.txt",
            "--- a/n.txt",
            "+++ b/n.txt",
            "@@ -1,3 +1,2 @@",
            " first",
            "-old",
            "\\ No newline at end of file",
            " new",
            "\\ No newline at end of file",
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn no_eol_marker_after_context_survives_selection() {
        let files = parse_patch(&modify_fixture());
        // Hunk 2 ends ` c3` + marker; transforming with the add selected
        // keeps the context line and therefore the marker.
        let out = transform(&files[0], &sel(&[(2, 2)]), false).unwrap();
        assert!(out.ends_with(" c3\n\\ No newline at end of file\n"));
    }

    // ---- CRLF through transform ----------------------------------------------

    #[test]
    fn crlf_survives_transform() {
        let files = parse_patch(&d(&[
            "diff --git a/dos.txt b/dos.txt",
            "--- a/dos.txt",
            "+++ b/dos.txt",
            "@@ -1,2 +1,2 @@",
            " keep\r",
            "-old\r",
            "+new\r",
        ]));
        let out = transform(&files[0], &sel(&[(0, 2)]), false).unwrap();
        assert!(out.contains("+new\r\n"));
        assert!(out.contains(" old\r\n"), "converted context keeps \\r");
    }

    // ---- transform_all ----------------------------------------------------------

    #[test]
    fn transform_all_keys_by_new_path_and_concatenates() {
        let diff = d(&[
            "diff --git a/one.txt b/one.txt",
            "--- a/one.txt",
            "+++ b/one.txt",
            "@@ -1,1 +1,1 @@",
            "-a",
            "+b",
            "diff --git a/two.txt b/two.txt",
            "--- a/two.txt",
            "+++ b/two.txt",
            "@@ -1,1 +1,1 @@",
            "-x",
            "+y",
        ]);
        let files = parse_patch(&diff);
        let mut sels = HashMap::new();
        sels.insert("two.txt".to_string(), Selection::whole_hunk(&files[1], 0));
        let out = transform_all(&files, &sels, false).unwrap();
        assert!(out.contains("two.txt") && !out.contains("one.txt"));

        sels.insert("one.txt".to_string(), Selection::whole_hunk(&files[0], 0));
        let out = transform_all(&files, &sels, false).unwrap();
        // File order follows `files`, not the map.
        let one = out.find("one.txt").unwrap();
        let two = out.find("two.txt").unwrap();
        assert!(one < two);
        // Concatenation parses back as two files.
        assert_eq!(parse_patch(&out).len(), 2);

        assert_eq!(transform_all(&files, &HashMap::new(), false), None);
        // Selections that yield nothing → None.
        let mut empty = HashMap::new();
        empty.insert("one.txt".to_string(), Selection::default());
        assert_eq!(transform_all(&files, &empty, true), None);
    }

    // ---- property-style: seeded LCG over random selections ---------------------

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn coin(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    /// Header counts of every emitted hunk must equal the actual line tallies.
    fn assert_counts_consistent(patch_text: &str) {
        let reparsed = parse_patch(patch_text);
        assert_eq!(reparsed.len(), 1);
        for h in &reparsed[0].hunks {
            let dels = h.lines.iter().filter(|l| l.kind == LineKind::Del).count() as u32;
            let adds = h.lines.iter().filter(|l| l.kind == LineKind::Add).count() as u32;
            let ctx = h
                .lines
                .iter()
                .filter(|l| l.kind == LineKind::Context)
                .count() as u32;
            assert_eq!(h.old_count, ctx + dels, "old count in {}", h.header_line);
            assert_eq!(h.new_count, ctx + adds, "new count in {}", h.header_line);
            assert!(adds + dels > 0, "emitted hunk must contain changes");
        }
    }

    #[test]
    fn property_random_selections_keep_invariants() {
        let fixture = modify_fixture();
        let files = parse_patch(&fixture);
        let file = &files[0];
        // (c) reparse fixpoint on the fixture itself.
        assert_eq!(parse_patch(&render_patch(&files)), files);

        let mut rng = Lcg(0x5eed_cafe_f00d_d1ce);
        for round in 0..200 {
            let reverse = round % 2 == 1;
            let mut s = Selection::default();
            for (hi, h) in file.hunks.iter().enumerate() {
                for li in 0..h.lines.len() {
                    if rng.coin() {
                        s.insert(hi, li);
                    }
                }
            }
            let Some(out) = transform(file, &s, reverse) else {
                // None only when no Add/Del line is selected.
                let any_change = file.hunks.iter().enumerate().any(|(hi, h)| {
                    h.lines.iter().enumerate().any(|(li, l)| {
                        matches!(l.kind, LineKind::Add | LineKind::Del) && s.contains(hi, li)
                    })
                });
                assert!(!any_change, "round {round}: dropped a real selection");
                continue;
            };
            // (a) header counts equal emitted line counts.
            assert_counts_consistent(&out);
            // (c) parse(render(parse(x))) == parse(x) on transformed output.
            let p1 = parse_patch(&out);
            assert_eq!(parse_patch(&render_patch(&p1)), p1, "round {round}");
        }

        // (b) select-all equals the original in both directions.
        let all = Selection::whole_file(file);
        assert_eq!(transform(file, &all, false).unwrap(), fixture);
        assert_eq!(transform(file, &all, true).unwrap(), fixture);
    }
}
