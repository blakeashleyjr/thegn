//! Live end-to-end test against the **real** Hetzner / DigitalOcean APIs.
//! `#[ignore]` so it never runs in CI: it needs a vendor token + network and it
//! creates + destroys a throwaway instance (real cloud spend, a few pennies).
//!
//!   HCLOUD_TOKEN=…        cargo test -p thegn-svc --test vps_live -- --ignored --nocapture live_hetzner
//!   DIGITALOCEAN_TOKEN=…  cargo test -p thegn-svc --test vps_live -- --ignored --nocapture live_digitalocean
//!
//! This is the ground-truth check the replay mocks (`vps_mock`, `vps_do_mock`)
//! are modeled on. It drives the FULL path the worktree-open lifecycle uses:
//! create → the box boots with our injected key → sshd reachable → a command
//! runs over the ssh shim → list shows it (tag/label-scoped) → destroy → list
//! shows it gone → the ledger under $XDG_STATE/thegn/vps is clean.
#![allow(clippy::disallowed_macros)]

use std::path::Path;
use std::process::Command;

use thegn_svc::provider::RemoteProvider;
use thegn_svc::vps::{VpsKind, VpsProvider, VpsSpec, registry};

/// Generate an ephemeral ed25519 keypair in `dir`; return `(key_path, pubkey)`.
fn ephemeral_key(dir: &Path) -> (std::path::PathBuf, String) {
    let key = dir.join("id_ed25519");
    let out = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "thegn-live-test", "-f"])
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

fn run_lifecycle(kind: VpsKind, token_env: &str) {
    let Ok(token) = std::env::var(token_env) else {
        eprintln!("{token_env} unset — skipping {kind:?} live test");
        return;
    };
    // Isolate the registry so the test never touches a real thegn state dir.
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("THEGN_DIR", tmp.path()) };

    let (key_path, pubkey) = ephemeral_key(tmp.path());
    let name = format!("sz-live-{}-{}", kind.as_str(), std::process::id());
    let spec = VpsSpec {
        kind,
        api_base: String::new(),
        token,
        name: name.clone(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 3,
        max_lifetime_secs: 0,
        key_path,
        pubkey,
        skip_ready_wait: false,
    };
    let prov = VpsProvider::new(spec);
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        // From here on the instance MUST be destroyed — success or failure.
        eprintln!("[{name}] creating (this boots a real VM + waits for sshd)…");
        let created = prov.create().await;
        // Always attempt teardown, even if a later assertion fails.
        let result = async {
            let handle = created?;
            eprintln!("[{name}] created: {:?}", handle.exec);

            // The ledger is finalized to `ready` with the vendor id + IP.
            let rec = registry::read(&name).expect("ledger record after create");
            assert_eq!(rec.state, "ready", "ledger finalized");
            assert!(!rec.instance_id.is_empty(), "vendor id recorded");
            assert!(!rec.ip.is_empty(), "public IP recorded");
            eprintln!("[{name}] ip={} id={}", rec.ip, rec.instance_id);

            // list() is server-side scoped to thegn-managed instances.
            let names = prov.list().await?;
            assert!(names.contains(&name), "list shows our instance: {names:?}");

            // The real proof: run a command over the ssh shim (our injected key).
            let (code, out) = prov
                .run_exec(&name, &["echo".into(), "sz-live-ok".into()], None, &[])
                .await?;
            assert_eq!(code, 0, "remote echo exit 0; out={out}");
            assert!(out.contains("sz-live-ok"), "remote echo output: {out}");
            eprintln!("[{name}] exec over ssh OK");
            anyhow::Ok(())
        }
        .await;

        eprintln!("[{name}] destroying…");
        prov.destroy(&name).await.expect("destroy");
        // Ledger cleared and the instance is gone from the vendor list.
        assert!(registry::read(&name).is_none(), "ledger cleared on destroy");
        let names = prov.list().await.unwrap_or_default();
        assert!(!names.contains(&name), "instance gone from list: {names:?}");
        // A second destroy is idempotent.
        prov.destroy(&name).await.expect("idempotent destroy");

        result.expect("lifecycle assertions");
        eprintln!("[{name}] ✓ full lifecycle verified");
    });
}

#[test]
#[ignore = "live: needs HCLOUD_TOKEN, network, creates a real VPS"]
fn live_hetzner_lifecycle() {
    run_lifecycle(VpsKind::Hetzner, "HCLOUD_TOKEN");
}

#[test]
#[ignore = "live: needs DIGITALOCEAN_TOKEN, network, creates a real droplet"]
fn live_digitalocean_lifecycle() {
    run_lifecycle(VpsKind::DigitalOcean, "DIGITALOCEAN_TOKEN");
}
