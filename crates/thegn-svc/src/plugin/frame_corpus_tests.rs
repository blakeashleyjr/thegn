use super::{PluginRun, SessionEvent, proc::spawn_ndjson, session::tests::FixtureSupervisor};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

fn octal(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>()
}

fn shell_stream(bytes: &[u8]) -> Vec<String> {
    let escaped = octal(bytes);
    vec!["sh".into(), "-c".into(), format!("printf '%b' '{escaped}'")]
}

fn every_split_stream(frame: &[u8]) -> Vec<String> {
    let mut script = String::new();
    for boundary in 1..frame.len() {
        script.push_str("printf '%b' '");
        script.push_str(&octal(&frame[..boundary]));
        script.push_str("'; sleep 0.002; printf '%b' '");
        script.push_str(&octal(&frame[boundary..]));
        script.push_str("\\n';");
    }
    vec!["sh".into(), "-c".into(), script]
}

fn one_shot(argv: &[String]) -> PluginRun {
    spawn_ndjson(argv, &BTreeMap::new(), None, Duration::from_secs(10))
        .expect("one-shot plugin stream")
}

#[test]
fn byte_stream_corpus_has_matching_message_and_rejection_order() {
    let bytes = concat!(
        "{\"method\":\"crlf\"}\r\n",
        "\r\n",
        "\r{\"method\":\"bare-cr\"}\n",
        "{\"method\":\"depth\",\"params\":",
        "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[0]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]}\n",
        "{\"method\":\"duplicate\",\"method\":\"again\"}\n",
        "{\"method\":\"mixed\",\"result\":1}\n",
        "{\"method\":\"unknown\",\"extra\":1}\n"
    )
    .as_bytes()
    .to_vec();
    let mut stream = bytes;
    stream.extend_from_slice(b"{\"method\":\"");
    stream.push(0xff);
    stream.extend_from_slice(b"\"}\n");
    stream.extend_from_slice(b"{\"method\":\"final\"}");
    let argv = shell_stream(&stream);

    let one_shot = one_shot(&argv);
    let (tx, rx) = mpsc::channel();
    let fixture = FixtureSupervisor::new();
    let _session = fixture
        .spawn(&argv, &BTreeMap::new(), None, move |event| {
            drop(tx.send(event));
        })
        .expect("resident plugin stream");
    let mut resident_messages = Vec::new();
    let mut resident_junk = Vec::new();
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("resident event");
        match event {
            SessionEvent::Message(message) => resident_messages.push(message.method),
            SessionEvent::Junk(text) => resident_junk.push(text),
            SessionEvent::Exit { .. } => break,
            SessionEvent::Response(_) => panic!("request-only corpus produced response"),
        }
    }

    let one_shot_messages: Vec<_> = one_shot
        .messages
        .iter()
        .map(|message| message.method.as_str())
        .collect();
    assert_eq!(
        one_shot_messages,
        ["crlf", "bare-cr", "final"],
        "the byte corpus must retain the same valid frames in order"
    );
    assert_eq!(
        resident_messages
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["crlf", "bare-cr", "final"]
    );
    assert_eq!(one_shot.junk.len(), 5, "each malformed line is rejected");
    assert_eq!(
        resident_junk.len(),
        5,
        "resident framing rejects the same lines"
    );
    assert!(!one_shot.truncated);
}

#[test]
fn line_cap_boundaries_reject_whole_records_and_resume_at_the_next_frame() {
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "for n in 1048575 1048576 1048577; do head -c \"$n\" /dev/zero | tr '\\000' x; printf '\\n'; printf '%s\\n' '{\"method\":\"after\"}'; done".into(),
    ];

    let one_shot = one_shot(&argv);
    assert_eq!(
        one_shot.messages.len(),
        3,
        "each accepted boundary or oversized record must leave the next frame intact"
    );
    assert!(
        one_shot
            .messages
            .iter()
            .all(|message| message.method == "after")
    );
    assert!(one_shot.truncated);

    let (tx, rx) = mpsc::channel();
    let fixture = FixtureSupervisor::new();
    let _session = fixture
        .spawn(&argv, &BTreeMap::new(), None, move |event| {
            drop(tx.send(event));
        })
        .expect("resident boundary stream");
    let mut accepted = Vec::new();
    let mut rejected = 0;
    loop {
        match rx
            .recv_timeout(Duration::from_secs(10))
            .expect("resident boundary event")
        {
            SessionEvent::Message(message) => accepted.push(message.method),
            SessionEvent::Junk(_) => rejected += 1,
            SessionEvent::Exit { .. } => break,
            SessionEvent::Response(_) => panic!("boundary stream produced response"),
        }
    }
    assert_eq!(
        accepted.iter().map(String::as_str).collect::<Vec<_>>(),
        ["after", "after", "after"]
    );
    assert_eq!(rejected, 3);
}

#[test]
fn short_frame_survives_a_write_split_at_each_byte_boundary() {
    let frame = br#"{"method":"split"}"#;
    let argv = every_split_stream(frame);
    let expected_count = frame.len() - 1;

    let one_shot = one_shot(&argv);
    assert_eq!(one_shot.messages.len(), expected_count);
    assert!(one_shot.junk.is_empty());

    let (tx, rx) = mpsc::channel();
    let fixture = FixtureSupervisor::new();
    let _session = fixture
        .spawn(&argv, &BTreeMap::new(), None, move |event| {
            drop(tx.send(event));
        })
        .expect("resident split stream");
    let mut accepted = 0;
    loop {
        match rx
            .recv_timeout(Duration::from_secs(10))
            .expect("resident split event")
        {
            SessionEvent::Message(message) => {
                assert_eq!(message.method, "split");
                accepted += 1;
            }
            SessionEvent::Junk(text) => panic!("split valid frame rejected: {text:?}"),
            SessionEvent::Exit { .. } => break,
            SessionEvent::Response(_) => panic!("split stream produced response"),
        }
    }
    assert_eq!(accepted, expected_count);
}
