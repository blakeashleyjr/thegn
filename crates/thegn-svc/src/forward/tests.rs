use super::*;

#[test]
fn ss_probe_cmd_prefers_ss_then_netstat() {
    let c = ss_probe_cmd();
    assert_eq!(c[0], "sh");
    assert_eq!(c[1], "-c");
    assert!(c[2].contains("ss -ltnH"));
    assert!(c[2].contains("netstat -ltn"));
    // `|| true` keeps the exec exit 0 so a missing tool reads as "no ports".
    assert!(c[2].ends_with("|| true"));
}

#[test]
fn exec_bridge_argv_enters_netns_and_dials_loopback() {
    let argv = exec_bridge_argv(&["podman".into()], "sz-app-feat", 3000);
    assert_eq!(argv[0], "podman");
    assert_eq!(argv[1], "exec");
    assert_eq!(argv[2], "-i"); // interactive: stdio is the bridge
    assert_eq!(argv[3], "sz-app-feat");
    // The bridge dials the container's own loopback (reachable from inside the
    // netns), not a host-visible address.
    let script = argv.last().unwrap();
    assert!(script.contains("TCP:127.0.0.1:3000"));
    assert!(script.contains("socat"));
    assert!(script.contains("nc 127.0.0.1 3000"));
}

#[test]
fn exec_bridge_argv_preserves_sudo_prefix() {
    let argv = exec_bridge_argv(&["sudo".into(), "-n".into(), "podman".into()], "c", 8080);
    assert_eq!(&argv[..4], &["sudo", "-n", "podman", "exec"]);
}
