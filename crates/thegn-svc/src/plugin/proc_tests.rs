use super::*;

/// These fixtures are written in POSIX `sh`, so they need a POSIX shell — not
/// the user's interactive one. On Windows that is the shell Git for Windows
/// ships, which `posix_shell()` resolves next to `git.exe`.
fn sh(script: &str) -> Vec<String> {
    vec![
        thegn_core::util::posix_shell().expect("a POSIX shell (Git for Windows ships one)"),
        "-c".into(),
        script.into(),
    ]
}

/// Headroom for a fixture that should finish instantly — not the thing under
/// test (`a_hung_plugin_is_killed_at_its_timeout` sets its own). "Instantly" is
/// relative: on Windows every one of these spawns MSYS `sh` through fork
/// emulation with a security agent inspecting the process creation, and under a
/// saturated suite that overran a 10s budget and failed on the fixture rather
/// than on anything the test asserts.
const FIXTURE_BUDGET: Duration = if cfg!(windows) {
    Duration::from_secs(60)
} else {
    Duration::from_secs(10)
};

fn run(script: &str) -> Result<PluginRun, PluginError> {
    spawn_ndjson(&sh(script), &BTreeMap::new(), None, FIXTURE_BUDGET)
}

#[test]
fn reads_newline_delimited_messages_in_order() {
    let out = run(
        r#"echo '{"method":"manifest","params":{"id":"x"}}'; echo '{"method":"events","params":{"events":[]}}'"#,
    )
    .unwrap();
    assert_eq!(out.messages.len(), 2);
    assert_eq!(out.messages[0].method, "manifest");
    assert_eq!(out.messages[1].method, "events");
    assert!(out.junk.is_empty());
    assert!(!out.truncated);
}

#[test]
fn a_verb_with_no_params_is_accepted() {
    // A plugin author shouldn't have to type `"params":{}`.
    let out = run(r#"echo '{"method":"ping"}'"#).unwrap();
    assert_eq!(out.messages.len(), 1);
    assert!(out.messages[0].params.is_null());
}

#[test]
fn stray_non_json_output_is_kept_as_junk_not_silently_dropped() {
    // A leftover `echo debugging` is the most common plugin mistake; surfacing
    // it is the difference between a two-minute fix and a mystery.
    let out = run(r#"echo hello; echo '{"method":"events","params":{}}'"#).unwrap();
    assert_eq!(out.messages.len(), 1);
    assert_eq!(out.junk, vec!["hello".to_string()]);
}

#[test]
fn blank_lines_are_ignored() {
    let out = run(r#"echo ''; echo '{"method":"a","params":{}}'; echo ''"#).unwrap();
    assert_eq!(out.messages.len(), 1);
    assert!(out.junk.is_empty());
}

#[test]
fn a_nonzero_exit_carries_the_stderr_tail() {
    // The detail `agent_run`'s discard-the-pipes approach throws away.
    let err = run("echo 'boom' >&2; exit 3").unwrap_err();
    match err {
        PluginError::Exit { code, stderr } => {
            assert_eq!(code, Some(3));
            assert!(stderr.contains("boom"), "got {stderr:?}");
        }
        other => panic!("expected Exit, got {other:?}"),
    }
}

#[test]
fn a_missing_program_is_a_spawn_error() {
    let err = spawn_ndjson(
        &["definitely-not-a-real-binary-xyz".into()],
        &BTreeMap::new(),
        None,
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(matches!(err, PluginError::Spawn(_)), "got {err:?}");
}

#[test]
fn a_hung_plugin_is_killed_at_its_timeout() {
    let start = std::time::Instant::now();
    let err = spawn_ndjson(
        &sh("sleep 30"),
        &BTreeMap::new(),
        None,
        Duration::from_millis(300),
    )
    .unwrap_err();
    assert!(matches!(err, PluginError::Timeout(_)), "got {err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "must not wait out the child"
    );
}

#[test]
fn the_environment_is_passed_through() {
    let mut env = BTreeMap::new();
    env.insert("THEGN_TEST_VALUE".into(), "42".into());
    let out = spawn_ndjson(
        &sh(r#"echo "{\"method\":\"v\",\"params\":{\"n\":$THEGN_TEST_VALUE}}""#),
        &env,
        None,
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(out.messages[0].params["n"], 42);
}

#[test]
fn inherited_git_state_is_scrubbed() {
    // A plugin that shells out to git must not operate on whatever repo thegn
    // happened to be looking at.
    unsafe { std::env::set_var("GIT_DIR", "/nonexistent/.git") };
    let out = run(r#"echo "{\"method\":\"v\",\"params\":{\"g\":\"${GIT_DIR:-unset}\"}}""#).unwrap();
    unsafe { std::env::remove_var("GIT_DIR") };
    assert_eq!(out.messages[0].params["g"], "unset");
}

#[test]
fn stdin_is_closed_so_a_reading_plugin_gets_eof() {
    // Otherwise a plugin that reads stdin would hang until its timeout.
    let out = run(r#"cat > /dev/null; echo '{"method":"done","params":{}}'"#).unwrap();
    assert_eq!(out.messages[0].method, "done");
}

#[test]
fn output_is_capped_rather_than_buffered_without_limit() {
    let script = format!(
        r#"i=0; while [ $i -lt {} ]; do echo '{{"method":"e","params":{{}}}}'; i=$((i+1)); done"#,
        MAX_LINES + 50
    );
    let out = run(&script).unwrap();
    assert!(out.truncated, "must report that it stopped early");
    assert!(out.messages.len() <= MAX_LINES);
}

#[test]
fn a_plugin_that_writes_a_lot_of_stderr_does_not_deadlock() {
    // Both pipes are drained concurrently; reading only stdout would block once
    // the stderr pipe filled.
    let out = run(
        r#"i=0; while [ $i -lt 2000 ]; do echo "noise line $i" >&2; i=$((i+1)); done; echo '{"method":"ok","params":{}}'"#,
    )
    .unwrap();
    assert_eq!(out.messages[0].method, "ok");
    assert!(out.stderr.len() <= MAX_STDERR);
}

#[test]
fn an_empty_command_is_rejected() {
    let err = spawn_ndjson(&[], &BTreeMap::new(), None, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(err, PluginError::Spawn(_)));
}

#[test]
fn errors_render_readably() {
    assert!(PluginError::Timeout(20).to_string().contains("20s"));
    assert!(
        PluginError::Exit {
            code: Some(1),
            stderr: "bad\n".into()
        }
        .to_string()
        .contains("bad")
    );
    assert!(
        PluginError::Spawn("nope".into())
            .to_string()
            .contains("nope")
    );
    assert!(
        PluginError::Protocol("x".into())
            .to_string()
            .contains("protocol")
    );
}
