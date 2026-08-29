//! Pure gitlink and submodule-domain types.
//!
//! A submodule is a mode-160000 entry in its superproject.  This module keeps
//! that distinction visible to the rest of the application without importing
//! a git implementation, filesystem access, or a terminal substrate.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A normalized entry from a root-level `.gitmodules` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleSpec {
    pub name: String,
    pub path: String,
    pub url: String,
}

/// A parse error with enough context to make malformed repository metadata
/// actionable without echoing any secret or executing the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for SubmoduleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SubmoduleParseError {}

fn parse_error(line: usize, message: impl Into<String>) -> SubmoduleParseError {
    SubmoduleParseError {
        line,
        message: message.into(),
    }
}

/// Validate a submodule path without resolving it against the local machine.
/// Paths are repository-relative, component-aware, and may be nested.
pub fn validate_submodule_path(path: &str) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("submodule path is empty".into());
    }
    if path.contains('\0') {
        return Err("submodule path contains NUL".into());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("submodule path must be relative".into());
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(format!(
            "submodule path {path:?} is empty or escapes the repository"
        ));
    }
    Ok(())
}

/// Parse a strict `.gitmodules` fixture.
pub fn parse_gitmodules(input: &str) -> Result<Vec<SubmoduleSpec>, SubmoduleParseError> {
    let mut records: Vec<(usize, String, Option<String>, Option<String>)> = Vec::new();
    let mut current: Option<usize> = None;

    for (index, raw) in input.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let Some(name) = line
                .strip_prefix("[submodule \"")
                .and_then(|s| s.strip_suffix("\"]"))
            else {
                return Err(parse_error(
                    line_no,
                    "expected [submodule \"name\"] section",
                ));
            };
            if name.trim().is_empty() {
                return Err(parse_error(line_no, "submodule name is empty"));
            }
            records.push((line_no, name.to_string(), None, None));
            current = Some(records.len() - 1);
            continue;
        }

        let Some(record) = current.and_then(|i| records.get_mut(i)) else {
            return Err(parse_error(line_no, "key outside a submodule section"));
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(parse_error(line_no, "expected key = value"));
        };
        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            return Err(parse_error(line_no, format!("{key} value is empty")));
        }
        match key {
            "path" if record.2.is_none() => record.2 = Some(value.to_string()),
            "url" if record.3.is_none() => record.3 = Some(value.to_string()),
            "path" | "url" => return Err(parse_error(line_no, format!("duplicate {key}"))),
            _ => {
                return Err(parse_error(
                    line_no,
                    format!("unknown submodule key {key:?}"),
                ));
            }
        }
    }

    let mut specs = Vec::with_capacity(records.len());
    for (line, name, path, url) in records {
        let path = path.ok_or_else(|| parse_error(line, "submodule section has no path"))?;
        validate_submodule_path(&path).map_err(|message| parse_error(line, message))?;
        let url = url.ok_or_else(|| parse_error(line, "submodule section has no url"))?;
        if specs.iter().any(|spec: &SubmoduleSpec| spec.path == path) {
            return Err(parse_error(
                line,
                format!("duplicate submodule path {path:?}"),
            ));
        }
        specs.push(SubmoduleSpec { name, path, url });
    }
    Ok(specs)
}

/// The pointer relationship visible in a superproject change row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmodulePointer {
    Clean,
    Moved,
    Rewind,
    Diverged,
    Conflict,
    Unknown,
}

/// Short alias for consumers that prefer the generic relationship name.
pub type PointerState = SubmodulePointer;

/// A parsed `git submodule status --recursive` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleState {
    pub path: String,
    pub recorded_sha: String,
    pub checked_out_sha: String,
    pub initialized: bool,
    pub dirty: bool,
    pub untracked: bool,
    pub pointer: SubmodulePointer,
}

/// Parse one status fixture line. The leading status marker is the first byte:
/// space means clean, `-` uninitialized, `+` checked out at another commit,
/// and `U` conflict.
pub fn parse_submodule_status_line(line: &str) -> Result<SubmoduleState, String> {
    let Some(marker) = line.as_bytes().first().copied().map(char::from) else {
        return Err("empty submodule status line".into());
    };
    if !matches!(marker, ' ' | '-' | '+' | 'U') {
        return Err(format!("unknown submodule status marker {marker:?}"));
    }
    let mut fields = line[1..].split_whitespace();
    let sha = fields.next().ok_or("submodule status has no object id")?;
    if !is_object_id(sha) {
        return Err(format!("invalid submodule object id {sha:?}"));
    }
    let path = fields.next().ok_or("submodule status has no path")?;
    validate_submodule_path(path)?;
    // The optional `(heads/main)` decoration is intentionally ignored. It is
    // display metadata, not a source of truth for pointer relationships.
    let initialized = marker != '-';
    let pointer = match marker {
        ' ' => SubmodulePointer::Clean,
        '-' => SubmodulePointer::Unknown,
        '+' => SubmodulePointer::Moved,
        'U' => SubmodulePointer::Conflict,
        _ => unreachable!(),
    };
    Ok(SubmoduleState {
        path: path.to_string(),
        recorded_sha: if matches!(marker, '+' | 'U') {
            String::new()
        } else {
            sha.to_string()
        },
        checked_out_sha: if initialized {
            sha.to_string()
        } else {
            String::new()
        },
        initialized,
        dirty: false,
        untracked: false,
        pointer,
    })
}

/// Parse all non-empty rows of `git submodule status --recursive` output.
pub fn parse_submodule_status(input: &str) -> Result<Vec<SubmoduleState>, String> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_submodule_status_line)
        .collect()
}

/// Classify a checked-out pointer using explicit ancestor facts. Equal object
/// ids are clean; a changed pointer with unavailable facts remains moved rather
/// than being guessed into a direction.
pub fn classify_pointer(
    recorded_sha: &str,
    checked_out_sha: &str,
    initialized: bool,
    conflict: bool,
    facts: AncestorFacts,
) -> SubmodulePointer {
    if conflict {
        return SubmodulePointer::Conflict;
    }
    if !initialized {
        return SubmodulePointer::Unknown;
    }
    if recorded_sha == checked_out_sha {
        return SubmodulePointer::Clean;
    }
    match direction_from_ancestors(facts) {
        SubmoduleDirection::Forward => SubmodulePointer::Moved,
        SubmoduleDirection::Rewind => SubmodulePointer::Rewind,
        SubmoduleDirection::Diverged => SubmodulePointer::Diverged,
        SubmoduleDirection::Unknown => SubmodulePointer::Moved,
    }
}

/// Add explicit ancestor-probe facts to a parsed status row.
pub fn apply_ancestor_facts(mut state: SubmoduleState, facts: AncestorFacts) -> SubmoduleState {
    state.pointer = classify_pointer(
        &state.recorded_sha,
        &state.checked_out_sha,
        state.initialized,
        state.pointer == SubmodulePointer::Conflict,
        facts,
    );
    state
}

/// Parse the raw mode-160000 index line and its `Subproject commit` bodies from
/// a fixture patch. The path is supplied separately because a raw patch can
/// contain quoted paths that are not safe to reinterpret here.
pub fn parse_submodule_diff(path: &str, fixture: &str) -> Result<SubmoduleDiff, String> {
    validate_submodule_path(path)?;
    let mut old_sha = None;
    let mut new_sha = None;
    for line in fixture.lines() {
        if let Some(rest) = line.strip_prefix("index ")
            && let Some((old, new_and_mode)) = rest.split_once("..")
        {
            let new = new_and_mode.split_whitespace().next().unwrap_or_default();
            if is_object_id(old) && is_object_id(new) {
                old_sha = Some(old.to_string());
                new_sha = Some(new.to_string());
            }
        }
        if let Some(sha) = line
            .strip_prefix("-Subproject commit ")
            .or_else(|| line.strip_prefix(" Subproject commit "))
        {
            old_sha = Some(sha.trim().to_string());
        }
        if let Some(sha) = line.strip_prefix("+Subproject commit ") {
            new_sha = Some(sha.trim().to_string());
        }
    }
    let old_sha = old_sha.ok_or("gitlink diff has no old object id")?;
    let new_sha = new_sha.ok_or("gitlink diff has no new object id")?;
    let kind = if old_sha.chars().all(|c| c == '0') {
        SubmoduleDiffKind::Added
    } else if new_sha.chars().all(|c| c == '0') {
        SubmoduleDiffKind::Deleted
    } else {
        SubmoduleDiffKind::Changed
    };
    Ok(SubmoduleDiff {
        path: path.to_string(),
        old_sha,
        new_sha,
        kind,
    })
}

/// Name emphasizing the gitlink terminology used by diff consumers.
pub use parse_submodule_diff as parse_gitlink_diff;

fn is_object_id(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Facts obtained from explicit local ancestor probes. `None` means the
/// object was unavailable or the probe failed; it is never inferred from SHA
/// spelling or lexical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AncestorFacts {
    pub old_is_ancestor: Option<bool>,
    pub new_is_ancestor: Option<bool>,
}

/// Direction of a pointer change, based only on local ancestor facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmoduleDirection {
    Forward,
    Rewind,
    Diverged,
    Unknown,
}

pub fn direction_from_ancestors(facts: AncestorFacts) -> SubmoduleDirection {
    match (facts.old_is_ancestor, facts.new_is_ancestor) {
        (Some(true), Some(false) | None) => SubmoduleDirection::Forward,
        (Some(false) | None, Some(true)) => SubmoduleDirection::Rewind,
        (Some(false), Some(false)) => SubmoduleDirection::Diverged,
        // Equal tips and failed/partial probes are intentionally not guessed.
        _ => SubmoduleDirection::Unknown,
    }
}

/// A raw superproject gitlink transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleDiff {
    pub path: String,
    pub old_sha: String,
    pub new_sha: String,
    pub kind: SubmoduleDiffKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmoduleDiffKind {
    Added,
    Deleted,
    Changed,
}

/// A bounded local summary of a pointer transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleSummary {
    pub direction: SubmoduleDirection,
    pub commits: Vec<String>,
    pub truncated: bool,
    pub unavailable: bool,
}

impl SubmoduleSummary {
    pub const DEFAULT_LIMIT: usize = 32;

    pub fn bounded(
        direction: SubmoduleDirection,
        mut commits: Vec<String>,
        limit: usize,
        unavailable: bool,
    ) -> Self {
        let limit = limit.min(Self::DEFAULT_LIMIT);
        let truncated = commits.len() > limit;
        commits.truncate(limit);
        Self {
            direction,
            commits,
            truncated,
            unavailable,
        }
    }
}

/// Pure row data shared by the changes and preview presentation layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleRow {
    pub path: String,
    pub old_sha: String,
    pub new_sha: String,
    pub label: String,
    pub commits: Vec<String>,
    pub truncated: bool,
    pub unavailable: bool,
}

/// Presentation policy for atomic gitlink rows. The caller supplies the
/// capability-resolved glyph and arrow, so this module does not create a draw
/// site or bypass terminal degradation.
pub struct SubmoduleRowPolicy;

impl SubmoduleRowPolicy {
    pub fn pointer(
        diff: &SubmoduleDiff,
        state: Option<&SubmoduleState>,
        summary: Option<&SubmoduleSummary>,
    ) -> SubmoduleRow {
        let label = if let Some(state) = state {
            if !state.initialized {
                "uninitialized".to_string()
            } else if state.dirty {
                "dirty".to_string()
            } else if state.untracked {
                "untracked".to_string()
            } else if matches!(state.pointer, SubmodulePointer::Diverged) {
                "diverged".to_string()
            } else if let Some(summary) = summary {
                if summary.unavailable {
                    "unavailable".to_string()
                } else {
                    format_direction(summary.direction)
                }
            } else {
                "unavailable".to_string()
            }
        } else {
            "unavailable".to_string()
        };
        SubmoduleRow {
            path: diff.path.clone(),
            old_sha: abbreviate_sha(&diff.old_sha),
            new_sha: abbreviate_sha(&diff.new_sha),
            label,
            commits: summary.map(|s| s.commits.clone()).unwrap_or_default(),
            truncated: summary.is_some_and(|s| s.truncated),
            unavailable: summary.is_none_or(|s| s.unavailable),
        }
    }

    pub fn format_pointer(
        glyph: &str,
        arrow: &str,
        diff: &SubmoduleDiff,
        state: Option<&SubmoduleState>,
        summary: Option<&SubmoduleSummary>,
    ) -> String {
        let row = Self::pointer(diff, state, summary);
        format!(
            "{glyph} {}  {} {arrow} {}  ({})",
            row.path, row.old_sha, row.new_sha, row.label
        )
    }

    pub fn format_preview(
        glyph: &str,
        arrow: &str,
        diff: &SubmoduleDiff,
        state: Option<&SubmoduleState>,
        summary: Option<&SubmoduleSummary>,
    ) -> String {
        let row = Self::pointer(diff, state, summary);
        let mut text = Self::format_pointer(glyph, arrow, diff, state, summary);
        if !row.commits.is_empty() {
            text.push_str(": ");
            text.push_str(&row.commits.join(", "));
            if row.truncated {
                text.push_str(", …");
            }
        } else if row.unavailable {
            text.push_str("; commit range unavailable locally");
        }
        text
    }
}

fn format_direction(direction: SubmoduleDirection) -> String {
    match direction {
        SubmoduleDirection::Forward => "forward".into(),
        SubmoduleDirection::Rewind => "rewind".into(),
        SubmoduleDirection::Diverged => "diverged".into(),
        SubmoduleDirection::Unknown => "unknown".into(),
    }
}

pub fn abbreviate_sha(sha: &str) -> String {
    if sha.is_empty() {
        "?".into()
    } else {
        sha.chars().take(7).collect()
    }
}

/// True when `path` is the submodule itself or a descendant of it.
pub fn is_submodule_descendant(path: &str, submodule_path: &str) -> bool {
    path == submodule_path
        || path
            .strip_prefix(submodule_path)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Return whether a path belongs to any listed submodule boundary.
pub fn is_submodule_boundary(path: &str, submodule_paths: &[String]) -> bool {
    submodule_paths
        .iter()
        .any(|root| is_submodule_descendant(path, root))
}

/// A typed gitlink conflict carried alongside the raw conflict path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleConflict {
    pub path: String,
    pub ours_sha: String,
    pub theirs_sha: String,
}

pub fn format_submodule_conflict(conflict: &SubmoduleConflict) -> String {
    format!(
        "submodule pointer conflict: {} ({} vs {})",
        conflict.path, conflict.ours_sha, conflict.theirs_sha
    )
}

pub fn format_submodule_conflicts(conflicts: &[SubmoduleConflict]) -> String {
    conflicts
        .iter()
        .map(format_submodule_conflict)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITMODULES: &str = r#"
[submodule "vendor/lib"]
    path = vendor/lib
    url = https://example.test/lib.git
[submodule "nested/tool"]
    path = vendor/lib/tool
    url = ../tool.git
"#;

    #[test]
    fn parses_nested_gitmodules_and_preserves_url_text() {
        let specs = parse_gitmodules(GITMODULES).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "vendor/lib");
        assert_eq!(specs[0].url, "https://example.test/lib.git");
        assert_eq!(specs[1].path, "vendor/lib/tool");
    }

    #[test]
    fn rejects_malformed_or_escaping_gitmodules_records() {
        for input in [
            "[submodule \"x\"]\npath = /tmp/x\nurl = x",
            "[submodule \"x\"]\npath = ../x\nurl = x",
            "[submodule \"x\"]\npath = lib\npath = other\nurl = x",
            "[submodule \"x\"]\npath = lib",
            "path = lib\nurl = x",
        ] {
            assert!(parse_gitmodules(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn parses_status_and_distinguishes_lifecycle_states() {
        let states = parse_submodule_status(
            " aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa vendor/lib (heads/main)\n- bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb nested/tool\n+cccccccccccccccccccccccccccccccccccccccc vendor/moved\nUdddddddddddddddddddddddddddddddddddddddd vendor/conflict\n",
        )
        .unwrap();
        assert!(states[0].initialized);
        assert_eq!(states[0].pointer, SubmodulePointer::Clean);
        assert!(!states[1].initialized);
        assert_eq!(states[1].pointer, SubmodulePointer::Unknown);
        assert_eq!(states[2].pointer, SubmodulePointer::Moved);
        assert_eq!(states[3].pointer, SubmodulePointer::Conflict);
    }

    #[test]
    fn direction_never_uses_sha_order() {
        assert_eq!(
            direction_from_ancestors(AncestorFacts {
                old_is_ancestor: Some(true),
                new_is_ancestor: Some(false),
            }),
            SubmoduleDirection::Forward
        );
        assert_eq!(
            direction_from_ancestors(AncestorFacts {
                old_is_ancestor: Some(false),
                new_is_ancestor: Some(true),
            }),
            SubmoduleDirection::Rewind
        );
        assert_eq!(
            direction_from_ancestors(AncestorFacts {
                old_is_ancestor: Some(false),
                new_is_ancestor: Some(false),
            }),
            SubmoduleDirection::Diverged
        );
        assert_eq!(
            direction_from_ancestors(AncestorFacts::default()),
            SubmoduleDirection::Unknown
        );
    }

    #[test]
    fn pointer_classification_uses_facts_for_rewind_and_divergence() {
        assert_eq!(
            classify_pointer(
                "aaaaaaaa",
                "bbbbbbbb",
                true,
                false,
                AncestorFacts {
                    old_is_ancestor: Some(false),
                    new_is_ancestor: Some(true),
                }
            ),
            SubmodulePointer::Rewind
        );
        assert_eq!(
            classify_pointer(
                "aaaaaaaa",
                "bbbbbbbb",
                true,
                false,
                AncestorFacts {
                    old_is_ancestor: Some(false),
                    new_is_ancestor: Some(false),
                }
            ),
            SubmodulePointer::Diverged
        );
    }

    #[test]
    fn parses_added_and_deleted_gitlink_fixtures() {
        let added = parse_gitlink_diff(
            "vendor/lib",
            "index 0000000..bbbbbbb 160000\n-Subproject commit 0000000\n+Subproject commit bbbbbbb\n",
        )
        .unwrap();
        assert_eq!(added.kind, SubmoduleDiffKind::Added);
        let deleted = parse_gitlink_diff(
            "vendor/lib",
            "index aaaaaaa..0000000 160000\n-Subproject commit aaaaaaa\n+Subproject commit 0000000\n",
        )
        .unwrap();
        assert_eq!(deleted.kind, SubmoduleDiffKind::Deleted);
    }

    #[test]
    fn summary_is_bounded_and_separates_unavailable_from_empty() {
        let summary = SubmoduleSummary::bounded(
            SubmoduleDirection::Forward,
            (0..40).map(|n| n.to_string()).collect(),
            4,
            false,
        );
        assert_eq!(summary.commits.len(), 4);
        assert!(summary.truncated);
        assert!(!summary.unavailable);
        let missing = SubmoduleSummary::bounded(SubmoduleDirection::Unknown, Vec::new(), 4, true);
        assert!(missing.commits.is_empty());
        assert!(missing.unavailable);
    }

    #[test]
    fn boundary_comparison_is_component_aware() {
        assert!(is_submodule_descendant("lib", "lib"));
        assert!(is_submodule_descendant("lib/src/main.rs", "lib"));
        assert!(!is_submodule_descendant("library/src/main.rs", "lib"));
        assert!(is_submodule_boundary(
            "vendor/lib/src/x.rs",
            &["vendor/lib".into()]
        ));
    }

    #[test]
    fn conflict_formatter_is_stable() {
        let conflict = SubmoduleConflict {
            path: "vendor/lib".into(),
            ours_sha: "abc1234".into(),
            theirs_sha: "def5678".into(),
        };
        assert_eq!(
            format_submodule_conflict(&conflict),
            "submodule pointer conflict: vendor/lib (abc1234 vs def5678)"
        );
    }
}
