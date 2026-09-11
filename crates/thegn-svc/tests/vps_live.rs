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
    let name = format!("tg-live-{}-{}", kind.as_str(), std::process::id());
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
            let rec = registry::read(&name)
                .ok_or_else(|| anyhow::anyhow!("ledger record missing after create"))?;
            anyhow::ensure!(rec.state == "ready", "ledger was not finalized: {rec:?}");
            anyhow::ensure!(!rec.instance_id.is_empty(), "vendor id was not recorded");
            anyhow::ensure!(!rec.ip.is_empty(), "public IP was not recorded");
            eprintln!("[{name}] ip={} id={}", rec.ip, rec.instance_id);

            // list() is server-side scoped to thegn-managed instances.
            let names = prov.list().await?;
            anyhow::ensure!(
                names.contains(&name),
                "provider list does not contain {name}: {names:?}"
            );

            // The real proof: run a command over the ssh shim (our injected key).
            let (code, out) = prov
                .run_exec(&name, &["echo".into(), "tg-live-ok".into()], None, &[])
                .await?;
            anyhow::ensure!(code == 0, "remote echo exited {code}; out={out}");
            anyhow::ensure!(out.contains("tg-live-ok"), "remote echo output: {out}");
            eprintln!("[{name}] exec over ssh OK");
            anyhow::Ok(())
        }
        .await;

        eprintln!("[{name}] destroying…");
        // Collect every teardown observation before asserting. In particular,
        // still make the idempotent second destroy attempt if the first call or
        // its verification fails; this test creates a billable public VM and a
        // failed assertion must not be what leaves it behind.
        let first_destroy = prov.destroy(&name).await;
        let names_after_first = prov.list().await;
        let second_destroy = prov.destroy(&name).await;
        let names_after_second = prov.list().await;
        let ledger_after = registry::read(&name);

        first_destroy.expect("destroy");
        second_destroy.expect("idempotent destroy");
        assert!(ledger_after.is_none(), "ledger cleared on destroy");
        let names_after_first = names_after_first.expect("list after destroy");
        assert!(
            !names_after_first.contains(&name),
            "instance remains after destroy: {names_after_first:?}"
        );
        let names_after_second = names_after_second.expect("list after idempotent destroy");
        assert!(
            !names_after_second.contains(&name),
            "instance returned after idempotent destroy: {names_after_second:?}"
        );

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
