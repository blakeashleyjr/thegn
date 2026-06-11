//! Report-file test ingestion: JUnit XML (Maven/Gradle/sbt) and TRX (.NET).
//!
//! These runners don't emit usable structured stdout, but they write machine
//! report files. After the run, the caller globs `task.report_glob` under the
//! worktree and feeds the files here. Parsing is a tolerant attribute scan — no
//! XML dependency — robust to the small, well-known shapes these tools produce.

use std::path::{Path, PathBuf};

use crate::panel::{self, TestNode, TestNodeKind, TestState};

/// Read every file matching `glob` under `worktree` and parse them into nodes.
/// `glob` is a simple `dir/.../*.ext` (optionally with `**`); that's all the
/// JVM/.NET tools need.
pub fn parse_glob(worktree: &Path, glob: &str) -> Vec<TestNode> {
    let mut nodes = Vec::new();
    for path in glob_files(worktree, glob) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        nodes.extend(parse_report(&text));
    }
    panel::tree_from_flat_tests(nodes)
}

/// Detect format by content and parse. Public for fixture tests.
pub fn parse_report(text: &str) -> Vec<TestNode> {
    if text.contains("<UnitTestResult") || text.contains("<TestRun") {
        parse_trx(text)
    } else {
        parse_junit(text)
    }
}

/// JUnit XML: `<testcase name="…" classname="…">` with optional `<failure>`,
/// `<error>`, or `<skipped/>` children; self-closing == pass.
fn parse_junit(text: &str) -> Vec<TestNode> {
    let mut nodes = Vec::new();
    for chunk in text.split("<testcase").skip(1) {
        let Some(gt) = chunk.find('>') else { continue };
        let attrs = &chunk[..gt];
        // Body up to the close tag (empty for self-closing `/>`).
        let body = chunk
            .split_once("</testcase>")
            .map(|(b, _)| b)
            .unwrap_or("");
        let name = attr(attrs, "name").unwrap_or_else(|| "<test>".into());
        let class = attr(attrs, "classname").unwrap_or_default();
        let id = if class.is_empty() {
            name.clone()
        } else {
            format!("{class}::{name}")
        };
        let (state, msg) = if body.contains("<failure") || body.contains("<error") {
            (TestState::Fail, panel::first_failure_message(body))
        } else if body.contains("<skipped") {
            (TestState::Skip, None)
        } else {
            (TestState::Pass, None)
        };
        let location = if state == TestState::Fail {
            panel::extract_locations(body).into_iter().next()
        } else {
            None
        };
        nodes.push(node(&id, state, location, msg));
    }
    nodes
}

/// TRX (.NET): `<UnitTestResult testName="…" outcome="Passed|Failed|…"/>` with
/// failure detail in a nested `<Message>`/`<StackTrace>`.
fn parse_trx(text: &str) -> Vec<TestNode> {
    let mut nodes = Vec::new();
    for chunk in text.split("<UnitTestResult").skip(1) {
        let Some(gt) = chunk.find('>') else { continue };
        let attrs = &chunk[..gt];
        let body = chunk
            .split_once("</UnitTestResult>")
            .map(|(b, _)| b)
            .unwrap_or("");
        let name = attr(attrs, "testName").unwrap_or_else(|| "<test>".into());
        let outcome = attr(attrs, "outcome").unwrap_or_default();
        let (state, msg) = match outcome.as_str() {
            "Passed" => (TestState::Pass, None),
            "NotExecuted" | "Inconclusive" => (TestState::Skip, None),
            _ => (TestState::Fail, panel::first_failure_message(body)),
        };
        let location = if state == TestState::Fail {
            panel::extract_locations(body).into_iter().next()
        } else {
            None
        };
        nodes.push(node(&name, state, location, msg));
    }
    nodes
}

fn node(
    id: &str,
    state: TestState,
    loc: Option<crate::panel::TestLocation>,
    msg: Option<String>,
) -> TestNode {
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

/// Extract a double-quoted XML attribute value (`key="value"`).
fn attr(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(xml_unescape(&rest[..end]))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Minimal glob: a literal directory prefix followed by `*.ext`, with optional
/// `**` for a recursive walk. Returns absolute paths under `worktree`.
fn glob_files(worktree: &Path, glob: &str) -> Vec<PathBuf> {
    let recursive = glob.contains("**");
    let cleaned = glob.replace("**/", "").replace("**", "");
    let (dir_part, file_part) = match cleaned.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f.to_string()),
        None => (String::new(), cleaned),
    };
    let ext = file_part.rsplit_once('.').map(|(_, e)| e.to_string());
    let base = worktree.join(&dir_part);
    let mut out = Vec::new();
    collect(&base, ext.as_deref(), recursive, &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, ext: Option<&str>, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if recursive {
                collect(&p, ext, recursive, out);
            }
            continue;
        }
        let matches = ext
            .map(|x| p.extension().and_then(|s| s.to_str()) == Some(x))
            .unwrap_or(true);
        if matches {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junit_maps_pass_fail_skip_with_classname_grouping() {
        let xml = r#"<?xml version="1.0"?>
<testsuite name="MathTests" tests="3">
  <testcase name="adds" classname="com.x.MathTests" time="0.01"/>
  <testcase name="broken" classname="com.x.MathTests" time="0.02">
    <failure message="expected 5">at com.x.MathTests.broken(MathTests.java:14)</failure>
  </testcase>
  <testcase name="wip" classname="com.x.MathTests"><skipped/></testcase>
</testsuite>"#;
        let nodes = parse_report(xml);
        let by = |id: &str| nodes.iter().find(|n| n.id == id).cloned();
        assert_eq!(by("com.x.MathTests::adds").unwrap().state, TestState::Pass);
        assert_eq!(by("com.x.MathTests::wip").unwrap().state, TestState::Skip);
        let f = by("com.x.MathTests::broken").unwrap();
        assert_eq!(f.state, TestState::Fail);
        assert_eq!(f.location.unwrap().line, 14);
    }

    #[test]
    fn trx_maps_outcomes() {
        let xml = r#"<TestRun>
  <Results>
    <UnitTestResult testName="Adds" outcome="Passed"/>
    <UnitTestResult testName="Broken" outcome="Failed">
      <Output><ErrorInfo><Message>Assert.AreEqual failed</Message></ErrorInfo></Output>
    </UnitTestResult>
    <UnitTestResult testName="Wip" outcome="NotExecuted"/>
  </Results>
</TestRun>"#;
        let nodes = parse_report(xml);
        let by = |id: &str| nodes.iter().find(|n| n.id == id).cloned();
        assert_eq!(by("Adds").unwrap().state, TestState::Pass);
        assert_eq!(by("Broken").unwrap().state, TestState::Fail);
        assert_eq!(by("Wip").unwrap().state, TestState::Skip);
    }

    #[test]
    fn glob_reads_xml_files_in_a_report_dir() {
        let dir = std::env::temp_dir().join(format!("sz-report-{}", std::process::id()));
        let reports = dir.join("target/surefire-reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(
            reports.join("TEST-a.xml"),
            r#"<testsuite><testcase name="a" classname="A"/></testsuite>"#,
        )
        .unwrap();
        std::fs::write(reports.join("ignore.txt"), "nope").unwrap();
        let nodes = parse_glob(&dir, "target/surefire-reports/*.xml");
        assert!(nodes.iter().any(|n| n.id == "A::a"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
