//! Live end-to-end test against the **real** Fly.io API. `#[ignore]` so it never
//! runs in CI: it needs `FLY_API_TOKEN` + network, allocates a dedicated IPv4,
//! and creates + destroys a throwaway app+machine (real cloud spend, pennies +
//! prorated IPv4). Run:
//!
//!   FLY_API_TOKEN=… cargo test -p thegn-svc --test fly_live -- --ignored --nocapture
//!
//! This is the ground truth `fly_mock` is modeled on, and it proves the user-
//! facing goal: a **live shell** into a Fly machine over ssh (dedicated IPv4 +
//! guest sshd) and **docker running a container** inside it (Fly machines are
//! real Firecracker VMs; SSHD_INIT preconfigures the vfs storage driver).
#![allow(clippy::disallowed_macros)]

use std::path::Path;
use std::process::Command;

use thegn_svc::fly::{FlyProvider, FlySpec};
use thegn_svc::provider::RemoteProvider;
use thegn_svc::vps::registry;

fn ephemeral_key(dir: &Path) -> (std::path::PathBuf, String) {
    let key = dir.join("id_ed25519");
    let out = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "sz-fly-live", "-f"])
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

#[test]
#[ignore = "live: needs FLY_API_TOKEN, network; creates a real Fly app+machine+IPv4"]
fn live_shell_and_docker() {
    let Ok(token) = std::env::var("FLY_API_TOKEN") else {
        eprintln!("FLY_API_TOKEN unset — skipping Fly live test");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("THEGN_DIR", tmp.path()) };
    let (key_path, pubkey) = ephemeral_key(tmp.path());
    let name = format!("sz-live-{}", std::process::id());
    let spec = FlySpec {
        api_base: String::new(),
        graphql_url: String::new(),
        token,
        org_slug: std::env::var("FLY_ORG").unwrap_or_default(),
        name: name.clone(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 3,
        max_lifetime_secs: 0,
        key_path,
        pubkey,
        iroh: None,
        skip_ready_wait: false,
    };
    let prov = FlyProvider::new(spec);
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        eprintln!("[{name}] creating app + IPv4 + sshd machine (installs sshd, ~40s)…");
        let created = prov.create().await;
        // Use `ensure!`/`?` (not `assert!`) so ANY failure returns Err and the
        // destroy below still runs — a panic here would leak a billing app.
        let result = async {
            let handle = created?;
            eprintln!("[{name}] created: {:?}", handle.exec);
            let rec = registry::read(&name).ok_or_else(|| anyhow::anyhow!("no ledger record"))?;
            anyhow::ensure!(rec.state == "ready" && rec.provider == "fly", "ledger not finalized");
            anyhow::ensure!(!rec.ip.is_empty(), "dedicated IPv4 recorded");
            eprintln!("[{name}] ip={}", rec.ip);

            anyhow::ensure!(prov.list().await? == vec![name.clone()], "ledger list");

            // 1) LIVE SHELL over ssh.
            let (code, out) = prov
                .run_exec(&name, &["echo".into(), "TG_FLY_SHELL_OK".into()], None, &[])
                .await?;
            anyhow::ensure!(code == 0 && out.contains("TG_FLY_SHELL_OK"), "shell echo (exit {code}): {out}");
            eprintln!("[{name}] ✓ live shell over ssh");

            // 2) DOCKER runs a container (Fly Firecracker VM + vfs driver).
            eprintln!("[{name}] installing docker + running a container (~1-2 min)…");
            let dscript = "set -e; export DEBIAN_FRONTEND=noninteractive; \
                apt-get install -y -qq curl ca-certificates >/dev/null 2>&1; \
                curl -fsSL https://get.docker.com | sh >/tmp/d.log 2>&1; \
                (dockerd >/tmp/dockerd.log 2>&1 &); sleep 10; \
                docker run --rm hello-world 2>&1 | grep -qi 'hello from docker' && echo TG_DOCKER_OK || (echo DOCKER_FAIL; tail -8 /tmp/dockerd.log)";
            let (dc, dout) = prov
                .run_exec(&name, &["/bin/sh".into(), "-c".into(), dscript.into()], None, &[])
                .await?;
            anyhow::ensure!(dout.contains("TG_DOCKER_OK"), "docker run (exit {dc}):\n{dout}");
            eprintln!("[{name}] ✓ docker runs a container");

            // 3) scale-to-zero: stop then start.
            eprintln!("[{name}] stop → start (scale-to-zero)…");
            prov.stop(&name).await?;
            prov.start(&name).await?;
            eprintln!("[{name}] ✓ stop/start");
            anyhow::Ok(())
        }
        .await;

        eprintln!("[{name}] destroying (deletes app, releases IPv4)…");
        prov.destroy(&name).await.expect("destroy");
        assert!(registry::read(&name).is_none(), "ledger cleared");
        prov.destroy(&name).await.expect("idempotent destroy");

        result.expect("lifecycle assertions");
        eprintln!("[{name}] ✓ live shell + docker + scale-to-zero verified");
    });
}
