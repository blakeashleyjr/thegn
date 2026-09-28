//! Validation-run classification — deciding what one validation command
//! actually established about a pipeline lane.
//!
//! # Why this is not just an exit code
//!
//! A stage worker cannot compile (the pipeline sandbox bind-mounts the Nix
//! store read-only, so `nix develop` cannot materialise a shell inside it), so
//! "implementation-ready" from a worker means *source-reviewed, never built*.
//! Somebody has to run the build, the tests and the linter, and record what
//! came back. That is the supervisor's job, and the interesting part is not
//! running the command — it is saying **what kind of thing** the result is:
//!
//!  * it **did not build** — nothing downstream is meaningful yet;
//!  * it built and a **test failed** — a verdict about behaviour;
//!  * it built and passed and a **linter objected** — a verdict about style or
//!    a latent hazard, actionable but not a breakage;
//!  * it **could not run at all** — a fact about the environment, which says
//!    nothing whatsoever about the branch;
//!  * it failed in a shape nothing here recognises — which must be reported as
//!    *unknown*, never guessed into one of the above.
//!
//! [`crate::gate`] already draws the last-but-one of those lines for the merge
//! gate, and argues at length why conflating an environment failure with a code
//! verdict is a correctness bug rather than a cosmetic one. This module
//! **composes** that function and adds the classification of the *output*.
//!
//! # Two distinctions that were learned the expensive way
//!
//! **`Inconclusive` is not `TestFailure`, and neither is `CompileError`.** A
//! prototype of this classifier matched a bare `^error(\[|:)` as "compile
//! error". `cargo nextest` ends a failing run with the line
//! `error: test run failed`, so every ordinary test failure was filed as a
//! build break, and the lane was sent back to a worker to fix a compile error
//! that did not exist. Hence: a compile break is recognised by
//! `error[E####]` or `could not compile`, and never by a bare `error:`.
//!
//! **An environment failure is never a verdict about the branch.** A gate run
//! on this machine has gone red purely from CPU contention (2062 tests unrun),
//! and re-running it alone was green. A classifier that reports that as
//! `TestFailure` sends an agent to fix working code. Signals, 126/127 and spawn
//! failures are therefore [`ValidationClass::EnvironmentError`] by construction,
//! straight out of [`crate::gate::classify_exit`].
//!
//! # Toolchain-agnostic
//!
//! The rules are keyed by the `matcher` of the `[[tasks]]` entry that produced
//! the output (`nextest`, `cargo-test`, `go-test`, `pytest`, `jest`, …), with a
//! generic fallback for an unmatched or absent matcher. Nothing here is
//! Rust-only, and nothing here names a coding-agent vendor.
//!
//! Pure: bytes and an exit status in, a class out. No clock, no filesystem, no
//! process.

use crate::gate::{GateClass, classify_exit};

/// What one validation run established.
///
/// Ordered by how much it blocks: a `CompileError` makes every later class
/// unknowable, which is why [`classify`] tests for it first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationClass {
    /// The command ran and exited zero. The task's own verdict is taken at face
    /// value: if an operator wants lint warnings to fail, that belongs in the
    /// task's command (`-D warnings`), not in a second opinion here.
    Green,
    /// The code did not build. Nothing downstream — tests, lints, coverage —
    /// says anything until this is fixed.
    CompileError,
    /// It built and a test failed. A verdict about behaviour.
    TestFailure,
    /// It built and the tests (if any ran) passed, and a linter objected.
    LintFinding,
    /// The command ran, exited non-zero, and the output matched nothing this
    /// module recognises. Deliberately its own class: guessing here is how a
    /// ratchet failure becomes a phantom compile error.
    Inconclusive,
    /// The command could not run — spawn failure, not found, not executable,
    /// or killed by a signal. **Says nothing about the branch** and must never
    /// be reported as one; see [`crate::gate`].
    EnvironmentError,
}

impl ValidationClass {
    /// A short, stable token for tables, `--json` and the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::CompileError => "compile-error",
            Self::TestFailure => "test-failure",
            Self::LintFinding => "lint-finding",
            Self::Inconclusive => "inconclusive",
            Self::EnvironmentError => "environment-error",
        }
    }

    /// Parse a stored token. Total by construction — an unrecognised string
    /// reads back as [`Self::Inconclusive`] rather than failing the row, for
    /// the same reason [`crate::issue::AgentDispatchStatus::parse`] coerces:
    /// a record written by a future build must stay listable.
    pub fn parse(s: &str) -> ValidationClass {
        match s.trim() {
            "green" => Self::Green,
            "compile-error" => Self::CompileError,
            "test-failure" => Self::TestFailure,
            "lint-finding" => Self::LintFinding,
            "environment-error" => Self::EnvironmentError,
            _ => Self::Inconclusive,
        }
    }

    /// Does this class establish that the lane is good as far as this task can
    /// tell? Only [`Self::Green`] does — in particular **not**
    /// [`Self::EnvironmentError`], which establishes nothing at all and must
    /// not be allowed to satisfy a `validation:green` requirement by being
    /// "not a failure".
    pub fn is_green(self) -> bool {
        matches!(self, Self::Green)
    }

    /// Is this a verdict about the *code* (as opposed to the environment, or an
    /// unrecognised shape)? These are the classes worth handing to an agent.
    pub fn blames_code(self) -> bool {
        matches!(
            self,
            Self::CompileError | Self::TestFailure | Self::LintFinding
        )
    }

    /// Whether re-running the same command unchanged could legitimately give a
    /// different answer. True only for the two classes that are statements
    /// about the run rather than the tree — which is exactly when a supervisor
    /// should retry before reporting anything.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::EnvironmentError | Self::Inconclusive)
    }
}

/// Lines that mean "this did not build", in any toolchain. Matched as
/// substrings against each line.
///
/// `error[E` is deliberately spelled with the bracket: a bare `error:` prefix
/// is what `cargo nextest` prints for an ordinary *test* failure
/// (`error: test run failed`), and matching it here is the single mistake this
/// module exists to make impossible.
const COMPILE_MARKERS: &[&str] = &[
    // rustc / cargo
    "error[E",
    "could not compile",
    // go — the BUILD failure line only. Go's runtime also prints
    // `fatal error: all goroutines are asleep - deadlock!` from inside a
    // running test, so `fatal error:` is NOT a build marker here.
    "[build failed]",
    "typecheck failed",
    // typescript / babel / swc
    "error TS",
    // python (an import-time break, not a test assertion)
    "SyntaxError:",
    "IndentationError:",
    // java / kotlin / scala
    "COMPILATION ERROR",
    "compilation failed",
    // c / c++ / clang / gcc — the line the driver prints when it gives up.
    // Deliberately NOT `fatal error:` on its own: clang and gcc emit that for a
    // missing header, but so does the Go runtime for a deadlock in a test.
    "compilation terminated",
    // generic build drivers
    "ninja: build stopped",
];

/// Lines that mean "it built and a test failed", per matcher family.
///
/// Every entry must be **failure-bearing**. A bare run-summary line
/// (`tests run:`, `Tests:`, `Total tests:`, `Finished in `) is printed on a
/// PASSING run too, and these markers are only consulted after a non-zero exit
/// — which is not the same thing as "the tests failed" when the task is a
/// composite command. A `[[tasks]]` entry running `just test`, where the suite
/// passes and a later step of the recipe fails, prints
/// `9322 tests run: 9322 passed` and would otherwise be reported as a test
/// failure, sending an agent to fix tests that passed.
fn test_failure_markers(matcher: Option<&str>) -> &'static [&'static str] {
    match matcher.unwrap_or("").trim() {
        "nextest" => &["FAIL [", "test run failed", " failed,"],
        "cargo-test" | "libtest-json" => &["test result: FAILED", "panicked at"],
        "go-test" => &["--- FAIL:", "FAIL\t", "\nFAIL"],
        "pytest" => &["FAILED ", "=== FAILURES ===", " failed,", " failed in "],
        "jest" | "vitest" | "javascript" => &["✕ ", "●  ", " failed,"],
        "rspec" | "ruby" => &["Failures:\n", "\nFailures:"],
        "junit" | "gradle" | "maven" => &[
            "FAILURES!",
            "There were failing tests",
            "<<< FAILURE!",
            "<<< ERROR!",
            "Failed tests:",
        ],
        "dotnet" | "trx" | "nunit" => &["Failed!", "Failed  -"],
        "tap" | "bats" | "prove" | "busted" | "pgtap" => &["not ok ", "# failed"],
        // ExUnit's summary says `N tests, 0 failures` on a PASS, so the
        // summary cannot be the marker. Its numbered failure blocks
        // (`  1) test …`) and raised-exception prefix only appear on a failure.
        "elixir" => &["Assertion with", "** (", ") test ", ") doctest "],
        "zig" => &["error: 'test.", " tests failed"],
        _ => GENERIC_TEST_MARKERS,
    }
}

/// The fallback test-failure vocabulary for a task with no matcher (or one this
/// module has no rules for). Kept broad but unambiguous: each of these appears
/// in test-runner output and not in a linter's.
const GENERIC_TEST_MARKERS: &[&str] = &[
    "test result: FAILED",
    "--- FAIL:",
    "FAILED ",
    "not ok ",
    "assertion failed",
    "AssertionError",
    "tests failed",
    "test failed",
    "panicked at",
];

/// Lines that mean "a linter objected". Checked only after compile and test
/// markers, so a build warning printed alongside a genuine error never
/// downgrades the class.
/// Lint markers for a task whose matcher says it IS a linter.
///
/// `warning: ` is meaningful here and nowhere else: when the operator has
/// declared the task a linter, a warning line is the finding. For any other
/// task it is just build chatter, which is why it is absent from
/// [`LINT_MARKERS`].
fn lint_markers(matcher: Option<&str>) -> &'static [&'static str] {
    match matcher.unwrap_or("").trim() {
        "clippy" | "eslint" | "biome" | "oxlint" | "ruff" | "flake8" | "pylint" | "shellcheck"
        | "golangci-lint" | "staticcheck" | "vet" | "rubocop" | "swiftlint" => LINT_TASK_MARKERS,
        _ => LINT_MARKERS,
    }
}

/// What counts as a finding when the task is DECLARED to be a linter.
const LINT_TASK_MARKERS: &[&str] = &[
    "warning: ",
    "warning ",
    "error: ",
    "✖ ",
    " problems (",
    "^-- SC",
];

/// Every entry must be unambiguously a LINTER'S output.
///
/// The temptation is to add `warning: `, which any cargo build prints, and
/// `lint`, which matches any path or test name containing the word. Both were
/// here and both were wrong: this tier runs before [`ValidationClass::Inconclusive`],
/// and [`ValidationClass::LintFinding`] reports
/// [`ValidationClass::blames_code`] — so a permissive marker turns an
/// unrecognised failure into a confident verdict about the code, which is
/// exactly what the `Inconclusive` class exists to prevent.
const LINT_MARKERS: &[&str] = &[
    // clippy / rustc, in its denied form only — a bare `warning: ` line is
    // printed by every ordinary build.
    "-D warnings",
    "denied by",
    "`#[deny(",
    // eslint / biome / oxlint
    "✖ ",
    " problems (",
    // ruff / flake8 / pylint
    " [*] ",
    "flake8",
    // golangci-lint / staticcheck / vet
    "level=error",
    "golangci-lint",
    // shellcheck
    "^-- SC",
];

/// Classify one validation run.
///
/// `exit` is the process exit status (`None` when killed by a signal),
/// `spawn_failed` whether the command could be launched at all, `matcher` the
/// `[[tasks]]` entry's matcher, and `output` the captured combined stdout and
/// stderr (the caller caps the capture; this function does not need all of it).
///
/// The order of the tests is the contract, not an implementation detail:
/// environment → success → compile → test → lint → unknown. A failing build
/// also prints test-runner and linter noise, so any other order misattributes.
pub fn classify(
    exit: Option<i32>,
    spawn_failed: bool,
    matcher: Option<&str>,
    output: &str,
) -> ValidationClass {
    // 1. Could it run at all? `gate` owns this question for the merge gate and
    //    owns it here too, so the two surfaces cannot drift apart.
    match classify_exit(exit, spawn_failed) {
        GateClass::Error => return ValidationClass::EnvironmentError,
        GateClass::Passed => return ValidationClass::Green,
        GateClass::Failed => {}
    }

    // 2. Did it build? A compile break makes every later signal meaningless.
    if contains_any(output, COMPILE_MARKERS) {
        return ValidationClass::CompileError;
    }

    // 3. Did a test fail?
    if contains_any(output, test_failure_markers(matcher)) {
        return ValidationClass::TestFailure;
    }

    // 4. Did a linter object?
    if contains_any(output, lint_markers(matcher)) {
        return ValidationClass::LintFinding;
    }

    // 5. It failed in a shape nothing here knows. Say so.
    ValidationClass::Inconclusive
}

/// Whether any marker appears in the output. Case-sensitive on purpose:
/// `FAILED` and `failed` carry different weight in several of these formats
/// (`pytest` prints `FAILED tests/…` per case and `failed,` in its summary),
/// and a case-insensitive scan folds a linter's prose into a test verdict.
fn contains_any(output: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| output.contains(m))
}

/// The most lines a digest may carry. A digest is read by a person (and pasted
/// into a revision brief), not parsed, so it is capped hard.
pub const DIGEST_MAX_LINES: usize = 12;

/// The most characters one digest line may carry.
pub const DIGEST_MAX_LINE_CHARS: usize = 180;

/// Reduce a validation run's output to the handful of lines worth reading.
///
/// This is not cosmetic. thegn's own control-schema snapshot test prints
/// roughly 120 KB on failure, and a `just test` run that trips several ratchets
/// prints more; storing that per lane makes the record unreadable and the queue
/// unusable. So: keep only the lines that carry a marker for the class, trim
/// each, de-duplicate, and cap the count.
///
/// [`ValidationClass::Green`] digests to the empty string — there is nothing to
/// read — and [`ValidationClass::EnvironmentError`] digests to the *tail*,
/// because what matters there is how the command died, not what it printed.
///
/// `matcher` must be the same one passed to [`classify`]: the digest selects
/// lines with the **rules that produced the class**, so a matcher-specific
/// verdict (nextest's `FAIL [`, say) selects matcher-specific lines. Passing a
/// different matcher here would silently fall back to the tail and bury the
/// findings under progress output.
pub fn digest(output: &str, class: ValidationClass, matcher: Option<&str>) -> String {
    if class == ValidationClass::Green {
        return String::new();
    }
    let markers: &[&str] = match class {
        ValidationClass::CompileError => COMPILE_MARKERS,
        ValidationClass::TestFailure => test_failure_markers(matcher),
        ValidationClass::LintFinding => lint_markers(matcher),
        // Nothing matched (or the run never produced a verdict), so there is no
        // marker to select on: the tail is the most informative slice.
        ValidationClass::Inconclusive | ValidationClass::EnvironmentError => &[],
        ValidationClass::Green => unreachable!("handled above"),
    };

    let mut picked: Vec<&str> = Vec::new();
    if markers.is_empty() {
        picked = output
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(DIGEST_MAX_LINES)
            .collect();
        picked.reverse();
    } else {
        for line in output.lines().map(str::trim_end) {
            if line.trim().is_empty() {
                continue;
            }
            if markers.iter().any(|m| line.contains(m)) {
                picked.push(line);
            }
        }
        // A class was assigned from a marker in the *combined* text, which can
        // straddle a line boundary in the generic markers; fall back to the
        // tail rather than emitting an empty digest for a real failure.
        if picked.is_empty() {
            picked = output
                .lines()
                .map(str::trim_end)
                .filter(|l| !l.trim().is_empty())
                .rev()
                .take(DIGEST_MAX_LINES)
                .collect();
            picked.reverse();
        }
    }

    let mut seen: Vec<String> = Vec::new();
    for line in picked {
        let truncated: String = line.chars().take(DIGEST_MAX_LINE_CHARS).collect();
        let truncated = truncated.trim().to_string();
        if truncated.is_empty() || seen.contains(&truncated) {
            continue;
        }
        seen.push(truncated);
        if seen.len() == DIGEST_MAX_LINES {
            break;
        }
    }
    seen.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the environment/code split, inherited from `gate` -------------------

    #[test]
    fn unrunnable_commands_never_blame_the_branch() {
        for (exit, spawn_failed) in [
            (None, true),       // could not spawn
            (None, false),      // killed by a signal (OOM, watchdog)
            (Some(127), false), // command not found
            (Some(126), false), // not executable
        ] {
            let class = classify(exit, spawn_failed, Some("nextest"), "error[E0308] whatever");
            assert_eq!(
                class,
                ValidationClass::EnvironmentError,
                "exit={exit:?} spawn_failed={spawn_failed} must not be a verdict about the code"
            );
            assert!(!class.blames_code());
            assert!(!class.is_green());
        }
    }

    #[test]
    fn a_clean_exit_is_green_whatever_it_printed() {
        // The task's own verdict is authoritative; a linter that exits zero has
        // passed as configured, and second-guessing it here would mean the
        // class disagrees with the gate the operator actually wrote.
        assert_eq!(
            classify(Some(0), false, Some("nextest"), "warning: unused import"),
            ValidationClass::Green
        );
        assert!(ValidationClass::Green.is_green());
    }

    // --- the regression this module exists for -------------------------------

    #[test]
    fn nextest_run_failure_is_a_test_failure_not_a_compile_error() {
        // The exact shape that made a prototype file every ratchet failure as a
        // build break: nextest's trailing `error: test run failed`.
        let out = "\
    Starting 9322 tests across 41 binaries
        FAIL [   0.011s] thegn-core config_duration::direct_environment_checks
------------
     Summary [  71.402s] 9322 tests run: 9321 passed, 1 failed, 0 skipped
error: test run failed";
        assert_eq!(
            classify(Some(100), false, Some("nextest"), out),
            ValidationClass::TestFailure
        );
    }

    #[test]
    fn a_bare_error_colon_is_never_a_compile_marker() {
        for line in [
            "error: test run failed",
            "error: could not find `Cargo.toml`",
            "error: 3 problems",
        ] {
            assert!(
                !COMPILE_MARKERS.iter().any(|m| line.contains(m)),
                "{line:?} must not read as a compile break"
            );
        }
    }

    #[test]
    fn a_go_runtime_fatal_error_is_a_test_failure_not_a_build_break() {
        // The Go RUNTIME prints `fatal error:` from inside a running test. An
        // earlier marker set matched it as a compile break, which sends the
        // lane back to fix a build error that does not exist.
        let out = "\
--- FAIL: TestConcurrentMap (0.01s)
fatal error: all goroutines are asleep - deadlock!";
        assert_eq!(
            classify(Some(2), false, Some("go-test"), out),
            ValidationClass::TestFailure
        );
        for line in [
            "fatal error: concurrent map writes",
            "fatal error: stack overflow",
        ] {
            assert!(
                !COMPILE_MARKERS.iter().any(|m| line.contains(m)),
                "{line:?} must not read as a build break"
            );
        }
    }

    #[test]
    fn a_failing_make_recipe_is_not_a_build_break() {
        // `make: *** [Makefile:12: test] Error 1` is printed for ANY failing
        // recipe. A `[[tasks]]` entry running `make test` with one failing test
        // was classified as a compile error.
        let out = "FAILED tests/test_a.py::test_b\nmake: *** [Makefile:12: test] Error 1";
        assert_eq!(
            classify(Some(2), false, Some("pytest"), out),
            ValidationClass::TestFailure
        );
    }

    #[test]
    fn a_passing_suite_inside_a_failing_composite_task_is_not_a_test_failure() {
        // `just test` where the suite passes and a later step of the recipe
        // fails. A bare run-summary marker (`tests run:`) reported this as a
        // test failure and would send an agent to fix tests that passed.
        let out = "\
     Summary [  71.402s] 9322 tests run: 9322 passed, 0 skipped
error: recipe `test` failed on line 4 with exit code 1";
        assert_ne!(
            classify(Some(1), false, Some("nextest"), out),
            ValidationClass::TestFailure,
            "a passing suite was reported as a test failure"
        );
    }

    #[test]
    fn an_ordinary_build_warning_does_not_become_a_lint_verdict() {
        // `warning: ` is printed by every cargo build. As a lint marker it
        // turned any unrecognised failure into a confident verdict about the
        // code — the thing `Inconclusive` exists to prevent.
        let class = classify(
            Some(3),
            false,
            None,
            "warning: unused import\nthe wind changed",
        );
        assert_eq!(class, ValidationClass::Inconclusive);
        assert!(!class.blames_code());
    }

    #[test]
    fn the_word_lint_in_a_path_is_not_a_lint_finding() {
        let class = classify(Some(3), false, None, "running crates/lint_rules/mod.rs");
        assert_eq!(class, ValidationClass::Inconclusive);
    }

    #[test]
    fn a_denied_clippy_lint_is_still_recognised() {
        let out = "warning: this `if` has identical blocks\nerror: aborting due to `-D warnings`";
        assert_eq!(
            classify(Some(1), false, Some("clippy"), out),
            ValidationClass::LintFinding
        );
    }

    #[test]
    fn no_test_marker_appears_in_passing_output() {
        // The invariant behind the composite-task fix: a marker that a PASSING
        // run prints cannot be evidence of failure.
        let passing = [
            "     Summary [  71.402s] 9322 tests run: 9322 passed, 0 skipped",
            "Tests:       12 passed, 12 total",
            "Total tests: 40. Passed: 40. Failed: 0.",
            "Finished in 0.4 seconds",
            "Tests run: 4, Failures: 0, Errors: 0",
            "40 examples, 0 failures",
            "12 tests, 0 failures",
        ];
        for matcher in [
            "nextest",
            "cargo-test",
            "go-test",
            "pytest",
            "jest",
            "rspec",
            "junit",
            "dotnet",
            "elixir",
            "zig",
            "tap",
        ] {
            for line in passing {
                assert!(
                    !test_failure_markers(Some(matcher))
                        .iter()
                        .any(|m| line.contains(m)),
                    "matcher {matcher} treats passing output as failure: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_real_compile_break_wins_over_the_test_noise_it_causes() {
        // A failing build prints BOTH a rustc error and nextest's run-failed
        // line. Compile must win, or the lane is sent back to fix a test that
        // never ran.
        let out = "\
error[E0308]: mismatched types
   --> crates/thegn-core/src/util.rs:42:9
error: could not compile `thegn-core` (lib) due to 1 previous error
error: test run failed";
        assert_eq!(
            classify(Some(101), false, Some("nextest"), out),
            ValidationClass::CompileError
        );
    }

    #[test]
    fn a_build_warning_beside_a_real_error_does_not_downgrade_the_class() {
        let out = "warning: unused variable: `x`\nerror[E0425]: cannot find value `y`";
        assert_eq!(
            classify(Some(101), false, None, out),
            ValidationClass::CompileError
        );
    }

    #[test]
    fn a_lint_is_only_a_lint_when_nothing_broke() {
        let out = "warning: this `if` has identical blocks\nerror: could not compile `x` due to 1 warning";
        // `-D warnings` turns the lint into a build failure; that IS a compile
        // break, and reporting it as a style finding would understate it.
        assert_eq!(
            classify(Some(101), false, None, out),
            ValidationClass::CompileError
        );

        // A bare warning counts as a finding only because the task's matcher
        // DECLARES it a linter …
        let clean = "warning: unneeded `return` statement\n  --> src/a.rs:3:5";
        assert_eq!(
            classify(Some(1), false, Some("clippy"), clean),
            ValidationClass::LintFinding
        );
        // … and the identical output from a task that is not a linter stays
        // unknown rather than becoming a confident verdict about the code.
        assert_eq!(
            classify(Some(1), false, Some("nextest"), clean),
            ValidationClass::Inconclusive
        );
    }

    #[test]
    fn an_unrecognised_failure_is_inconclusive_not_guessed() {
        let class = classify(Some(2), false, Some("nextest"), "the wind changed\n");
        assert_eq!(class, ValidationClass::Inconclusive);
        assert!(!class.blames_code(), "an unknown shape blames nothing");
        assert!(class.is_retryable());
    }

    // --- toolchain independence ---------------------------------------------

    #[test]
    fn every_matcher_family_recognises_its_own_test_failure() {
        for (matcher, out) in [
            ("go-test", "--- FAIL: TestFoo (0.00s)"),
            ("pytest", "FAILED tests/test_a.py::test_b - assert 1 == 2"),
            ("jest", "Tests:       1 failed, 3 passed"),
            ("rspec", "\nFailures:\n\n  1) Thing does"),
            ("cargo-test", "test result: FAILED. 1 passed; 1 failed"),
            ("tap", "not ok 3 - the thing"),
            (
                "junit",
                "Tests run: 4, Failures: 1\n\nResults :\n\nFailed tests:  testThing",
            ),
            ("dotnet", "Failed!  - Failed:     1"),
        ] {
            assert_eq!(
                classify(Some(1), false, Some(matcher), out),
                ValidationClass::TestFailure,
                "matcher {matcher} failed to recognise its own failure output"
            );
        }
    }

    #[test]
    fn an_absent_matcher_still_classifies_common_shapes() {
        assert_eq!(
            classify(Some(1), false, None, "--- FAIL: TestThing"),
            ValidationClass::TestFailure
        );
        assert_eq!(
            classify(Some(1), false, None, "thread 'main' panicked at src/x.rs:1"),
            ValidationClass::TestFailure
        );
    }

    #[test]
    fn no_rule_names_a_coding_agent_vendor() {
        // The classifier must stay agent-agnostic: it classifies TOOLCHAIN
        // output, never a harness's chatter.
        let all: Vec<&str> = COMPILE_MARKERS
            .iter()
            .chain(LINT_MARKERS.iter())
            .chain(LINT_TASK_MARKERS.iter())
            .chain(GENERIC_TEST_MARKERS.iter())
            .copied()
            .collect();
        // Whole words only: `pi` is a substring of `compilation`, and a
        // substring test would forbid ordinary toolchain vocabulary.
        let words: Vec<String> = all
            .iter()
            .flat_map(|m| {
                m.to_lowercase()
                    .split(|c: char| !c.is_alphanumeric())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        for vendor in ["claude", "codex", "aider", "pi", "antigravity", "gemini"] {
            assert!(
                !words.iter().any(|w| w == vendor),
                "marker set names the {vendor} harness"
            );
        }
    }

    // --- token round-trip ----------------------------------------------------

    #[test]
    fn every_class_round_trips_through_its_token() {
        for class in [
            ValidationClass::Green,
            ValidationClass::CompileError,
            ValidationClass::TestFailure,
            ValidationClass::LintFinding,
            ValidationClass::Inconclusive,
            ValidationClass::EnvironmentError,
        ] {
            assert_eq!(ValidationClass::parse(class.as_str()), class);
        }
        // Total: a token from a future build reads back as unknown, not a panic.
        assert_eq!(
            ValidationClass::parse("flaky-quarantined"),
            ValidationClass::Inconclusive
        );
        assert_eq!(ValidationClass::parse(""), ValidationClass::Inconclusive);
    }

    #[test]
    fn an_environment_error_does_not_satisfy_green() {
        // The trap this guards: "not a failure" is not "passed". A
        // `validation:green` requirement must never be satisfied by a command
        // that never ran.
        assert!(!ValidationClass::EnvironmentError.is_green());
        assert!(!ValidationClass::Inconclusive.is_green());
        assert!(ValidationClass::Green.is_green());
    }

    #[test]
    fn only_run_shaped_classes_are_retryable() {
        assert!(ValidationClass::EnvironmentError.is_retryable());
        assert!(ValidationClass::Inconclusive.is_retryable());
        for class in [
            ValidationClass::Green,
            ValidationClass::CompileError,
            ValidationClass::TestFailure,
            ValidationClass::LintFinding,
        ] {
            assert!(!class.is_retryable(), "{class:?} is a fact about the tree");
        }
    }

    // --- digests -------------------------------------------------------------

    #[test]
    fn a_green_run_digests_to_nothing() {
        assert_eq!(
            digest("9322 tests run: 9322 passed", ValidationClass::Green, None),
            ""
        );
    }

    #[test]
    fn a_digest_is_capped_in_both_dimensions() {
        // The real motivation: the control-schema snapshot prints ~120 KB.
        let mut out = String::new();
        for i in 0..500 {
            out.push_str(&format!("    FAIL [   0.0{i}s] crate test_number_{i}\n"));
            out.push_str(&format!("{}\n", "x".repeat(4_000)));
        }
        let d = digest(&out, ValidationClass::TestFailure, Some("nextest"));
        assert!(d.lines().count() <= DIGEST_MAX_LINES, "line count uncapped");
        for line in d.lines() {
            assert!(
                line.chars().count() <= DIGEST_MAX_LINE_CHARS,
                "line length uncapped: {} chars",
                line.chars().count()
            );
        }
        assert!(
            d.len() < 4_000,
            "digest still too large to read: {} bytes",
            d.len()
        );
    }

    #[test]
    fn a_digest_keeps_the_lines_that_explain_the_class() {
        let out = "\
   Compiling thegn-core v0.1.0
warning: unused import
    FAIL [   0.011s] thegn-core config::a
    FAIL [   0.012s] thegn-core config::b
     Summary 2 failed";
        let d = digest(out, ValidationClass::TestFailure, Some("nextest"));
        assert!(d.contains("config::a"), "dropped the failing test: {d}");
        assert!(d.contains("config::b"));
        assert!(
            !d.contains("Compiling"),
            "kept progress noise instead of findings: {d}"
        );
    }

    #[test]
    fn a_digest_de_duplicates_repeated_lines() {
        let out = "error[E0433]: failed to resolve\n".repeat(40);
        let d = digest(&out, ValidationClass::CompileError, None);
        assert_eq!(d.lines().count(), 1, "identical lines were not folded: {d}");
    }

    #[test]
    fn an_unrecognised_failure_digests_to_its_tail() {
        // Nothing matched, so there is no marker to select on — the last lines
        // are what a person needs.
        let out = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = digest(&out, ValidationClass::Inconclusive, Some("nextest"));
        assert!(d.contains("line 39"), "tail missing: {d}");
        assert!(
            !d.contains("line 0"),
            "kept the head instead of the tail: {d}"
        );
        assert!(d.lines().count() <= DIGEST_MAX_LINES);
    }

    #[test]
    fn an_environment_error_digests_to_how_it_died() {
        let out =
            "nix develop: error: creating directory '/nix/store/tmp-1': Read-only file system";
        let d = digest(out, ValidationClass::EnvironmentError, Some("nextest"));
        assert!(d.contains("Read-only file system"), "lost the cause: {d}");
    }

    #[test]
    fn a_digest_never_returns_empty_for_a_real_failure() {
        // The generic markers can match across a line boundary; falling through
        // to an empty digest would hide the failure entirely.
        let d = digest(
            "something broke in an unfamiliar way",
            ValidationClass::TestFailure,
            None,
        );
        assert!(!d.is_empty());
    }

    #[test]
    fn digests_handle_multibyte_output_without_splitting_a_character() {
        // `chars().take()` rather than byte slicing: a truncated UTF-8 sequence
        // would panic or corrupt the record.
        let long = format!("FAILED {}", "é".repeat(400));
        let d = digest(&long, ValidationClass::TestFailure, Some("pytest"));
        assert!(d.chars().count() <= DIGEST_MAX_LINE_CHARS);
        assert!(d.contains('é'));
    }
}
