use super::*;
use anyhow::anyhow;

fn runtime(prefix: &[&str]) -> OciRuntime {
    OciRuntime::new(prefix.iter().map(|part| (*part).to_string()).collect())
}

fn ports(values: &[u16]) -> BTreeSet<u16> {
    values.iter().copied().collect()
}

#[test]
fn pick_runtime_uses_the_only_runtime_that_reached() {
    let result = pick_runtime(
        "thegn-app",
        vec![
            (runtime(&["podman"]), Reach::Reached(ports(&[3000]))),
            (runtime(&["docker"]), Reach::Absent),
            (runtime(&["sudo", "-n", "podman"]), Reach::Absent),
        ],
    )
    .unwrap();

    assert_eq!(result, (vec!["podman".to_string()], ports(&[3000])));
}

#[test]
fn pick_runtime_refuses_a_name_reached_in_two_runtimes() {
    let result = pick_runtime(
        "thegn-app",
        vec![
            (runtime(&["podman"]), Reach::Reached(ports(&[3000]))),
            (runtime(&["docker"]), Reach::Reached(ports(&[8080]))),
        ],
    );

    let Err(error) = result else {
        panic!("same-named containers in two runtimes must be refused");
    };
    let refusal = error
        .downcast_ref::<AmbiguousRuntime>()
        .expect("ambiguous reach must retain its typed refusal");
    assert_eq!(
        refusal.runtimes,
        vec!["podman".to_string(), "docker".to_string()]
    );
    let message = error.to_string();
    assert!(message.contains("thegn-app"));
    assert!(message.contains("podman"));
    assert!(message.contains("docker"));
}

#[test]
fn pick_runtime_is_blocked_by_an_unknown_probe() {
    let result = pick_runtime(
        "thegn-app",
        vec![
            (runtime(&["podman"]), Reach::Reached(ports(&[3000]))),
            (
                runtime(&["docker"]),
                Reach::Unknown("vpn: subprocess timed out after 10s".into()),
            ),
        ],
    );

    let Err(error) = result else {
        panic!("unknown reachability must block selecting a runtime");
    };
    assert!(error.downcast_ref::<AmbiguousRuntime>().is_none());
    assert!(error.to_string().contains("docker"));
    assert!(error.to_string().contains("timed out"));
}

#[test]
fn pick_runtime_without_any_reach_is_an_error() {
    let result = pick_runtime(
        "thegn-app",
        vec![
            (runtime(&["podman"]), Reach::Absent),
            (runtime(&["docker"]), Reach::Absent),
        ],
    );

    let Err(error) = result else {
        panic!("no runtime reach must be an error");
    };
    assert!(error.to_string().contains("no OCI runtime could reach"));
}

#[test]
fn classify_probe_treats_a_missing_runtime_binary_as_absent() {
    let missing = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
        .context("vpn: spawn docker");
    assert_eq!(classify_probe(Err(missing)), Reach::Absent);
    assert_eq!(classify_probe(Ok((false, String::new()))), Reach::Absent);
    assert_eq!(
        classify_probe(Ok((true, "LISTEN 0 4096 0.0.0.0:3000 0.0.0.0:*\n".into()))),
        Reach::Reached(ports(&[3000]))
    );
}

#[test]
fn classify_probe_keeps_a_timeout_unknown() {
    let result = classify_probe(Err(anyhow!("vpn: subprocess timed out after 10s")));

    assert!(matches!(result, Reach::Unknown(reason) if reason.contains("timed out")));
}

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
    let argv = exec_bridge_argv(&["podman".into()], "tg-app-feat", 3000);
    assert_eq!(argv[0], "podman");
    assert_eq!(argv[1], "exec");
    assert_eq!(argv[2], "-i"); // interactive: stdio is the bridge
    assert_eq!(argv[3], "tg-app-feat");
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
