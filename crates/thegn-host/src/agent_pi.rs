//! Managed-pi ("Agent") provisioning inside a sandbox — the `StepKind::ManagedPi`
//! step's work, split out of `agent.rs` (a pinned god-file) per the ratchet rule.
//!
//! Every operation here is bounded: the sprite `managed_pi` step, unlike the
//! `StepKind::Exec` steps, is not wrapped by the provision loop, so an unbounded
//! host subprocess (`agent setup`), agent-dir upload, or in-sprite `npm install`
//! would otherwise freeze the loading screen forever (the freeze this module
//! fixes). The step is best-effort — a timeout warns-and-continues (the sandbox
//! still comes up; the host "Agent" entry is unaffected).

use crate::agent::{block_on_provider, provision_step_timeout};

/// Provision the MANAGED pi inside the sandbox: carry the host's seeded agent dir
/// (`~/.thegn/pi/agent` → `<sprite_home>/.thegn/pi/agent`) and npm-install the
/// pinned binary there, so the "Agent" picker entry's `$HOME/.thegn/pi` snippet
/// resolves in-sprite exactly as on the host. Best-effort.
pub(crate) fn provision_managed_pi(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    sprite_home: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    // Make sure the host's managed agent dir is seeded — it's the bytes we carry.
    // (Host `agent setup` is bounded by `run_setup_cmd`'s own subprocess timeout.)
    if let Err(e) = crate::cmd::agent::setup(false) {
        thegn_core::msg::warn(&format!("managed pi: host setup before carry failed: {e}"));
    }
    let host_agent = thegn_core::util::managed_pi_agent_dir();
    anyhow::ensure!(
        host_agent.is_dir(),
        "host managed pi agent dir missing ({}); run `thegn agent setup`",
        host_agent.display()
    );

    // 1. Carry the agent dir (thegn-acp package + settings) into the sandbox.
    //    Bound the upload: `provider_http_client` only caps the *connect*, so a
    //    half-open/slow transfer to a stalled control plane would hang the loading
    //    screen forever. The dir is tiny (a package + settings.json), so a short
    //    budget is safe (mirrors AGENT_CONFIG_STEP_BUDGET in agent_configs.rs).
    const MANAGED_PI_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let dest = format!("{}/.thegn/pi/agent", sprite_home.trim_end_matches('/'));
    block_on_provider(|| async {
        match tokio::time::timeout(
            MANAGED_PI_UPLOAD_TIMEOUT,
            provider.upload_dir(id, &host_agent, &dest),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "upload timed out after {}s",
                MANAGED_PI_UPLOAD_TIMEOUT.as_secs()
            )),
        }
    })
    .map_err(|e| anyhow::anyhow!("carry managed agent dir → {dest}: {e}"))?;

    // 2. Install the pinned pi binary in the sandbox (best-effort — needs node/npm;
    //    a missing npm just means the Agent entry won't work here, not a hard fail).
    //    Fetch-retry flags harden against a fresh sprite's transient egress (same
    //    failure class as the Nix cold-boot egress race), and the run_exec is bounded
    //    by `provision_step_timeout` — the ManagedPi step, unlike Exec steps, is not
    //    wrapped by the caller, so a hung npm install would otherwise freeze here.
    let pin = crate::pi_assets::PI_PIN;
    let script = format!(
        "command -v npm >/dev/null 2>&1 || {{ echo 'npm not found — managed pi binary not installed'; exit 0; }}; \
         npm install --prefix \"$HOME/.thegn/pi\" @earendil-works/pi-coding-agent@{pin} \
         --fetch-retries=3 --fetch-retry-mintimeout=2000 --fetch-timeout=60000 2>&1"
    );
    // remote/sandbox target is Linux; POSIX sh is correct here
    let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
    let to = provision_step_timeout("managed_pi");
    block_on_provider(|| async {
        match tokio::time::timeout(to, provider.run_exec(id, &argv, None, exec_env)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "in-sprite npm install timed out after {}s",
                to.as_secs()
            )),
        }
    })
    .map(|_| ())
    .map_err(|e| anyhow::anyhow!("npm install managed pi in sandbox: {e}"))
}

#[cfg(test)]
mod tests {
    use super::provision_step_timeout;

    // The managed-pi step's in-sprite `npm install` is bounded by
    // `provision_step_timeout("managed_pi")`. It must resolve to the generous
    // build/download ceiling, NOT the 2-min "instant" one reserved for
    // mkdir/git-config — a fetch-bound install can legitimately run for minutes,
    // so an early bound would be a false failure. This locks the bound in place.
    #[test]
    fn managed_pi_gets_build_ceiling_not_instant() {
        let managed = provision_step_timeout("managed_pi");
        let instant = provision_step_timeout("workspace");
        assert_eq!(
            managed.as_secs(),
            1800,
            "managed_pi should get the 30-min ceiling"
        );
        assert!(
            managed > instant,
            "managed_pi ({:?}) must exceed the instant ceiling ({:?})",
            managed,
            instant
        );
    }
}
