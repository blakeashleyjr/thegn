//! End-to-end: spawn the real `szhost bridge` resident-agent subcommand as a
//! subprocess and drive it over stdio with `BridgeClient` — proving the hidden
//! `Command::Bridge` arm + the spawn transport + the framing wire up correctly
//! (the same path used over ssh / `sprite exec`, here against the local binary).

use std::process::Command;
use superzej_svc::bridge::BridgeClient;

#[test]
fn bridge_subcommand_serves_exec_over_stdio() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_szhost"));
    cmd.arg("bridge");
    let client = BridgeClient::spawn(cmd).expect("spawn `szhost bridge`");

    // A plain command round-trips (stdout of the agent is the protocol channel —
    // a clean response here proves nothing leaked onto it).
    let r = client
        .exec(&["echo", "via-real-szhost"], None, &[])
        .expect("exec echo");
    assert_eq!(r.exit, 0);
    assert_eq!(r.stdout.trim(), "via-real-szhost");

    // Sequential calls reuse the one persistent connection.
    let r2 = client
        .exec(&["sh", "-c", "exit 7"], None, &[])
        .expect("exec");
    assert_eq!(r2.exit, 7);
}
