//! Default-deny tool exposure filter.
//!
//! An upstream contributes **nothing** to the proxy until its
//! `[mcp_servers.<name>.proxy] tools` list names it — the tool-poisoning
//! blast-radius control. Patterns are grants-style globs ([`glob_match`]): `*`
//! matches within a segment, `**` across; `["*"]` is the explicit everything
//! opt-in. Tool names carry no `/`, so `*` alone already matches any whole
//! name. An empty (or absent) list matches nothing — the default-deny floor.

use crate::grants::glob_match;

/// Whether a tool name is exposed by an upstream's `proxy.tools` glob list.
/// Default-deny: an empty `patterns` slice returns `false` for every name.
pub fn tool_exposed(patterns: &[String], tool: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, tool))
}

/// Split an upstream's advertised tool names into `(exposed, hidden)` by the
/// filter, preserving input order. Used by aggregation, `mcp list`, `status`
/// and doctor so the effective policy is inspectable.
pub fn partition_tools<'a>(
    patterns: &[String],
    tools: impl IntoIterator<Item = &'a str>,
) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut exposed = Vec::new();
    let mut hidden = Vec::new();
    for t in tools {
        if tool_exposed(patterns, t) {
            exposed.push(t);
        } else {
            hidden.push(t);
        }
    }
    (exposed, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_list_is_default_deny() {
        assert!(!tool_exposed(&[], "search"));
        assert!(!tool_exposed(&pats(&[]), "anything"));
    }

    #[test]
    fn star_exposes_everything() {
        let p = pats(&["*"]);
        assert!(tool_exposed(&p, "search"));
        assert!(tool_exposed(&p, "delete_page"));
        assert!(tool_exposed(&p, "a_b_c"));
    }

    #[test]
    fn prefix_glob_within_segment() {
        let p = pats(&["read_*"]);
        assert!(tool_exposed(&p, "read_page"));
        assert!(tool_exposed(&p, "read_file"));
        // Default-deny: a non-matching name is hidden.
        assert!(!tool_exposed(&p, "delete_page"));
        assert!(!tool_exposed(&p, "write_page"));
    }

    #[test]
    fn exact_and_multiple_patterns() {
        let p = pats(&["search", "list_dir"]);
        assert!(tool_exposed(&p, "search"));
        assert!(tool_exposed(&p, "list_dir"));
        assert!(!tool_exposed(&p, "search2"));
        assert!(!tool_exposed(&p, "delete"));
    }

    #[test]
    fn partition_splits_and_preserves_order() {
        let p = pats(&["read_*", "search"]);
        let (exposed, hidden) =
            partition_tools(&p, ["read_a", "delete", "search", "read_b", "write"]);
        assert_eq!(exposed, ["read_a", "search", "read_b"]);
        assert_eq!(hidden, ["delete", "write"]);
    }

    #[test]
    fn partition_default_deny_hides_all_when_unset() {
        let (exposed, hidden) = partition_tools(&[], ["a", "b"]);
        assert!(exposed.is_empty());
        assert_eq!(hidden, ["a", "b"]);
    }
}
