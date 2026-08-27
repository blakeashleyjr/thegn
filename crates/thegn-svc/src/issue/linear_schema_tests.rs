//! Contract tests for the Linear GraphQL documents, against a recorded schema.
//!
//! Every THE-72 defect was a query string no test could reach: the backend
//! selected `Issue.assignees` (which does not exist), sent state `type` values
//! Linear does not use, and emitted an argument list that would not even parse.
//! These tests are offline — the fixture is `linear_schema.json` beside this
//! file, refreshed by hand from a live introspection.

use super::*;
use serde_json::Value;

fn schema() -> Value {
    serde_json::from_str(include_str!("linear_schema.json")).expect("recorded schema parses")
}

/// Split a GraphQL selection set into `(field, Option<sub-selection>)` pairs.
/// Only what a selection set can hold — names and nested braces; arguments are
/// not selections and never appear here.
fn parse_selection(s: &str) -> Vec<(String, Option<String>)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut word = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                if !word.is_empty() {
                    out.push((std::mem::take(&mut word), None));
                }
                let start = i + 1;
                let mut depth = 1usize;
                let mut j = start;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    if depth > 0 {
                        j += 1;
                    }
                }
                let inner: String = chars[start..j.min(chars.len())].iter().collect();
                out.last_mut()
                    .expect("sub-selection must follow a field name")
                    .1 = Some(inner);
                i = j + 1;
            }
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push((std::mem::take(&mut word), None));
                }
                i += 1;
            }
            c => {
                word.push(c);
                i += 1;
            }
        }
    }
    if !word.is_empty() {
        out.push((word, None));
    }
    out
}

/// Walk `selection` against the recorded `type_name`, collecting **every**
/// unknown field rather than stopping at the first — one drifted selection is
/// usually several.
fn check_selection(sel: &str, type_name: &str, path: &str, errors: &mut Vec<String>) {
    let schema = schema();
    check_inner(&schema, sel, type_name, path, errors);
}

fn check_inner(schema: &Value, sel: &str, type_name: &str, path: &str, errors: &mut Vec<String>) {
    let Some(ty) = schema["types"].get(type_name) else {
        errors.push(format!("{path}: recorded schema has no type `{type_name}`"));
        return;
    };
    for (name, sub) in parse_selection(sel) {
        let Some(field_type) = ty.get(&name).and_then(Value::as_str) else {
            errors.push(format!(
                "{path}.{name}: `{type_name}` has no field `{name}` in the recorded Linear schema"
            ));
            continue;
        };
        if let Some(sub) = sub {
            check_inner(schema, &sub, field_type, &format!("{path}.{name}"), errors);
        }
    }
}

/// The balanced-brace group following the first occurrence of `needle`.
fn selection_after(hay: &str, needle: &str) -> String {
    let at = hay
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in document"));
    let chars: Vec<char> = hay[at..].chars().collect();
    let open = chars
        .iter()
        .position(|c| *c == '{')
        .unwrap_or_else(|| panic!("no selection set after `{needle}`"));
    let mut depth = 0usize;
    for (n, c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return chars[open + 1..n].iter().collect();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after `{needle}`");
}

fn braces_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

const ALL_STATUSES: [IssueStatus; 5] = [
    IssueStatus::Backlog,
    IssueStatus::Todo,
    IssueStatus::InProgress,
    IssueStatus::Done,
    IssueStatus::Cancelled,
];

fn recorded_state_types() -> Vec<String> {
    schema()["workflowStateTypes"]
        .as_array()
        .expect("workflowStateTypes is a list")
        .iter()
        .map(|v| v.as_str().expect("state type is a string").to_string())
        .collect()
}

fn state_of(ty: &str) -> Option<LinearState> {
    serde_json::from_value(serde_json::json!({ "type": ty })).unwrap()
}

// ---- selection contracts ----------------------------------------------------

#[test]
fn issue_fields_selection_matches_recorded_schema() {
    let mut errors = Vec::new();
    check_selection(ISSUE_FIELDS, "Issue", "Issue", &mut errors);
    assert!(
        errors.is_empty(),
        "ISSUE_FIELDS drifted:\n{}",
        errors.join("\n")
    );
}

#[test]
fn recorded_issue_type_has_no_assignees_field() {
    // THE-72: the backend selected `assignees { nodes { name } }` on every read
    // path. Linear's Issue has singular `assignee`, so the whole document was
    // rejected with GRAPHQL_VALIDATION_FAILED. This is the regression itself.
    let schema = schema();
    let issue = &schema["types"]["Issue"];
    assert!(
        issue.get("assignees").is_none(),
        "Linear's Issue has no `assignees` field — the plural is THE-72"
    );
    assert_eq!(issue["assignee"].as_str(), Some("User"));
    assert!(
        !ISSUE_FIELDS.contains("assignees"),
        "no Linear selection may name `assignees`"
    );
}

#[test]
fn get_issue_comments_selection_matches_recorded_schema() {
    let query = build_get_query("ABC-123");
    let comments = selection_after(&query, "comments");
    let mut errors = Vec::new();
    check_selection(
        &comments,
        "CommentConnection",
        "Issue.comments",
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "comments selection drifted:\n{}",
        errors.join("\n")
    );
}

#[test]
fn create_mutation_selects_the_same_fields_as_issue_fields() {
    let mutation = build_create_mutation();
    assert!(
        mutation.contains(ISSUE_FIELDS),
        "the create mutation must interpolate ISSUE_FIELDS, not duplicate it"
    );
    assert!(!mutation.contains("assignees"));
}

// ---- state-type contracts ---------------------------------------------------

#[test]
fn state_type_strings_are_recorded_linear_types() {
    let recorded = recorded_state_types();
    for s in ALL_STATUSES {
        for t in status_to_state_types(s) {
            assert!(
                recorded.iter().any(|r| r == t),
                "{s:?} filters on `{t}`, which Linear does not define"
            );
        }
        let w = status_to_write_state_type(s);
        assert!(
            recorded.iter().any(|r| r == w),
            "{s:?} writes to `{w}`, which Linear does not define"
        );
    }
    // The `cancelled`/`canceled` spelling is the whole of §1.3.
    assert_eq!(
        status_to_write_state_type(IssueStatus::Cancelled),
        "canceled"
    );
    // Backlog writes to the backlog, never to the triage intake queue.
    assert_eq!(status_to_write_state_type(IssueStatus::Backlog), "backlog");
}

#[test]
fn every_recorded_state_type_maps_deliberately() {
    // A Linear-side addition must fail here rather than fall silently into
    // `_ => Backlog`.
    let expected = [
        ("triage", IssueStatus::Backlog),
        ("backlog", IssueStatus::Backlog),
        ("unstarted", IssueStatus::Todo),
        ("started", IssueStatus::InProgress),
        ("completed", IssueStatus::Done),
        ("canceled", IssueStatus::Cancelled),
    ];
    let recorded = recorded_state_types();
    assert_eq!(
        recorded.len(),
        expected.len(),
        "recorded workflowStateTypes changed — extend map_state deliberately"
    );
    for (ty, want) in expected {
        assert!(
            recorded.iter().any(|r| r == ty),
            "`{ty}` no longer recorded"
        );
        assert_eq!(map_state(state_of(ty).as_ref()), want, "map_state({ty})");
    }
}

// ---- query-shape contracts --------------------------------------------------

#[test]
fn unfiltered_list_query_is_well_formed() {
    // The default CLI shape used to emit `issues(, first: 0, …)` — a parse
    // error, not merely a validation one.
    let q = build_list_query(&IssueFilter::default(), None);
    assert!(
        !q.contains("(,"),
        "leading comma in the argument list:\n{q}"
    );
    assert!(!q.contains(", ,"), "empty argument slot:\n{q}");
    assert!(braces_balanced(&q), "unbalanced braces:\n{q}");
    assert!(
        q.contains("first: 250"),
        "unfiltered list must ask for a full page:\n{q}"
    );
    assert!(
        !q.contains("filter:"),
        "no conditions ⇒ no filter argument:\n{q}"
    );
}

#[test]
fn backlog_filter_requests_both_backlog_and_triage() {
    let filter = IssueFilter {
        statuses: vec![IssueStatus::Backlog],
        ..Default::default()
    };
    let q = build_list_query(&filter, None);
    assert!(
        q.contains(r#"type: { in: ["backlog", "triage"] }"#),
        "bare strings, both values (a triage issue reads back as Backlog):\n{q}"
    );
    // Not the old nested-comparator shape.
    assert!(
        !q.contains("{ eq: \"backlog\" }"),
        "`in` takes [String!]:\n{q}"
    );
}

#[test]
fn overlapping_statuses_deduplicate_in_order() {
    let filter = IssueFilter {
        statuses: vec![
            IssueStatus::Backlog,
            IssueStatus::Done,
            IssueStatus::Backlog,
        ],
        ..Default::default()
    };
    let q = build_list_query(&filter, None);
    assert!(
        q.contains(r#"type: { in: ["backlog", "triage", "completed"] }"#),
        "{q}"
    );
}

#[test]
fn limit_clamps_to_the_page_maximum() {
    // 0 means "no cap" to the caller, and Linear rejects `first: 0`.
    for (limit, want) in [
        (0usize, "first: 250"),
        (10, "first: 10"),
        (9999, "first: 250"),
    ] {
        let filter = IssueFilter {
            limit,
            ..Default::default()
        };
        let list = build_list_query(&filter, None);
        assert!(list.contains(want), "list limit {limit}:\n{list}");
        let search = build_search_query("hello", limit);
        assert!(search.contains(want), "search limit {limit}:\n{search}");
    }
}

#[test]
fn team_scope_and_assignee_me_join_as_arguments() {
    let filter = IssueFilter {
        assignee_me: true,
        limit: 5,
        ..Default::default()
    };
    let q = build_list_query(&filter, Some("team-uuid"));
    assert!(q.contains(r#"assignee: { isMe: { eq: true } }"#), "{q}");
    assert!(q.contains(r#"team: { id: { eq: "team-uuid" } }"#), "{q}");
    assert!(!q.contains("(,") && !q.contains(", ,"), "{q}");
    assert!(braces_balanced(&q), "{q}");
}

#[test]
fn user_controlled_identifiers_cannot_break_out_of_the_literal() {
    // `issues.get`/`issues.update` are control-API verbs, so the id is not
    // necessarily a local user's own string. A bare `"` would close the literal
    // and graft selections onto a document sent with the user's Linear token.
    let q = build_get_query("X-1\") id url \n#");
    assert!(
        q.contains(r#"issue(id: "X-1\") id url \n#")"#),
        "identifier must be escaped, not spliced:\n{q}"
    );
    // No raw line terminator got in either — GraphQL literals cannot hold one.
    assert!(!q.contains("url \n"), "raw newline survived:\n{q}");

    // A legitimate identifier is byte-identical to the unescaped form.
    assert!(build_get_query("ABC-123").contains(r#"issue(id: "ABC-123")"#));

    // Same for the config-supplied team id.
    let q = build_list_query(&IssueFilter::default(), Some("t\" }, foo: \"x"));
    assert!(q.contains(r#"eq: "t\" }, foo: \"x""#), "{q}");
}

// ---- the tokenizer itself ---------------------------------------------------

#[test]
fn parse_selection_handles_nesting_and_reports_unknown_fields() {
    let parsed = parse_selection("a b { c { d } } e");
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0], ("a".into(), None));
    assert_eq!(parsed[1].0, "b");
    assert_eq!(parsed[2], ("e".into(), None));

    let mut errors = Vec::new();
    check_selection(
        "title assignees { nodes { name } } nope",
        "Issue",
        "Issue",
        &mut errors,
    );
    assert_eq!(errors.len(), 2, "every unknown field reported: {errors:?}");
}
