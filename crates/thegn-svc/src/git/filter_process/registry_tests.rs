use super::*;
use std::fs;
use std::process::Command as ProcessCommand;

fn fixture_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("thegn-filter-registry-")
        .tempdir()
        .unwrap()
}

fn command(script: &str, args: &[&str]) -> ProcessCommand {
    let mut command = ProcessCommand::new("sh");
    command.arg("-c").arg(script).arg("sh").args(args);
    command
}

fn pkt(bytes: &mut Vec<u8>, payload: &[u8]) {
    let length = payload.len() + 4;
    bytes.extend_from_slice(format!("{length:04x}").as_bytes());
    bytes.extend_from_slice(payload);
}

fn flush(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(b"0000");
}

fn server_bytes(responses: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in [b"git-filter-server\n".as_slice(), b"version=2\n"] {
        pkt(&mut bytes, line);
    }
    flush(&mut bytes);
    pkt(&mut bytes, b"capability=clean\n");
    flush(&mut bytes);
    for _ in 0..responses {
        pkt(&mut bytes, b"status=success\n");
        flush(&mut bytes);
        pkt(&mut bytes, b"cleaned\n");
        flush(&mut bytes);
        pkt(&mut bytes, b"status=success\n");
        flush(&mut bytes);
    }
    bytes
}

fn key(dir: &tempfile::TempDir, name: &str) -> FilterKey {
    FilterKey {
        workdir: dir.path().to_owned(),
        process: name.to_owned(),
    }
}

fn clean(registry: &FilterRegistry, key: FilterKey, spawn: impl Fn() -> ProcessCommand) -> Routed {
    registry.clean(key, || Ok(spawn()), "file.dat", Arc::from(&b"raw"[..]))
}

#[test]
fn many_cleans_reuse_one_process_and_drop_reaps_it() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let canned = dir.path().join("server");
    let marker = dir.path().join("marker");
    fs::write(&canned, server_bytes(5)).unwrap();
    let marker_arg = marker.to_string_lossy().into_owned();
    let canned_arg = canned.to_string_lossy().into_owned();
    {
        let registry =
            FilterRegistry::with_limits(Duration::from_secs(120), 8, Duration::from_secs(60));
        for _ in 0..5 {
            assert!(matches!(
                clean(&registry, key(&dir, "driver"), || command(
                    "echo x >> \"$1\"; cat \"$2\"; cat >/dev/null; echo exited >> \"$1\"",
                    &[&marker_arg, &canned_arg],
                )),
                Routed::Done(Outcome::Success(_))
            ));
        }
    }
    let marker = fs::read_to_string(marker).unwrap();
    assert_eq!(marker.lines().collect::<Vec<_>>(), ["x", "exited"]);
}

#[test]
fn a_dead_process_respawns_once_with_the_same_request() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let marker = dir.path().join("marker");
    let marker_arg = marker.to_string_lossy().into_owned();
    let mut canned = server_bytes(1);
    canned.extend_from_slice(b"zzzz");
    let input = dir.path().join("server");
    fs::write(&input, canned).unwrap();
    let input_arg = input.to_string_lossy().into_owned();
    let registry =
        FilterRegistry::with_limits(Duration::from_secs(120), 8, Duration::from_secs(60));
    for _ in 0..2 {
        assert!(matches!(
            clean(&registry, key(&dir, "driver"), || command(
                "echo x >> \"$1\"; cat \"$2\"; exec cat >/dev/null",
                &[&marker_arg, &input_arg],
            )),
            Routed::Done(Outcome::Success(_))
        ));
    }
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 2);
}

#[test]
fn a_broken_filter_respawns_once_then_backs_off() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let marker = dir.path().join("marker");
    let marker_arg = marker.to_string_lossy().into_owned();
    let input = dir.path().join("server");
    fs::write(&input, [server_bytes(0), b"zzzz".to_vec()].concat()).unwrap();
    let input_arg = input.to_string_lossy().into_owned();
    let registry =
        FilterRegistry::with_limits(Duration::from_secs(120), 8, Duration::from_secs(60));
    let spawn = || {
        command(
            "echo x >> \"$1\"; cat \"$2\"; exec cat >/dev/null",
            &[&marker_arg, &input_arg],
        )
    };
    assert!(matches!(
        clean(&registry, key(&dir, "driver"), spawn),
        Routed::Unavailable
    ));
    assert!(matches!(
        clean(&registry, key(&dir, "driver"), spawn),
        Routed::Unavailable
    ));
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 2);
}

#[test]
fn handshake_failure_backs_off_after_one_spawn() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let marker = dir.path().join("marker");
    let marker_arg = marker.to_string_lossy().into_owned();
    let registry =
        FilterRegistry::with_limits(Duration::from_secs(120), 8, Duration::from_secs(60));
    let spawn = || command("echo x >> \"$1\"; exec cat", &[&marker_arg]);
    for _ in 0..3 {
        assert!(matches!(
            clean(&registry, key(&dir, "driver"), spawn),
            Routed::Unavailable
        ));
    }
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
}

#[test]
fn idle_entries_shut_down_on_the_next_use() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let input = dir.path().join("server");
    let marker = dir.path().join("marker");
    fs::write(&input, server_bytes(2)).unwrap();
    let input_arg = input.to_string_lossy().into_owned();
    let marker_arg = marker.to_string_lossy().into_owned();
    let registry = FilterRegistry::with_limits(Duration::ZERO, 8, Duration::from_secs(60));
    let spawn = || {
        command(
            "echo x >> \"$1\"; cat \"$2\"; cat >/dev/null; echo exited >> \"$1\"",
            &[&marker_arg, &input_arg],
        )
    };
    assert!(matches!(
        clean(&registry, key(&dir, "a"), spawn),
        Routed::Done(_)
    ));
    assert!(matches!(
        clean(&registry, key(&dir, "b"), spawn),
        Routed::Done(_)
    ));
    assert!(
        fs::read_to_string(&marker)
            .unwrap()
            .lines()
            .any(|line| line == "exited")
    );
    assert_eq!(registry.live(), 1);
}

#[test]
fn lru_cap_evicts_the_oldest_entry() {
    if !thegn_core::util::have("sh") {
        return;
    }
    let dir = fixture_dir();
    let input = dir.path().join("server");
    fs::write(&input, server_bytes(2)).unwrap();
    let input_arg = input.to_string_lossy().into_owned();
    let marker = dir.path().join("evicted");
    let marker_arg = marker.to_string_lossy().into_owned();
    let registry =
        FilterRegistry::with_limits(Duration::from_secs(120), 1, Duration::from_secs(60));
    let spawn_a = || {
        command(
            "cat \"$1\"; cat >/dev/null; echo exited >> \"$2\"",
            &[&input_arg, &marker_arg],
        )
    };
    let spawn_b = || command("cat \"$1\"; exec cat >/dev/null", &[&input_arg]);
    assert!(matches!(
        clean(&registry, key(&dir, "a"), spawn_a),
        Routed::Done(_)
    ));
    assert!(matches!(
        clean(&registry, key(&dir, "b"), spawn_b),
        Routed::Done(_)
    ));
    assert!(
        fs::read_to_string(&marker)
            .unwrap()
            .lines()
            .any(|line| line == "exited")
    );
    assert_eq!(registry.live(), 1);
    assert!(matches!(
        clean(&registry, key(&dir, "a"), spawn_a),
        Routed::Done(_)
    ));
}
