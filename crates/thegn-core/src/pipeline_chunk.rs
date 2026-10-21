//! Chunk file-scope semantics — the pure half of the chunk-scope gate (THE-86).
//!
//! A coder's chunk file (`.thegn/pipeline/<ISSUE>/code/chunk-N.md`) may open
//! with a frontmatter block declaring the paths that chunk may touch
//! (`files:`), the sibling chunks it is allowed to share a file with
//! (`overlaps:` — the architect's blessing), and the siblings that must be
//! `done` first (`after:`). This module parses that block and decides whether
//! a new scope collides with active siblings; the host (`cmd/dispatch.rs`)
//! owns every byte of I/O and builds the refusal text from the verdict data.
//!
//! Everything here is **pure**: no I/O, no subprocess, no filesystem, no
//! tokio, and no [`crate::db::Db`] — the same doctrine
//! [`crate::pipeline_run`] states. File contents arrive as plain `&str`, and
//! sibling scopes arrive as plain data. That keeps thegn-core substrate-free
//! and its 95% line-coverage gate satisfiable, and it makes every rule below
//! a table-testable function.
//!
//! Two shapes are deliberately NOT here: the refusal wording (host) and the
//! roster query that finds siblings (host). A parse failure is an error that
//! names the offending line number, so the host's refusal can point at the
//! exact line instead of "bad frontmatter".

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

/// The scope a chunk file declares: the paths it may touch, the siblings it
/// may share a file with, and the siblings that must be done first.
///
/// All three lists are empty for a chunk file with no (or an empty)
/// frontmatter block — the gate is **opt-in**: a chunk that declares no
/// `files:` never file-conflicts with anything, exactly like a dispatch made
/// before this gate existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkScope {
    /// Exact paths or globs (`*` within a segment, `**` across segments).
    pub files: Vec<String>,
    /// Sibling chunk names whose scopes may intersect this one.
    pub overlaps: Vec<String>,
    /// Sibling chunks that must be `done` before this one dispatches.
    pub after: Vec<String>,
}

/// Why a chunk file's frontmatter could not be parsed. `line` is the 1-based
/// line number in the file — the host's refusal quotes it verbatim, so a
/// typo'd block is fixable in one glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a chunk file's frontmatter block into its [`ChunkScope`].
///
/// The block is a `---`-delimited YAML-ish header at the very top of the file:
/// the first line must be `---` (trailing whitespace tolerated) and the block
/// ends at the next `---` line. Known keys are `files`, `overlaps`, `after`;
/// each takes either `- item` lines under a bare `key:` line or an inline
/// `[a, b]` list on the key line (a bare scalar — `after: chunk-1` — is read
/// as that one item). Unknown keys are ignored, along with any `- item` lines
/// they carry (forward compatibility: the architect may annotate chunks with
/// keys thegn does not know yet). A file with no opening `---` parses as an
/// all-empty [`ChunkScope`] — never an error, because the gate is opt-in.
///
/// Errors (each naming the line):
/// - the opening `---` is never closed;
/// - an inline `[` list is not closed on its own line;
/// - a known key appears twice (a silent overwrite would quietly re-scope a
///   gate — refuse instead).
pub fn parse_frontmatter(md: &str) -> Result<ChunkScope, ParseError> {
    let line_count = md.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!md.is_empty() && !md.ends_with('\n'));
    if md.len() > 1024 * 1024 || line_count > MAX_FRONTMATTER_LINES {
        return Err(ParseError {
            line: 1,
            message: "chunk frontmatter exceeds the byte or line limit".to_string(),
        });
    }
    let mut lines = md.lines();
    match lines.next() {
        // An empty body has no lines at all, and any other first line means
        // "no frontmatter" — both are the all-empty, gate-opted-out scope.
        Some(first) if first.trim() == "---" => {}
        _ => return Ok(ChunkScope::default()),
    }
    #[derive(PartialEq, Eq, Clone, Copy)]
    enum Key {
        Files,
        Overlaps,
        After,
    }
    let mut scope = ChunkScope::default();
    // Which known keys have been declared, independent of their values.
    let mut seen = [false; 3];
    // The key the current `- item` lines belong to (`None` = none yet, or the
    // last key was unknown/ignored).
    let mut current: Option<Key> = None;
    for (i, raw) in lines.enumerate() {
        // 1-based, and the opening fence consumed line 1.
        let lineno = i + 2;
        let trimmed = raw.trim();
        if trimmed == "---" {
            return Ok(scope);
        }
        if trimmed.is_empty() {
            continue;
        }
        // A list item: `- item` (any indentation). It belongs to the key
        // above it; under an unknown key it is skipped with the key.
        if let Some(rest) = trimmed.strip_prefix('-') {
            let item = rest.trim();
            if let (Some(key), false) = (current, item.is_empty()) {
                let list = match key {
                    Key::Files => &mut scope.files,
                    Key::Overlaps => &mut scope.overlaps,
                    Key::After => &mut scope.after,
                };
                if matches!(key, Key::Files) && list.len() >= MAX_PATTERNS {
                    return Err(ParseError {
                        line: lineno,
                        message: format!("files scope exceeds {MAX_PATTERNS} patterns"),
                    });
                }
                let item = if matches!(key, Key::Files) {
                    canonicalize_file_pattern(item, lineno)?
                } else {
                    item.to_string()
                };
                list.push(item);
            }
            continue;
        }
        // A `key: value` line (or bare `key:` opening a block list). Anything
        // else ends the current block list.
        let Some((key, value)) = raw.split_once(':') else {
            current = None;
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        current = match key {
            "files" => Some(Key::Files),
            "overlaps" => Some(Key::Overlaps),
            "after" => Some(Key::After),
            _ => {
                // Unknown key: ignored, including its items.
                current = None;
                continue;
            }
        };
        // Key presence is tracked apart from the accumulated values: an empty
        // first declaration (`files: []`, bare `files:`) must still count.
        let slot = match current {
            Some(Key::Files) => 0,
            Some(Key::Overlaps) => 1,
            Some(Key::After) => 2,
            None => unreachable!(),
        };
        if std::mem::replace(&mut seen[slot], true) {
            return Err(ParseError {
                line: lineno,
                message: format!("duplicate `{key}:` key — a scope is declared once"),
            });
        }
        if value.is_empty() {
            continue; // block list: the `- item` lines that follow fill it in
        }
        if let Some(inner) = value.strip_prefix('[') {
            let Some(items) = inner.strip_suffix(']') else {
                return Err(ParseError {
                    line: lineno,
                    message: "inline list `[…]` is not closed on this line".to_string(),
                });
            };
            let list = match current {
                Some(Key::Files) => &mut scope.files,
                Some(Key::Overlaps) => &mut scope.overlaps,
                Some(Key::After) => &mut scope.after,
                None => unreachable!(),
            };
            for item in items.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if matches!(current, Some(Key::Files)) && list.len() >= MAX_PATTERNS {
                    return Err(ParseError {
                        line: lineno,
                        message: format!("files scope exceeds {MAX_PATTERNS} patterns"),
                    });
                }
                let item = if matches!(current, Some(Key::Files)) {
                    canonicalize_file_pattern(item, lineno)?
                } else {
                    item.to_string()
                };
                list.push(item);
            }
            // The inline list is complete; a later `- item` line has no key to
            // attach to.
            current = None;
        } else {
            // A bare scalar (`after: chunk-1`) is that one item.
            let list = match current {
                Some(Key::Files) => &mut scope.files,
                Some(Key::Overlaps) => &mut scope.overlaps,
                Some(Key::After) => &mut scope.after,
                None => unreachable!(),
            };
            if matches!(current, Some(Key::Files)) && list.len() >= MAX_PATTERNS {
                return Err(ParseError {
                    line: lineno,
                    message: format!("files scope exceeds {MAX_PATTERNS} patterns"),
                });
            }
            let item = if matches!(current, Some(Key::Files)) {
                canonicalize_file_pattern(value, lineno)?
            } else {
                value.to_string()
            };
            list.push(item);
            current = None;
        }
    }
    Err(ParseError {
        line: 1,
        message: "frontmatter block opens with `---` but is never closed".to_string(),
    })
}

const MAX_PATTERNS: usize = 64;
const MAX_PATTERN_BYTES: usize = 256;
const MAX_SEGMENTS: usize = 32;
const MAX_FRONTMATTER_LINES: usize = 8192;
const MAX_PAIR_MATCH_WORK: usize = 100_000;
const MAX_TOTAL_MATCH_WORK: usize = 50_000_000;
type StateSet = HashSet<(usize, usize)>;

// `files:` is a repository-relative, slash-separated ASCII grammar. One
// leading `./` is normalized; empty, dot, parent, drive, control, backslash,
// wildcard-escape and Windows device-name components are refused. Matching
// folds ASCII case for conservative conflict detection. These scopes are
// lexical; symlink resolution of repository directories is a host concern.
fn canonicalize_file_pattern(pattern: &str, line: usize) -> Result<String, ParseError> {
    canonical_repo_relative_path(pattern).ok_or_else(|| ParseError {
        line,
        message: format!("unsupported or non-canonical file pattern `{pattern}`"),
    })
}

/// Validate a repository-relative path or scope pattern and return its
/// normalized spelling. One leading `./` is removed. `*` and `**` are
/// accepted for scope patterns; callers opening a concrete file must reject
/// wildcard characters. Windows device components are rejected
/// ASCII-case-insensitively, including on Unix, to keep shared scopes portable.
pub fn canonical_repo_relative_path(path: &str) -> Option<String> {
    if path.len() > MAX_PATTERN_BYTES {
        return None;
    }
    let normalized = path.strip_prefix("./").unwrap_or(path);
    let segments: Vec<&str> = normalized.split('/').collect();
    if normalized.is_empty()
        || segments.len() > MAX_SEGMENTS
        || !normalized.is_ascii()
        || normalized.starts_with('/')
        || normalized.contains('\\')
        || normalized.contains(':')
        || normalized.bytes().any(|b| b < 0x20 || b == 0x7f)
        || segments.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || (s.contains("**") && *s != "**")
                || s.ends_with('.')
                || s.ends_with(' ')
                || s.bytes()
                    .any(|b| matches!(b, b'?' | b'<' | b'>' | b'"' | b'|'))
                || windows_reserved_component(s)
        })
    {
        return None;
    }
    Some(normalized.to_string())
}

/// Does `path` match `pattern`? `*` matches any run of characters within one
/// path segment; `**` as a whole segment matches zero or more segments
/// (across `/`); any other pattern compares literally. There is no escape
/// syntax and no `?` wildcard — a gate should be predictable.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.len() > MAX_PATTERN_BYTES
        || path.len() > MAX_PATTERN_BYTES * MAX_SEGMENTS
        || !canonical_pattern(pattern)
        || path.split('/').count() > MAX_SEGMENTS
    {
        return false;
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    let mut memo = HashMap::new();
    let mut work = 0;
    match_segments(&p, &s, &mut memo, &mut work)
}

fn match_segments(
    p: &[&str],
    s: &[&str],
    memo: &mut HashMap<(usize, usize), bool>,
    work: &mut usize,
) -> bool {
    fn go(
        p: &[&str],
        s: &[&str],
        i: usize,
        j: usize,
        memo: &mut HashMap<(usize, usize), bool>,
        work: &mut usize,
    ) -> bool {
        if let Some(result) = memo.get(&(i, j)) {
            return *result;
        }
        if *work >= MAX_PAIR_MATCH_WORK {
            return false;
        }
        *work += 1;
        let result = if i == p.len() {
            j == s.len()
        } else if p[i] == "**" {
            go(p, s, i + 1, j, memo, work) || (j < s.len() && go(p, s, i, j + 1, memo, work))
        } else {
            j < s.len() && match_segment(p[i], s[j]) && go(p, s, i + 1, j + 1, memo, work)
        };
        memo.insert((i, j), result);
        result
    }
    go(p, s, 0, 0, memo, work)
}

/// One segment: literal characters plus `*` wildcards (iterative with one
/// backtrack point, the classic single-`*` matcher — enough for `config_*.rs`).
fn match_segment(pat: &str, seg: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = seg.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            // Let the last `*` swallow one more character and retry.
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The concrete colliding `(new, sibling)` path pairs between two scopes —
/// every pair, not the first: the refusal names everything at once. Matching
/// returns an error rather than claiming disjointness if its bounded
/// automata product cannot complete. Empty `files` on either side yields no
/// pairs — the opt-in rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    OverBudget,
    UnsupportedPattern,
}

pub fn paths_overlap(a: &[String], b: &[String]) -> Result<Vec<(String, String)>, MatchError> {
    let mut work = 0usize;
    paths_overlap_with_work(a, b, &mut work)
}

fn paths_overlap_with_work(
    a: &[String],
    b: &[String],
    work: &mut usize,
) -> Result<Vec<(String, String)>, MatchError> {
    if a.len() > MAX_PATTERNS || b.len() > MAX_PATTERNS {
        return Err(MatchError::OverBudget);
    }
    let mut pairs = Vec::new();
    for pa in a {
        for pb in b {
            if pa.len() > MAX_PATTERN_BYTES || pb.len() > MAX_PATTERN_BYTES {
                return Err(MatchError::OverBudget);
            }
            if patterns_intersect(pa, pb, work)? {
                pairs.push((pa.clone(), pb.clone()));
            }
        }
    }
    Ok(pairs)
}

fn patterns_intersect(a: &str, b: &str, total_work: &mut usize) -> Result<bool, MatchError> {
    if !canonical_pattern(a) || !canonical_pattern(b) {
        return Err(MatchError::UnsupportedPattern);
    }
    let a: Vec<&str> = a.split('/').collect();
    let b: Vec<&str> = b.split('/').collect();
    let mut seen = StateSet::new();
    let mut pending = vec![(0usize, 0usize)];
    let mut pair_work = 0usize;
    while let Some((i, j)) = pending.pop() {
        if !seen.insert((i, j)) {
            continue;
        }
        charge_match_work(&mut pair_work, total_work)?;
        if i < a.len() && j < b.len() && a[i] != "**" && b[j] != "**" {
            if segment_intersects(a[i], b[j], &mut pair_work, total_work)? {
                pending.push((i + 1, j + 1));
            }
            continue;
        }
        if i == a.len() && j == b.len() {
            return Ok(true);
        }
        if i < a.len() && a[i] == "**" {
            pending.push((i + 1, j));
        }
        if j < b.len() && b[j] == "**" {
            pending.push((i, j + 1));
        }
        if i < a.len() && j < b.len() {
            if a[i] == "**" && b[j] == "**" {
                pending.push((i, j));
            } else if a[i] == "**" {
                pending.push((i, j + 1));
            } else if b[j] == "**" {
                pending.push((i + 1, j));
            }
        }
    }
    Ok(false)
}

fn canonical_pattern(p: &str) -> bool {
    canonical_repo_relative_path(p).as_deref() == Some(p)
}

fn charge_match_work(pair_work: &mut usize, total_work: &mut usize) -> Result<(), MatchError> {
    *pair_work += 1;
    *total_work += 1;
    if *pair_work > MAX_PAIR_MATCH_WORK || *total_work > MAX_TOTAL_MATCH_WORK {
        return Err(MatchError::OverBudget);
    }
    Ok(())
}

fn windows_reserved_component(segment: &str) -> bool {
    let base = segment.split('.').next().unwrap_or(segment);
    ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|name| base.eq_ignore_ascii_case(name))
        || ["COM", "LPT"].iter().any(|prefix| {
            (1..=9).any(|digit| base.eq_ignore_ascii_case(&format!("{prefix}{digit}")))
        })
}

// Product traversal of two epsilon NFAs. `*` has an epsilon edge to the next
// token and a consuming self-loop; ASCII case folding is conservative for
// repositories shared with case-insensitive filesystems.
fn segment_intersects(
    a: &str,
    b: &str,
    pair_work: &mut usize,
    total_work: &mut usize,
) -> Result<bool, MatchError> {
    let a: Vec<u8> = a.bytes().map(|c| c.to_ascii_lowercase()).collect();
    let b: Vec<u8> = b.bytes().map(|c| c.to_ascii_lowercase()).collect();
    let mut pending = vec![(0usize, 0usize)];
    let mut seen = StateSet::new();
    while let Some((i, j)) = pending.pop() {
        if !seen.insert((i, j)) {
            continue;
        }
        charge_match_work(pair_work, total_work)?;
        if i == a.len() && j == b.len() {
            return Ok(true);
        }
        if i < a.len() && a[i] == b'*' {
            pending.push((i + 1, j));
        }
        if j < b.len() && b[j] == b'*' {
            pending.push((i, j + 1));
        }
        if i < a.len() && j < b.len() {
            match (a[i] == b'*', b[j] == b'*') {
                (true, true) => pending.push((i, j)),
                (true, false) => pending.push((i, j + 1)),
                (false, true) => pending.push((i + 1, j)),
                (false, false) if a[i] == b[j] => pending.push((i + 1, j + 1)),
                _ => {}
            }
        }
    }
    Ok(false)
}

/// The `after:` entries NOT in the `done` set — the sequencing half of the
/// gate. Order-preserving and deduplicated, so a refusal names each unmet
/// chunk once, in the order the chunk file listed it.
pub fn after_unmet(after: &[String], done: &HashSet<String>) -> Vec<String> {
    let mut unmet = Vec::new();
    for name in after {
        if !done.contains(name) && !unmet.contains(name) {
            unmet.push(name.clone());
        }
    }
    unmet
}

/// One same-stage roster candidate for resolving an `after:` chunk name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCandidate {
    pub row: i64,
    pub path: String,
    pub status: crate::issue::AgentDispatchStatus,
}

/// Index of same-stage candidates for the `after:` admission gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AfterIndex {
    /// Chunk names whose governing attempt is Done or Merged.
    pub done: HashSet<String>,
    /// The governing row and status for each unambiguous chunk name.
    pub governing: HashMap<String, (i64, crate::issue::AgentDispatchStatus)>,
    /// Names shared by distinct paths, with every row/path pair in row order.
    pub ambiguous: BTreeMap<String, Vec<(i64, String)>>,
}

/// Resolve `after:` names against candidates already restricted to one stage.
/// Newest means highest roster row id. Empty and non-UTF-8 stems are ignored.
pub fn after_index(candidates: &[AfterCandidate]) -> AfterIndex {
    let mut by_name: HashMap<String, Vec<&AfterCandidate>> = HashMap::new();
    for candidate in candidates {
        let Some(name) = std::path::Path::new(&candidate.path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        by_name.entry(name.to_string()).or_default().push(candidate);
    }

    let mut index = AfterIndex::default();
    for (name, attempts) in by_name {
        let mut paths = HashSet::new();
        for attempt in &attempts {
            paths.insert(attempt.path.as_str());
        }
        if paths.len() > 1 {
            let mut rows: Vec<_> = attempts
                .iter()
                .map(|attempt| (attempt.row, attempt.path.clone()))
                .collect();
            rows.sort_by_key(|(row, _)| *row);
            index.ambiguous.insert(name, rows);
            continue;
        }
        if let Some(governing) = attempts.iter().max_by_key(|attempt| attempt.row) {
            index
                .governing
                .insert(name.clone(), (governing.row, governing.status));
            if matches!(
                governing.status,
                crate::issue::AgentDispatchStatus::Done | crate::issue::AgentDispatchStatus::Merged
            ) {
                index.done.insert(name);
            }
        }
    }
    index
}

/// One active sibling's recorded scope, as plain data — the host builds these
/// from roster rows that share the new row's worktree + issue, are active
/// (non-terminal), and carry a `chunk_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveScope {
    /// The roster row id the sibling dispatch was written under.
    pub row: i64,
    /// The sibling's chunk name (its chunk file's basename, e.g. `chunk-2`) —
    /// what `overlaps:` blessings and `after:` entries refer to.
    pub name: String,
    /// The sibling's parsed `files:` list. The host refuses admission if its
    /// scope cannot be read or parsed.
    pub files: Vec<String>,
}

/// Scope facts read and parsed by the host before a dispatch takes SQLite's
/// write lock. `None` means the row appeared after preparation and requires a
/// fresh snapshot; `Err` is an active sibling whose ownership is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSiblingScope {
    pub row: i64,
    pub path: String,
    pub scope: Option<Result<ChunkScope, String>>,
}

/// Filesystem-free inputs to the serialized chunk admission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedChunkAdmission {
    pub path: String,
    pub scope: ChunkScope,
    pub siblings: Vec<PreparedSiblingScope>,
}

/// The gate's decision for one new scope against the active siblings and the
/// done set. The host turns every variant except [`ScopeVerdict::Ok`] into a
/// refusal (with `--force` as the way out); [`ScopeVerdict::Conflict`] wins
/// when both kinds of problem exist, and the host renders the `after:` axis
/// from [`after_unmet`] alongside the overlap data, so a mixed refusal still
/// names everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeVerdict {
    /// No conflict: dispatch may proceed.
    Ok,
    /// Each conflicting sibling, by its index into the `active` slice passed
    /// to [`verdict`], with every colliding `(new, sibling)` path pair.
    /// Reported exhaustively — one refusal lists every colliding sibling.
    Conflict {
        overlaps: Vec<(usize, Vec<(String, String)>)>,
    },
    /// The `after:` chunks that are not `done` yet (exhaustive, ordered).
    UnmetAfter(Vec<String>),
    /// Matching could not be completed safely; callers must refuse admission.
    Unknown(MatchError),
}

/// Decide whether `new` may dispatch alongside `active`, given the set of
/// sibling chunk names that are already `done`.
///
/// A sibling conflicts only when its name is NOT in `new.overlaps` (the
/// architect's blessing suppresses the refusal for that sibling only) and a
/// file collides. Overlap conflicts are checked against every active sibling
/// before the `after:` axis, and everything on the winning axis is reported —
/// the gate is all-or-nothing per dispatch, and the message is the whole
/// picture.
pub fn verdict(new: &ChunkScope, active: &[ActiveScope], done: &HashSet<String>) -> ScopeVerdict {
    let mut overlaps = Vec::new();
    let mut work = 0usize;
    for (i, sib) in active.iter().enumerate() {
        if new.overlaps.iter().any(|o| o == &sib.name) {
            continue;
        }
        let pairs = match paths_overlap_with_work(&new.files, &sib.files, &mut work) {
            Ok(pairs) => pairs,
            Err(error) => return ScopeVerdict::Unknown(error),
        };
        if !pairs.is_empty() {
            overlaps.push((i, pairs));
        }
    }
    if !overlaps.is_empty() {
        return ScopeVerdict::Conflict { overlaps };
    }
    let unmet = after_unmet(&new.after, done);
    if !unmet.is_empty() {
        return ScopeVerdict::UnmetAfter(unmet);
    }
    ScopeVerdict::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_known_keys_are_rejected_whatever_the_value_shape() {
        // Every first/second declaration shape, for every known key.
        let shapes = [
            ("{k}: []", "inline-empty"),
            ("{k}:", "bare-empty"),
            ("{k}: [ , ]", "blank-items"),
            ("{k}:\n  -\n  - # c", "empty-block"),
            ("{k}: a", "scalar"),
            ("{k}: [a, b]", "inline"),
            ("{k}:\n  - a", "block"),
        ];
        for key in ["files", "overlaps", "after"] {
            for (first, f_name) in shapes {
                for (second, s_name) in shapes {
                    let a = first.replace("{k}", key);
                    let b = second.replace("{k}", key);
                    let md = format!("---\n{a}\n{b}\n---\n");
                    let err = parse_frontmatter(&md)
                        .expect_err(&format!("{key} {f_name} then {s_name} must fail"));
                    let line = 2 + a.lines().count();
                    assert_eq!(err.line, line, "{key} {f_name} then {s_name}");
                    assert!(err.message.contains(&format!("duplicate `{key}:`")));
                }
            }
        }
    }

    #[test]
    fn single_empty_declaration_and_repeated_unknown_keys_stay_legal() {
        for key in ["files", "overlaps", "after"] {
            assert_eq!(
                parse_frontmatter(&format!("---\n{key}: []\n---\n")),
                Ok(ChunkScope::default())
            );
            assert_eq!(
                parse_frontmatter(&format!("---\n{key}:\n---\n")),
                Ok(ChunkScope::default())
            );
        }
        let md = "---\nnote: a\nnote: b\nfiles: [x]\nother:\nother:\n---\n";
        assert_eq!(parse_frontmatter(md).unwrap().files, s(&["x"]));
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn done(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // --- parser -------------------------------------------------------------

    #[test]
    fn missing_and_empty_frontmatter_are_all_empty_scopes() {
        assert_eq!(parse_frontmatter("").unwrap(), ChunkScope::default());
        assert_eq!(
            parse_frontmatter("no block at all\n").unwrap(),
            ChunkScope::default()
        );
        assert_eq!(
            parse_frontmatter("# a chunk with prose only\n\nbody\n").unwrap(),
            ChunkScope::default()
        );
        // A closed-but-empty block is legal and empty.
        assert_eq!(
            parse_frontmatter("---\n---\nbody\n").unwrap(),
            ChunkScope::default()
        );
    }

    #[test]
    fn block_list_style_parses_with_indentation() {
        let md = "---\nfiles:\n  - crates/a.rs\n  - crates/b.rs\noverlaps: [chunk-2]\nafter:\n  - chunk-1\n---\n\n# Chunk 3\n";
        let scope = parse_frontmatter(md).unwrap();
        assert_eq!(scope.files, s(&["crates/a.rs", "crates/b.rs"]));
        assert_eq!(scope.overlaps, s(&["chunk-2"]));
        assert_eq!(scope.after, s(&["chunk-1"]));
    }

    #[test]
    fn inline_list_style_parses() {
        let md = "---\nfiles: [a.rs, b.rs]\noverlaps: []\nafter: [chunk-1, chunk-2]\n---\n";
        let scope = parse_frontmatter(md).unwrap();
        assert_eq!(scope.files, s(&["a.rs", "b.rs"]));
        assert_eq!(scope.overlaps, s(&[]));
        assert_eq!(scope.after, s(&["chunk-1", "chunk-2"]));
    }

    #[test]
    fn bare_scalar_value_is_one_item() {
        let md = "---\nafter: chunk-1\n---\n";
        assert_eq!(parse_frontmatter(md).unwrap().after, s(&["chunk-1"]));
    }

    #[test]
    fn unknown_keys_and_their_items_are_ignored() {
        let md = "---\nowner: bob\nreviewers:\n  - alice\n  - carol\nfiles:\n  - a.rs\nfuture_key: [x, y]\n---\n";
        let scope = parse_frontmatter(md).unwrap();
        assert_eq!(scope.files, s(&["a.rs"]));
        assert_eq!(scope.overlaps, s(&[]));
        assert_eq!(scope.after, s(&[]));
    }

    #[test]
    fn the_closing_fence_takes_trailing_whitespace_and_the_body_is_ignored() {
        let md = "---\nfiles: [a.rs]\n---   \nfiles: [SHOULD_NOT_PARSE]\n";
        assert_eq!(parse_frontmatter(md).unwrap().files, s(&["a.rs"]));
    }

    #[test]
    fn unsupported_colon_in_a_file_path_is_rejected() {
        let md = "---\nfiles:\n  - crates/_a:b.rs\n---\n";
        assert!(parse_frontmatter(md).is_err());
    }

    #[test]
    fn a_stray_item_before_any_key_is_ignored() {
        let md = "---\n  - orphan\nfiles: [a.rs]\n---\n";
        assert_eq!(parse_frontmatter(md).unwrap().files, s(&["a.rs"]));
    }

    #[test]
    fn an_unterminated_block_is_an_error_naming_the_opening_line() {
        let err = parse_frontmatter("---\nfiles: [a.rs]\n\nbody without a fence\n").unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains("never closed"), "{err}");
        // Display names the line, which is what the host's refusal quotes.
        assert_eq!(
            err.to_string(),
            "line 1: frontmatter block opens with `---` but is never closed"
        );
    }

    #[test]
    fn an_unclosed_inline_list_is_an_error_naming_its_line() {
        let err = parse_frontmatter("---\nfiles:\n  - a.rs\nafter: [chunk-1\n---\n").unwrap_err();
        assert_eq!(err.line, 4);
        assert!(err.message.contains("not closed"), "{err}");
    }

    #[test]
    fn a_duplicate_known_key_is_an_error_naming_its_line() {
        let err = parse_frontmatter("---\nfiles: [a.rs]\nfiles: [b.rs]\n---\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("duplicate"), "{err}");
        // Unknown keys may repeat freely.
        parse_frontmatter("---\nx: 1\nx: 2\nfiles: [a.rs]\n---\n").unwrap();
    }

    // --- glob matcher -------------------------------------------------------

    #[test]
    fn exact_paths_compare_literally() {
        assert!(glob_match("crates/a.rs", "crates/a.rs"));
        assert!(!glob_match("crates/a.rs", "crates/b.rs"));
        assert!(!glob_match("crates/a.rs", "crates/a.rs/b.rs"));
    }

    #[test]
    fn star_matches_within_one_segment_only() {
        assert!(glob_match(
            "crates/config_*.rs",
            "crates/config_pipeline.rs"
        ));
        assert!(glob_match("crates/*", "crates/a.rs"));
        // `*` swallows dots and underscores within the segment…
        assert!(glob_match("*.rs", "a.b.rs"));
        // …but never crosses `/`.
        assert!(!glob_match("crates/*", "crates/cmd/a.rs"));
        assert!(!glob_match("*/a.rs", "x/y/a.rs"));
        // Empty runs are fine.
        assert!(glob_match(
            "crates/*-core/src/lib.rs",
            "crates/thegn-core/src/lib.rs"
        ));
    }

    #[test]
    fn double_star_matches_across_segments_including_zero() {
        assert!(glob_match("crates/**/*.rs", "crates/a.rs"));
        assert!(glob_match("crates/**/*.rs", "crates/cmd/a.rs"));
        assert!(glob_match("crates/**/*.rs", "crates/a/b/c.rs"));
        assert!(!glob_match("crates/**/*.rs", "other/a.rs"));
        assert!(glob_match("**/lib.rs", "crates/thegn-core/src/lib.rs"));
        assert!(glob_match("crates/**", "crates/x/y.rs"));
    }

    // --- overlap pairs ------------------------------------------------------

    #[test]
    fn pairs_are_reported_exhaustively_in_order() {
        let a = s(&["a.rs", "b.rs", "c*.rs"]);
        let b = s(&["b.rs", "c1.rs", "c2.rs"]);
        assert_eq!(
            paths_overlap(&a, &b).unwrap(),
            vec![
                ("b.rs".to_string(), "b.rs".to_string()),
                ("c*.rs".to_string(), "c1.rs".to_string()),
                ("c*.rs".to_string(), "c2.rs".to_string()),
            ]
        );
    }

    #[test]
    fn a_glob_collides_with_an_exact_path_in_either_direction() {
        assert_eq!(
            paths_overlap(&s(&["config_*.rs"]), &s(&["config_pipeline.rs"])).unwrap(),
            vec![("config_*.rs".to_string(), "config_pipeline.rs".to_string())]
        );
        assert_eq!(
            paths_overlap(&s(&["config_pipeline.rs"]), &s(&["config_*.rs"])).unwrap(),
            vec![("config_pipeline.rs".to_string(), "config_*.rs".to_string())]
        );
    }

    #[test]
    fn disjoint_or_empty_scopes_never_overlap() {
        assert!(
            paths_overlap(&s(&["a.rs"]), &s(&["b.rs"]))
                .unwrap()
                .is_empty()
        );
        assert!(paths_overlap(&[], &s(&["b.rs"])).unwrap().is_empty());
        assert!(paths_overlap(&s(&["a.rs"]), &[]).unwrap().is_empty());
    }

    #[test]
    fn glob_language_intersection_matrix_is_sound_and_case_conservative() {
        let cases = [
            ("src/*.rs", "src/lib.*", true),
            ("src/*.rs", "src/lib.rs", true),
            ("src/*.rs", "tests/*.rs", false),
            ("src/a.rs", "src/b.rs", false),
            ("src/**/lib.rs", "src/lib.rs", true),
            ("src/**/lib.rs", "src/bin/lib.rs", true),
            ("src/*.rs", "src/lib.py", false),
            ("Src/Lib.rs", "src/lib.rs", true),
        ];
        for (a, b, intersects) in cases {
            assert_eq!(
                !paths_overlap(&s(&[a]), &s(&[b])).unwrap().is_empty(),
                intersects,
                "{a} vs {b}"
            );
        }
    }

    #[test]
    fn file_patterns_are_canonicalized_or_rejected_at_parse_time() {
        assert_eq!(
            parse_frontmatter("---\nfiles: [./src/lib.rs]\n---\n")
                .unwrap()
                .files,
            s(&["src/lib.rs"])
        );
        for path in [
            "../secret",
            "/etc/passwd",
            "src//lib.rs",
            "src/./lib.rs",
            "src\\lib.rs",
            "C:/src/lib.rs",
            "src/foo.",
            "src/foo /bar",
            "src/foo?.rs",
            "src/foo\tbar",
            "src/café.rs",
            "src/CON.txt",
        ] {
            let md = format!("---\nfiles: [{path}]\n---\n");
            assert!(parse_frontmatter(&md).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn frontmatter_input_pattern_count_and_pattern_length_are_capped() {
        let too_many = std::iter::repeat_n("a", MAX_PATTERNS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_frontmatter(&format!("---\nfiles: [{too_many}]\n---\n")).is_err());
        assert!(
            parse_frontmatter(&format!(
                "---\nfiles: [{}]\n---\n",
                "a".repeat(MAX_PATTERN_BYTES + 1)
            ))
            .is_err()
        );
        assert!(parse_frontmatter(&"x".repeat(1024 * 1024 + 1)).is_err());
        assert!(parse_frontmatter(&"\n".repeat(MAX_FRONTMATTER_LINES + 1)).is_err());
    }

    #[test]
    fn repeated_double_star_matching_is_memoized_with_a_deterministic_work_bound() {
        let pattern = std::iter::repeat_n("**", 30).collect::<Vec<_>>().join("/");
        let path = std::iter::repeat_n("x", 30).collect::<Vec<_>>().join("/");
        let p: Vec<&str> = pattern.split('/').collect();
        let s: Vec<&str> = path.split('/').collect();
        let mut memo = HashMap::new();
        let mut work = 0;
        assert!(match_segments(&p, &s, &mut memo, &mut work));
        assert!(work <= (p.len() + 1) * (s.len() + 1));
        let repeated = std::iter::repeat_n("**", 30).collect::<Vec<_>>().join("/");
        let mut intersection_work = 0;
        assert!(patterns_intersect(&repeated, &repeated, &mut intersection_work).unwrap());
        assert!(intersection_work < MAX_PAIR_MATCH_WORK);
    }

    #[test]
    fn unsupported_and_over_budget_scope_data_is_a_typed_refusal() {
        assert_eq!(
            paths_overlap(&s(&["src/../secret"]), &s(&["src/*"])),
            Err(MatchError::UnsupportedPattern)
        );
        assert_eq!(
            paths_overlap(&s(&["src/a?.rs"]), &s(&["src/a.rs"])),
            Err(MatchError::UnsupportedPattern)
        );
        let too_many = vec!["src/*".to_string(); MAX_PATTERNS + 1];
        let scope = ChunkScope {
            files: too_many,
            ..ChunkScope::default()
        };
        assert_eq!(
            verdict(&scope, &[active(1, "chunk-1", &["src/lib.rs"])], &done(&[])),
            ScopeVerdict::Unknown(MatchError::OverBudget)
        );
        let mut near_aggregate_ceiling = MAX_TOTAL_MATCH_WORK - 1;
        assert_eq!(
            paths_overlap_with_work(
                &s(&["same.rs"]),
                &s(&["same.rs"]),
                &mut near_aggregate_ceiling
            ),
            Err(MatchError::OverBudget)
        );

        let long_path = "a".repeat(40);
        let siblings: Vec<ActiveScope> = (0..2500)
            .map(|row| ActiveScope {
                row,
                name: format!("chunk-{row}"),
                files: vec![format!("sibling-{row}/{long_path}")],
            })
            .collect();
        let scope = ChunkScope {
            files: vec![format!("incoming/{long_path}")],
            ..ChunkScope::default()
        };
        assert_eq!(verdict(&scope, &siblings, &done(&[])), ScopeVerdict::Ok);
    }

    #[test]
    fn realistic_roster_has_an_independent_budget_for_each_pattern_pair() {
        let files: Vec<String> = (0..20)
            .map(|n| format!("crates/thegn-host/src/file_{n}.rs"))
            .collect();
        let siblings: Vec<ActiveScope> = (0..10)
            .map(|row| ActiveScope {
                row,
                name: format!("chunk-{row}"),
                files: (0..20)
                    .map(|n| format!("crates/thegn-host/src/other_{row}_{n}.rs"))
                    .collect(),
            })
            .collect();
        let scope = ChunkScope {
            files,
            ..ChunkScope::default()
        };
        assert_eq!(verdict(&scope, &siblings, &done(&[])), ScopeVerdict::Ok);
    }

    #[test]
    fn invalid_file_pattern_errors_name_the_item_line_and_colon_items_still_parse() {
        let error =
            parse_frontmatter("---\nfiles:\n  - good.rs\n  - ../bad.rs\n---\n").unwrap_err();
        assert_eq!(error.line, 4);
        for (frontmatter, line) in [
            ("---\nfiles: [../bad.rs]\n---\n", 2),
            ("---\nfiles: ../bad.rs\n---\n", 2),
        ] {
            assert_eq!(parse_frontmatter(frontmatter).unwrap_err().line, line);
        }
        let scope =
            parse_frontmatter("---\nafter:\n  - issue:123\noverlaps: [chunk:2]\n---\n").unwrap();
        assert_eq!(scope.after, s(&["issue:123"]));
        assert_eq!(scope.overlaps, s(&["chunk:2"]));
    }

    #[test]
    fn shared_path_grammar_normalizes_one_dot_prefix_and_rejects_aliases() {
        assert_eq!(
            canonical_repo_relative_path("./src/*.rs").as_deref(),
            Some("src/*.rs")
        );
        for path in [
            "../outside.md",
            "dir//scope.md",
            "dir/./scope.md",
            "dir\\scope.md",
            "dir/scope:bad.md",
            "café.md",
            "src/aux.rs",
        ] {
            assert_eq!(canonical_repo_relative_path(path), None, "accepted {path}");
        }
    }

    // --- after / done -------------------------------------------------------

    #[test]
    fn after_unmet_reports_missing_names_in_order_without_duplicates() {
        let d = done(&["chunk-1", "chunk-2"]);
        assert_eq!(
            after_unmet(&s(&["chunk-2", "chunk-3", "chunk-1", "chunk-3"]), &d),
            s(&["chunk-3"])
        );
        assert!(after_unmet(&s(&["chunk-1"]), &d).is_empty());
        assert!(after_unmet(&[], &d).is_empty());
        assert_eq!(after_unmet(&s(&["chunk-9"]), &d), s(&["chunk-9"]));
    }

    #[test]
    fn after_index_newest_attempt_governs() {
        let candidates = [
            AfterCandidate {
                row: 10,
                path: "code/chunk-1.md".into(),
                status: crate::issue::AgentDispatchStatus::Done,
            },
            AfterCandidate {
                row: 11,
                path: "code/chunk-1.md".into(),
                status: crate::issue::AgentDispatchStatus::Failed,
            },
        ];
        let index = after_index(&candidates);
        assert!(!index.done.contains("chunk-1"));
        assert_eq!(
            index.governing.get("chunk-1"),
            Some(&(11, crate::issue::AgentDispatchStatus::Failed))
        );
        assert!(index.ambiguous.is_empty());
    }

    #[test]
    fn after_index_merged_counts_as_done() {
        let index = after_index(&[AfterCandidate {
            row: 12,
            path: "code/chunk-1.md".into(),
            status: crate::issue::AgentDispatchStatus::Merged,
        }]);
        assert!(index.done.contains("chunk-1"));
        assert_eq!(
            index.governing.get("chunk-1"),
            Some(&(12, crate::issue::AgentDispatchStatus::Merged))
        );
        assert!(index.ambiguous.is_empty());
    }

    #[test]
    fn after_index_distinct_paths_with_one_stem_are_ambiguous() {
        let candidates = [
            AfterCandidate {
                row: 12,
                path: "code/a/chunk-1.md".into(),
                status: crate::issue::AgentDispatchStatus::Done,
            },
            AfterCandidate {
                row: 15,
                path: "code/b/chunk-1.md".into(),
                status: crate::issue::AgentDispatchStatus::Done,
            },
        ];
        let index = after_index(&candidates);
        assert!(!index.done.contains("chunk-1"));
        assert!(!index.governing.contains_key("chunk-1"));
        assert_eq!(
            index.ambiguous.get("chunk-1"),
            Some(&vec![
                (12, "code/a/chunk-1.md".into()),
                (15, "code/b/chunk-1.md".into()),
            ])
        );
    }

    #[test]
    fn after_index_skips_empty_stems() {
        let index = after_index(&[AfterCandidate {
            row: 1,
            path: String::new(),
            status: crate::issue::AgentDispatchStatus::Done,
        }]);
        assert!(index.done.is_empty());
        assert!(index.governing.is_empty());
        assert!(index.ambiguous.is_empty());
    }

    // --- verdict ------------------------------------------------------------

    fn active(row: i64, name: &str, files: &[&str]) -> ActiveScope {
        ActiveScope {
            row,
            name: name.to_string(),
            files: s(files),
        }
    }

    #[test]
    fn a_scope_without_files_or_after_is_always_ok() {
        let v = verdict(
            &ChunkScope::default(),
            &[active(7, "chunk-1", &["a.rs"])],
            &done(&[]),
        );
        assert_eq!(v, ScopeVerdict::Ok);
    }

    #[test]
    fn an_overlapping_active_sibling_is_a_conflict_naming_rows_and_paths() {
        let new = ChunkScope {
            files: s(&["lib.rs", "cmd/mod.rs"]),
            ..ChunkScope::default()
        };
        let actives = vec![
            active(11, "chunk-1", &["other.rs"]),
            active(12, "chunk-2", &["readme.md", "lib.rs"]),
        ];
        match verdict(&new, &actives, &done(&[])) {
            ScopeVerdict::Conflict { overlaps } => {
                // Index 1 (chunk-2), and only the colliding pair.
                assert_eq!(overlaps.len(), 1);
                assert_eq!(overlaps[0].0, 1);
                assert_eq!(
                    overlaps[0].1,
                    vec![("lib.rs".to_string(), "lib.rs".to_string())]
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn an_overlaps_blessing_suppresses_only_that_sibling() {
        let new = ChunkScope {
            files: s(&["lib.rs"]),
            overlaps: s(&["chunk-2"]),
            ..ChunkScope::default()
        };
        let actives = vec![
            active(12, "chunk-2", &["lib.rs"]),
            active(13, "chunk-3", &["lib.rs"]),
        ];
        match verdict(&new, &actives, &done(&[])) {
            ScopeVerdict::Conflict { overlaps } => {
                // Only chunk-3 (index 1) is reported; the blessed chunk-2 is
                // not.
                assert_eq!(overlaps.len(), 1);
                assert_eq!(overlaps[0].0, 1);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn every_conflicting_sibling_is_reported_not_just_the_first() {
        let new = ChunkScope {
            files: s(&["lib.rs"]),
            ..ChunkScope::default()
        };
        let actives = vec![
            active(11, "chunk-1", &["lib.rs"]),
            active(12, "chunk-2", &["other.rs"]),
            active(13, "chunk-3", &["lib.rs", "x.rs"]),
        ];
        match verdict(&new, &actives, &done(&[])) {
            ScopeVerdict::Conflict { overlaps } => {
                assert_eq!(overlaps.len(), 2, "both colliding siblings reported");
                assert_eq!(overlaps[0].0, 0);
                assert_eq!(overlaps[1].0, 2);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn a_satisfied_done_set_is_ok_and_an_unmet_one_names_the_chunk() {
        let new = ChunkScope {
            after: s(&["chunk-1", "chunk-2"]),
            ..ChunkScope::default()
        };
        assert_eq!(
            verdict(&new, &[], &done(&["chunk-1", "chunk-2"])),
            ScopeVerdict::Ok
        );
        assert_eq!(
            verdict(&new, &[], &done(&["chunk-1"])),
            ScopeVerdict::UnmetAfter(s(&["chunk-2"]))
        );
    }

    #[test]
    fn overlap_conflicts_take_precedence_over_unmet_after() {
        let new = ChunkScope {
            files: s(&["lib.rs"]),
            after: s(&["chunk-9"]),
            ..ChunkScope::default()
        };
        let actives = vec![active(12, "chunk-2", &["lib.rs"])];
        match verdict(&new, &actives, &done(&[])) {
            ScopeVerdict::Conflict { overlaps } => assert_eq!(overlaps.len(), 1),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}
