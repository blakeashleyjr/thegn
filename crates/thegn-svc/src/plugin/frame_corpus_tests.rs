use super::{
    PluginRun, SessionEvent, proc::FrameReject, proc::spawn_ndjson,
    session::tests::FixtureSupervisor,
};
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
    vec![
        "sh".into(),
        "-c".into(),
        format!("printf '%b' '{escaped}'; sleep 0.2"),
    ]
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
    script.push_str("sleep 0.2");
    vec!["sh".into(), "-c".into(), script]
}

fn one_shot(argv: &[String]) -> PluginRun {
    spawn_ndjson(argv, &BTreeMap::new(), None, Duration::from_secs(10))
        .expect("one-shot plugin stream")
}

fn resident_events(argv: &[String]) -> (Vec<String>, Vec<String>, usize) {
    let (tx, rx) = mpsc::channel();
    let fixture = FixtureSupervisor::new();
    let _session = fixture
        .spawn(argv, &BTreeMap::new(), None, move |event| {
            drop(tx.send(event));
        })
        .expect("resident plugin stream");
    let mut messages = Vec::new();
    let mut junk = Vec::new();
    let mut responses = 0;
    loop {
        match rx
            .recv_timeout(Duration::from_secs(10))
            .expect("resident event")
        {
            SessionEvent::Message(message) => messages.push(message.method),
            SessionEvent::Junk(text) => junk.push(text),
            SessionEvent::Response(_) => responses += 1,
            SessionEvent::Exit { .. } => break,
        }
    }
    (messages, junk, responses)
}

fn nested_frame(array_count: usize) -> String {
    format!(
        "{{\"method\":\"depth\",\"params\":{}0{}}}",
        "[".repeat(array_count),
        "]".repeat(array_count)
    )
}

#[test]
fn byte_stream_corpus_matches_messages_and_reject_classes() {
    let mut lines = vec![
        (b"{\"method\":\"crlf\"}".to_vec(), None),
        (Vec::new(), None),
        (Vec::new(), None),
        (b"\r{\"method\":\"bare-cr\"}".to_vec(), None),
        (nested_frame(31).into_bytes(), None),
        (nested_frame(32).into_bytes(), Some(FrameReject::Depth)),
        (
            b"{\"method\":\"duplicate\",\"method\":\"again\"}".to_vec(),
            Some(FrameReject::Member),
        ),
        (
            b"{\"method\":\"mixed\",\"result\":1}".to_vec(),
            Some(FrameReject::Mixed),
        ),
        (
            b"{\"id\":1,\"result\":1,\"error\":{\"code\":\"invalid\",\"message\":\"x\"}}".to_vec(),
            Some(FrameReject::ResponseArms),
        ),
        (
            b"{\"method\":\"unknown\",\"extra\":1}".to_vec(),
            Some(FrameReject::Member),
        ),
        (
            b"{\"method\":\"a\"}\r{\"method\":\"b\"}".to_vec(),
            Some(FrameReject::Syntax),
        ),
    ];
    let mut invalid_utf8 = b"{\"method\":\"".to_vec();
    invalid_utf8.push(0xff);
    invalid_utf8.extend_from_slice(b"\"}");
    lines.push((invalid_utf8, Some(FrameReject::InvalidUtf8)));

    let mut stream = Vec::new();
    for (index, (line, _)) in lines.iter().enumerate() {
        if line.is_empty() {
            if index == 1 {
                stream.push(b'\n');
            } else {
                stream.extend_from_slice(b"\r\n");
            }
        } else if index == 0 {
            stream.extend_from_slice(line);
            stream.extend_from_slice(b"\r\n");
        } else {
            stream.extend_from_slice(line);
            stream.push(b'\n');
        }
    }
    stream.extend_from_slice(b"{\"method\":\"final\"}");
    let argv = shell_stream(&stream);

    let mut expected_classes = Vec::new();
    for (line, expected) in &lines {
        if line.is_empty() {
            continue;
        }
        let actual = std::str::from_utf8(line)
            .map_or(Err(FrameReject::InvalidUtf8), |text| {
                super::proc::decode_frame(text)
            })
            .map(|_| ());
        match expected {
            Some(class) => {
                assert_eq!(actual, Err(*class), "reject class for {line:?}");
                expected_classes.push(*class);
            }
            None => assert!(actual.is_ok(), "valid frame rejected: {line:?}"),
        }
    }
    assert_eq!(
        expected_classes,
        [
            FrameReject::Depth,
            FrameReject::Member,
            FrameReject::Mixed,
            FrameReject::ResponseArms,
            FrameReject::Member,
            FrameReject::Syntax,
            FrameReject::InvalidUtf8,
        ]
    );
    let expected_junk: Vec<_> = lines
        .iter()
        .filter_map(|(line, expected)| {
            expected.map(|_| {
                String::from_utf8_lossy(line)
                    .trim()
                    .chars()
                    .take(200)
                    .collect::<String>()
            })
        })
        .collect();

    let one_shot = one_shot(&argv);
    let (resident_messages, resident_junk, responses) = resident_events(&argv);
    let one_shot_messages: Vec<_> = one_shot
        .messages
        .iter()
        .map(|message| message.method.as_str())
        .collect();
    assert_eq!(one_shot_messages, ["crlf", "bare-cr", "depth", "final"]);
    assert_eq!(
        resident_messages
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["crlf", "bare-cr", "depth", "final"]
    );
    assert_eq!(
        one_shot.junk, expected_junk,
        "one-shot reject lines/classes"
    );
    assert_eq!(
        resident_junk, expected_junk,
        "resident reject lines/classes"
    );
    assert_eq!(responses, 0);
    assert!(!one_shot.truncated);
}

#[test]
fn response_frame_is_one_shot_junk_but_a_resident_response() {
    let line = br#"{"id":1,"result":1}"#;
    let argv = shell_stream(&[&line[..], b"\n"].concat());

    let one_shot = one_shot(&argv);
    let (messages, resident_junk, responses) = resident_events(&argv);

    assert_eq!(one_shot.junk, [String::from_utf8_lossy(line).into_owned()]);
    assert!(messages.is_empty());
    assert!(resident_junk.is_empty());
    assert_eq!(responses, 1);
}

#[test]
fn line_cap_boundaries_accept_max_minus_one_and_reject_max_and_plus_one() {
    // The fixed JSON prefix/suffix use 28 bytes, so each padded frame has the
    // requested content length before its newline.
    let argv = vec![
        "sh".into(),
        "-c".into(),
        r#"for n in 1048575 1048576 1048577; do pad=$((n - 28)); printf '%s' '{"method":"pad","params":"'; head -c "$pad" /dev/zero | tr '\000' x; printf '%s\n' '"}'; printf '%s\n' '{"method":"after"}'; done; sleep 0.2"#.into(),
    ];

    let one_shot = one_shot(&argv);
    assert_eq!(
        one_shot
            .messages
            .iter()
            .map(|message| message.method.as_str())
            .collect::<Vec<_>>(),
        ["pad", "after", "after", "after"],
        "MAX-1 valid JSON is accepted and each rejected boundary resumes at after"
    );
    assert!(
        one_shot.junk.is_empty(),
        "oversized records signal via truncated"
    );
    assert!(one_shot.truncated);

    let (resident_messages, resident_junk, _) = resident_events(&argv);
    assert_eq!(
        resident_messages
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["pad", "after", "after", "after"]
    );
    let oversized_prefix = format!("{{\"method\":\"pad\",\"params\":\"{}", "x".repeat(174));
    assert_eq!(resident_junk, [oversized_prefix.clone(), oversized_prefix]);
    // One-shot signals oversized boundaries through truncation; the resident
    // transport emits a bounded Junk row for each one.
}

#[test]
fn short_frame_survives_a_write_split_at_each_byte_boundary() {
    let frame = br#"{"method":"split"}"#;
    let argv = every_split_stream(frame);
    let expected_count = frame.len() - 1;

    let one_shot = one_shot(&argv);
    assert_eq!(one_shot.messages.len(), expected_count);
    assert!(one_shot.junk.is_empty());

    let (messages, junk, responses) = resident_events(&argv);
    assert!(junk.is_empty(), "split valid frames are never rejected");
    assert_eq!(responses, 0);
    assert_eq!(messages.len(), expected_count);
    assert!(messages.iter().all(|method| method == "split"));
}
