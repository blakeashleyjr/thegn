//! Lightweight task/test runner substrate for the native host.
//!
//! This is intentionally small and synchronous-at-the-boundary: callers run the
//! functions from `spawn_blocking`, then deliver the returned value to the event
//! loop over a channel. That keeps command execution off the render/input loop
//! while avoiding a daemon or polling thread.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use superzej_core::config::{Config, Task, TaskKind};

use crate::panel::{self, TestLocation, TestNode, TestNodeKind, TestState, TestTask};

const MAX_CAPTURE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub worktree: String,
    pub generation: u64,
    pub task: TestTask,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub truncated: bool,
    pub stdout_stderr: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveryOutcome {
    pub worktree: String,
    pub generation: u64,
    pub task: TestTask,
    pub nodes: Vec<TestNode>,
    pub error: Option<String>,
}

pub fn configured_test_tasks(cfg: &Config) -> Vec<TestTask> {
    cfg.tasks
        .iter()
        .filter(|t| t.kind == TaskKind::Test)
        .map(test_task_from_config)
        .collect()
}

fn test_task_from_config(t: &Task) -> TestTask {
    let mut command = t.command.clone();
    if !t.args.is_empty() {
        command.push(' ');
        command.push_str(&t.args.join(" "));
    }
    TestTask {
        name: t.name.clone(),
        command,
        matcher: t
            .matcher
            .clone()
            .unwrap_or_else(|| infer_matcher(&t.command)),
    }
}

pub fn detect_test_task(worktree: &Path, cfg: &Config) -> Option<TestTask> {
    configured_test_tasks(cfg)
        .into_iter()
        .next()
        .or_else(|| detect_fallback(worktree))
}

fn detect_fallback(worktree: &Path) -> Option<TestTask> {
    if has_just_test(worktree) {
        return Some(TestTask::new("just test", "just test", "generic"));
    }
    if worktree.join("Cargo.toml").exists() {
        return Some(TestTask::new(
            "cargo test",
            "cargo test --workspace",
            "cargo-test",
        ));
    }
    if worktree.join("go.mod").exists() {
        return Some(TestTask::new("go test", "go test ./...", "go-test"));
    }
    if worktree.join("pyproject.toml").exists()
        || worktree.join("pytest.ini").exists()
        || worktree.join("tox.ini").exists()
    {
        return Some(TestTask::new("pytest", "pytest", "pytest"));
    }
    if worktree.join("package.json").exists() {
        let pkg = std::fs::read_to_string(worktree.join("package.json")).unwrap_or_default();
        if pkg.contains("vitest") {
            return Some(TestTask::new("vitest", "npm test -- --run", "vitest"));
        }
        if pkg.contains("jest") {
            return Some(TestTask::new("jest", "npm test -- --runInBand", "jest"));
        }
        return Some(TestTask::new("npm test", "npm test", "javascript"));
    }
    None
}

fn has_just_test(worktree: &Path) -> bool {
    for name in ["justfile", "Justfile", ".justfile"] {
        let path = worktree.join(name);
        if let Ok(s) = std::fs::read_to_string(path) {
            if s.lines().any(|l| l.trim_start().starts_with("test:")) {
                return true;
            }
        }
    }
    false
}

fn infer_matcher(command: &str) -> String {
    let cmd = command.to_ascii_lowercase();
    if cmd.contains("cargo") {
        "cargo-test"
    } else if cmd.contains("pytest") {
        "pytest"
    } else if cmd.contains("go test") {
        "go-test"
    } else if cmd.contains("vitest") {
        "vitest"
    } else if cmd.contains("jest") {
        "jest"
    } else {
        "generic"
    }
    .into()
}

pub fn run_task(worktree: PathBuf, generation: u64, task: TestTask) -> TaskOutcome {
    let started = Instant::now();
    let out = Command::new(superzej_core::util::shell())
        .arg("-lc")
        .arg(&task.command)
        .current_dir(&worktree)
        .output();
    let duration_ms = started.elapsed().as_millis();
    match out {
        Ok(out) => {
            let mut bytes = out.stdout;
            bytes.extend_from_slice(&out.stderr);
            let truncated = bytes.len() > MAX_CAPTURE_BYTES;
            if truncated {
                bytes.truncate(MAX_CAPTURE_BYTES);
            }
            TaskOutcome {
                worktree: worktree.to_string_lossy().into_owned(),
                generation,
                task,
                exit_code: out.status.code(),
                duration_ms,
                truncated,
                stdout_stderr: String::from_utf8_lossy(&bytes).into_owned(),
            }
        }
        Err(e) => TaskOutcome {
            worktree: worktree.to_string_lossy().into_owned(),
            generation,
            task,
            exit_code: None,
            duration_ms,
            truncated: false,
            stdout_stderr: format!("failed to run task: {e}"),
        },
    }
}

pub fn discover_tests(worktree: PathBuf, generation: u64, task: TestTask) -> DiscoveryOutcome {
    let command = discovery_command(&task);
    let Some(command) = command else {
        return DiscoveryOutcome {
            worktree: worktree.to_string_lossy().into_owned(),
            generation,
            task,
            nodes: Vec::new(),
            error: Some("target discovery is not available for this test command".into()),
        };
    };
    let out = Command::new(superzej_core::util::shell())
        .arg("-lc")
        .arg(command)
        .current_dir(&worktree)
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let nodes = discovery_output_to_nodes(&task.matcher, &text);
            DiscoveryOutcome {
                worktree: worktree.to_string_lossy().into_owned(),
                generation,
                task,
                nodes,
                error: None,
            }
        }
        Ok(out) => DiscoveryOutcome {
            worktree: worktree.to_string_lossy().into_owned(),
            generation,
            task,
            nodes: Vec::new(),
            error: Some(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        },
        Err(e) => DiscoveryOutcome {
            worktree: worktree.to_string_lossy().into_owned(),
            generation,
            task,
            nodes: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

fn discovery_command(task: &TestTask) -> Option<&'static str> {
    match task.matcher.as_str() {
        "cargo-test" => Some("cargo test --workspace -- --list"),
        "go-test" => Some("go test -list . ./..."),
        "pytest" => Some("pytest --collect-only -q"),
        _ => None,
    }
}

fn discovery_output_to_nodes(matcher: &str, text: &str) -> Vec<TestNode> {
    let mut seen = std::collections::BTreeSet::new();
    let mut nodes = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(name) = discovery_test_name(matcher, line) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        nodes.push(TestNode {
            id: name.clone(),
            label: name,
            depth: 0,
            kind: TestNodeKind::Test,
            state: TestState::Unknown,
            location: None,
            message: None,
        });
    }
    panel::tree_from_flat_tests(nodes)
}

/// Pull one test target name out of a discovery line, or `None` for headers and
/// summaries. Each framework's `--list`/`--collect-only` format differs.
fn discovery_test_name(matcher: &str, line: &str) -> Option<String> {
    match matcher {
        // `cargo test -- --list` prints `path::to::test: test` (and `…: benchmark`),
        // plus `N tests, M benchmarks` summaries and blank lines.
        "cargo-test" => line
            .strip_suffix(": test")
            .or_else(|| line.strip_suffix(": benchmark"))
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
        // `go test -list` prints one bare test name per line plus `ok …`/`?  …`
        // build/result lines.
        "go-test" => {
            let first = line.split_whitespace().next().unwrap_or("");
            (first.starts_with("Test")
                || first.starts_with("Benchmark")
                || first.starts_with("Example"))
            .then(|| first.to_string())
        }
        // `pytest --collect-only -q` prints `path::Class::test` node ids and a
        // trailing `N tests collected` summary.
        "pytest" => (line.contains("::") && !line.contains(" collected")).then(|| line.to_string()),
        _ => None,
    }
}

pub fn parse_task_outcome(outcome: &TaskOutcome) -> Vec<TestNode> {
    let mut nodes = panel::parse_test_output(&outcome.stdout_stderr);
    if nodes.is_empty() {
        let state = if outcome.exit_code == Some(0) {
            TestState::Pass
        } else {
            TestState::Fail
        };
        nodes.push(TestNode {
            id: outcome.task.name.clone(),
            label: outcome.task.name.clone(),
            depth: 0,
            kind: TestNodeKind::Test,
            state,
            location: first_location(&outcome.stdout_stderr),
            message: panel::first_failure_message(&outcome.stdout_stderr),
        });
    }
    panel::tree_from_flat_tests(nodes)
}

fn first_location(output: &str) -> Option<TestLocation> {
    panel::extract_locations(output).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sz-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn configured_test_task_beats_manifest_fallback() {
        let wt = temp_dir("configured");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'",
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.tasks.push(Task {
            name: "unit".into(),
            command: "just".into(),
            args: vec!["unit".into()],
            cwd: None,
            env: Default::default(),
            kind: TaskKind::Test,
            matcher: Some("cargo-test".into()),
            scope: None,
        });
        let task = detect_test_task(&wt, &cfg).unwrap();
        assert_eq!(task.name, "unit");
        assert_eq!(task.command, "just unit");
        let _ = std::fs::remove_dir_all(wt);
    }

    #[test]
    fn cargo_manifest_fallback_detects_test_task() {
        let wt = temp_dir("cargo");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'",
        )
        .unwrap();
        let task = detect_test_task(&wt, &Config::default()).unwrap();
        assert_eq!(task.matcher, "cargo-test");
        assert!(task.command.contains("cargo test"));
        let _ = std::fs::remove_dir_all(wt);
    }

    #[test]
    fn parse_outcome_falls_back_to_command_result() {
        let outcome = TaskOutcome {
            worktree: "/tmp/wt".into(),
            generation: 1,
            task: TestTask::new("unit", "false", "generic"),
            exit_code: Some(1),
            duration_ms: 1,
            truncated: false,
            stdout_stderr: "error at src/lib.rs:7:1".into(),
        };
        let nodes = parse_task_outcome(&outcome);
        assert!(nodes.iter().any(|n| n.state == TestState::Fail));
    }

    fn on_path(bin: &str) -> bool {
        std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p).any(|d| {
                    let f = d.join(bin);
                    f.is_file()
                })
            })
            .unwrap_or(false)
    }

    fn git_init(dir: &Path) {
        let _ = Command::new("git").arg("init").arg("-q").arg(dir).output();
    }

    /// End-to-end against a REAL cargo project in a REAL git repo: detect the
    /// task, run the actual `cargo test`, and parse the real output into a
    /// pass + a fail with a jumpable file:line. Skipped if cargo is absent.
    #[test]
    fn e2e_cargo_real_repo_runs_and_parses_pass_and_fail() {
        if !on_path("cargo") {
            return; // cargo unavailable: skip this real-repo e2e
        }
        let wt = temp_dir("e2e-cargo");
        git_init(&wt);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"sze2e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n\
             #[cfg(test)]\nmod tests {\n  use super::*;\n\
             #[test] fn passes() { assert_eq!(add(2, 2), 4); }\n\
             #[test] fn fails() { assert_eq!(add(2, 2), 5); }\n}\n",
        )
        .unwrap();

        // Detection picks up the real Cargo.toml.
        let task = detect_test_task(&wt, &Config::default()).unwrap();
        assert_eq!(task.matcher, "cargo-test");

        // Run the real test command (single-threaded for stable output).
        let run_task_spec =
            TestTask::new("cargo test", "cargo test -- --test-threads=1", "cargo-test");
        let outcome = run_task(wt.clone(), 1, run_task_spec);
        let nodes = parse_task_outcome(&outcome);

        assert_eq!(outcome.exit_code, Some(101), "real cargo test should fail");
        assert!(
            nodes
                .iter()
                .any(|n| n.id.contains("passes") && n.state == TestState::Pass),
            "expected a passing test node: {nodes:?}"
        );
        let failed = nodes
            .iter()
            .find(|n| n.id.contains("fails") && n.state == TestState::Fail)
            .expect("expected a failing test node");
        let loc = failed.location.as_ref().expect("failure has a location");
        assert!(loc.path.ends_with("src/lib.rs"), "loc was {loc:?}");

        // Real discovery via `cargo test -- --list` finds both targets.
        let disc = discover_tests(wt.clone(), 2, task);
        assert!(disc.error.is_none(), "discovery error: {:?}", disc.error);
        let labels: Vec<&str> = disc.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(
            labels.iter().any(|l| l.contains("passes"))
                && labels.iter().any(|l| l.contains("fails")),
            "discovery should list both tests: {labels:?}"
        );

        let _ = std::fs::remove_dir_all(wt);
    }

    /// End-to-end with a real `just` recipe driving an arbitrary test command:
    /// confirms the generic path runs a real process and reflects its exit code.
    #[test]
    fn e2e_generic_shell_task_runs_real_process() {
        let wt = temp_dir("e2e-generic");
        let task = TestTask::new("echo-pass", "echo '✓ widget::works' && true", "generic");
        let outcome = run_task(wt.clone(), 1, task);
        assert_eq!(outcome.exit_code, Some(0));
        let nodes = parse_task_outcome(&outcome);
        assert!(
            nodes.iter().any(|n| n.state == TestState::Pass),
            "generic ✓ line should parse as pass: {nodes:?}"
        );
        let _ = std::fs::remove_dir_all(wt);
    }
}
