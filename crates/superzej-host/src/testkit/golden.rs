//! Golden-fixture tests: assert our parsers against REAL output captured from
//! each ecosystem's real test runner.
//!
//! The fixtures in `crates/superzej-host/testdata/` are produced by
//! `test/gen-test-fixtures.sh`, which runs the actual tool (fetched via
//! `nix shell nixpkgs#…`) on a minimal real project with one passing and one
//! failing test. We commit only the captured OUTPUT, never project source. These
//! tests guard against format drift and prove the parsers handle real-world
//! output, not just hand-authored samples.

#![cfg(test)]

use crate::panel::{self, TestNodeKind, TestState};
use crate::task::{self, TaskOutcome};
use crate::testkit::{json, report, tap};

fn counts(nodes: &[panel::TestNode]) -> (usize, usize, usize) {
    let tests: Vec<_> = nodes
        .iter()
        .filter(|n| n.kind == TestNodeKind::Test)
        .collect();
    (
        tests.iter().filter(|n| n.state == TestState::Pass).count(),
        tests.iter().filter(|n| n.state == TestState::Fail).count(),
        tests.iter().filter(|n| n.state == TestState::Skip).count(),
    )
}

fn has_fail_with_location(nodes: &[panel::TestNode]) -> bool {
    nodes
        .iter()
        .any(|n| n.state == TestState::Fail && n.location.is_some())
}

// --- JSON ingestion --------------------------------------------------------

#[test]
fn golden_nextest_libtest_json() {
    let nodes = json::parse(
        "nextest",
        include_str!("../../testdata/nextest.libtest-json"),
    );
    let (pass, fail, skip) = counts(&nodes);
    assert_eq!(
        (pass, fail, skip),
        (1, 1, 1),
        "real nextest json: {nodes:?}"
    );
    assert!(
        has_fail_with_location(&nodes),
        "failure carries src/lib.rs:N"
    );
}

#[test]
fn golden_dart_json() {
    let nodes = json::parse("dart", include_str!("../../testdata/dart.json"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real dart test json: {nodes:?}");
}

#[test]
fn golden_rspec_json() {
    let nodes = json::parse("rspec", include_str!("../../testdata/rspec.json"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real rspec json: {nodes:?}");
    assert!(has_fail_with_location(&nodes));
}

// --- Report ingestion (JUnit XML) ------------------------------------------

#[test]
fn golden_deno_junit() {
    // deno writes JUnit to stdout; parse_report auto-detects the format.
    let nodes = report::parse_report(include_str!("../../testdata/deno.junit.xml"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real deno junit: {nodes:?}");
}

#[test]
fn golden_maven_surefire_junit() {
    let nodes = report::parse_report(include_str!("../../testdata/maven.junit.xml"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real maven surefire junit: {nodes:?}");
}

#[test]
fn golden_dotnet_trx() {
    let nodes = report::parse_report(include_str!("../../testdata/dotnet.trx"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real dotnet trx: {nodes:?}");
}

#[test]
fn golden_phpunit_junit() {
    let nodes = report::parse_report(include_str!("../../testdata/phpunit.junit.xml"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real phpunit junit: {nodes:?}");
    assert!(
        has_fail_with_location(&nodes),
        "phpunit failure has CalcTest.php:5"
    );
}

// --- TAP ingestion (bats / prove / busted) ---------------------------------

#[test]
fn golden_bats_tap() {
    let nodes = tap::parse(include_str!("../../testdata/bats.tap"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real bats tap: {nodes:?}");
    // bats diagnostic: `# (in test file test/calc.bats, line 2)`
    assert!(
        has_fail_with_location(&nodes),
        "bats failure has a location"
    );
}

#[test]
fn golden_perl_prove_tap() {
    let nodes = tap::parse(include_str!("../../testdata/perl.tap"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real perl prove tap: {nodes:?}");
}

#[test]
fn golden_lua_busted_tap() {
    let nodes = tap::parse(include_str!("../../testdata/busted.tap"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real busted tap: {nodes:?}");
    assert!(
        has_fail_with_location(&nodes),
        "busted failure has a location"
    );
}

// --- Text ingestion --------------------------------------------------------

#[test]
fn golden_go_text() {
    let nodes = panel::parse_test_output(include_str!("../../testdata/go.txt"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real go -v: {nodes:?}");
}

#[test]
fn golden_pytest_text() {
    let nodes = panel::parse_test_output(include_str!("../../testdata/pytest.txt"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real pytest -v: {nodes:?}");
}

#[test]
fn golden_ctest_text() {
    let nodes = panel::parse_test_output(include_str!("../../testdata/ctest.txt"));
    let (pass, fail, _) = counts(&nodes);
    assert_eq!((pass, fail), (1, 1), "real ctest: {nodes:?}");
}

// --- Synthetic fallback (sparse runners) -----------------------------------
// zig/elixir don't emit clean per-test lines; the runner's exit code drives a
// single synthetic node with the first failure location. We drive the real
// dispatcher with the captured output to prove that path.

fn synthetic(matcher: &str, fixture: &str) -> Vec<panel::TestNode> {
    let outcome = TaskOutcome {
        worktree: "/tmp/wt".into(),
        generation: 1,
        task: panel::TestTask::new("t", "t", matcher),
        exit_code: Some(1), // a real failing run
        duration_ms: 1,
        truncated: false,
        stdout_stderr: fixture.to_string(),
    };
    task::parse_task_outcome(&outcome)
}

#[test]
fn golden_zig_text_synthetic_fail() {
    let nodes = synthetic("zig", include_str!("../../testdata/zig.txt"));
    assert!(
        nodes.iter().any(|n| n.state == TestState::Fail),
        "real zig failing run yields a fail node: {nodes:?}"
    );
}

#[test]
fn golden_elixir_text_synthetic_fail_with_location() {
    let nodes = synthetic("elixir", include_str!("../../testdata/elixir.txt"));
    assert!(
        has_fail_with_location(&nodes),
        "real elixir failure carries calc_test.exs:N: {nodes:?}"
    );
}

#[test]
fn golden_ocaml_text_synthetic_fail() {
    // dune runtest output is sparse; a failing run yields a synthetic fail node.
    let nodes = synthetic("ocaml", include_str!("../../testdata/ocaml.txt"));
    assert!(
        nodes.iter().any(|n| n.state == TestState::Fail),
        "real ocaml failing run yields a fail node: {nodes:?}"
    );
}
