//! Ingestion-agnostic test model + parsers, used by the Tests panel and the
//! `testkit` matchers. Re-exported from `panel` so `crate::panel::TestNode`
//! etc. keep resolving after the model moved out of `panel.rs`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a runner's output is turned into test results. Text scraping is the
/// fragile baseline; structured JSON and post-run report files are preferred
/// where a toolchain offers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ingestion {
    #[default]
    Text,
    Json,
    Report,
    /// Test Anything Protocol (bats, prove, busted, pgTAP, node --test, …).
    Tap,
}

/// A configured/detected test task that can be run in a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestTask {
    pub name: String,
    pub command: String,
    pub matcher: String,
    /// How to parse this task's results (default: text scraping).
    #[serde(default)]
    pub ingestion: Ingestion,
    /// For `Ingestion::Report`: a worktree-relative glob of report files to read
    /// after the run completes (e.g. `target/surefire-reports/*.xml`).
    #[serde(default)]
    pub report_glob: Option<String>,
}

impl TestTask {
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        matcher: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            matcher: matcher.into(),
            ingestion: Ingestion::Text,
            report_glob: None,
        }
    }

    // Used by the JSON/Report matchers landing in later phases.
    #[allow(dead_code)]
    pub fn with_ingestion(mut self, ingestion: Ingestion) -> Self {
        self.ingestion = ingestion;
        self
    }

    #[allow(dead_code)]
    pub fn with_report_glob(mut self, glob: impl Into<String>) -> Self {
        self.report_glob = Some(glob.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestState {
    Pass,
    Fail,
    Skip,
    Running,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestNodeKind {
    Group,
    Test,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestLocation {
    pub path: String,
    pub line: usize,
    #[serde(default)]
    pub column: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestNode {
    pub id: String,
    pub label: String,
    pub depth: usize,
    pub kind: TestNodeKind,
    pub state: TestState,
    #[serde(default)]
    pub location: Option<TestLocation>,
    #[serde(default)]
    pub message: Option<String>,
    /// A discovery-only placeholder (e.g. a cargo test *target* surfaced by
    /// `cargo metadata` before any run). Dropped once real per-test results
    /// arrive — see [`TestPanelState::merge_results`].
    #[serde(default)]
    pub placeholder: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSummary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub running: bool,
    pub stale: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl TestSummary {
    pub fn label(&self) -> String {
        if let Some(err) = &self.error {
            return format!("error: {err}");
        }
        let mut parts = Vec::new();
        if self.running {
            parts.push("running".to_string());
        }
        if self.passed > 0 {
            parts.push(format!("{} passed", self.passed));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.skipped > 0 {
            parts.push(format!("{} skipped", self.skipped));
        }
        if parts.is_empty() {
            "not run".into()
        } else {
            parts.join(" · ")
        }
    }
}

/// One completed test run, for the panel's HISTORY block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunRec {
    /// Epoch seconds when the run finished.
    pub at: i64,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub duration_ms: u64,
    #[serde(default)]
    pub branch: String,
}

/// Runs kept in [`TestCache::history`] (newest first).
pub const TEST_HISTORY_CAP: usize = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCache {
    #[serde(default)]
    pub task: Option<TestTask>,
    #[serde(default)]
    pub nodes: Vec<TestNode>,
    #[serde(default)]
    pub summary: TestSummary,
    #[serde(default)]
    pub discovered: bool,
    /// Fingerprint of the worktree's build manifests at discovery time. When it
    /// still matches on a later open, discovery is skipped entirely (no
    /// subprocess) — see `maybe_discover_tests`.
    #[serde(default)]
    pub fingerprint: String,
    /// Recent completed runs, newest first, capped at [`TEST_HISTORY_CAP`].
    #[serde(default)]
    pub history: Vec<TestRunRec>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct TestPanelState {
    pub task: Option<TestTask>,
    /// Flat source of truth: every known test keyed by id, carrying its
    /// most-recent status. Discovery seeds it; each run upserts.
    by_id: BTreeMap<String, TestNode>,
    /// Derived display tree (grouped, failed-on-top). Rebuilt on every merge.
    pub nodes: Vec<TestNode>,
    pub summary: TestSummary,
    pub discovered: bool,
    pub discovering: bool,
    pub running: bool,
    pub stale: bool,
    pub cursor: usize,
    pub scroll: usize,
    pub filter: String,
    /// Manifest fingerprint recorded at the last discovery. Compared against a
    /// freshly-computed one to decide whether discovery can be skipped.
    pub fingerprint: String,
    /// Recent completed runs, newest first (rides the cache).
    pub history: Vec<TestRunRec>,
}

impl TestPanelState {
    /// Record a completed run at the head of the history (capped).
    pub fn push_history(&mut self, rec: TestRunRec) {
        self.history.insert(0, rec);
        self.history.truncate(TEST_HISTORY_CAP);
    }

    pub fn to_cache(&self) -> TestCache {
        TestCache {
            task: self.task.clone(),
            // Persist the flat per-test source so most-recent status survives.
            nodes: self.by_id.values().cloned().collect(),
            summary: self.summary.clone(),
            discovered: self.discovered,
            fingerprint: self.fingerprint.clone(),
            history: self.history.clone(),
        }
    }

    pub fn apply_cache(&mut self, cache: TestCache) {
        self.task = cache.task;
        self.discovered = cache.discovered;
        self.fingerprint = cache.fingerprint;
        self.history = cache.history;
        self.by_id = cache
            .nodes
            .into_iter()
            .filter(|n| n.kind == TestNodeKind::Test)
            .map(|n| (n.id.clone(), n))
            .collect();
        self.refresh();
        self.cursor = self
            .cursor
            .min(self.visible_indices().len().saturating_sub(1));
    }

    /// Seed newly-discovered tests without disturbing known statuses. New ids
    /// land as `Unknown`; an existing test only gains a discovery location if it
    /// didn't have one.
    pub fn merge_discovered(&mut self, incoming: &[TestNode]) {
        // Once a run has produced real per-test rows, coarse discovery
        // placeholders (cargo metadata targets) add nothing — suppress them so
        // re-discovery (e.g. after a manifest change) doesn't resurrect them.
        let have_real = self
            .by_id
            .values()
            .any(|n| n.kind == TestNodeKind::Test && !n.placeholder);
        for n in incoming.iter().filter(|n| n.kind == TestNodeKind::Test) {
            if n.placeholder && have_real {
                continue;
            }
            self.by_id
                .entry(n.id.clone())
                .and_modify(|existing| {
                    if existing.location.is_none() && n.location.is_some() {
                        existing.location = n.location.clone();
                    }
                })
                .or_insert_with(|| TestNode {
                    state: TestState::Unknown,
                    ..n.clone()
                });
        }
        self.discovered = true;
        self.refresh();
    }

    /// Upsert run results: the latest run wins for each reported test. Real
    /// results supersede discovery placeholders (coarse `cargo metadata`
    /// targets), so the moment a run reports actual tests the target stand-ins
    /// drop out and only real per-test rows remain.
    pub fn merge_results(&mut self, incoming: &[TestNode]) {
        let real: Vec<&TestNode> = incoming
            .iter()
            .filter(|n| n.kind == TestNodeKind::Test && !n.placeholder)
            .collect();
        if !real.is_empty() {
            self.by_id.retain(|_, n| !n.placeholder);
        }
        for n in real {
            self.by_id.insert(n.id.clone(), n.clone());
        }
        self.discovered = true;
        self.stale = false;
        self.refresh();
    }

    /// Recompute the summary and rebuild the display tree (failed-on-top).
    fn refresh(&mut self) {
        let flat: Vec<TestNode> = self.by_id.values().cloned().collect();
        self.summary = summarize_nodes(&flat);
        self.summary.running = self.running;
        self.summary.stale = self.stale;
        self.nodes = tree_failed_first(flat);
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        let q = self.filter.trim().to_ascii_lowercase();
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                q.is_empty()
                    || n.label.to_ascii_lowercase().contains(&q)
                    || n.id.to_ascii_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_node(&self) -> Option<&TestNode> {
        let visible = self.visible_indices();
        visible
            .get(self.cursor)
            .and_then(|idx| self.nodes.get(*idx))
    }

    /// Mark cached results as stale (e.g. on a file change). This is the ONLY
    /// thing a file-change/watch path is allowed to do: it never starts a run or
    /// discovery — superzej never auto-runs tests. Pure: keeps task/nodes intact.
    /// Wired to the opt-in watch toggle in a later phase.
    #[allow(dead_code)]
    pub fn mark_stale(&mut self) {
        self.stale = true;
        self.summary.stale = true;
    }
}

pub fn test_name_of(test_id: &str) -> &str {
    let seg = test_id.rsplit("::").next().unwrap_or(test_id).trim();
    seg.rsplit(|c: char| c.is_whitespace() || c == '.')
        .find(|t| !t.is_empty())
        .unwrap_or(seg)
}

/// Ordered ripgrep patterns to locate a test's definition when the runner gave
/// no `file:line`. Strongest (def keyword) first, then quoted, then bare word.
/// Used by the editor/peek "open any test" fallback.
pub fn locate_regexes(test_id: &str) -> Vec<String> {
    let name = regex_escape(test_name_of(test_id));
    vec![
        format!("(fn|def|func|sub|it|test|describe|@test)[ \\t\"'(]+{name}"),
        format!("[\"']{name}[\"']"),
        format!("\\b{name}\\b"),
    ]
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn summarize_nodes(nodes: &[TestNode]) -> TestSummary {
    let mut s = TestSummary::default();
    for n in nodes.iter().filter(|n| n.kind == TestNodeKind::Test) {
        match n.state {
            TestState::Pass => s.passed += 1,
            TestState::Fail => s.failed += 1,
            TestState::Skip => s.skipped += 1,
            _ => {}
        }
    }
    s
}

pub fn tree_from_flat_tests(mut tests: Vec<TestNode>) -> Vec<TestNode> {
    if tests.is_empty() {
        return tests;
    }
    tests.sort_by(|a, b| a.id.cmp(&b.id));
    group_flat(tests)
}

/// Sort priority so failures float to the top of the explorer (and a group
/// containing a failure sorts above all-passing groups).
fn status_rank(s: TestState) -> u8 {
    match s {
        TestState::Fail => 0,
        TestState::Running => 1,
        TestState::Skip => 2,
        TestState::Unknown => 3,
        TestState::Pass => 4,
    }
}

fn group_of(id: &str) -> String {
    id.split("::").next().unwrap_or("tests").to_string()
}

/// Build the display tree with **failed on top**: groups are ordered by their
/// worst (lowest-rank) member, and tests within a group by status then id.
pub fn tree_failed_first(tests: Vec<TestNode>) -> Vec<TestNode> {
    if tests.is_empty() {
        return tests;
    }
    // Bucket tests by group, tracking each group's worst status.
    let mut groups: BTreeMap<String, Vec<TestNode>> = BTreeMap::new();
    for t in tests {
        groups.entry(group_of(&t.id)).or_default().push(t);
    }
    let mut ordered: Vec<(String, Vec<TestNode>)> = groups.into_iter().collect();
    let worst = |ts: &[TestNode]| ts.iter().map(|t| status_rank(t.state)).min().unwrap_or(3);
    ordered.sort_by(|(an, at), (bn, bt)| worst(at).cmp(&worst(bt)).then(an.cmp(bn)));

    let mut out = Vec::new();
    for (group, mut ts) in ordered {
        ts.sort_by(|a, b| {
            status_rank(a.state)
                .cmp(&status_rank(b.state))
                .then(a.id.cmp(&b.id))
        });
        out.push(TestNode {
            id: format!("group:{group}"),
            label: group,
            depth: 0,
            kind: TestNodeKind::Group,
            state: TestState::Unknown,
            location: None,
            message: None,
            placeholder: false,
        });
        for mut t in ts {
            t.depth = 1;
            out.push(t);
        }
    }
    out
}

/// Group already-sorted tests by `::` prefix, inserting group headers (preserves
/// the incoming order within each group).
fn group_flat(tests: Vec<TestNode>) -> Vec<TestNode> {
    let mut out = Vec::new();
    let mut last_group = String::new();
    for mut t in tests {
        let group = group_of(&t.id);
        if group != last_group {
            out.push(TestNode {
                id: format!("group:{group}"),
                label: group.clone(),
                depth: 0,
                kind: TestNodeKind::Group,
                state: TestState::Unknown,
                location: None,
                message: None,
                placeholder: false,
            });
            last_group = group;
        }
        t.depth = 1;
        out.push(t);
    }
    out
}

pub fn parse_test_output(output: &str) -> Vec<TestNode> {
    let locations = extract_locations(output);
    let first_location = locations.first().cloned();
    let mut nodes = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(name) = cargo_pass(trimmed) {
            nodes.push(test_node(name, TestState::Pass, None, None));
        } else if let Some(name) = cargo_fail(trimmed) {
            nodes.push(test_node(
                name,
                TestState::Fail,
                first_location.clone(),
                first_failure_message(output),
            ));
        } else if let Some(name) = pytest_result(trimmed, " PASSED") {
            nodes.push(test_node(name, TestState::Pass, None, None));
        } else if let Some(name) = pytest_result(trimmed, " FAILED") {
            nodes.push(test_node(
                name,
                TestState::Fail,
                first_location.clone(),
                first_failure_message(output),
            ));
        } else if let Some(name) = go_result(trimmed, "--- PASS:") {
            nodes.push(test_node(name, TestState::Pass, None, None));
        } else if let Some(name) = go_result(trimmed, "--- FAIL:") {
            nodes.push(test_node(
                name,
                TestState::Fail,
                first_location.clone(),
                first_failure_message(output),
            ));
        } else if let Some(name) = swift_result(trimmed, "passed") {
            nodes.push(test_node(name, TestState::Pass, None, None));
        } else if let Some(name) = swift_result(trimmed, "failed") {
            nodes.push(test_node(
                name,
                TestState::Fail,
                first_location.clone(),
                first_failure_message(output),
            ));
        } else if let Some(name) = ctest_result(trimmed, "Passed") {
            nodes.push(test_node(name, TestState::Pass, None, None));
        } else if let Some(name) = ctest_result(trimmed, "Failed") {
            nodes.push(test_node(
                name,
                TestState::Fail,
                first_location.clone(),
                first_failure_message(output),
            ));
        } else if trimmed.starts_with('✓') || trimmed.starts_with("PASS ") {
            nodes.push(test_node(
                trimmed.trim_start_matches('✓').trim(),
                TestState::Pass,
                None,
                None,
            ));
        } else if trimmed.starts_with('✗') || trimmed.starts_with("FAIL ") {
            nodes.push(test_node(
                trimmed.trim_start_matches('✗').trim(),
                TestState::Fail,
                first_location.clone(),
                first_failure_message(output),
            ));
        }
    }
    dedup_nodes(nodes)
}

/// XCTest / swift-testing line: `Test Case '-[Module.Suite testX]' passed (…)`
/// or `Test Case 'Suite.testX' failed (…)`.
fn swift_result(line: &str, verb: &str) -> Option<String> {
    let rest = line.strip_prefix("Test Case ")?;
    if !rest.contains(&format!(" {verb}")) {
        return None;
    }
    let inner = rest.trim_start_matches('\'');
    let name = inner.split('\'').next().unwrap_or(inner).trim();
    let name = name.trim_start_matches("-[").trim_end_matches(']');
    (!name.is_empty()).then(|| name.replace(' ', "."))
}

/// CTest line: `    1/3 Test #1: suite.name .......   Passed    0.01 sec`.
fn ctest_result(line: &str, verb: &str) -> Option<String> {
    if !line.contains("Test #") || !line.contains(verb) {
        return None;
    }
    let after = line.split_once(':')?.1;
    let name = after.split_whitespace().next()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn test_node(
    name: impl Into<String>,
    state: TestState,
    location: Option<TestLocation>,
    message: Option<String>,
) -> TestNode {
    let name = name.into();
    TestNode {
        id: name.clone(),
        label: name,
        depth: 0,
        kind: TestNodeKind::Test,
        state,
        location,
        message,
        placeholder: false,
    }
}

fn dedup_nodes(nodes: Vec<TestNode>) -> Vec<TestNode> {
    let mut seen = std::collections::BTreeMap::<String, TestNode>::new();
    for n in nodes {
        seen.insert(n.id.clone(), n);
    }
    seen.into_values().collect()
}

fn cargo_pass(line: &str) -> Option<String> {
    line.strip_prefix("test ")
        .and_then(|s| s.split_once(" ... ok").map(|(name, _)| name.to_string()))
}

fn cargo_fail(line: &str) -> Option<String> {
    line.strip_prefix("test ").and_then(|s| {
        s.split_once(" ... FAILED")
            .map(|(name, _)| name.to_string())
    })
}

fn pytest_result(line: &str, marker: &str) -> Option<String> {
    // pytest -v prints `path::test PASSED   [ NN%]` — the status is mid-line,
    // not a suffix. The node id before it always contains `::`, which also keeps
    // us off the `FAILED path::test - msg` short-summary line (no leading space).
    let idx = line.find(marker)?;
    let name = line[..idx].trim();
    (name.contains("::")).then(|| name.to_string())
}

fn go_result(line: &str, prefix: &str) -> Option<String> {
    line.strip_prefix(prefix)
        .map(str::trim)
        .and_then(|s| s.split_whitespace().next())
        .map(str::to_string)
}

pub fn extract_locations(output: &str) -> Vec<TestLocation> {
    output.lines().filter_map(extract_location).collect()
}

fn extract_location(line: &str) -> Option<TestLocation> {
    for token in line.split_whitespace() {
        let token = token
            .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | ',' | ';' | '\'' | '"'))
            .trim_end_matches(':');
        let Some((path, rest)) = token.rsplit_once(':') else {
            continue;
        };
        let (path, line_part, col_part) = if let Some((path, line_part)) = path.rsplit_once(':') {
            (path, line_part, Some(rest))
        } else {
            (path, rest, None)
        };
        // The numeric segments may carry trailing junk in XML/stack traces
        // (e.g. `MathTests.java:14)</failure>`); take the leading digit run.
        let line_no = match leading_usize(line_part) {
            Some(n) => n,
            None => continue,
        };
        let column = col_part.and_then(leading_usize);
        if path.is_empty() || (!path.contains('.') && !path.contains('/')) {
            continue;
        }
        return Some(TestLocation {
            path: path.to_string(),
            line: line_no,
            column,
        });
    }
    None
}

/// Parse the leading run of ASCII digits (e.g. `"14)</failure>"` → `14`).
fn leading_usize(s: &str) -> Option<usize> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

pub fn first_failure_message(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|l| {
            l.contains("assert")
                || l.contains("panicked")
                || l.contains("Error")
                || l.contains("FAILED")
        })
        .map(|s| s.chars().take(160).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_output_with_failure_location() {
        let out = "test db::roundtrip ... ok\ntest activity::quiet ... FAILED\nthread 'activity::quiet' panicked at crates/superzej-core/src/activity.rs:123:9:\nassertion failed";
        let nodes = parse_test_output(out);
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "db::roundtrip" && n.state == TestState::Pass)
        );
        let failed = nodes.iter().find(|n| n.id == "activity::quiet").unwrap();
        assert_eq!(failed.state, TestState::Fail);
        assert_eq!(failed.location.as_ref().unwrap().line, 123);
    }

    #[test]
    fn mark_stale_is_pure_and_never_clears_results() {
        let mut st = TestPanelState {
            task: Some(TestTask::new("cargo test", "cargo test", "cargo-test")),
            ..Default::default()
        };
        st.merge_results(&[test_node("a::b", TestState::Pass, None, None)]);
        st.mark_stale();
        // Stale flag set on both state and summary; results preserved (the watch
        // path marks stale, it never spawns a run).
        assert!(st.stale && st.summary.stale);
        assert_eq!(test_nodes(&st.nodes).len(), 1);
        assert!(st.task.is_some());
        assert!(st.discovered);
    }

    fn placeholder_node(id: &str) -> TestNode {
        TestNode {
            placeholder: true,
            ..test_node(id, TestState::Unknown, None, None)
        }
    }

    #[test]
    fn run_results_supersede_discovery_placeholders() {
        let mut st = TestPanelState::default();
        // Cargo metadata discovery seeds coarse target placeholders.
        st.merge_discovered(&[
            placeholder_node("demo::lib tests"),
            placeholder_node("demo::it"),
        ]);
        assert_eq!(test_nodes(&st.nodes).len(), 2, "placeholders shown pre-run");
        // A real run reports per-test rows; placeholders must drop out.
        st.merge_results(&[
            test_node("config::tests::a", TestState::Pass, None, None),
            test_node("db::tests::b", TestState::Fail, None, None),
        ]);
        let tests = test_nodes(&st.nodes);
        assert_eq!(tests.len(), 2, "only real rows remain");
        assert!(
            tests.iter().all(|n| !n.placeholder),
            "no placeholders survive a real run: {tests:?}"
        );
        // Re-discovery (e.g. after a manifest change) must not resurrect them.
        st.merge_discovered(&[placeholder_node("demo::lib tests")]);
        assert!(
            test_nodes(&st.nodes).iter().all(|n| !n.placeholder),
            "real results suppress re-seeded placeholders"
        );
    }

    #[test]
    fn fingerprint_survives_cache_roundtrip() {
        let mut st = TestPanelState {
            fingerprint: "Cargo.toml:120:1700".into(),
            ..Default::default()
        };
        st.merge_results(&[test_node("a::b", TestState::Pass, None, None)]);
        let json = serde_json::to_string(&st.to_cache()).unwrap();
        let mut restored = TestPanelState::default();
        restored.apply_cache(serde_json::from_str(&json).unwrap());
        assert_eq!(restored.fingerprint, "Cargo.toml:120:1700");
        assert!(restored.discovered);
    }

    #[test]
    fn merge_keeps_all_tests_with_most_recent_status_failed_on_top() {
        let mut st = TestPanelState::default();
        // Discovery seeds three tests as Unknown.
        st.merge_discovered(&[
            test_node("m::a", TestState::Unknown, None, None),
            test_node("m::b", TestState::Unknown, None, None),
            test_node("z::c", TestState::Unknown, None, None),
        ]);
        assert_eq!(test_nodes(&st.nodes).len(), 3);
        // A run reports only a/c; b keeps its prior (Unknown) status.
        st.merge_results(&[
            test_node("m::a", TestState::Pass, None, None),
            test_node("z::c", TestState::Fail, None, None),
        ]);
        let tests = test_nodes(&st.nodes);
        assert_eq!(tests.len(), 3, "all known tests still listed");
        assert_eq!(
            tests.iter().find(|n| n.id == "m::a").unwrap().state,
            TestState::Pass
        );
        assert_eq!(
            tests.iter().find(|n| n.id == "m::b").unwrap().state,
            TestState::Unknown,
            "untouched test retains its last status"
        );
        // Failed-on-top: group z (contains the fail) sorts before group m.
        let first_test = test_nodes(&st.nodes)[0];
        assert_eq!(first_test.id, "z::c", "failure floats to the top");
    }

    fn test_nodes(nodes: &[TestNode]) -> Vec<&TestNode> {
        nodes
            .iter()
            .filter(|n| n.kind == TestNodeKind::Test)
            .collect()
    }

    #[test]
    fn test_name_and_locate_patterns() {
        assert_eq!(test_name_of("core::config::loads"), "loads");
        assert_eq!(test_name_of("t.py::test_adds"), "test_adds");
        assert_eq!(test_name_of("Calc adds"), "adds");
        let pats = locate_regexes("m::adds");
        assert!(pats[0].contains("adds") && pats[0].contains("fn|def"));
        assert!(pats.iter().any(|p| p.contains("[\"']adds")));
    }

    #[test]
    fn parses_swift_xctest_pass_and_fail() {
        let out = "Test Case '-[AppTests.MathTests testAdds]' passed (0.001 seconds).\n\
                   Test Case '-[AppTests.MathTests testBroken]' failed (0.002 seconds).\n\
                   /x/Tests/MathTests.swift:14: error: -[AppTests.MathTests testBroken]";
        let nodes = parse_test_output(out);
        assert!(
            nodes
                .iter()
                .any(|n| n.id.contains("testAdds") && n.state == TestState::Pass)
        );
        let failed = nodes.iter().find(|n| n.id.contains("testBroken")).unwrap();
        assert_eq!(failed.state, TestState::Fail);
        assert_eq!(failed.location.as_ref().unwrap().line, 14);
    }

    #[test]
    fn parses_ctest_pass_and_fail() {
        let out = "    1/2 Test #1: math.adds .........   Passed    0.01 sec\n\
                       2/2 Test #2: math.broken ......***Failed    0.01 sec";
        let nodes = parse_test_output(out);
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "math.adds" && n.state == TestState::Pass)
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "math.broken" && n.state == TestState::Fail)
        );
    }

    #[test]
    fn tree_groups_by_prefix() {
        let nodes = tree_from_flat_tests(vec![
            test_node("core::a", TestState::Pass, None, None),
            test_node("host::b", TestState::Fail, None, None),
        ]);
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == TestNodeKind::Group && n.label == "core")
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == TestNodeKind::Group && n.label == "host")
        );
    }
}
