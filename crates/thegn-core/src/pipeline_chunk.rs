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

use std::collections::HashSet;
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
                list.push(item.to_string());
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
        let already = match current {
            Some(Key::Files) => !scope.files.is_empty(),
            Some(Key::Overlaps) => !scope.overlaps.is_empty(),
            Some(Key::After) => !scope.after.is_empty(),
            None => unreachable!(),
        };
        if already {
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
            list.extend(
                items
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            );
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
            list.push(value.to_string());
            current = None;
        }
    }
    Err(ParseError {
        line: 1,
        message: "frontmatter block opens with `---` but is never closed".to_string(),
    })
}

/// Does `path` match `pattern`? `*` matches any run of characters within one
/// path segment; `**` as a whole segment matches zero or more segments
/// (across `/`); any other pattern compares literally. There is no escape
/// syntax and no `?` — a gate should be predictable, not clever.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    match_segments(&p, &s)
}

fn match_segments(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        // `**` across segments — including zero of them (`a/**/b` matches
        // `a/b`), the usual glob convention.
        Some((&"**", rest)) => (0..=s.len()).any(|skip| match_segments(rest, &s[skip..])),
        Some((pat, rest)) => match s.split_first() {
            None => false,
            Some((seg, srest)) => match_segment(pat, seg) && match_segments(rest, srest),
        },
    }
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
/// every pair, not the first: the refusal names everything at once. A pair
/// collides when either side's pattern matches the other side's string, so
/// `config_*.rs` in one scope collides with the exact `config_pipeline.rs` in
/// another and vice versa. Empty `files` on either side yields no pairs — the
/// opt-in rule.
pub fn paths_overlap(a: &[String], b: &[String]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for pa in a {
        for pb in b {
            if glob_match(pa, pb) || glob_match(pb, pa) {
                pairs.push((pa.clone(), pb.clone()));
            }
        }
    }
    pairs
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
    /// The sibling's parsed `files:` list (empty when its file is unreadable —
    /// a sibling that cannot be read never conflicts, degrading to the
    /// pre-gate behaviour rather than wedging the roster).
    pub files: Vec<String>,
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
    for (i, sib) in active.iter().enumerate() {
        if new.overlaps.iter().any(|o| o == &sib.name) {
            continue;
        }
        let pairs = paths_overlap(&new.files, &sib.files);
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
    fn item_lines_with_colons_are_items_not_keys() {
        let md = "---\nfiles:\n  - crates/_a:b.rs\n---\n";
        assert_eq!(parse_frontmatter(md).unwrap().files, s(&["crates/_a:b.rs"]));
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
            paths_overlap(&a, &b),
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
            paths_overlap(&s(&["config_*.rs"]), &s(&["config_pipeline.rs"])),
            vec![("config_*.rs".to_string(), "config_pipeline.rs".to_string())]
        );
        assert_eq!(
            paths_overlap(&s(&["config_pipeline.rs"]), &s(&["config_*.rs"])),
            vec![("config_pipeline.rs".to_string(), "config_*.rs".to_string())]
        );
    }

    #[test]
    fn disjoint_or_empty_scopes_never_overlap() {
        assert!(paths_overlap(&s(&["a.rs"]), &s(&["b.rs"])).is_empty());
        assert!(paths_overlap(&[], &s(&["b.rs"])).is_empty());
        assert!(paths_overlap(&s(&["a.rs"]), &[]).is_empty());
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
