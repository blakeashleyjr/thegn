use super::*;
use std::fs;
use std::process::Command as ProcessCommand;
use std::time::Instant;

fn fixture_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("thegn-filter-")
        .tempdir()
        .unwrap()
}

fn server_bytes(responses: &[&[u8]]) -> Vec<u8> {
    let mut bytes = b"0016git-filter-server\n000eversion=2\n0000".to_vec();
    bytes.extend_from_slice(b"0015capability=clean\n0000");
    for response in responses {
        bytes.extend_from_slice(response);
    }
    bytes
}

fn shell_command(script: &str, args: &[&str]) -> ProcessCommand {
    let mut command = ProcessCommand::new("sh");
    command.arg("-c").arg(script).arg("sh").args(args);
    command
}

#[test]
fn one_process_serves_many_requests() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let input = dir.path().join("server-bytes");
    let marker = dir.path().join("spawn-marker");
    let responses = [
        b"0013status=success\n00000007one00000000".as_slice(),
        b"0013status=success\n00000007two00000000".as_slice(),
        b"0013status=success\n00000009three00000000".as_slice(),
    ];
    fs::write(&input, server_bytes(&responses)).unwrap();
    let marker_s = marker.to_string_lossy().into_owned();
    let input_s = input.to_string_lossy().into_owned();
    let script = "echo x >> \"$1\"; cat \"$2\"; exec cat >/dev/null";
    let mut process = FilterProcess::spawn(
        shell_command(script, &[&marker_s, &input_s]),
        Capabilities::new(true, false),
        Duration::from_secs(2),
    )
    .unwrap();
    for (path, expected) in [
        ("a", b"one".as_slice()),
        ("b", b"two".as_slice()),
        ("c", b"three".as_slice()),
    ] {
        assert_eq!(
            process
                .request(Command::Clean, path, b"in".to_vec(), 64)
                .unwrap(),
            Outcome::Success(expected.to_vec())
        );
    }
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
    match process.shutdown() {
        Reap::Exited(status) => assert!(status.success()),
        Reap::Killed(_) | Reap::Failed(_) => panic!("filter did not exit cleanly"),
    }
}

#[test]
fn a_stalled_filter_is_killed_and_reaped_at_the_deadline() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let input = dir.path().join("server-bytes");
    fs::write(
        &input,
        b"0016git-filter-server\n000eversion=2\n00000015capability=clean\n0000",
    )
    .unwrap();
    let input_s = input.to_string_lossy().into_owned();
    let mut process = FilterProcess::spawn(
        shell_command("cat \"$1\"; exec sleep 30", &[&input_s]),
        Capabilities::new(true, false),
        Duration::from_millis(300),
    )
    .unwrap();
    let started = Instant::now();
    assert!(matches!(
        process.request(Command::Clean, "x", Vec::new(), 8),
        Err(FilterProcessError::TimedOut { .. })
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(
        process.request(Command::Clean, "x", Vec::new(), 8),
        Err(FilterProcessError::Dead)
    ));
    assert!(matches!(process.shutdown(), Reap::Killed(_)));
}

#[test]
fn handshake_rejection_is_reaped() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let result = FilterProcess::spawn(
        shell_command("exec cat", &[]),
        Capabilities::new(true, false),
        Duration::from_secs(2),
    );
    match result {
        Err(FilterProcessError::Handshake {
            reap: Reap::Exited(_) | Reap::Killed(_),
            ..
        }) => {}
        Err(FilterProcessError::Handshake {
            reap: Reap::Failed(_),
            ..
        }) => panic!("handshake child was not reaped"),
        Err(_) => panic!("unexpected spawn failure"),
        Ok(process) => {
            let _reap = process.shutdown();
            panic!("echo server unexpectedly completed handshake");
        }
    }
}

#[test]
fn missing_program_is_a_spawn_error() {
    let result = FilterProcess::spawn(
        ProcessCommand::new("thegn-no-such-filter-program"),
        Capabilities::new(true, false),
        Duration::from_secs(1),
    );
    assert!(matches!(result, Err(FilterProcessError::Spawn(_))));
}
