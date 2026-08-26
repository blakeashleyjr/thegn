//! Workspace search & replace — the pure model (THE-5).
//!
//! Everything a workspace-wide find/replace needs that is *not* I/O lives here,
//! substrate-free and under the 95% coverage gate: the match model, the literal/
//! regex matcher (with precise per-line spans), before/after replacement
//! rendering (regex capture expansion), the single guarded-apply *computation*
//! (drift detection + bottom-up edits), the glob include/exclude predicate, and
//! the structural-tier vocabulary (the [`StructuralSearch`] seam + a defensive
//! ast-grep JSON parser).
//!
//! The host owns the moving parts this module deliberately does not: walking the
//! worktree (the `ignore` crate), reading/writing files, spawning the off-loop
//! worker, and running the ast-grep subprocess. Those call into the pure
//! functions here so the logic that decides *what* changes — and refuses to
//! clobber a drifted file — is exhaustively unit-tested.
//!
//! Design tie-ins:
//! - **One matcher, precise spans.** The fff grep tier finds candidate lines but
//!   exposes no match span; [`Matcher::scan_line`] is the authority on the exact
//!   byte range (and captures) so a replacement edits the right bytes.
//! - **Drift skip.** Every [`Match`] snapshots a [`fnv1a_64`] hash of its line;
//!   [`apply_edits`] re-derives the line from the current file and skips (never
//!   applies) any whose snapshot no longer holds — the scooter safety rule.
//! - **Bottom-up edits.** Multiple accepted matches on one line apply
//!   right-to-left so an earlier edit never invalidates a later span's offsets.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::seam::{ErrorClass, SeamError};

// ── FNV-1a content hash (stable, dependency-free) ───────────────────────────

/// 64-bit FNV-1a. Deterministic and allocation-free — the drift snapshot only
/// has to detect *change* within a session, and a stable non-crypto hash keeps
/// `thegn-core` free of a hashing dependency.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ── Search spec + matcher ───────────────────────────────────────────────────

/// How the query is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Verbatim substring match.
    Literal,
    /// A `regex` crate pattern; capture groups expand in the replacement.
    Regex,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchMode::Literal => "literal",
            SearchMode::Regex => "regex",
        }
    }
}

/// The query half of a search: what to match and how. File selection
/// (globs/gitignore/hidden) is a separate [`WalkFilter`] applied by the walker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSpec {
    pub query: String,
    pub mode: SearchMode,
    /// `false` ⇒ ASCII case-insensitive.
    pub case_sensitive: bool,
    /// Require the match to sit on `\w` word boundaries.
    pub whole_word: bool,
}

impl Default for SearchSpec {
    fn default() -> Self {
        SearchSpec {
            query: String::new(),
            mode: SearchMode::Literal,
            case_sensitive: false,
            whole_word: false,
        }
    }
}

/// A single match on one line: the byte span within the line plus its capture
/// groups (`captures[0]` is the whole match; `1..` are regex groups, empty for a
/// group that did not participate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    pub start: usize,
    pub end: usize,
    pub captures: Vec<String>,
}

/// A compiled matcher: literal needle or a compiled regex. Built once per
/// search and reused per line (the regex compile is the expensive part).
#[derive(Debug)]
pub enum Matcher {
    Literal {
        needle: String,
        case_sensitive: bool,
        whole_word: bool,
    },
    Regex(regex::Regex),
}

/// A word character for the whole-word boundary test (ASCII `\w`).
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl Matcher {
    /// Build a matcher, or return a human-readable error (an invalid regex or an
    /// empty query) — the caller surfaces it inline and never spawns a worker.
    pub fn build(spec: &SearchSpec) -> Result<Matcher, String> {
        if spec.query.is_empty() {
            return Err("empty query".to_string());
        }
        match spec.mode {
            SearchMode::Literal => Ok(Matcher::Literal {
                needle: spec.query.clone(),
                case_sensitive: spec.case_sensitive,
                whole_word: spec.whole_word,
            }),
            SearchMode::Regex => {
                let mut pat = spec.query.clone();
                if spec.whole_word {
                    // `\b` anchors; wrap in a non-capturing group so alternations
                    // in the user pattern still bind inside the boundaries.
                    pat = format!(r"\b(?:{pat})\b");
                }
                let re = regex::RegexBuilder::new(&pat)
                    .case_insensitive(!spec.case_sensitive)
                    .build()
                    .map_err(|e| format!("invalid regex: {e}"))?;
                Ok(Matcher::Regex(re))
            }
        }
    }

    /// Whether this matcher expands `$N` capture groups in the replacement
    /// (regex only). Literal replacements are verbatim.
    pub fn expands_captures(&self) -> bool {
        matches!(self, Matcher::Regex(_))
    }

    /// All non-overlapping matches on one line, left to right.
    pub fn scan_line(&self, line: &str) -> Vec<LineMatch> {
        match self {
            Matcher::Literal {
                needle,
                case_sensitive,
                whole_word,
            } => literal_scan(line, needle, *case_sensitive, *whole_word),
            Matcher::Regex(re) => re
                .captures_iter(line)
                .filter_map(|caps| {
                    let whole = caps.get(0)?;
                    // A zero-width match can't be replaced meaningfully and would
                    // loop forever on apply — skip it.
                    if whole.start() == whole.end() {
                        return None;
                    }
                    let groups = (0..caps.len())
                        .map(|i| {
                            caps.get(i)
                                .map(|m| m.as_str().to_string())
                                .unwrap_or_default()
                        })
                        .collect();
                    Some(LineMatch {
                        start: whole.start(),
                        end: whole.end(),
                        captures: groups,
                    })
                })
                .collect(),
        }
    }
}

/// Literal substring scan honoring case + whole-word, returning byte spans.
fn literal_scan(
    line: &str,
    needle: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<LineMatch> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let hay_bytes = line.as_bytes();
    let (hay_cmp, needle_cmp): (Vec<u8>, Vec<u8>) = if case_sensitive {
        (hay_bytes.to_vec(), needle.as_bytes().to_vec())
    } else {
        (
            hay_bytes.iter().map(|b| b.to_ascii_lowercase()).collect(),
            needle.bytes().map(|b| b.to_ascii_lowercase()).collect(),
        )
    };
    let nlen = needle_cmp.len();
    let mut i = 0usize;
    while i + nlen <= hay_cmp.len() {
        if hay_cmp[i..i + nlen] == needle_cmp[..] {
            let start = i;
            let end = i + nlen;
            // The comparison is byte-wise; a match on a UTF-8 boundary is only
            // valid if both ends land on char boundaries of the original line.
            let on_boundary = line.is_char_boundary(start) && line.is_char_boundary(end);
            let word_ok = !whole_word || {
                let before_ok = start == 0 || !is_word_byte(hay_bytes[start - 1]);
                let after_ok = end >= hay_bytes.len() || !is_word_byte(hay_bytes[end]);
                before_ok && after_ok
            };
            if on_boundary && word_ok {
                out.push(LineMatch {
                    start,
                    end,
                    captures: vec![line[start..end].to_string()],
                });
                i = end; // non-overlapping
                continue;
            }
        }
        i += 1;
    }
    out
}

// ── Match model ─────────────────────────────────────────────────────────────

/// One workspace-search hit, snapshotted so a later apply can detect drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Worktree-relative, forward-slashed path.
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// Byte span of the match *within the line* (`line_text`).
    pub byte_start: usize,
    pub byte_end: usize,
    /// The full line the match sits on (excludes the `\n`; the "before" preview).
    pub line_text: String,
    /// [`fnv1a_64`] of `line_text` at scan time — the drift snapshot.
    pub content_hash: u64,
    /// Capture groups (`[0]` = whole match). Drives regex replacement rendering.
    pub captures: Vec<String>,
}

/// Split content into line segments exactly the way [`apply_edits`] rejoins
/// them: `split('\n')`, so a trailing `\n` yields a final empty segment and a
/// join with `\n` reconstructs the bytes verbatim (CRLF, trailing-newline, and
/// no-trailing-newline all preserved). Each segment excludes the `\n` but keeps
/// any trailing `\r`, so byte offsets map back to the file exactly.
pub fn split_lines(content: &str) -> impl Iterator<Item = &str> {
    content.split('\n')
}

/// Scan one file's content for matches, up to `limit` (0 = unbounded). Pure:
/// the host reads the bytes and passes them in.
pub fn scan_content(path: &str, content: &str, matcher: &Matcher, limit: usize) -> Vec<Match> {
    let mut out = Vec::new();
    for (idx, seg) in split_lines(content).enumerate() {
        let hash = fnv1a_64(seg.as_bytes());
        for lm in matcher.scan_line(seg) {
            out.push(Match {
                path: path.to_string(),
                line: idx + 1,
                byte_start: lm.start,
                byte_end: lm.end,
                line_text: seg.to_string(),
                content_hash: hash,
                captures: lm.captures,
            });
            if limit != 0 && out.len() >= limit {
                return out;
            }
        }
    }
    out
}

// ── Replacement rendering ───────────────────────────────────────────────────

/// Expand a replacement template against capture groups. In [`SearchMode::Regex`]
/// `$0`/`$1`/`${12}` expand to the corresponding group (out-of-range ⇒ empty),
/// `$$` is a literal `$`; in [`SearchMode::Literal`] the template is verbatim.
pub fn render_replacement(captures: &[String], template: &str, mode: SearchMode) -> String {
    if mode == SearchMode::Literal {
        return template.to_string();
    }
    expand_template(template, captures)
}

/// `$N` / `${N}` / `$$` expansion over positional captures.
fn expand_template(template: &str, captures: &[String]) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            // Copy one UTF-8 char.
            let ch_len = utf8_len(bytes[i]);
            let end = (i + ch_len).min(bytes.len());
            out.push_str(&template[i..end]);
            i = end;
            continue;
        }
        // At a `$`.
        if i + 1 >= bytes.len() {
            out.push('$');
            i += 1;
            continue;
        }
        let next = bytes[i + 1];
        if next == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if next == b'{' {
            // `${N}`
            if let Some(close) = template[i + 2..].find('}') {
                let inner = &template[i + 2..i + 2 + close];
                if let Ok(n) = inner.trim().parse::<usize>() {
                    if let Some(g) = captures.get(n) {
                        out.push_str(g);
                    }
                    i = i + 2 + close + 1;
                    continue;
                }
            }
            // Not a valid `${N}` — emit the `$` literally.
            out.push('$');
            i += 1;
            continue;
        }
        if next.is_ascii_digit() {
            // `$N` — greedily consume digits.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if let Ok(n) = template[i + 1..j].parse::<usize>()
                && let Some(g) = captures.get(n)
            {
                out.push_str(g);
            }
            i = j;
            continue;
        }
        // A lone `$` before a non-special char — literal.
        out.push('$');
        i += 1;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// The line as it would read *after* replacing one match's span — the "after"
/// half of the before/after preview. No filesystem touch.
pub fn render_after_line(m: &Match, template: &str, mode: SearchMode) -> String {
    let repl = render_replacement(&m.captures, template, mode);
    let mut s = String::with_capacity(m.line_text.len());
    s.push_str(&m.line_text[..m.byte_start]);
    s.push_str(&repl);
    s.push_str(&m.line_text[m.byte_end..]);
    s
}

// ── The single guarded-apply computation ────────────────────────────────────

/// One edit to perform, derived from an accepted [`Match`] + its rendered
/// replacement. The `content_hash` is the line's drift snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub content_hash: u64,
    pub replacement: String,
}

impl Edit {
    /// Build the edit for an accepted match under a replacement template.
    pub fn from_match(m: &Match, template: &str, mode: SearchMode) -> Edit {
        Edit {
            line: m.line,
            byte_start: m.byte_start,
            byte_end: m.byte_end,
            content_hash: m.content_hash,
            replacement: render_replacement(&m.captures, template, mode),
        }
    }
}

/// The result of applying edits to one file's content in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedContent {
    pub content: String,
    pub applied: usize,
    /// Edits skipped because their line drifted since the scan.
    pub skipped_drift: usize,
}

/// Apply `edits` to `original`, purely. Per line: re-derive the current line,
/// verify every edit's drift snapshot still holds (a drifted line's edits are
/// **skipped**, never applied against changed content), then apply that line's
/// accepted edits bottom-up (right to left) so no edit invalidates another's
/// offsets. Reconstruction is byte-exact for unedited lines.
pub fn apply_edits(original: &str, edits: &[Edit]) -> AppliedContent {
    let mut lines: Vec<String> = split_lines(original).map(str::to_string).collect();

    // Group edits by line.
    let mut by_line: std::collections::BTreeMap<usize, Vec<&Edit>> =
        std::collections::BTreeMap::new();
    for e in edits {
        by_line.entry(e.line).or_default().push(e);
    }

    let mut applied = 0usize;
    let mut skipped = 0usize;
    for (line, mut group) in by_line {
        let Some(seg) = lines.get(line.saturating_sub(1)).cloned() else {
            // Line no longer exists (file shrank) — treat as drift.
            skipped += group.len();
            continue;
        };
        let cur_hash = fnv1a_64(seg.as_bytes());
        if group.iter().any(|e| e.content_hash != cur_hash) {
            // The line changed since the scan — skip *all* its edits.
            skipped += group.len();
            continue;
        }
        // Apply right-to-left; drop any edit whose span is out of range or
        // overlaps one already applied (defensive — scan spans never overlap).
        group.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
        let mut new_seg = seg;
        let mut last_start = new_seg.len() + 1;
        for e in group {
            if e.byte_end > new_seg.len()
                || e.byte_start > e.byte_end
                || e.byte_end > last_start
                || !new_seg.is_char_boundary(e.byte_start)
                || !new_seg.is_char_boundary(e.byte_end)
            {
                skipped += 1;
                continue;
            }
            new_seg.replace_range(e.byte_start..e.byte_end, &e.replacement);
            last_start = e.byte_start;
            applied += 1;
        }
        lines[line - 1] = new_seg;
    }

    AppliedContent {
        content: lines.join("\n"),
        applied,
        skipped_drift: skipped,
    }
}

// ── Structural span edits (file-absolute, multi-line capable) ───────────────

/// A file-absolute byte-span edit — the shape the structural (ast-grep) tier
/// produces, since an AST match can cross newlines. Its drift snapshot hashes
/// the exact span bytes rather than a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    /// [`fnv1a_64`] of `original[byte_start..byte_end]` at scan time.
    pub content_hash: u64,
    pub replacement: String,
}

impl SpanEdit {
    /// Build a span edit from a structural match with a computed replacement,
    /// snapshotting the current span bytes for drift detection. Returns `None`
    /// when the match carries no replacement or the span is out of range.
    pub fn from_structural(original: &str, m: &StructuralMatch) -> Option<SpanEdit> {
        let repl = m.replacement.clone()?;
        if m.byte_end > original.len() || m.byte_start > m.byte_end {
            return None;
        }
        if !original.is_char_boundary(m.byte_start) || !original.is_char_boundary(m.byte_end) {
            return None;
        }
        Some(SpanEdit {
            byte_start: m.byte_start,
            byte_end: m.byte_end,
            content_hash: fnv1a_64(&original.as_bytes()[m.byte_start..m.byte_end]),
            replacement: repl,
        })
    }
}

/// Apply file-absolute span edits, purely, with the same guarantees as
/// [`apply_edits`]: each span's drift snapshot is verified against the *current*
/// bytes (a drifted span is skipped), and edits apply bottom-up (highest offset
/// first) so no edit invalidates another's offsets.
pub fn apply_span_edits(original: &str, edits: &[SpanEdit]) -> AppliedContent {
    let mut sorted: Vec<&SpanEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
    let mut content = original.to_string();
    let mut applied = 0usize;
    let mut skipped = 0usize;
    let mut last_start = content.len() + 1;
    for e in sorted {
        if e.byte_end > content.len()
            || e.byte_start > e.byte_end
            || e.byte_end > last_start
            || !content.is_char_boundary(e.byte_start)
            || !content.is_char_boundary(e.byte_end)
        {
            skipped += 1;
            continue;
        }
        // Bottom-up ⇒ the bytes at [start,end) are still the originals.
        if fnv1a_64(&content.as_bytes()[e.byte_start..e.byte_end]) != e.content_hash {
            skipped += 1;
            continue;
        }
        content.replace_range(e.byte_start..e.byte_end, &e.replacement);
        last_start = e.byte_start;
        applied += 1;
    }
    AppliedContent {
        content,
        applied,
        skipped_drift: skipped,
    }
}

// ── Apply report (surfaced to the overlay / CLI / status) ────────────────────

/// Per-file outcome of an apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileApplyResult {
    pub path: String,
    pub applied: usize,
    pub skipped_drift: usize,
    /// Set when the file could not be written (read-only, permission denied);
    /// the batch continues past it.
    pub error: Option<String>,
}

/// The whole-batch summary. Never silently swallowed — surfaced by the overlay
/// status line and the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub files: Vec<FileApplyResult>,
}

impl ApplyReport {
    pub fn push(&mut self, r: FileApplyResult) {
        self.files.push(r);
    }
    pub fn total_applied(&self) -> usize {
        self.files.iter().map(|f| f.applied).sum()
    }
    pub fn total_skipped(&self) -> usize {
        self.files.iter().map(|f| f.skipped_drift).sum()
    }
    pub fn files_changed(&self) -> usize {
        self.files.iter().filter(|f| f.applied > 0).count()
    }
    pub fn files_failed(&self) -> usize {
        self.files.iter().filter(|f| f.error.is_some()).count()
    }
    pub fn files_drifted(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.skipped_drift > 0 && f.error.is_none())
            .count()
    }
    /// A one-line human summary for `model.status` / the CLI.
    pub fn summary_line(&self) -> String {
        let mut s = format!(
            "{} replacement(s) in {} file(s)",
            self.total_applied(),
            self.files_changed()
        );
        if self.total_skipped() > 0 {
            s.push_str(&format!("; {} skipped (drifted)", self.total_skipped()));
        }
        if self.files_failed() > 0 {
            s.push_str(&format!("; {} failed", self.files_failed()));
        }
        s
    }
}

// ── File selection (glob include/exclude) ───────────────────────────────────

/// The file-selection half of a search, applied by the walker. `include_globs`
/// (if non-empty) is a whitelist; `exclude_globs` always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkFilter {
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub respect_gitignore: bool,
    pub include_hidden: bool,
}

impl WalkFilter {
    /// Whether a worktree-relative, forward-slashed path passes the glob filters.
    /// Exclusions win; a non-empty include set is a whitelist.
    pub fn path_selected(&self, rel: &str) -> bool {
        if self.exclude_globs.iter().any(|g| glob_match(g, rel)) {
            return false;
        }
        if self.include_globs.is_empty() {
            return true;
        }
        self.include_globs.iter().any(|g| glob_match(g, rel))
    }
}

/// Glob match supporting `*` (any run within a path segment), `**` (any run
/// across segments), `?` (one non-`/` char), and a leading-segment convenience:
/// a pattern with no `/` matches against the basename too (so `*.rs` matches
/// `src/a.rs`). Pure and unit-tested.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    if glob_match_inner(pattern.as_bytes(), path.as_bytes()) {
        return true;
    }
    // Convenience: a slash-free pattern also tries the basename.
    if !pattern.as_bytes().contains(&b'/') {
        let base = path.rsplit('/').next().unwrap_or(path);
        return glob_match_inner(pattern.as_bytes(), base.as_bytes());
    }
    false
}

fn glob_match_inner(pat: &[u8], txt: &[u8]) -> bool {
    // Recursive glob: correct for two star groups separated by a `/` (e.g.
    // `src/**/*.rs`), which single-slot backtracking gets wrong.
    let mut pi = 0;
    let mut ti = 0;
    while pi < pat.len() {
        match pat[pi] {
            b'*' => {
                let double = pi + 1 < pat.len() && pat[pi + 1] == b'*';
                // Collapse a run of `*`.
                let mut after = pi + 1;
                while after < pat.len() && pat[after] == b'*' {
                    after += 1;
                }
                if double {
                    // `**` matches any run, including `/`. Also allow a following
                    // `**/` to match *zero* segments (so `a/**/b` matches `a/b`).
                    let tail = &pat[after..];
                    if tail.first() == Some(&b'/')
                        && glob_match_inner(&pat[after + 1..], &txt[ti..])
                    {
                        return true;
                    }
                    for k in ti..=txt.len() {
                        if glob_match_inner(tail, &txt[k..]) {
                            return true;
                        }
                    }
                    return false;
                }
                // Single `*`: any run *within* one path segment (no `/`).
                let tail = &pat[after..];
                for k in ti..=txt.len() {
                    if glob_match_inner(tail, &txt[k..]) {
                        return true;
                    }
                    if k < txt.len() && txt[k] == b'/' {
                        break;
                    }
                }
                return false;
            }
            b'?' => {
                if ti >= txt.len() || txt[ti] == b'/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            c => {
                if ti >= txt.len() || txt[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == txt.len()
}

// ── Structural (AST) tier: the seam vocabulary ──────────────────────────────

/// What a structural provider can do. An operation exists iff its cap is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StructuralCaps {
    pub search: bool,
    pub rewrite: bool,
}

/// The structural query: an AST pattern in `lang`, optionally with a rewrite
/// template. The rewrite is *computed* by the provider and applied by thegn's
/// guarded write path — never by the provider itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralSpec {
    pub pattern: String,
    /// Explicit language (`rust`, `ts`, …). Empty ⇒ the provider's own
    /// extension-map default.
    pub lang: String,
    /// A rewrite template, or `None` for search-only.
    pub rewrite: Option<String>,
}

/// One structural match, in the same neutral shape the textual tier uses, plus
/// the provider-computed `replacement` when the spec carried a rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralMatch {
    pub path: String,
    pub line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
    pub replacement: Option<String>,
}

/// The error a structural provider raises, classified for the degradation
/// ladder and `thegn doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralError {
    pub class: ErrorClass,
    pub message: String,
}

impl StructuralError {
    pub fn not_installed(bin: &str) -> Self {
        StructuralError {
            class: ErrorClass::NotInstalled,
            message: format!("`{bin}` is not installed"),
        }
    }
    pub fn other(msg: impl Into<String>) -> Self {
        StructuralError {
            class: ErrorClass::Other,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for StructuralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for StructuralError {}
impl SeamError for StructuralError {
    fn class(&self) -> ErrorClass {
        self.class
    }
    fn unsupported(op: &'static str) -> Self {
        StructuralError {
            class: ErrorClass::Unsupported,
            message: format!("structural provider does not support {op}"),
        }
    }
}

/// The structural-search provider seam. Object-safe (sync — subprocess-bound);
/// the ast-grep implementation lives in the host and invokes the vendor CLI
/// argv-only. `search`/`rewrite` default to `unsupported` so a provider only
/// implements what its caps advertise.
pub trait StructuralSearch: Send + Sync {
    /// The provider id (`"ast-grep"`).
    fn id(&self) -> &'static str;
    /// Which operations this provider offers.
    fn caps(&self) -> StructuralCaps;
    /// AST-pattern search over `root`.
    fn search(
        &self,
        _root: &Path,
        _spec: &StructuralSpec,
    ) -> Result<Vec<StructuralMatch>, StructuralError> {
        Err(StructuralError::unsupported("search"))
    }
    /// Compute matches *with* replacement text for a rewrite spec. The provider
    /// MUST NOT write files — the guarded apply path performs every write.
    fn rewrite(
        &self,
        _root: &Path,
        _spec: &StructuralSpec,
    ) -> Result<Vec<StructuralMatch>, StructuralError> {
        Err(StructuralError::unsupported("rewrite"))
    }
}

// ── ast-grep JSON parsing (defensive, bounded) ──────────────────────────────

/// The subset of ast-grep's `--json` object we consume. Defensive: every field
/// is optional/defaulted so a schema drift degrades to a skipped entry, never a
/// crash. ast-grep emits an array of these.
#[derive(Debug, Deserialize)]
struct SgRaw {
    #[serde(default)]
    file: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    replacement: Option<String>,
    #[serde(default)]
    range: Option<SgRange>,
}

#[derive(Debug, Deserialize)]
struct SgRange {
    #[serde(rename = "byteOffset", default)]
    byte_offset: Option<SgByteOffset>,
    #[serde(default)]
    start: Option<SgPos>,
}

#[derive(Debug, Deserialize)]
struct SgByteOffset {
    #[serde(default)]
    start: usize,
    #[serde(default)]
    end: usize,
}

#[derive(Debug, Deserialize)]
struct SgPos {
    #[serde(default)]
    line: usize,
}

/// Parse ast-grep `--json` output into neutral [`StructuralMatch`]es, bounded at
/// `limit` (0 = unbounded). A malformed document is a recoverable error, never a
/// panic; individual entries missing a byte range are skipped.
pub fn parse_ast_grep_json(bytes: &[u8], limit: usize) -> Result<Vec<StructuralMatch>, String> {
    let raws: Vec<SgRaw> =
        serde_json::from_slice(bytes).map_err(|e| format!("ast-grep JSON parse failed: {e}"))?;
    let mut out = Vec::new();
    for r in raws {
        let Some(range) = r.range else { continue };
        let Some(bo) = range.byte_offset else {
            continue;
        };
        if bo.end < bo.start {
            continue;
        }
        // ast-grep reports 0-based lines; present 1-based to match the textual
        // tier. A missing position defaults to line 1.
        let line = range.start.map(|p| p.line + 1).unwrap_or(1);
        out.push(StructuralMatch {
            path: r.file,
            line,
            byte_start: bo.start,
            byte_end: bo.end,
            text: r.text,
            replacement: r.replacement,
        });
        if limit != 0 && out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(query: &str) -> SearchSpec {
        SearchSpec {
            query: query.into(),
            mode: SearchMode::Literal,
            case_sensitive: false,
            whole_word: false,
        }
    }
    fn rx(query: &str) -> SearchSpec {
        SearchSpec {
            query: query.into(),
            mode: SearchMode::Regex,
            case_sensitive: true,
            whole_word: false,
        }
    }

    // ── fnv ────────────────────────────────────────────────────────────────
    #[test]
    fn fnv_is_stable_and_distinguishes() {
        assert_eq!(fnv1a_64(b"abc"), fnv1a_64(b"abc"));
        assert_ne!(fnv1a_64(b"abc"), fnv1a_64(b"abd"));
        assert_ne!(fnv1a_64(b""), fnv1a_64(b"a"));
    }

    // ── literal matcher ─────────────────────────────────────────────────────
    #[test]
    fn literal_case_insensitive_by_default() {
        let m = Matcher::build(&lit("foo")).unwrap();
        let hits = m.scan_line("Foo foO xfoox");
        assert_eq!(hits.len(), 3);
        assert_eq!((hits[0].start, hits[0].end), (0, 3));
        assert_eq!(hits[2].captures, vec!["foo".to_string()]);
    }

    #[test]
    fn literal_case_sensitive() {
        let mut s = lit("Foo");
        s.case_sensitive = true;
        let m = Matcher::build(&s).unwrap();
        assert_eq!(m.scan_line("Foo foo FOO").len(), 1);
    }

    #[test]
    fn literal_whole_word() {
        let mut s = lit("cat");
        s.whole_word = true;
        let m = Matcher::build(&s).unwrap();
        let hits = m.scan_line("cat category scatter cat.");
        // "cat" and "cat." (the trailing dot is a boundary) match; the interior
        // "cat" of "category"/"scatter" does not.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].start, 0);
    }

    #[test]
    fn literal_non_overlapping() {
        let m = Matcher::build(&lit("aa")).unwrap();
        let hits = m.scan_line("aaaa");
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].start, hits[1].start), (0, 2));
    }

    #[test]
    fn literal_respects_utf8_boundaries() {
        // "é" is two bytes; a needle must not match across a char boundary.
        let m = Matcher::build(&lit("\u{e9}")).unwrap();
        let hits = m.scan_line("caf\u{e9} test");
        assert_eq!(hits.len(), 1);
        assert!("caf\u{e9} test".is_char_boundary(hits[0].start));
    }

    // ── regex matcher ───────────────────────────────────────────────────────
    #[test]
    fn regex_captures_recorded() {
        let m = Matcher::build(&rx(r"fn (\w+)")).unwrap();
        let hits = m.scan_line("fn alpha fn beta");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].captures[0], "fn alpha");
        assert_eq!(hits[0].captures[1], "alpha");
        assert_eq!(hits[1].captures[1], "beta");
    }

    #[test]
    fn regex_invalid_is_error_not_panic() {
        let err = Matcher::build(&rx("fn (")).unwrap_err();
        assert!(err.contains("invalid regex"), "{err}");
    }

    #[test]
    fn empty_query_is_error() {
        assert!(Matcher::build(&lit("")).is_err());
    }

    #[test]
    fn regex_zero_width_match_is_skipped() {
        let m = Matcher::build(&rx("x*")).unwrap();
        // "x*" can match empty; only the real "xx" run should be reported.
        let hits = m.scan_line("axxb");
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].end), (1, 3));
    }

    #[test]
    fn regex_whole_word_wraps_boundaries() {
        let mut s = rx("cat");
        s.whole_word = true;
        let m = Matcher::build(&s).unwrap();
        assert_eq!(m.scan_line("cat category").len(), 1);
    }

    // ── replacement rendering ───────────────────────────────────────────────
    #[test]
    fn capture_expansion() {
        let caps = vec!["fn alpha".to_string(), "alpha".to_string()];
        assert_eq!(
            render_replacement(&caps, "fn $1_v2", SearchMode::Regex),
            "fn alpha_v2"
        );
        assert_eq!(
            render_replacement(&caps, "${1}X", SearchMode::Regex),
            "alphaX"
        );
        assert_eq!(
            render_replacement(&caps, "$0!", SearchMode::Regex),
            "fn alpha!"
        );
        // Out-of-range group ⇒ empty.
        assert_eq!(render_replacement(&caps, "a$9b", SearchMode::Regex), "ab");
        // `$$` is a literal dollar.
        assert_eq!(render_replacement(&caps, "$$1", SearchMode::Regex), "$1");
    }

    #[test]
    fn literal_replacement_is_verbatim() {
        let caps = vec!["foo".to_string()];
        assert_eq!(
            render_replacement(&caps, "bar $1 baz", SearchMode::Literal),
            "bar $1 baz"
        );
    }

    #[test]
    fn after_line_preview() {
        let m = Match {
            path: "a.rs".into(),
            line: 1,
            byte_start: 0,
            byte_end: 8,
            line_text: "fn alpha() {}".into(),
            content_hash: 0,
            captures: vec!["fn alpha".into(), "alpha".into()],
        };
        assert_eq!(
            render_after_line(&m, "fn $1_v2", SearchMode::Regex),
            "fn alpha_v2() {}"
        );
    }

    #[test]
    fn expand_preserves_unicode_literals() {
        let caps = vec!["x".to_string()];
        assert_eq!(
            render_replacement(&caps, "café $0", SearchMode::Regex),
            "café x"
        );
    }

    // ── scan_content ────────────────────────────────────────────────────────
    #[test]
    fn scan_content_lines_and_hash() {
        let content = "foo\nbar foo\nbaz\n";
        let m = Matcher::build(&lit("foo")).unwrap();
        let hits = scan_content("f.txt", content, &m, 0);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[1].line, 2);
        assert_eq!(hits[1].byte_start, 4);
        assert_eq!(hits[0].content_hash, fnv1a_64(b"foo"));
    }

    #[test]
    fn scan_content_limit_stops_early() {
        let content = "x x x x x";
        let m = Matcher::build(&lit("x")).unwrap();
        assert_eq!(scan_content("f", content, &m, 2).len(), 2);
    }

    // ── apply: bottom-up, drift, byte-identity ──────────────────────────────
    fn edit(line: usize, s: usize, e: usize, hash: u64, r: &str) -> Edit {
        Edit {
            line,
            byte_start: s,
            byte_end: e,
            content_hash: hash,
            replacement: r.into(),
        }
    }

    #[test]
    fn apply_bottom_up_same_line() {
        let original = "foo and foo\n";
        let h = fnv1a_64(b"foo and foo");
        let edits = vec![edit(1, 0, 3, h, "BAR"), edit(1, 8, 11, h, "BAZ")];
        let out = apply_edits(original, &edits);
        assert_eq!(out.content, "BAR and BAZ\n");
        assert_eq!(out.applied, 2);
        assert_eq!(out.skipped_drift, 0);
    }

    #[test]
    fn apply_skips_deselected_span_byte_identical() {
        // Two matches on the line; only the first is an edit — the second span
        // must be left byte-identical.
        let original = "foo foo";
        let h = fnv1a_64(b"foo foo");
        let edits = vec![edit(1, 0, 3, h, "BAR")];
        let out = apply_edits(original, &edits);
        assert_eq!(out.content, "BAR foo");
    }

    #[test]
    fn apply_drift_is_skipped_and_reported() {
        // The recorded hash is for old content; the line now differs.
        let original = "changed line\n";
        let stale = fnv1a_64(b"original line");
        let edits = vec![edit(1, 0, 8, stale, "X")];
        let out = apply_edits(original, &edits);
        assert_eq!(out.content, "changed line\n"); // untouched
        assert_eq!(out.applied, 0);
        assert_eq!(out.skipped_drift, 1);
    }

    #[test]
    fn apply_mixed_files_drift_isolates() {
        // Line 1 drifted, line 3 is fine — only line 3 applies.
        let original = "aaa\nbbb\nccc\n";
        let good = fnv1a_64(b"ccc");
        let stale = fnv1a_64(b"AAA");
        let edits = vec![edit(1, 0, 3, stale, "X"), edit(3, 0, 3, good, "Z")];
        let out = apply_edits(original, &edits);
        assert_eq!(out.content, "aaa\nbbb\nZ\n");
        assert_eq!(out.applied, 1);
        assert_eq!(out.skipped_drift, 1);
    }

    #[test]
    fn apply_preserves_crlf_and_no_trailing_newline() {
        let original = "a\r\nfoo";
        let h = fnv1a_64(b"foo");
        let edits = vec![edit(2, 0, 3, h, "BAR")];
        let out = apply_edits(original, &edits);
        assert_eq!(out.content, "a\r\nBAR");
    }

    #[test]
    fn apply_missing_line_is_drift() {
        let out = apply_edits("only one line", &[edit(5, 0, 1, 0, "X")]);
        assert_eq!(out.applied, 0);
        assert_eq!(out.skipped_drift, 1);
    }

    #[test]
    fn edit_from_match_regex() {
        let m = Match {
            path: "a".into(),
            line: 2,
            byte_start: 3,
            byte_end: 8,
            line_text: "fn foo()".into(),
            content_hash: 42,
            captures: vec!["foo".into(), "foo".into()],
        };
        let e = Edit::from_match(&m, "$1_v2", SearchMode::Regex);
        assert_eq!(e.replacement, "foo_v2");
        assert_eq!(e.content_hash, 42);
        assert_eq!(e.line, 2);
    }

    // ── structural span edits ───────────────────────────────────────────────
    #[test]
    fn span_edits_multiline_bottom_up() {
        let original = "let x = foo(\n  a,\n  b,\n);\n";
        // Replace the whole call (crosses newlines): bytes 8..24 = "foo(\n  a,\n  b,\n)".
        let start = original.find("foo(").unwrap();
        let end = original.find(')').unwrap() + 1;
        let h = fnv1a_64(original[start..end].as_bytes());
        let out = apply_span_edits(
            original,
            &[SpanEdit {
                byte_start: start,
                byte_end: end,
                content_hash: h,
                replacement: "bar(a, b)".into(),
            }],
        );
        assert_eq!(out.content, "let x = bar(a, b);\n");
        assert_eq!(out.applied, 1);
    }

    #[test]
    fn span_edit_drift_skipped() {
        let original = "abcdef";
        let out = apply_span_edits(
            original,
            &[SpanEdit {
                byte_start: 0,
                byte_end: 3,
                content_hash: fnv1a_64(b"XYZ"), // wrong snapshot
                replacement: "Q".into(),
            }],
        );
        assert_eq!(out.content, "abcdef");
        assert_eq!(out.applied, 0);
        assert_eq!(out.skipped_drift, 1);
    }

    #[test]
    fn span_edit_from_structural() {
        let original = "hello world";
        let m = StructuralMatch {
            path: "a".into(),
            line: 1,
            byte_start: 0,
            byte_end: 5,
            text: "hello".into(),
            replacement: Some("hi".into()),
        };
        let e = SpanEdit::from_structural(original, &m).unwrap();
        assert_eq!(e.replacement, "hi");
        assert_eq!(e.content_hash, fnv1a_64(b"hello"));
        // No replacement ⇒ None.
        let m2 = StructuralMatch {
            replacement: None,
            ..m.clone()
        };
        assert!(SpanEdit::from_structural(original, &m2).is_none());
        // Out-of-range span ⇒ None.
        let m3 = StructuralMatch { byte_end: 999, ..m };
        assert!(SpanEdit::from_structural(original, &m3).is_none());
    }

    // ── apply report ────────────────────────────────────────────────────────
    #[test]
    fn apply_report_totals_and_summary() {
        let mut r = ApplyReport::default();
        r.push(FileApplyResult {
            path: "a.rs".into(),
            applied: 2,
            skipped_drift: 1,
            error: None,
        });
        r.push(FileApplyResult {
            path: "b.rs".into(),
            applied: 0,
            skipped_drift: 0,
            error: Some("read-only".into()),
        });
        assert_eq!(r.total_applied(), 2);
        assert_eq!(r.total_skipped(), 1);
        assert_eq!(r.files_changed(), 1);
        assert_eq!(r.files_failed(), 1);
        assert_eq!(r.files_drifted(), 1);
        let s = r.summary_line();
        assert!(s.contains("2 replacement"));
        assert!(s.contains("drifted"));
        assert!(s.contains("failed"));
    }

    // ── glob include/exclude ────────────────────────────────────────────────
    #[test]
    fn glob_basename_and_segments() {
        assert!(glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/deep/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/main.rs")); // ** matches zero dirs
        assert!(glob_match("**/main.rs", "a/b/main.rs"));
        assert!(glob_match("a/**/b", "a/b")); // ** zero segments
        assert!(glob_match("a/**/b", "a/x/y/b"));
        assert!(!glob_match("a/**/b", "a/x/y/c"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        assert!(!glob_match("*.rs", "src/main.py"));
    }

    #[test]
    fn walk_filter_include_exclude() {
        let f = WalkFilter {
            include_globs: vec!["*.rs".into()],
            exclude_globs: vec!["**/generated/*".into()],
            respect_gitignore: true,
            include_hidden: false,
        };
        assert!(f.path_selected("src/a.rs"));
        assert!(!f.path_selected("src/a.py")); // not in include set
        assert!(!f.path_selected("src/generated/a.rs")); // excluded
        // No include set ⇒ everything not excluded.
        let f2 = WalkFilter {
            exclude_globs: vec!["*.lock".into()],
            ..WalkFilter::default()
        };
        assert!(f2.path_selected("anything.txt"));
        assert!(!f2.path_selected("Cargo.lock"));
    }

    // ── structural seam ─────────────────────────────────────────────────────
    #[test]
    fn structural_error_classifies() {
        let e = StructuralError::not_installed("ast-grep");
        assert_eq!(e.class(), ErrorClass::NotInstalled);
        assert!(e.falls_through());
        assert!(e.to_string().contains("ast-grep"));
        assert_eq!(
            StructuralError::unsupported("rewrite").class(),
            ErrorClass::Unsupported
        );
    }

    struct Dummy;
    impl StructuralSearch for Dummy {
        fn id(&self) -> &'static str {
            "dummy"
        }
        fn caps(&self) -> StructuralCaps {
            StructuralCaps {
                search: true,
                rewrite: false,
            }
        }
    }

    #[test]
    fn structural_default_ops_are_unsupported() {
        let d = Dummy;
        assert_eq!(d.id(), "dummy");
        assert!(d.caps().search && !d.caps().rewrite);
        let root = Path::new("/tmp");
        let spec = StructuralSpec {
            pattern: "x".into(),
            lang: String::new(),
            rewrite: None,
        };
        // search defaults to unsupported (Dummy didn't override), rewrite too.
        assert_eq!(
            d.search(root, &spec).unwrap_err().class(),
            ErrorClass::Unsupported
        );
        assert_eq!(
            d.rewrite(root, &spec).unwrap_err().class(),
            ErrorClass::Unsupported
        );
    }

    #[test]
    fn other_error_is_classified() {
        assert_eq!(StructuralError::other("boom").class(), ErrorClass::Other);
    }

    // ── ast-grep JSON ───────────────────────────────────────────────────────
    #[test]
    fn parse_sg_json_ok() {
        let json = br#"[
          {"file":"src/a.rs","text":"foo","replacement":"bar",
           "range":{"byteOffset":{"start":10,"end":13},"start":{"line":4,"column":0}}},
          {"file":"src/b.rs","text":"baz",
           "range":{"byteOffset":{"start":0,"end":3},"start":{"line":0,"column":0}}}
        ]"#;
        let ms = parse_ast_grep_json(json, 0).unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].path, "src/a.rs");
        assert_eq!(ms[0].byte_start, 10);
        assert_eq!(ms[0].line, 5); // 0-based → 1-based
        assert_eq!(ms[0].replacement.as_deref(), Some("bar"));
        assert_eq!(ms[1].replacement, None);
    }

    #[test]
    fn parse_sg_json_malformed_is_err() {
        assert!(parse_ast_grep_json(b"not json", 0).is_err());
        assert!(parse_ast_grep_json(b"", 0).is_err());
    }

    #[test]
    fn parse_sg_json_skips_entry_without_range() {
        let json = br#"[{"file":"a","text":"x"},
          {"file":"b","text":"y","range":{"byteOffset":{"start":0,"end":1}}}]"#;
        let ms = parse_ast_grep_json(json, 0).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].path, "b");
    }

    #[test]
    fn parse_sg_json_limit() {
        let json = br#"[
          {"file":"a","range":{"byteOffset":{"start":0,"end":1}}},
          {"file":"b","range":{"byteOffset":{"start":1,"end":2}}}
        ]"#;
        assert_eq!(parse_ast_grep_json(json, 1).unwrap().len(), 1);
    }
}
