//! Contract tests for the synthetic resident fault-fixture plugin.

use std::io::Write;
use std::path::PathBuf;

use thegn_core::plugin_api::ExtensionPoint;
use thegn_core::plugin_app::{AppView, Widget};
use thegn_svc::plugin::{discover, negotiate};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/fixtures")
}

#[allow(
    clippy::disallowed_macros,
    reason = "report when the optional Python runtime is unavailable"
)]
fn python_available() -> bool {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: python3 is not installed");
        false
    } else {
        true
    }
}

fn run_fixture(args: &[&str], input: &str) -> (u32, std::process::Output) {
    let mut child = std::process::Command::new("python3")
        .arg("fault.py")
        .args(args)
        .current_dir(fixtures_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 fixture starts");
    let pid = child.id();
    let write_result = child
        .stdin
        .take()
        .expect("child stdin is piped")
        .write_all(input.as_bytes());
    let output = child.wait_with_output().expect("wait for python child");
    write_result.expect("write protocol input");
    (pid, output)
}

fn json_lines(output: &std::process::Output) -> Vec<serde_json::Value> {
    String::from_utf8(output.stdout.clone())
        .expect("plugin stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout line is JSON"))
        .collect()
}

#[test]
fn fault_fixture_manifest_negotiates_one_app_tab() {
    let tmp = tempfile::tempdir().unwrap();
    let staged = tmp.path().join("plugins/fixture");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::copy(
        fixtures_dir().join("plugin.toml"),
        staged.join("plugin.toml"),
    )
    .unwrap();
    std::fs::copy(fixtures_dir().join("fault.py"), staged.join("fault.py")).unwrap();

    let (loaded, problems) = discover(&thegn_core::config::Config::default(), tmp.path());
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert_eq!(loaded[0].spec.manifest.id.as_str(), "fixture");

    let negotiated = negotiate(&loaded[0].spec).expect("manifest negotiates");
    assert_eq!(negotiated.accepted_contributions.len(), 1);
    assert_eq!(
        negotiated.accepted_contributions[0].extension_point,
        ExtensionPoint::AppTab
    );
    assert!(negotiated.unsupported_contributions.is_empty());
}

#[test]
fn fault_fixture_normal_mode_registers_before_activate_and_answers_requests() {
    if !python_available() {
        return;
    }
    let input = concat!(
        "{\"id\":1,\"method\":\"ping\",\"params\":{}}\n",
        "{\"method\":\"deactivate\"}\n"
    );
    let (_, output) = run_fixture(&["--mode", "normal"], input);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let messages = json_lines(&output);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0]["method"], "update");
    assert_eq!(messages[0]["params"]["surface"], "fixture.workspace");
    let app: AppView = serde_json::from_value(messages[0]["params"]["app"].clone())
        .expect("registration deserializes as AppView");
    app.validate().expect("registration view validates");
    assert_eq!(app.revision, 1);
    assert_eq!(app.state_scope, "fixture");
    match &app.root {
        Widget::Text {
            id, title, text, ..
        } => {
            assert_eq!(id, "text");
            assert_eq!(title, "Fixture");
            assert_eq!(text, "Ready");
        }
        other => panic!("expected text widget, got {other:?}"),
    }
    assert_eq!(messages[1]["id"], 1);
    assert!(messages[1]["result"].is_null());
    assert!(messages[1].get("error").is_none());
}

#[test]
fn fault_fixture_invalid_view_mode_emits_a_view_that_fails_validation() {
    if !python_available() {
        return;
    }
    let (_, output) = run_fixture(&["--mode", "invalid-view"], "{\"method\":\"deactivate\"}\n");
    assert!(output.status.success());
    let messages = json_lines(&output);
    assert_eq!(messages.len(), 2, "{messages:?}");
    let valid: AppView = serde_json::from_value(messages[0]["params"]["app"].clone())
        .expect("first view deserializes");
    valid.validate().expect("first view validates");
    let invalid: AppView = serde_json::from_value(messages[1]["params"]["app"].clone())
        .expect("invalid view still deserializes");
    assert!(
        invalid.validate().is_err(),
        "empty widget id must fail validation"
    );
}

#[test]
fn fault_fixture_malformed_mode_emits_one_line_that_is_not_json() {
    if !python_available() {
        return;
    }
    let (_, output) = run_fixture(&["--mode", "malformed"], "{\"method\":\"deactivate\"}\n");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("plugin stdout is UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    serde_json::from_str::<serde_json::Value>(lines[0]).expect("first stdout line is JSON");
    assert!(serde_json::from_str::<serde_json::Value>(lines[1]).is_err());
}

#[test]
fn fault_fixture_flood_mode_emits_2001_ordered_revisions() {
    if !python_available() {
        return;
    }
    let (_, output) = run_fixture(&["--mode", "flood"], "{\"method\":\"deactivate\"}\n");
    assert!(output.status.success());
    let messages = json_lines(&output);
    assert_eq!(messages.len(), 2001);
    for (index, message) in messages.iter().enumerate() {
        assert_eq!(
            message["params"]["app"]["revision"].as_u64(),
            Some(index as u64 + 1)
        );
    }
}

#[test]
fn fault_fixture_stderr_flood_mode_writes_76000_stderr_bytes_and_exits_cleanly() {
    if !python_available() {
        return;
    }
    let (_, output) = run_fixture(&["--mode", "stderr-flood"], "{\"method\":\"deactivate\"}\n");
    assert!(output.status.success());
    assert_eq!(output.stderr.len(), 76_000);
    assert_eq!(json_lines(&output).len(), 1);
}

#[test]
fn fault_fixture_crash_mode_exits_3_without_output() {
    if !python_available() {
        return;
    }
    let (_, output) = run_fixture(&["--mode", "crash"], "");
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
}

#[test]
fn fault_fixture_delay_mode_waits_before_the_first_view() {
    if !python_available() {
        return;
    }
    let started = std::time::Instant::now();
    let (_, output) = run_fixture(&["--mode", "delay"], "{\"method\":\"deactivate\"}\n");
    assert!(started.elapsed() >= std::time::Duration::from_millis(150));
    assert!(output.status.success());
    let messages = json_lines(&output);
    assert_eq!(messages[0]["method"], "update");
    assert_eq!(messages[0]["params"]["app"]["revision"], 1);
}

#[test]
fn fault_fixture_pid_file_names_the_child() {
    if !python_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("pid");
    let pid_path = pid_file.to_str().expect("temporary path is UTF-8");
    let (pid, output) = run_fixture(
        &["--mode", "normal", "--pid-file", pid_path],
        "{\"method\":\"deactivate\"}\n",
    );
    assert!(output.status.success());
    let recorded_pid: u32 = std::fs::read_to_string(pid_file)
        .expect("pid file exists")
        .parse()
        .expect("pid file contains a process id");
    assert_eq!(recorded_pid, pid);
}

#[test]
fn fault_fixture_no_read_mode_produces_nothing_until_killed() {
    if !python_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("pid");
    let pid_path = pid_file.to_str().expect("temporary path is UTF-8");
    let mut child = std::process::Command::new("python3")
        .arg("fault.py")
        .args(["--mode", "no-read", "--pid-file", pid_path])
        .current_dir(fixtures_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 no-read fixture starts");
    let child_pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid_ready = loop {
        if std::fs::read_to_string(&pid_file).is_ok_and(|contents| !contents.is_empty()) {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    if !pid_ready {
        let kill_result = child.kill();
        let _output = child.wait_with_output().expect("wait for timed-out child");
        kill_result.expect("kill no-read child after timeout");
        panic!("no-read fixture did not create its pid file within 10 seconds");
    }
    let kill_result = child.kill();
    let output = child.wait_with_output().expect("wait for killed child");
    kill_result.expect("kill no-read child");
    assert!(output.stdout.is_empty());
    assert!(!output.status.success());
    let recorded_pid: u32 = std::fs::read_to_string(pid_file)
        .expect("pid file exists")
        .parse()
        .expect("pid file contains a process id");
    assert_eq!(recorded_pid, child_pid);
}
