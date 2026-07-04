//! `superzej env image-bake` — bake a reusable VPS base image: create a
//! throwaway instance from the stock image, run the **repo-independent**
//! provisioning prefix ([`superzej_core::envplan::bake_scripts`]: the Nix
//! install every cold provision otherwise pays, plus direnv; docker rides the
//! stock image's cloud-init), power off, snapshot, destroy the instance, and
//! print the `template = "snapshot:<id>"` line to paste into the env. This is
//! the biggest single latency win for VPS envs (~3-6 min cold → ~30-90 s),
//! standing in for the checkpoint step a VPS cannot have.
//!
//! The provision marker is **not** written — per-worktree steps (clone,
//! dotfiles, agents…) still run at provision time on top of the baked image.

use anyhow::{Context, Result, anyhow};
use superzej_core::config::Config;
use superzej_core::{msg, outln};

pub fn run(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let env = super::env::resolve_for(cfg, worktree);
    let envc = cfg
        .env
        .get(&env.name)
        .ok_or_else(|| anyhow!("env {} has no [env.{}] table", env.name, env.name))?;
    let pc = &envc.provider;
    if !superzej_core::config::vps_provider_kind(&pc.provider) {
        anyhow::bail!(
            "image bake is for VPS providers (hetzner); env {} uses {:?}",
            env.name,
            pc.provider
        );
    }
    // Bake FROM stock even when the env already points at a snapshot (re-bake).
    let mut bake_pc = pc.clone();
    if superzej_svc::vps::hetzner::snapshot_image(&bake_pc.template).is_some() {
        bake_pc.template = String::new();
    }
    let name = format!(
        "sz-bake-{}",
        superzej_core::util::short_hash(
            &format!("{}-{}", std::process::id(), superzej_core::util::now()),
            6
        )
    );
    let provider = crate::provider_factory::vps_provider_for(&bake_pc, &name)
        .ok_or_else(|| anyhow!("the {} API token is not set", pc.provider))?;
    let rt = tokio::runtime::Runtime::new()?;

    outln!("baking a {} base image (instance {name})…", pc.provider);
    use superzej_svc::provider::RemoteProvider;
    rt.block_on(provider.create())
        .context("create bake instance")?;
    // From here on the instance MUST be destroyed — success or failure.
    let result = bake(&rt, &provider, &name, &bake_pc);
    outln!("destroying bake instance {name}…");
    if let Err(e) = rt.block_on(provider.destroy(&name)) {
        msg::warn(&format!(
            "could not destroy bake instance {name}: {e}; destroy it manually — it bills until then"
        ));
    }
    let snapshot = result?;
    outln!("baked snapshot {snapshot}. To use it, set:");
    outln!("  [env.{}.provider]", env.name);
    outln!("  template = \"snapshot:{snapshot}\"");
    outln!("(snapshots bill ~pennies/GB-month; re-run image-bake to refresh a stale one)");
    Ok(())
}

/// The bake body: run the repo-independent scripts, quiesce, snapshot.
fn bake(
    rt: &tokio::runtime::Runtime,
    provider: &superzej_svc::vps::VpsProvider,
    name: &str,
    pc: &superzej_core::config::EnvProviderConfig,
) -> Result<String> {
    for (id, script) in superzej_core::envplan::bake_scripts(pc.nix_installer, pc.nix_parallel()) {
        outln!("  [{id}] running…");
        // The same non-login PATH prelude the provision pipeline uses, so a
        // later step sees what an earlier one installed.
        let argv = vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            format!(
                "[ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && \
                 . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh; \
                 export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; {script} 2>&1"
            ),
        ];
        let timeout = crate::agent::provision_step_timeout(id);
        let (code, out) = rt
            .block_on(async {
                tokio::time::timeout(timeout, provider.run_exec(name, &argv, None, &[]))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("timed out after {}s", timeout.as_secs())))
            })
            .with_context(|| format!("bake step {id}"))?;
        if code != 0 {
            return Err(anyhow!(
                "bake step {id} failed (exit {code}):\n{}",
                out.trim()
            ));
        }
    }
    outln!("  powering off for a clean snapshot…");
    rt.block_on(provider.poweroff(name))?;
    outln!("  snapshotting (this can take a few minutes)…");
    rt.block_on(provider.snapshot(name, "superzej-base"))
}
