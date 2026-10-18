use super::*;

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

fn run(script: &str) -> Result<PluginRun, PluginError> {
    spawn_ndjson(&sh(script), &BTreeMap::new(), None, Duration::from_secs(10))
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
fn strict_frame_decoder_accepts_only_canonical_rpc_frames() {
    use thegn_core::plugin_api::{Frame, RpcErrorCode};

    let valid_message = decode_frame(r#"{"id":7,"method":"ping","params":{"x":1}}"#).unwrap();
    assert!(matches!(valid_message, Frame::Message(message) if message.id == Some(7)));
    let bare_message = decode_frame(r#"{"method":"ping"}"#).unwrap();
    assert!(matches!(bare_message, Frame::Message(message) if message.params.is_null()));
    let valid_response = decode_frame(r#"{"id":7,"result":null}"#).unwrap();
    assert!(
        matches!(valid_response, Frame::Response(response) if response.result == Some(serde_json::Value::Null))
    );
    let valid_error =
        decode_frame(r#"{"id":7,"error":{"code":"invalid","message":"bad"}}"#).unwrap();
    assert!(
        matches!(valid_error, Frame::Response(response) if response.error.as_ref().is_some_and(|error| error.code == RpcErrorCode::Invalid))
    );

    let overlong_method = format!(r#"{{"method":"{}"}}"#, "m".repeat(MAX_METHOD_BYTES + 1));
    let rejected = [
        (
            r#"{"id":7,"result":1,"error":{"code":"invalid","message":"bad"}}"#,
            FrameReject::ResponseArms,
        ),
        (r#"{"id":7}"#, FrameReject::ResponseArms),
        (r#"{"method":"x","result":1}"#, FrameReject::Mixed),
        (r#"{"method":"x","error":{}}"#, FrameReject::Mixed),
        (r#"{"method":"x","bogus":1}"#, FrameReject::Member),
        (r#"{"method":"x","method":"y"}"#, FrameReject::Member),
        (r#"{"method":"x","id":null}"#, FrameReject::Id),
        (r#"{"method":"x","id":-1}"#, FrameReject::Id),
        (r#"{"method":"x","id":"1"}"#, FrameReject::Id),
        (r#"{"method":""}"#, FrameReject::Method),
        (overlong_method.as_str(), FrameReject::Method),
        ("[]", FrameReject::NotObject),
    ];
    for (text, expected) in rejected {
        assert_eq!(decode_frame(text).unwrap_err(), expected, "frame {text:?}");
    }
}

#[test]
fn frame_depth_is_bounded_before_json_materialization() {
    let at_limit = format!(
        "{{\"method\":\"x\",\"params\":{}0{}}}",
        "[".repeat(MAX_FRAME_DEPTH - 1),
        "]".repeat(MAX_FRAME_DEPTH - 1)
    );
    assert!(!json_depth_exceeds(&at_limit, MAX_FRAME_DEPTH));
    assert!(matches!(decode_frame(&at_limit), Ok(_)));

    let over_limit = format!(
        "{{\"method\":\"x\",\"params\":{}0{}}}",
        "[".repeat(MAX_FRAME_DEPTH),
        "]".repeat(MAX_FRAME_DEPTH)
    );
    assert!(json_depth_exceeds(&over_limit, MAX_FRAME_DEPTH));
    assert!(matches!(decode_frame(&over_limit), Err(FrameReject::Depth)));
}

#[test]
fn frame_depth_prescan_stops_early_and_ignores_string_brackets() {
    let excessive = "[".repeat(10_000);
    assert!(json_depth_exceeds(&excessive, MAX_FRAME_DEPTH));
    assert!(matches!(decode_frame(&excessive), Err(FrameReject::Depth)));

    assert!(!json_depth_exceeds(r#"{"text":"[[[["}"#, MAX_FRAME_DEPTH));
    assert!(!json_depth_exceeds(r#"{"text":"\"[[[["}"#, MAX_FRAME_DEPTH));
}

#[test]
fn kill_group_errno_classification_silences_esrch_and_warns_other_errors() {
    assert!(!kill_errno_should_warn(libc::ESRCH));
    assert!(kill_errno_should_warn(libc::EPERM));
}

#[test]
fn malformed_frames_are_junk_and_do_not_stop_later_messages() {
    for malformed in [
        r#"printf '%s\n' '{"method":"x","result":1}'"#,
        r#"printf '%s\n' '{"method":"x","bogus":1}'"#,
        r#"printf '{"method":"\377"}\n'"#,
    ] {
        let script = format!("{malformed}; printf '%s\\n' '{{\"method\":\"after\"}}'");
        let out = run(&script).unwrap();
        assert_eq!(out.messages.len(), 1, "script: {script}");
        assert_eq!(out.messages[0].method, "after");
        assert_eq!(out.junk.len(), 1, "script: {script}");
    }
}

#[test]
fn one_shot_response_frames_are_junk() {
    let out = run(r#"printf '%s\n' '{"id":1,"result":null}'"#).unwrap();
    assert!(out.messages.is_empty());
    assert_eq!(out.junk, vec![r#"{"id":1,"result":null}"#.to_string()]);
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
fn collected_output_has_a_total_retained_byte_cap() {
    let line = format!(r#"{{"method":"x","params":"{}"}}"#, "a".repeat(64 * 1024));
    let mut collect = Collect {
        run: PluginRun::default(),
        retained_bytes: 0,
    };
    let mut accepted = 0usize;
    loop {
        if !collect.line(&line) {
            break;
        }
        accepted += 1;
    }

    assert!(
        collect.run.truncated,
        "byte-cap rejection must be observable"
    );
    assert!(accepted > 0);
    assert!(collect.retained_bytes <= MAX_RUN_BYTES + line.len());
}

#[test]
fn an_oversized_frame_is_discarded_as_one_frame() {
    let oversized_bytes = MAX_LINE_BYTES * 5 / 2;
    let script = format!(
        r#"head -c {oversized_bytes} /dev/zero | tr '\000' x; printf '\n{{"method":"ok","params":{{}}}}\n'"#
    );
    let out = run(&script).unwrap();
    assert!(out.truncated, "oversized frame must be reported");
    assert_eq!(out.messages.len(), 1, "the following frame must survive");
    assert_eq!(out.messages[0].method, "ok");
    assert!(
        out.junk.is_empty(),
        "oversized fragments are not pseudo-lines"
    );
}

#[test]
fn an_exited_leader_cannot_hold_pipe_reader_joins_open() {
    let start = std::time::Instant::now();
    let out = spawn_ndjson(
        &sh(r#"sleep 30 & printf '{"method":"ok","params":{}}\n'"#),
        &BTreeMap::new(),
        None,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(out.messages.len(), 1);
    assert_eq!(out.messages[0].method, "ok");
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "inherited pipe handles must be closed within the bounded drain grace"
    );
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
