//! In-process ground-truth proof of the iroh call-home transport.
//!
//! A home endpoint (`superzej-svc::iroh_reach`) and a sandbox agent (this crate's
//! `serve`) connect over two loopback `presets::Minimal` iroh endpoints — offline,
//! no relay, no network — the home opens an exec PTY, runs a shell command, and
//! reads its output + exit code back over iroh. This is the ground truth the Fly
//! live test (`superzej-svc/tests/fly_live.rs`) mirrors end-to-end.

use std::sync::Arc;
use std::time::Duration;

use superzej_agent::serve;
use superzej_core::iroh_wire::{ALPN, Hello};
use superzej_svc::iroh_reach::{FnVerifier, IrohHome, TokenVerifier};
use superzej_svc::provider::{ExecFrame, ExecSpec};

/// Bind a local, relay-free iroh endpoint for the test.
async fn local_endpoint() -> iroh::Endpoint {
    iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .expect("bind local endpoint")
}

#[tokio::test]
async fn shell_command_runs_over_iroh_pty() {
    // Home accepts any token and maps it to the sandbox id "wt-test".
    let verifier: Arc<dyn TokenVerifier> =
        Arc::new(FnVerifier(|_h: &Hello| Some("wt-test".to_string())));
    let (home, mut registered) = IrohHome::serve(local_endpoint().await, verifier);
    let home_addr = home.addr();

    // Agent dials the home's full addr (direct loopback — no discovery needed).
    let agent_ep = local_endpoint().await;
    let agent_task = tokio::spawn(async move {
        // Holds agent_ep alive for the connection's lifetime.
        serve::dial_and_serve(
            &agent_ep,
            home_addr,
            Hello {
                token: "test-token".into(),
                sandbox: "wt-test".into(),
            },
        )
        .await
    });

    // The agent should register within a few seconds.
    let sandbox = tokio::time::timeout(Duration::from_secs(20), registered.recv())
        .await
        .expect("registration timed out")
        .expect("registered channel closed");
    assert_eq!(sandbox, "wt-test");
    assert!(home.is_connected("wt-test"));

    // Open a PTY that prints a marker and exits non-zero.
    let spec = ExecSpec {
        argv: vec![
            "/bin/sh".into(),
            "-lc".into(),
            "echo SZ_IROH_PTY_OK; exit 7".into(),
        ],
        tty: true,
        cols: 80,
        rows: 24,
        env: vec![],
        cwd: None,
    };
    let mut session = home
        .open_exec("wt-test", spec)
        .await
        .expect("open exec over iroh");

    let mut output = Vec::new();
    let mut exit = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), session.frames.recv()).await {
            Ok(Some(ExecFrame::Stdout(b))) => output.extend(b),
            Ok(Some(ExecFrame::Exit(c))) => {
                exit = Some(c);
                break;
            }
            Ok(None) => break,
            Err(_) => panic!("timed out waiting for exec output"),
        }
    }

    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("SZ_IROH_PTY_OK"),
        "marker missing in: {text:?}"
    );
    assert_eq!(exit, Some(7), "expected exit code 7, got {exit:?}");

    home.forget("wt-test");
    agent_task.abort();
}

#[tokio::test]
async fn unauthorized_token_is_rejected() {
    // Home rejects every token.
    let verifier: Arc<dyn TokenVerifier> = Arc::new(FnVerifier(|_h: &Hello| None));
    let (home, mut registered) = IrohHome::serve(local_endpoint().await, verifier);
    let home_addr = home.addr();

    let agent_ep = local_endpoint().await;
    let _agent = tokio::spawn(async move {
        let _ = serve::dial_and_serve(
            &agent_ep,
            home_addr,
            Hello {
                token: "bad".into(),
                sandbox: "wt-x".into(),
            },
        )
        .await;
    });

    // No registration should arrive.
    let res = tokio::time::timeout(Duration::from_secs(3), registered.recv()).await;
    assert!(res.is_err(), "unauthorized sandbox must not register");
    assert!(!home.is_connected("wt-x"));
}
