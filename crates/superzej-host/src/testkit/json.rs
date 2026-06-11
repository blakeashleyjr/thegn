//! Structured (JSON) test-result ingestion.
//!
//! Far more reliable than scraping human-readable stdout: each runner emits one
//! JSON object per event, so pass/fail/skip and (often) precise `file:line`
//! come straight from the tool. Parsers are tolerant — unknown fields are
//! ignored and malformed lines are skipped, never panicking.

use serde_json::Value;

use crate::panel::{self, TestLocation, TestNode, TestNodeKind, TestState};

/// Parse a JSON event stream for `matcher` into test nodes.
pub fn parse(matcher: &str, text: &str) -> Vec<TestNode> {
    let nodes = match matcher {
        "dart" | "flutter" => parse_dart(text),
        "rspec" => parse_rspec(text),
        // nextest `--message-format libtest-json[-plus]` and cargo libtest
        // `--format json` share the per-test object shape.
        _ => parse_libtest(text),
    };
    panel::tree_from_flat_tests(nodes)
}

fn node(id: &str, state: TestState, loc: Option<TestLocation>, msg: Option<String>) -> TestNode {
    TestNode {
        id: id.to_string(),
        label: id.to_string(),
        depth: 0,
        kind: TestNodeKind::Test,
        state,
        location: loc,
        message: msg,
    }
}

/// libtest / nextest JSON: one object per line; `{ "type":"test",
/// "name":"…", "event":"ok"|"failed"|"ignored", "stdout":"…" }`.
fn parse_libtest(text: &str) -> Vec<TestNode> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("test") {
            continue;
        }
        let Some(name) = v.get("name").and_then(Value::as_str) else {
            continue;
        };
        let event = v.get("event").and_then(Value::as_str).unwrap_or("");
        let (state, msg, loc) = match event {
            "ok" => (TestState::Pass, None, None),
            "ignored" => (TestState::Skip, None, None),
            "failed" => {
                let stdout = v.get("stdout").and_then(Value::as_str).unwrap_or("");
                (
                    TestState::Fail,
                    panel::first_failure_message(stdout),
                    panel::extract_locations(stdout).into_iter().next(),
                )
            }
            _ => continue, // "started" and others: not a terminal result
        };
        out.push(node(name, state, loc, msg));
    }
    out
}

/// Dart/Flutter `test --reporter json`: `testStart` carries name + `url`/`line`,
/// `testDone` carries the result, `error` carries the failure message.
fn parse_dart(text: &str) -> Vec<TestNode> {
    use std::collections::BTreeMap;
    struct Pending {
        name: String,
        loc: Option<TestLocation>,
        state: TestState,
        msg: Option<String>,
    }
    let mut by_id: BTreeMap<i64, Pending> = BTreeMap::new();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("testStart") => {
                let t = &v["test"];
                let Some(id) = t.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let name = t
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("<test>")
                    .to_string();
                // Skip dart's synthetic "loading …" harness tests.
                if name.starts_with("loading ") {
                    continue;
                }
                let loc = dart_location(t);
                by_id.insert(
                    id,
                    Pending {
                        name,
                        loc,
                        state: TestState::Unknown,
                        msg: None,
                    },
                );
            }
            Some("testDone") => {
                let Some(id) = v.get("testID").and_then(Value::as_i64) else {
                    continue;
                };
                if let Some(p) = by_id.get_mut(&id) {
                    let hidden = v.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                    if hidden {
                        by_id.remove(&id);
                        continue;
                    }
                    let skipped = v.get("skipped").and_then(Value::as_bool).unwrap_or(false);
                    let result = v.get("result").and_then(Value::as_str).unwrap_or("");
                    p.state = if skipped {
                        TestState::Skip
                    } else if result == "success" {
                        TestState::Pass
                    } else {
                        TestState::Fail
                    };
                }
            }
            Some("error") => {
                let Some(id) = v.get("testID").and_then(Value::as_i64) else {
                    continue;
                };
                if let Some(p) = by_id.get_mut(&id) {
                    let err = v.get("error").and_then(Value::as_str).unwrap_or("");
                    if !err.is_empty() {
                        p.msg = Some(err.chars().take(160).collect());
                    }
                    if p.loc.is_none() {
                        let trace = v.get("stackTrace").and_then(Value::as_str).unwrap_or("");
                        p.loc = panel::extract_locations(trace).into_iter().next();
                    }
                    p.state = TestState::Fail;
                }
            }
            _ => {}
        }
    }
    by_id
        .into_values()
        .filter(|p| p.state != TestState::Unknown)
        .map(|p| node(&p.name, p.state, p.loc, p.msg))
        .collect()
}

/// `nix flake show --json`: enumerate `checks.<system>.<name>` as discovery
/// targets. Node id is `<system>::<name>` so the tree groups by system; the
/// runner reconstructs `.#checks.<system>.<name>` from it.
pub fn parse_nix_flake_show(text: &str) -> Vec<TestNode> {
    let Ok(doc) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(systems) = doc.get("checks").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (system, checks) in systems {
        let Some(names) = checks.as_object() else {
            continue;
        };
        for name in names.keys() {
            let id = format!("{system}::{name}");
            out.push(TestNode {
                id: id.clone(),
                label: name.clone(),
                depth: 0,
                kind: TestNodeKind::Test,
                state: TestState::Unknown,
                location: None,
                message: None,
            });
        }
    }
    panel::tree_from_flat_tests(out)
}

/// RSpec `--format json`: a single document with an `examples` array, each
/// `{ description, status, file_path, line_number, exception }`.
fn parse_rspec(text: &str) -> Vec<TestNode> {
    let start = match text.find('{') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text[start..]) else {
        return Vec::new();
    };
    let Some(examples) = doc.get("examples").and_then(Value::as_array) else {
        return Vec::new();
    };
    examples
        .iter()
        .map(|ex| {
            let name = ex
                .get("full_description")
                .or_else(|| ex.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("<example>")
                .to_string();
            let status = ex.get("status").and_then(Value::as_str).unwrap_or("");
            let state = match status {
                "passed" => TestState::Pass,
                "pending" => TestState::Skip,
                _ => TestState::Fail,
            };
            let loc = match (
                ex.get("file_path").and_then(Value::as_str),
                ex.get("line_number").and_then(Value::as_i64),
            ) {
                (Some(p), Some(l)) => Some(TestLocation {
                    path: p.trim_start_matches("./").to_string(),
                    line: l as usize,
                    column: None,
                }),
                _ => None,
            };
            let msg = ex
                .get("exception")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(|m| m.chars().take(160).collect());
            node(
                &name,
                state,
                if state == TestState::Fail { loc } else { None },
                msg,
            )
        })
        .collect()
}

fn dart_location(t: &Value) -> Option<TestLocation> {
    let url = t.get("url").and_then(Value::as_str)?;
    let path = url.strip_prefix("file://").unwrap_or(url).to_string();
    let line = t.get("line").and_then(Value::as_i64)? as usize;
    let column = t.get("column").and_then(Value::as_i64).map(|c| c as usize);
    Some(TestLocation { path, line, column })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libtest_json_maps_ok_failed_ignored_with_location() {
        let text = r#"
{ "type": "suite", "event": "started", "test_count": 3 }
{ "type": "test", "event": "ok", "name": "tests::passes" }
{ "type": "test", "event": "ignored", "name": "tests::wip" }
{ "type": "test", "event": "failed", "name": "tests::fails", "stdout": "thread 'tests::fails' panicked at src/lib.rs:6:5:\nassertion failed" }
{ "type": "suite", "event": "failed", "passed": 1, "failed": 1 }
"#;
        let nodes = parse("nextest", text);
        let by = |id: &str| nodes.iter().find(|n| n.id == id).cloned();
        assert_eq!(by("tests::passes").unwrap().state, TestState::Pass);
        assert_eq!(by("tests::wip").unwrap().state, TestState::Skip);
        let f = by("tests::fails").unwrap();
        assert_eq!(f.state, TestState::Fail);
        assert_eq!(f.location.unwrap().line, 6);
    }

    #[test]
    fn dart_json_pairs_start_and_done_with_url_location() {
        let text = r#"
{"type":"testStart","test":{"id":1,"name":"loading /x/test/foo_test.dart","url":null}}
{"type":"testDone","testID":1,"result":"success","hidden":true}
{"type":"testStart","test":{"id":2,"name":"adds numbers","url":"file:///x/test/foo_test.dart","line":3,"column":5}}
{"type":"testDone","testID":2,"result":"success","skipped":false,"hidden":false}
{"type":"testStart","test":{"id":3,"name":"is broken","url":"file:///x/test/foo_test.dart","line":7,"column":5}}
{"type":"error","testID":3,"error":"Expected: 5 Actual: 4","stackTrace":"test/foo_test.dart 8:9"}
{"type":"testDone","testID":3,"result":"error","hidden":false}
"#;
        let nodes = parse("dart", text);
        // The hidden "loading" harness test is dropped.
        assert!(!nodes.iter().any(|n| n.label.starts_with("loading ")));
        let pass = nodes.iter().find(|n| n.id == "adds numbers").unwrap();
        assert_eq!(pass.state, TestState::Pass);
        assert_eq!(pass.location.as_ref().unwrap().line, 3);
        let fail = nodes.iter().find(|n| n.id == "is broken").unwrap();
        assert_eq!(fail.state, TestState::Fail);
        assert!(fail.message.as_deref().unwrap().contains("Expected"));
    }

    #[test]
    fn rspec_json_maps_examples_with_location() {
        let text = r#"{"version":"3.12","examples":[
          {"description":"adds","full_description":"Calc adds","status":"passed","file_path":"./spec/calc_spec.rb","line_number":4},
          {"description":"breaks","full_description":"Calc breaks","status":"failed","file_path":"./spec/calc_spec.rb","line_number":8,"exception":{"message":"expected 5 got 4"}},
          {"description":"todo","full_description":"Calc todo","status":"pending","file_path":"./spec/calc_spec.rb","line_number":12}],
          "summary_line":"3 examples, 1 failure, 1 pending"}"#;
        let nodes = parse("rspec", text);
        let by = |id: &str| nodes.iter().find(|n| n.id == id).cloned();
        assert_eq!(by("Calc adds").unwrap().state, TestState::Pass);
        assert_eq!(by("Calc todo").unwrap().state, TestState::Skip);
        let f = by("Calc breaks").unwrap();
        assert_eq!(f.state, TestState::Fail);
        assert_eq!(f.location.unwrap().line, 8);
    }

    #[test]
    fn nix_flake_show_enumerates_checks_per_system() {
        let text = r#"{
          "checks": {
            "x86_64-linux": {
              "build": {"type":"derivation","name":"build"},
              "lint":  {"type":"derivation","name":"lint"}
            }
          },
          "packages": {"x86_64-linux": {"default": {"type":"derivation"}}}
        }"#;
        let nodes = parse_nix_flake_show(text);
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"x86_64-linux::build"));
        assert!(ids.contains(&"x86_64-linux::lint"));
        // No packages leak in as checks.
        assert!(!ids.iter().any(|i| i.contains("default")));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let nodes = parse(
            "nextest",
            "not json\n{bad\n{ \"type\":\"test\",\"event\":\"ok\",\"name\":\"a\" }",
        );
        // One real test (plus its group header from tree_from_flat_tests).
        assert_eq!(
            nodes
                .iter()
                .filter(|n| n.kind == TestNodeKind::Test)
                .count(),
            1
        );
    }
}
