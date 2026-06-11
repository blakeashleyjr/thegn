//! The right panel: a tabbed Diff / PR / Checks / Tests view over the focused worktree.
//!
//! Split into two halves:
//! - [`PanelData`] — git/GitHub payload rebuilt by background hydration.
//! - [`PanelUi`] — interactive state (current tab, file cursor, Tests tree, scroll).
//!
//! Rendering lives in `chrome.rs`; this module owns data shapes and pure
//! key→intent/test parsing logic.

use serde::{Deserialize, Serialize};
use termwiz::input::KeyCode;

/// Which panel tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    #[default]
    Diff,
    Pr,
    Checks,
    Tests,
}

impl PanelTab {
    /// Cycle to the next tab (Tab key).
    #[allow(dead_code)]
    pub fn next(self) -> Self {
        match self {
            PanelTab::Diff => PanelTab::Pr,
            PanelTab::Pr => PanelTab::Checks,
            PanelTab::Checks => PanelTab::Tests,
            PanelTab::Tests => PanelTab::Diff,
        }
    }
}

/// The Diff tab has two stacked views: the file list and a single-file diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum DiffView {
    #[default]
    FileList,
    FileDiff,
}

/// A pass/fail/pending tri-state mirrored from `github::Bucket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CheckState {
    Pass,
    Fail,
    Pending,
}

/// One changed file in the Diff tab's file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub status: char,
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

/// One CI check in the Checks tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLine {
    pub name: String,
    pub state: CheckState,
}

/// A compact PR summary for the PR tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub state: String, // OPEN | CLOSED | MERGED
    pub url: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
}

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
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct TestPanelState {
    pub task: Option<TestTask>,
    pub nodes: Vec<TestNode>,
    pub summary: TestSummary,
    pub discovered: bool,
    pub discovering: bool,
    pub running: bool,
    pub stale: bool,
    pub cursor: usize,
    pub scroll: usize,
    pub filter: String,
}

impl TestPanelState {
    pub fn to_cache(&self) -> TestCache {
        TestCache {
            task: self.task.clone(),
            nodes: self.nodes.clone(),
            summary: self.summary.clone(),
            discovered: self.discovered,
        }
    }

    pub fn apply_cache(&mut self, cache: TestCache) {
        self.task = cache.task;
        self.nodes = cache.nodes;
        self.summary = cache.summary;
        self.discovered = cache.discovered;
        self.cursor = self
            .cursor
            .min(self.visible_indices().len().saturating_sub(1));
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

    pub fn recompute_summary(&mut self) {
        self.summary = summarize_nodes(&self.nodes);
        self.summary.running = self.running;
        self.summary.stale = self.stale;
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

/// The panel's data payload (git + GitHub), rebuilt on background refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelData {
    pub branch: String,
    pub files: Vec<DiffFile>,
    pub pr: Option<PrSummary>,
    pub pr_note: Option<String>,
    pub checks: Vec<CheckLine>,
}

/// The panel's interactive state, owned by the event loop.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct PanelUi {
    pub tab: PanelTab,
    pub diff_view: DiffView,
    pub diff_cursor: usize,
    pub diff_scroll: usize,
    pub file_diff: String,
    pub focused_path: String,
    pub tests: TestPanelState,
}

/// A decoded panel navigation intent. `None` means the key isn't owned by the
/// panel and should fall through to the global keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PanelNav {
    SelectTab(PanelTab),
    CycleTab,
    Up,
    Down,
    Enter,
    Back,
    Open,
    OpenEditor,
    Merge,
    Approve,
    Create,
    Rerun,
    RunAll,
    RunFailed,
    Refresh,
    Debug,
}

pub fn panel_nav_key(key: &KeyCode, tab: PanelTab, view: DiffView) -> Option<PanelNav> {
    match key {
        KeyCode::Char('1') => Some(PanelNav::SelectTab(PanelTab::Diff)),
        KeyCode::Char('2') => Some(PanelNav::SelectTab(PanelTab::Pr)),
        KeyCode::Char('3') => Some(PanelNav::SelectTab(PanelTab::Checks)),
        KeyCode::Char('4') => Some(PanelNav::SelectTab(PanelTab::Tests)),
        KeyCode::Tab => Some(PanelNav::CycleTab),
        KeyCode::UpArrow | KeyCode::Char('k') => Some(PanelNav::Up),
        KeyCode::DownArrow | KeyCode::Char('j') => Some(PanelNav::Down),
        KeyCode::Enter => match tab {
            PanelTab::Diff if view == DiffView::FileList => Some(PanelNav::Enter),
            PanelTab::Tests => Some(PanelNav::Enter),
            _ => None,
        },
        KeyCode::Escape => Some(PanelNav::Back),
        KeyCode::Char('o') => match tab {
            PanelTab::Diff | PanelTab::Pr | PanelTab::Tests => Some(PanelNav::Open),
            PanelTab::Checks => None,
        },
        KeyCode::Char('e') if tab == PanelTab::Tests => Some(PanelNav::OpenEditor),
        KeyCode::Char('m') if tab == PanelTab::Pr => Some(PanelNav::Merge),
        KeyCode::Char('a') if tab == PanelTab::Pr => Some(PanelNav::Approve),
        KeyCode::Char('c') if tab == PanelTab::Pr => Some(PanelNav::Create),
        KeyCode::Char('r') if matches!(tab, PanelTab::Pr | PanelTab::Checks | PanelTab::Tests) => {
            Some(PanelNav::Rerun)
        }
        KeyCode::Char('R') if tab == PanelTab::Tests => Some(PanelNav::RunAll),
        KeyCode::Char('f') if tab == PanelTab::Tests => Some(PanelNav::RunFailed),
        KeyCode::Char('u') if tab == PanelTab::Tests => Some(PanelNav::Refresh),
        KeyCode::Char('d') if tab == PanelTab::Tests => Some(PanelNav::Debug),
        _ => None,
    }
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
    let mut out = Vec::new();
    let mut last_group = String::new();
    for mut t in tests {
        let group = t.id.split("::").next().unwrap_or("tests").to_string();
        if group != last_group {
            out.push(TestNode {
                id: format!("group:{group}"),
                label: group.clone(),
                depth: 0,
                kind: TestNodeKind::Group,
                state: TestState::Unknown,
                location: None,
                message: None,
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

fn pytest_result(line: &str, suffix: &str) -> Option<String> {
    line.strip_suffix(suffix).map(|s| s.trim().to_string())
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
        let (path, line_part, column) = if let Some((path, line_part)) = path.rsplit_once(':') {
            (path, line_part, rest.parse::<usize>().ok())
        } else {
            (path, rest, None)
        };
        let Ok(line_no) = line_part.parse::<usize>() else {
            continue;
        };
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
    fn tab_cycles_diff_pr_checks_tests() {
        assert_eq!(PanelTab::Diff.next(), PanelTab::Pr);
        assert_eq!(PanelTab::Pr.next(), PanelTab::Checks);
        assert_eq!(PanelTab::Checks.next(), PanelTab::Tests);
        assert_eq!(PanelTab::Tests.next(), PanelTab::Diff);
    }

    #[test]
    fn digit_keys_select_tabs() {
        let d = DiffView::FileList;
        assert_eq!(
            panel_nav_key(&KeyCode::Char('1'), PanelTab::Pr, d),
            Some(PanelNav::SelectTab(PanelTab::Diff))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('2'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Pr))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('3'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Checks))
        );
        assert_eq!(
            panel_nav_key(&KeyCode::Char('4'), PanelTab::Diff, d),
            Some(PanelNav::SelectTab(PanelTab::Tests))
        );
    }

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
            nodes: vec![test_node("a::b", TestState::Pass, None, None)],
            discovered: true,
            ..Default::default()
        };
        st.recompute_summary();
        st.mark_stale();
        // Stale flag set on both state and summary; results preserved (the watch
        // path marks stale, it never spawns a run).
        assert!(st.stale && st.summary.stale);
        assert_eq!(st.nodes.len(), 1);
        assert!(st.task.is_some());
        assert!(st.discovered);
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
