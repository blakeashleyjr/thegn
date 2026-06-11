//! Lightweight task/test runner substrate for the native host.
//!
//! This is intentionally small and synchronous-at-the-boundary: callers run the
//! functions from `spawn_blocking`, then deliver the returned value to the event
//! loop over a channel. That keeps command execution off the render/input loop
//! while avoiding a daemon or polling thread.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use superzej_core::config::{Config, LimitsConfig, Task, TaskKind};

use crate::panel::{self, TestLocation, TestNode, TestNodeKind, TestState, TestTask};

const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// How a capped child gets its CPU/mem ceiling. Resolved once from `PATH`;
/// `wrap_capped` is pure over it so it can be unit-tested deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapBackend {
    /// `systemd-run --user --scope` with `CPUQuota`/`MemoryMax`/`Nice`.
    Systemd,
    /// `nice -n N` (and `ionice -c3` when present) — no hard cgroup cap.
    Nice,
    /// No wrapper available; run bare.
    None,
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

pub fn detect_cap_backend() -> CapBackend {
    if on_path("systemd-run") {
        CapBackend::Systemd
    } else if on_path("nice") {
        CapBackend::Nice
    } else {
        CapBackend::None
    }
}

fn limits_disabled(limits: &LimitsConfig) -> bool {
    limits.test_cpu_quota.trim().is_empty()
        && limits.test_mem_max.trim().is_empty()
        && limits.test_nice == 0
}

/// Wrap `argv` so an explicit test run is CPU/memory-capped and can't pin the
/// machine. Pure over `backend` for testability. Returns `argv` unchanged when
/// all limits are disabled or no backend is available.
pub fn wrap_capped(argv: &[String], limits: &LimitsConfig, backend: CapBackend) -> Vec<String> {
    if limits_disabled(limits) {
        return argv.to_vec();
    }
    match backend {
        CapBackend::Systemd => {
            let mut v = vec![
                "systemd-run".to_string(),
                "--user".into(),
                "--scope".into(),
                "--quiet".into(),
                "--collect".into(),
            ];
            if !limits.test_cpu_quota.trim().is_empty() {
                v.push("-p".into());
                v.push(format!("CPUQuota={}", limits.test_cpu_quota.trim()));
            }
            if !limits.test_mem_max.trim().is_empty() {
                v.push("-p".into());
                v.push(format!("MemoryMax={}", limits.test_mem_max.trim()));
            }
            if limits.test_nice != 0 {
                v.push("--nice".into());
                v.push(limits.test_nice.to_string());
            }
            v.push("--".into());
            v.extend_from_slice(argv);
            v
        }
        CapBackend::Nice => {
            let mut v = Vec::new();
            if on_path("ionice") {
                v.extend(["ionice".into(), "-c3".into()]);
            }
            if limits.test_nice != 0 {
                v.extend(["nice".into(), "-n".into(), limits.test_nice.to_string()]);
            }
            if v.is_empty() {
                return argv.to_vec();
            }
            v.extend_from_slice(argv);
            v
        }
        CapBackend::None => argv.to_vec(),
    }
}

/// Live child registry for single-flight + real cancellation. Keyed by a logical
/// slot (`"<worktree>:run"` / `"<worktree>:disc"`); the value is `(generation,
/// process-group id)`. Starting a newer job in the same slot kills the older
/// group so a superseded `cargo test` stops burning CPU immediately.
fn registry() -> &'static Mutex<HashMap<String, (u64, i32)>> {
    static R: OnceLock<Mutex<HashMap<String, (u64, i32)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(unix)]
fn kill_group(pgid: i32) {
    // Negative pid would also work via `kill(2)`; killpg is explicit.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: i32) {}

/// Kill whatever is currently registered in `slot` (used when a newer job
/// supersedes an in-flight one). Public for the supersede path in `run.rs`.
pub fn cancel_slot(slot: &str) {
    if let Ok(mut map) = registry().lock() {
        if let Some((_, pgid)) = map.remove(slot) {
            kill_group(pgid);
        }
    }
}

/// Run `command` in `worktree` under a CPU/mem cap and a single-flight `slot`,
/// capturing bounded combined stdout+stderr. Kills any older job in the slot
/// first. Returns `(exit_code, truncated, captured)`.
fn run_capped(
    command: &str,
    worktree: &Path,
    limits: &LimitsConfig,
    slot: &str,
    generation: u64,
) -> (Option<i32>, bool, String) {
    let inner = vec![
        superzej_core::util::shell(),
        "-lc".to_string(),
        command.to_string(),
    ];
    let argv = wrap_capped(&inner, limits, detect_cap_backend());

    // Supersede any older job in this slot.
    cancel_slot(slot);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (None, false, format!("failed to spawn task: {e}")),
    };
    let pgid = child.id() as i32;
    if let Ok(mut map) = registry().lock() {
        map.insert(slot.to_string(), (generation, pgid));
    }

    // Read stdout and stderr concurrently on threads: reading one to EOF while
    // the child fills the other pipe's buffer would deadlock. Each stream is
    // capped, so a chatty suite can't blow memory.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(o) = stdout {
            let _ = o.take(MAX_CAPTURE_BYTES as u64).read_to_end(&mut b);
        }
        b
    });
    let err_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(e) = stderr {
            let _ = e.take(MAX_CAPTURE_BYTES as u64).read_to_end(&mut b);
        }
        b
    });
    let status = child.wait();
    let mut buf = out_h.join().unwrap_or_default();
    let errbuf = err_h.join().unwrap_or_default();
    let truncated = buf.len() >= MAX_CAPTURE_BYTES || errbuf.len() >= MAX_CAPTURE_BYTES;
    buf.extend_from_slice(&errbuf);
    if buf.len() > MAX_CAPTURE_BYTES {
        buf.truncate(MAX_CAPTURE_BYTES);
    }

    // Deregister iff we still own the slot (a newer job may have replaced us).
    if let Ok(mut map) = registry().lock() {
        if map
            .get(slot)
            .map(|(g, _)| *g == generation)
            .unwrap_or(false)
        {
            map.remove(slot);
        }
    }

    let exit_code = status.ok().and_then(|s| s.code());
    (
        exit_code,
        truncated,
        String::from_utf8_lossy(&buf).into_owned(),
    )
}

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
    let matcher = t
        .matcher
        .clone()
        .unwrap_or_else(|| infer_matcher(&t.command));
    let ingestion = ingestion_for_matcher(&matcher);
    TestTask {
        name: t.name.clone(),
        command,
        matcher,
        ingestion,
        report_glob: None,
    }
}

/// Default ingestion mode for a matcher id. Text is the safe baseline; JSON/
/// Report matchers (added in later phases) override this.
fn ingestion_for_matcher(matcher: &str) -> crate::panel::Ingestion {
    use crate::panel::Ingestion;
    match matcher {
        "nextest" | "libtest-json" | "dart" | "flutter" | "deno" | "bun" | "rspec" => {
            Ingestion::Json
        }
        "junit" | "trx" | "gradle" | "maven" | "dotnet" | "sbt" => Ingestion::Report,
        _ => Ingestion::Text,
    }
}

pub fn detect_test_task(worktree: &Path, cfg: &Config) -> Option<TestTask> {
    configured_test_tasks(cfg)
        .into_iter()
        .next()
        .or_else(|| detect_fallback(worktree))
}

fn detect_fallback(worktree: &Path) -> Option<TestTask> {
    use crate::panel::Ingestion;
    let has = |f: &str| worktree.join(f).exists();

    if has_just_test(worktree) {
        return Some(TestTask::new("just test", "just test", "generic"));
    }
    if has("Cargo.toml") {
        // Prefer nextest (structured JSON + timing) when it's installed.
        if on_path("cargo-nextest") {
            // libtest-json is gated behind an experimental env flag in nextest;
            // the inline assignment is fine because we run via `sh -lc`.
            return Some(
                TestTask::new(
                    "cargo nextest",
                    "NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 \
                     cargo nextest run --message-format libtest-json",
                    "nextest",
                )
                .with_ingestion(Ingestion::Json),
            );
        }
        return Some(TestTask::new(
            "cargo test",
            "cargo test --workspace",
            "cargo-test",
        ));
    }
    if has("go.mod") {
        return Some(TestTask::new("go test", "go test ./...", "go-test"));
    }
    if has("pyproject.toml") || has("pytest.ini") || has("tox.ini") {
        return Some(TestTask::new("pytest", "pytest", "pytest"));
    }
    if has("pubspec.yaml") {
        let pubspec = std::fs::read_to_string(worktree.join("pubspec.yaml")).unwrap_or_default();
        let is_flutter = pubspec.contains("flutter:") || pubspec.contains("sdk: flutter");
        return Some(if is_flutter {
            TestTask::new("flutter test", "flutter test --reporter json", "flutter")
                .with_ingestion(Ingestion::Json)
        } else {
            TestTask::new("dart test", "dart test --reporter json", "dart")
                .with_ingestion(Ingestion::Json)
        });
    }
    if has("Package.swift") {
        return Some(TestTask::new("swift test", "swift test", "swift"));
    }
    if has("mix.exs") {
        return Some(TestTask::new("mix test", "mix test", "elixir"));
    }
    if has("CMakeLists.txt") {
        // CTest runs the suite; ctest's stdout is a stable per-test text format.
        return Some(TestTask::new("ctest", "ctest --output-on-failure", "ctest"));
    }
    // JVM / .NET: stdout is unusable, but each writes machine report files we
    // parse after the run (JUnit XML / TRX).
    if has("pom.xml") {
        return Some(
            TestTask::new("maven", "mvn -q test", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("target/surefire-reports/*.xml"),
        );
    }
    if has("build.gradle") || has("build.gradle.kts") {
        return Some(
            TestTask::new("gradle", "gradle test", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("build/test-results/**/*.xml"),
        );
    }
    if has("build.sbt") {
        return Some(
            TestTask::new("sbt", "sbt test", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("target/test-reports/*.xml"),
        );
    }
    if has_dotnet_project(worktree) {
        return Some(
            TestTask::new(
                "dotnet test",
                "dotnet test --logger \"trx;LogFileName=sz.trx\"",
                "trx",
            )
            .with_ingestion(Ingestion::Report)
            .with_report_glob("TestResults/*.trx"),
        );
    }
    // Tier C — text scrapers / lighter integrations.
    if has("rebar.config") {
        return Some(TestTask::new("rebar3 eunit", "rebar3 eunit", "erlang"));
    }
    if has("build.zig") {
        return Some(TestTask::new("zig build test", "zig build test", "zig"));
    }
    if has(".rspec") {
        return Some(
            TestTask::new("rspec", "rspec --format json", "rspec").with_ingestion(Ingestion::Json),
        );
    }
    if has("phpunit.xml") || has("phpunit.xml.dist") {
        return Some(
            TestTask::new("phpunit", "phpunit --log-junit target/phpunit.xml", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("target/phpunit.xml"),
        );
    }
    if has("Gemfile") {
        // No `.rspec` → assume minitest via rake (sparse text; synthetic result).
        return Some(TestTask::new("rake test", "rake test", "ruby"));
    }
    if has("dub.json") || has("dub.sdl") {
        return Some(TestTask::new("dub test", "dub test", "d"));
    }
    if has("package.json") {
        let pkg = std::fs::read_to_string(worktree.join("package.json")).unwrap_or_default();
        if pkg.contains("vitest") {
            return Some(TestTask::new("vitest", "npm test -- --run", "vitest"));
        }
        if pkg.contains("jest") {
            return Some(TestTask::new("jest", "npm test -- --runInBand", "jest"));
        }
        return Some(TestTask::new("npm test", "npm test", "javascript"));
    }
    // Lowest priority: a pure-Nix repo with no language manifest. A polyglot
    // repo (e.g. Cargo.toml + flake.nix) still uses its language runner above.
    if has("flake.nix") {
        return Some(TestTask::new(
            "flake checks",
            "nix flake check -L",
            "nix-flake",
        ));
    }
    None
}

fn has_dotnet_project(worktree: &Path) -> bool {
    std::fs::read_dir(worktree)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|ext| ext == "sln" || ext == "csproj")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
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

pub fn run_task(
    worktree: PathBuf,
    generation: u64,
    task: TestTask,
    limits: &LimitsConfig,
) -> TaskOutcome {
    let started = Instant::now();
    let slot = format!("{}:run", worktree.display());
    let (exit_code, truncated, stdout_stderr) =
        run_capped(&task.command, &worktree, limits, &slot, generation);
    TaskOutcome {
        worktree: worktree.to_string_lossy().into_owned(),
        generation,
        task,
        exit_code,
        duration_ms: started.elapsed().as_millis(),
        truncated,
        stdout_stderr,
    }
}

pub fn discover_tests(
    worktree: PathBuf,
    generation: u64,
    task: TestTask,
    limits: &LimitsConfig,
) -> DiscoveryOutcome {
    let wt = worktree.to_string_lossy().into_owned();
    let Some(command) = discovery_command(&task) else {
        return DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes: Vec::new(),
            error: Some("target discovery is not available for this test command".into()),
        };
    };
    let slot = format!("{}:disc", worktree.display());
    let (exit_code, _truncated, text) = run_capped(command, &worktree, limits, &slot, generation);
    if exit_code == Some(0) {
        let nodes = if task.matcher == "nix-flake" {
            crate::testkit::json::parse_nix_flake_show(&text)
        } else {
            discovery_output_to_nodes(&task.matcher, &text)
        };
        DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes,
            error: None,
        }
    } else {
        DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes: Vec::new(),
            error: Some(text.trim().chars().take(200).collect()),
        }
    }
}

fn discovery_command(task: &TestTask) -> Option<&'static str> {
    match task.matcher.as_str() {
        // `cargo test --list` lists tests regardless of nextest.
        "cargo-test" | "nextest" => Some("cargo test --workspace -- --list"),
        "go-test" => Some("go test -list . ./..."),
        "pytest" => Some("pytest --collect-only -q"),
        "swift" => Some("swift test --list-tests"),
        // `ctest -N` ("show only") prints `Test #N: name` without running.
        "ctest" => Some("ctest -N"),
        // Enumerate flake checks as targets (JSON, parsed specially).
        "nix-flake" => Some("nix flake show --json"),
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
        "cargo-test" | "nextest" => line
            .strip_suffix(": test")
            .or_else(|| line.strip_suffix(": benchmark"))
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
        // `swift test --list-tests` prints `Suite/testMethod` per line.
        "swift" => (line.contains('/') && !line.contains(' ')).then(|| line.to_string()),
        // `ctest -N` prints `  Test #3: suite.name`; ignore the `Total Tests` line.
        "ctest" => line
            .split_once(':')
            .filter(|(head, _)| head.trim_start().starts_with("Test #"))
            .map(|(_, name)| name.trim().to_string())
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
    use crate::panel::Ingestion;
    // Structured ingestion is preferred where the runner provides it; text
    // scraping is the fragile fallback. Report-file ingestion lands in a later
    // phase (parsed from disk by the caller, not from captured stdout).
    if outcome.task.ingestion == Ingestion::Json {
        let nodes = crate::testkit::json::parse(&outcome.task.matcher, &outcome.stdout_stderr);
        if !nodes.is_empty() {
            return nodes;
        }
        // Fall through to the synthetic single-node result below on empty JSON
        // (e.g. the runner failed before emitting any events).
    }
    if outcome.task.ingestion == Ingestion::Report {
        if let Some(glob) = &outcome.task.report_glob {
            let nodes = crate::testkit::report::parse_glob(Path::new(&outcome.worktree), glob);
            if !nodes.is_empty() {
                return nodes;
            }
        }
        // No report files (build failed before producing them): fall through to
        // the synthetic node so the user still sees the failure.
    }
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

    /// Caps fully disabled → runs bare, so e2e is deterministic and doesn't
    /// depend on systemd-run/nice being present in the test sandbox.
    fn uncapped() -> LimitsConfig {
        LimitsConfig {
            test_cpu_quota: String::new(),
            test_mem_max: String::new(),
            test_nice: 0,
            test_max_parallel: 1,
            ..LimitsConfig::default()
        }
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
    fn detects_tier_a_manifests_with_right_matcher_and_ingestion() {
        use crate::panel::Ingestion;
        let cases: &[(&str, &str, &str, Ingestion)] = &[
            ("pubspec.yaml", "name: x\n", "dart", Ingestion::Json),
            (
                "pubspec.yaml",
                "name: x\ndependencies:\n  flutter:\n    sdk: flutter\n",
                "flutter",
                Ingestion::Json,
            ),
            (
                "Package.swift",
                "// swift-tools-version:5.9\n",
                "swift",
                Ingestion::Text,
            ),
            (
                "mix.exs",
                "defmodule X do\nend\n",
                "elixir",
                Ingestion::Text,
            ),
            ("CMakeLists.txt", "project(x)\n", "ctest", Ingestion::Text),
            ("pom.xml", "<project/>\n", "junit", Ingestion::Report),
            (
                "build.gradle.kts",
                "plugins {}\n",
                "junit",
                Ingestion::Report,
            ),
            ("build.sbt", "name := \"x\"\n", "junit", Ingestion::Report),
            (
                "rebar.config",
                "{erl_opts, []}.\n",
                "erlang",
                Ingestion::Text,
            ),
            (
                "build.zig",
                "pub fn build() void {}\n",
                "zig",
                Ingestion::Text,
            ),
            (
                ".rspec",
                "--require spec_helper\n",
                "rspec",
                Ingestion::Json,
            ),
            ("phpunit.xml", "<phpunit/>\n", "junit", Ingestion::Report),
            ("dub.json", "{\"name\":\"x\"}\n", "d", Ingestion::Text),
        ];
        for (file, body, matcher, ingestion) in cases {
            let wt = temp_dir(&format!("detect-{}-{}", matcher, file.replace('.', "_")));
            std::fs::write(wt.join(file), body).unwrap();
            let task = detect_test_task(&wt, &Config::default()).unwrap();
            assert_eq!(&task.matcher, matcher, "matcher for {file}");
            assert_eq!(task.ingestion, *ingestion, "ingestion for {file}");
            let _ = std::fs::remove_dir_all(wt);
        }
    }

    #[test]
    fn flake_nix_is_lowest_priority_and_polyglot_prefers_language() {
        // Pure-nix repo → flake checks.
        let nixonly = temp_dir("nixonly");
        std::fs::write(nixonly.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        assert_eq!(
            detect_test_task(&nixonly, &Config::default())
                .unwrap()
                .matcher,
            "nix-flake"
        );
        let _ = std::fs::remove_dir_all(nixonly);

        // Cargo + flake → the language runner wins (cargo/nextest), not nix.
        let poly = temp_dir("polyglot");
        std::fs::write(poly.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        std::fs::write(
            poly.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'",
        )
        .unwrap();
        let m = detect_test_task(&poly, &Config::default()).unwrap().matcher;
        assert!(matches!(m.as_str(), "cargo-test" | "nextest"), "got {m}");
        let _ = std::fs::remove_dir_all(poly);
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
        // nextest is preferred when installed; otherwise plain cargo test.
        assert!(
            matches!(task.matcher.as_str(), "cargo-test" | "nextest"),
            "matcher was {}",
            task.matcher
        );
        assert!(task.command.contains("cargo"));
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

        // Detection picks up the real Cargo.toml (nextest if installed).
        let task = detect_test_task(&wt, &Config::default()).unwrap();
        assert!(matches!(task.matcher.as_str(), "cargo-test" | "nextest"));

        // Run the real test command (single-threaded for stable output).
        let run_task_spec =
            TestTask::new("cargo test", "cargo test -- --test-threads=1", "cargo-test");
        let outcome = run_task(wt.clone(), 1, run_task_spec, &uncapped());
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
        let disc = discover_tests(wt.clone(), 2, task, &uncapped());
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
        let outcome = run_task(wt.clone(), 1, task, &uncapped());
        assert_eq!(outcome.exit_code, Some(0));
        let nodes = parse_task_outcome(&outcome);
        assert!(
            nodes.iter().any(|n| n.state == TestState::Pass),
            "generic ✓ line should parse as pass: {nodes:?}"
        );
        let _ = std::fs::remove_dir_all(wt);
    }

    /// End-to-end JSON ingestion against REAL nextest: scaffold a cargo project
    /// with a pass + a fail, run nextest's experimental libtest-json, and confirm
    /// the JSON parser maps results. Skips unless nextest emits that format.
    #[test]
    fn e2e_nextest_json_real_repo() {
        if !on_path("cargo") || !on_path("cargo-nextest") {
            return;
        }
        let wt = temp_dir("e2e-nextest");
        git_init(&wt);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"szjson\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n\
             #[test] fn passes() { assert!(true); }\n\
             #[test] fn fails() { assert!(false); }\n}\n",
        )
        .unwrap();

        let task = TestTask::new(
            "cargo nextest",
            "NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 cargo nextest run --message-format libtest-json",
            "nextest",
        )
        .with_ingestion(crate::panel::Ingestion::Json);
        let outcome = run_task(wt.clone(), 1, task, &uncapped());

        // Only assert structured results if this nextest version actually emitted
        // libtest-json `"type":"test"` events; otherwise the experimental format
        // isn't supported here and we skip without failing.
        if outcome.stdout_stderr.contains("\"type\": \"test\"")
            || outcome.stdout_stderr.contains("\"type\":\"test\"")
        {
            let nodes = parse_task_outcome(&outcome);
            assert!(
                nodes
                    .iter()
                    .any(|n| n.id.contains("passes") && n.state == TestState::Pass),
                "nextest json should yield a pass node: {nodes:?}"
            );
            assert!(
                nodes
                    .iter()
                    .any(|n| n.id.contains("fails") && n.state == TestState::Fail),
                "nextest json should yield a fail node: {nodes:?}"
            );
        }
        let _ = std::fs::remove_dir_all(wt);
    }

    /// Report-file ingestion through the real dispatcher: a JUnit file on disk
    /// (as Maven/Gradle would write) is parsed when the task's ingestion=Report.
    #[test]
    fn report_ingestion_reads_junit_from_disk() {
        let wt = temp_dir("report");
        let reports = wt.join("target/surefire-reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(
            reports.join("TEST-MathTests.xml"),
            r#"<testsuite><testcase name="adds" classname="com.x.M"/>
               <testcase name="broken" classname="com.x.M"><failure>at com.x.M.broken(M.java:9)</failure></testcase>
               </testsuite>"#,
        )
        .unwrap();
        let task = TestTask::new("maven", "mvn -q test", "junit")
            .with_ingestion(crate::panel::Ingestion::Report)
            .with_report_glob("target/surefire-reports/*.xml");
        let outcome = TaskOutcome {
            worktree: wt.to_string_lossy().into_owned(),
            generation: 1,
            task,
            exit_code: Some(1),
            duration_ms: 1,
            truncated: false,
            stdout_stderr: String::new(),
        };
        let nodes = parse_task_outcome(&outcome);
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "com.x.M::adds" && n.state == TestState::Pass)
        );
        let f = nodes.iter().find(|n| n.id == "com.x.M::broken").unwrap();
        assert_eq!(f.state, TestState::Fail);
        assert_eq!(f.location.as_ref().unwrap().line, 9);
        let _ = std::fs::remove_dir_all(wt);
    }

    // --- Phase 0 resource-discipline tests ---------------------------------

    #[test]
    fn wrap_capped_disabled_is_passthrough() {
        let argv = vec!["sh".into(), "-lc".into(), "true".into()];
        // Even with a systemd backend, all-disabled limits must not wrap.
        let out = wrap_capped(&argv, &uncapped(), CapBackend::Systemd);
        assert_eq!(out, argv);
    }

    #[test]
    fn wrap_capped_systemd_sets_cpu_mem_nice_scope() {
        let argv = vec!["sh".into(), "-lc".into(), "cargo test".into()];
        let limits = LimitsConfig {
            test_cpu_quota: "150%".into(),
            test_mem_max: "4G".into(),
            test_nice: 10,
            ..LimitsConfig::default()
        };
        let out = wrap_capped(&argv, &limits, CapBackend::Systemd);
        assert_eq!(out[0], "systemd-run");
        assert!(out.contains(&"--scope".to_string()));
        assert!(out.contains(&"CPUQuota=150%".to_string()));
        assert!(out.contains(&"MemoryMax=4G".to_string()));
        assert!(out.windows(2).any(|w| w[0] == "--nice" && w[1] == "10"));
        // The real command is preserved after the `--` separator.
        let sep = out.iter().position(|s| s == "--").unwrap();
        assert_eq!(&out[sep + 1..], &argv[..]);
    }

    #[test]
    fn wrap_capped_nice_prefixes_when_no_systemd() {
        let argv = vec!["sh".into(), "-lc".into(), "go test ./...".into()];
        let limits = LimitsConfig {
            test_cpu_quota: String::new(),
            test_mem_max: String::new(),
            test_nice: 5,
            ..LimitsConfig::default()
        };
        let out = wrap_capped(&argv, &limits, CapBackend::Nice);
        let nice = out.iter().position(|s| s == "nice").expect("nice prefix");
        assert_eq!(out[nice + 1], "-n");
        assert_eq!(out[nice + 2], "5");
        assert!(out.ends_with(&argv));
    }

    /// Single-flight + real cancellation: a long run is killed when a newer job
    /// supersedes it (or `cancel_slot` is called), so it stops burning CPU.
    #[test]
    fn run_capped_cancel_kills_the_process_group() {
        let wt = temp_dir("cancel");
        let slot = format!("{}:run", wt.display());
        let slot2 = slot.clone();
        let handle = std::thread::spawn(move || {
            // Sleeps 30s unless its process group is killed.
            run_capped("sleep 30", &wt, &uncapped(), &slot2, 1)
        });
        // Wait for the child to register, then supersede it.
        let mut waited = 0;
        while registry().lock().unwrap().get(&slot).is_none() && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            waited += 1;
        }
        cancel_slot(&slot);
        let start = Instant::now();
        let (_code, _trunc, _out) = handle.join().unwrap();
        assert!(
            start.elapsed().as_secs() < 10,
            "cancelled run should return promptly, not after the full sleep"
        );
    }
}
