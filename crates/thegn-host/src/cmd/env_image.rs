//! `thegn env image-bake` — bake a reusable VPS base image: create a
//! throwaway instance from the stock image, run the **repo-independent**
//! provisioning prefix ([`thegn_core::envplan::bake_scripts`]: the Nix
//! install every cold provision otherwise pays, plus direnv; docker rides the
//! stock image's cloud-init), power off, snapshot, destroy the instance, and
//! print the `template = "snapshot:<id>"` line to paste into the env. This is
//! the biggest single latency win for VPS envs (~3-6 min cold → ~30-90 s),
//! standing in for the checkpoint step a VPS cannot have.
//!
//! The provision marker is **not** written — per-worktree steps (clone,
//! dotfiles, agents…) still run at provision time on top of the baked image.

use anyhow::{Context, Result, anyhow};
use thegn_core::config::Config;
use thegn_core::outln;

pub fn run(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let env = super::env::resolve_for(cfg, worktree);
    let envc = cfg
        .env
        .get(&env.name)
        .ok_or_else(|| anyhow!("env {} has no [env.{}] table", env.name, env.name))?;
    let pc = &envc.provider;
    if !thegn_core::config::vps_provider_kind(&pc.provider) {
        anyhow::bail!(
            "image bake is for VPS providers (hetzner, digitalocean); env {} uses {:?}",
            env.name,
            pc.provider
        );
    }
    // Bake FROM stock even when the env already points at a snapshot (re-bake).
    // The `snapshot:<id>` template prefix is a shared thegn convention across
    // vendors, so the Hetzner parser recognises it for any VPS kind.
    let mut bake_pc = pc.clone();
    if thegn_svc::vps::hetzner::snapshot_image(&bake_pc.template).is_some() {
        bake_pc.template = String::new();
    }
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow!("generate bake operation name: {error}"))?;
    let name = bake_name(random);
    let provider = crate::provider_factory::vps_provider_for(&bake_pc, &name)
        .ok_or_else(|| anyhow!("the {} API token is not set", pc.provider))?;
    let rt = tokio::runtime::Runtime::new()?;

    outln!("baking a {} base image (instance {name})…", pc.provider);
    use thegn_svc::provider::RemoteProvider;
    let result = rt
        .block_on(provider.create())
        .context("create bake instance")
        .and_then(|_| bake(&rt, &provider, &name, &bake_pc));
    outln!("destroying bake instance {name}…");
    let cleanup = rt.block_on(provider.destroy(&name));
    let snapshot = bake_outcome(result, cleanup, &name)?;
    outln!("baked snapshot {snapshot}. To use it, set:");
    outln!("  [env.{}.provider]", env.name);
    outln!("  template = \"snapshot:{snapshot}\"");
    outln!("(snapshots bill ~pennies/GB-month; re-run image-bake to refresh a stale one)");
    Ok(())
}

fn bake_name(bytes: [u8; 8]) -> String {
    let mut name = String::from(thegn_svc::vps::registry::BAKE_NAME_PREFIX);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}").expect("writing to String cannot fail");
    }
    name
}

fn bake_outcome(bake: Result<String>, cleanup: Result<()>, name: &str) -> Result<String> {
    match (bake, cleanup) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Ok(snapshot), Err(cleanup)) => Err(anyhow!(
            "bake produced snapshot {snapshot}, but source instance {name} may still be billing; cleanup failed: {cleanup:#}"
        )),
        (Err(bake), Err(cleanup)) => Err(anyhow!(
            "bake failed: {bake:#}; cleanup of source instance {name} failed: {cleanup:#}"
        )),
        (Err(bake), Ok(())) => Err(anyhow!(
            "bake failed: {bake:#}; source instance {name} cleanup succeeded"
        )),
    }
}

/// The bake body: run the repo-independent scripts, quiesce, snapshot.
fn bake(
    rt: &tokio::runtime::Runtime,
    provider: &thegn_svc::vps::VpsProvider,
    name: &str,
    pc: &thegn_core::config::EnvProviderConfig,
) -> Result<String> {
    for (id, script) in thegn_core::envplan::bake_scripts(pc.nix_installer, pc.nix_parallel()) {
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
    rt.block_on(provider.snapshot(name, "thegn-base"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_outcome_requires_bake_and_cleanup_success() {
        assert_eq!(
            bake_outcome(Ok("42".into()), Ok(()), "tg-bake-a").unwrap(),
            "42"
        );

        let cleanup_error = bake_outcome(
            Ok("42".into()),
            Err(anyhow!("delete unavailable")),
            "tg-bake-a",
        )
        .unwrap_err()
        .to_string();
        assert!(cleanup_error.contains("snapshot 42"));
        assert!(cleanup_error.contains("tg-bake-a"));
        assert!(cleanup_error.contains("delete unavailable"));

        let bake_error = bake_outcome(Err(anyhow!("script failed")), Ok(()), "tg-bake-a")
            .unwrap_err()
            .to_string();
        assert!(bake_error.contains("script failed"));

        let both_error = bake_outcome(
            Err(anyhow!("script failed")),
            Err(anyhow!("delete unavailable")),
            "tg-bake-a",
        )
        .unwrap_err()
        .to_string();
        assert!(both_error.contains("script failed"));
        assert!(both_error.contains("delete unavailable"));
        assert!(both_error.contains("tg-bake-a"));
    }

    #[test]
    fn bake_name_is_a_provider_safe_dns_name() {
        let name = bake_name([0, 1, 2, 3, 0xfa, 0xfb, 0xfe, 0xff]);
        assert_eq!(name, "tg-bake-00010203fafbfeff");
        assert!(name.len() <= 63);
        assert!(name.split('.').all(|label| {
            !label.is_empty()
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }));
        assert!(thegn_svc::vps::registry::is_bake_name(&name));
    }
}
