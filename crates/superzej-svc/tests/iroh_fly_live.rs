//! Live end-to-end proof of the **iroh call-home reach** against real Fly.io.
//! `#[ignore]` — needs a token + network + a baked image containing `sz-agent`.
//!
//! Unlike `fly_live.rs` (which ssh's into a dedicated IPv4), this proves the new
//! model: a Fly machine boots the baked `sz-agent`, which **dials home** to a
//! compositor iroh endpoint we stand up in-process, authenticates with a minted
//! token, and serves a **PTY shell over iroh** — no inbound, no public IP needed.
//!
//! Run (org must own the baked image; image must contain `sz-agent` on PATH and an
//! entrypoint that launches it — see `nix/fly-sandbox-image.nix`):
//!
//!   FLY_API_TOKEN=…  FLY_ORG=tech-surfer-co  FLY_IMAGE=image:registry.fly.io/<app>:<tag> \
//!     cargo test -p superzej-svc --test iroh_fly_live -- --ignored --nocapture

#![allow(clippy::disallowed_macros)]

use std::sync::Arc;
use std::time::Duration;

use superzej_core::iroh_wire::Hello;
use superzej_svc::fly::{FlyProvider, FlySpec, IrohInject};
use superzej_svc::iroh_reach::{FnVerifier, IrohHome, TokenVerifier};
use superzej_svc::provider::{ExecFrame, ExecSpec, RemoteProvider};
use superzej_svc::vps::registry;

fn ephemeral_key(dir: &std::path::Path) -> (std::path::PathBuf, String) {
    let key = dir.join("id_ed25519");
    let out = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "sz-iroh-live", "-f"])
        .arg(&key)
        .output()
        .expect("ssh-keygen");
    assert!(out.status.success(), "ssh-keygen failed: {out:?}");
    let pubkey = std::fs::read_to_string(key.with_extension("pub"))
        .expect("read pubkey")
        .trim()
        .to_string();
    (key, pubkey)
}

#[tokio::test]
#[ignore = "live: needs FLY_API_TOKEN + FLY_IMAGE (baked sz-agent) + network; creates a real Fly machine"]
async fn machine_dials_home_and_serves_shell_over_iroh() {
    let Ok(token) = std::env::var("FLY_API_TOKEN") else {
        eprintln!("FLY_API_TOKEN unset — skipping");
        return;
    };
    let image = std::env::var("FLY_IMAGE").unwrap_or_default();
    assert!(
        image.starts_with("image:"),
        "FLY_IMAGE must be a baked image ref (image:<registry>/<app>:<tag>) containing sz-agent"
    );

    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("SUPERZEJ_DIR", tmp.path()) };
    let (key_path, pubkey) = ephemeral_key(tmp.path());
    let name = format!("sz-iroh-{}", std::process::id());
    let auth_token = format!("sztok-{}-{}", std::process::id(), name);

    // 1) Stand up the compositor's home endpoint (real N0 relays so the machine
    //    can reach it), with a verifier that accepts our minted token.
    let want = auth_token.clone();
    let want_sandbox = name.clone();
    let verifier: Arc<dyn TokenVerifier> = Arc::new(FnVerifier(move |h: &Hello| {
        (h.token == want).then(|| want_sandbox.clone())
    }));
    let (home, mut registered) = IrohHome::bind(None, verifier).await.expect("bind home");
    let home_node = home.endpoint_id().to_string();
    eprintln!("[{name}] home node = {home_node}");

    // 2) Provision a Fly machine that boots sz-agent with the injected env.
    let spec = FlySpec {
        api_base: String::new(),
        graphql_url: String::new(),
        token,
        org_slug: std::env::var("FLY_ORG").unwrap_or_default(),
        name: name.clone(),
        region: String::new(),
        size: String::new(),
        image,
        max_instances: 3,
        max_lifetime_secs: 0,
        key_path,
        pubkey,
        iroh: Some(IrohInject {
            home_node,
            sandbox_auth: auth_token,
            sandbox_id: name.clone(),
        }),
        // We gate on iroh registration, not sshd reachability.
        skip_ready_wait: true,
    };
    let prov = FlyProvider::new(spec);

    eprintln!("[{name}] creating machine (boots sz-agent → dials home)…");
    let created = prov.create().await;

    let result = async {
        created?;
        anyhow::ensure!(registry::read(&name).is_some(), "ledger record");

        // 3) The machine's agent should register within the boot budget.
        eprintln!("[{name}] waiting for the agent to dial home…");
        let sandbox = tokio::time::timeout(Duration::from_secs(120), registered.recv())
            .await
            .map_err(|_| anyhow::anyhow!("agent never dialed home"))?
            .ok_or_else(|| anyhow::anyhow!("home channel closed"))?;
        anyhow::ensure!(sandbox == name, "unexpected sandbox {sandbox}");
        eprintln!("[{name}] ✓ agent registered over iroh");

        // 4) Open a PTY shell over iroh and read a marker back.
        let mut session = home
            .open_exec(
                &name,
                ExecSpec {
                    argv: vec![
                        "/bin/sh".into(),
                        "-lc".into(),
                        "echo SZ_IROH_REMOTE_OK; exit 0".into(),
                    ],
                    tty: true,
                    cols: 80,
                    rows: 24,
                    env: vec![],
                    cwd: None,
                },
            )
            .await?;
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(30), session.frames.recv()).await {
                Ok(Some(ExecFrame::Stdout(b))) => out.extend(b),
                Ok(Some(ExecFrame::Exit(_))) | Ok(None) => break,
                Err(_) => anyhow::bail!("timed out reading shell output"),
            }
        }
        let text = String::from_utf8_lossy(&out);
        anyhow::ensure!(
            text.contains("SZ_IROH_REMOTE_OK"),
            "missing marker in: {text:?}"
        );
        eprintln!("[{name}] ✓ live shell over iroh");
        anyhow::Ok(())
    }
    .await;

    eprintln!("[{name}] destroying…");
    prov.destroy(&name).await.expect("destroy");
    assert!(registry::read(&name).is_none(), "ledger cleared");

    result.expect("iroh call-home lifecycle");
    eprintln!("[{name}] ✓ provision → auto-dial-home → shell over iroh verified");
}
