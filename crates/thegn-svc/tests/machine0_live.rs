//! Live end-to-end test of the machine0 provider against the real MCP endpoint.
//!
//! Ignored by default (creates paid infra + needs network); run explicitly:
//!   MACHINE0_API_KEY=... cargo test -p thegn-svc --test machine0_live -- --ignored --nocapture
//!
//! Drives the ACTUAL provider code path (MCP control plane + ssh data plane):
//! import key → create VM → resolve dynamic size → ssh run_exec → file
//! round-trip → destroy. If `MACHINE0_API_KEY` is unset the test no-ops.

use std::path::PathBuf;

use thegn_svc::machine0::{Machine0Provider, Machine0Spec, SizeReq};
use thegn_svc::provider::{ProviderFiles, RemoteProvider};

fn gen_keypair(dir: &std::path::Path) -> (PathBuf, String) {
    let key = dir.join("id_ed25519");
    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "thegn-machine0-live",
            "-f",
        ])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
    let pubkey = std::fs::read_to_string(dir.join("id_ed25519.pub")).unwrap();
    (key, pubkey.trim().to_string())
}

#[tokio::test]
#[ignore = "creates paid machine0 infra; run with MACHINE0_API_KEY + --ignored"]
async fn machine0_live_lifecycle() {
    let Ok(api_key) = std::env::var("MACHINE0_API_KEY") else {
        eprintln!("MACHINE0_API_KEY unset — skipping live test");
        return;
    };
    let dir = std::env::temp_dir().join(format!("tg-m0-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (key_path, pubkey) = gen_keypair(&dir);
    let name = format!("thegn-live-{}", std::process::id());

    // Default NixOS image (empty template ⇒ DEFAULT_IMAGE = nixos-25-11-loaded) +
    // a DYNAMIC size (cheapest ≥1 vCPU) — exercises the default substrate + a
    // devenv-capable (nix) shell.
    let provider = Machine0Provider::new(Machine0Spec {
        endpoint: String::new(),
        api_key,
        name: name.clone(),
        image: String::new(), // ⇒ DEFAULT_IMAGE (nixos-25-11-loaded)
        size: "auto".into(),
        size_req: SizeReq {
            min_vcpu: 1,
            ..Default::default()
        },
        region: String::new(),   // ⇒ DEFAULT_REGION
        provision_flake: String::new(),
        ssh_user: String::new(), // ⇒ VM's defaultSSHUsername
        key_path,
        pubkey,
        max_instances: 5,
        max_lifetime_secs: 0,
        skip_ready_wait: false,
    });

    // Ensure teardown even on failure.
    let result = run_lifecycle(&provider, &name).await;
    let _ = provider.destroy(&name).await;
    result.expect("live lifecycle");
}

async fn run_lifecycle(provider: &Machine0Provider, name: &str) -> anyhow::Result<()> {
    use anyhow::Context;

    eprintln!("[live] create {name} …");
    let handle = provider.create().await.context("create")?;
    eprintln!("[live] handle exec = {:?}", handle.exec);

    eprintln!("[live] run_exec whoami/uname …");
    let (code, out) = provider
        .run_exec(
            name,
            &[
                "/bin/sh".into(),
                "-lc".into(),
                "echo READY=$(whoami)@$(uname -s)".into(),
            ],
            None,
            &[],
        )
        .await
        .context("run_exec")?;
    eprintln!("[live] exit={code} out={out:?}");
    anyhow::ensure!(code == 0, "run_exec exit {code}: {out}");
    anyhow::ensure!(out.contains("READY="), "unexpected exec output: {out}");

    eprintln!("[live] file round-trip …");
    let payload = b"thegn-machine0-roundtrip\n";
    provider
        .write(name, "/tmp/tg-live.txt", payload)
        .await
        .context("write")?;
    let got = provider.read(name, "/tmp/tg-live.txt").await.context("read")?;
    anyhow::ensure!(got == payload, "file round-trip mismatch: {got:?}");

    eprintln!("[live] list contains {name} …");
    let vms = provider.list().await.context("list")?;
    anyhow::ensure!(vms.iter().any(|v| v == name), "list missing {name}: {vms:?}");

    // Devenv substrate: the NixOS image must give us `nix` + a writable-enough
    // store to build a devShell. Hard-assert nix + /nix/store; then attempt a real
    // `nix develop` (bounded, best-effort — proves a devenv-enabled shell but a
    // slow nixpkgs fetch shouldn't fail the transport test).
    eprintln!("[live] devenv: nix + /nix/store …");
    let (code, out) = provider
        .run_exec(
            name,
            &sh("command -v nix >/dev/null && nix --version && ls -d /nix/store && echo NIX_OK"),
            None,
            &[],
        )
        .await
        .context("nix probe")?;
    eprintln!("[live] nix: exit={code} out={out:?}");
    anyhow::ensure!(code == 0 && out.contains("NIX_OK"), "no nix devenv substrate: {out}");

    eprintln!("[live] devenv: nix develop a tiny flake (best-effort, ≤240s) …");
    let flake = r#"{ inputs.nixpkgs.url = "flake:nixpkgs"; outputs = { self, nixpkgs }: let p = nixpkgs.legacyPackages.x86_64-linux; in { devShells.x86_64-linux.default = p.mkShell { packages = [ p.hello ]; }; }; }"#;
    provider
        .write(name, "/tmp/tgtest/flake.nix", flake.as_bytes())
        .await
        .context("write flake")?;
    let devshell = tokio::time::timeout(
        std::time::Duration::from_secs(240),
        provider.run_exec(
            name,
            &sh("cd /tmp/tgtest && nix --extra-experimental-features 'nix-command flakes' develop --command sh -c 'command -v hello'"),
            None,
            &[],
        ),
    )
    .await;
    match devshell {
        Ok(Ok((0, path))) if path.contains("/nix/store") => {
            eprintln!("[live] devShell OK: hello -> {}", path.trim())
        }
        other => eprintln!("[live] devShell (best-effort) did not confirm: {other:?}"),
    }

    // mosh-server presence (drives the bridge's mosh↔ssh choice — informational).
    let (_c, mout) = provider
        .run_exec(name, &sh("command -v mosh-server || echo NO_MOSH_SERVER"), None, &[])
        .await
        .context("mosh probe")?;
    eprintln!("[live] mosh-server: {}", mout.trim());

    // Suspend (explicit park) → the VM must leave RUNNING (peek errors) → resume
    // (vm_start + wait reachable) → exec works again.
    eprintln!("[live] suspend …");
    provider.suspend(name).await.context("suspend")?;
    let mut parked = false;
    for _ in 0..15 {
        if provider.peek_endpoint(name).await.is_err() {
            parked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    }
    anyhow::ensure!(parked, "vm {name} did not leave RUNNING after suspend");
    eprintln!("[live] resume …");
    provider.resume(name).await.context("resume")?;
    let (code, out) = provider
        .run_exec(name, &sh("echo RESUMED=$(whoami)"), None, &[])
        .await
        .context("run_exec after resume")?;
    anyhow::ensure!(code == 0 && out.contains("RESUMED="), "exec after resume failed: {out}");

    eprintln!("[live] OK");
    Ok(())
}

/// A `/bin/sh -lc <script>` argv.
fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-lc".into(), script.into()]
}
