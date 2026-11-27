use std::path::Path;
use std::process::Command;

use thegn_core::store::AutomationStore;

fn fixture(kind: &str) -> String {
    serde_json::json!({
        "id": "fixture",
        "occurred_at": 1,
        "key": "fixture",
        "kind": kind,
        "workspace": null,
        "repo": null,
        "worktree": null,
        "branch": null,
        "agent_role": null,
        "notification_kind": null,
        "priority": null,
        "source_ref": null,
        "message": null,
        "session_id": null,
        "pr_checks_passed": null,
        "pr_review_requested": null,
        "pr_merged": null,
        "origin": null
    })
    .to_string()
}

fn isolate_thegn_environment(
    command: &mut Command,
    home: &Path,
    config_home: &Path,
    state_home: &Path,
    runtime_dir: &Path,
) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("THEGN_") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_STATE_HOME", state_home)
        .env("XDG_RUNTIME_DIR", runtime_dir);
}

#[test]
fn automation_dry_run_leaves_an_empty_state_home_empty() {
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config_home = home.path().join("config");
    std::fs::create_dir_all(&config_home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
    command.args([
        "automations",
        "test",
        "missing",
        "--event",
        &fixture("notification"),
    ]);
    isolate_thegn_environment(
        &mut command,
        home.path(),
        &config_home,
        state.path(),
        runtime.path(),
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "process integration test synchronously waits for the isolated CLI under test"
    )]
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(
        std::fs::read_dir(state.path()).unwrap().next().is_none(),
        "dry-run created state: {:?}",
        std::fs::read_dir(state.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );

    // Logging and usage are enabled in a real config to ensure the pure test
    // command still returns before installing file logging or starting usage
    // collection. The config file itself lives outside the state home.
    let configured_state = tempfile::tempdir().unwrap();
    let configured_runtime = tempfile::tempdir().unwrap();
    let config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        config.path(),
        "[log]\nfile = true\n\n[usage]\nenabled = true\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
    command.args([
        "--config",
        config.path().to_str().unwrap(),
        "automations",
        "test",
        "missing",
        "--event",
        &fixture("notification"),
    ]);
    isolate_thegn_environment(
        &mut command,
        home.path(),
        &config_home,
        configured_state.path(),
        configured_runtime.path(),
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "process integration test synchronously waits for the isolated CLI under test"
    )]
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(
        std::fs::read_dir(configured_state.path())
            .unwrap()
            .next()
            .is_none(),
        "configured dry-run created state: {:?}",
        std::fs::read_dir(configured_state.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    );
}

#[test]
fn one_shot_notification_drains_to_a_terminal_audit_outcome() {
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        config.path(),
        r#"
[automations]
enabled = true
max_concurrent = 1
queue_capacity = 4
action_timeout_secs = 2

[[automations.rules]]
name = "forward-test-failure"
when = "notification"
debounce_secs = 0
max_per_hour = 10
max_action_per_hour = 10

[automations.rules.if]
notification_kind = "test_failed"

[automations.rules.then]
cap = "notify.push"
body = "forwarded: {message}"
"#,
    )
    .unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "process integration test synchronously waits for the isolated CLI under test"
    )]
    let output = Command::new(env!("CARGO_BIN_EXE_thegn"))
        .args([
            "--config",
            config.path().to_str().unwrap(),
            "notify",
            "push",
            "--kind",
            "test_failed",
            "tests failed",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("THEGN_LOG")
        // Launcher overrides must not leak into a spawned test binary: `just
        // live` exports THEGN_DATABASE_MIGRATION_EXECUTABLE, which pins the
        // migration controller to the release build and makes this test's own
        // isolated database unmigratable. Same rule as XDG_STATE_HOME above —
        // a test that spawns thegn owns its whole environment.
        .env_remove("THEGN_DATABASE_MIGRATION_EXECUTABLE")
        .env_remove("THEGN_DATABASE_MIGRATION_AUTHORITY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let db = thegn_core::db::Db::open_at(&state.path().join("thegn/thegn.db")).unwrap();
    let runs = db
        .automation_runs(Some("forward-test-failure"), 10)
        .unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        matches!(runs[0].outcome.as_str(), "failed" | "timed_out"),
        "command exited before terminal audit: {runs:?}"
    );
    assert!(runs[0].finished_at.is_some());
}
