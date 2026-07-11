//! Per-worktree ingress sharing — the *inbound* sibling of [`crate::sandbox`]'s
//! VPN seam.
//!
//! `[sandbox.vpn]` attaches a worktree's sandbox to an overlay for *egress*;
//! this module resolves a `[share]` config block into a [`ShareSpec`] that
//! exposes a worktree-local port at a public URL (a dev server, a PR preview, a
//! webhook/OAuth callback). Like the VPN seam, this is pure data: the behavior
//! (spawn the tunnel client, scrape its URL, tear it down) lives in
//! `thegn-svc::share`, and the host sequences it off the event loop.
//!
//! Provider knowledge is split exactly as the VPN seam: the config shape lives
//! in [`crate::config`], the resolved [`ShareSpec`] here, and the plan builders
//! plus subprocess I/O in `thegn-svc`. `bore` is the first backend; the
//! [`ShareProviderKind`] enum reserves room for rathole/zrok/ngrok/iroh.

use crate::config::{
    BoreConfig, FrpConfig, IrohShareConfig, ShareConfig, ShareOnError, ShareProviderKind,
    ShareReach, ShareVisibility, TailscaleShareConfig,
};
use crate::msg;
use std::time::Duration;

/// A resolved request to expose one worktree-local port. Pure data assembled by
/// [`build_share_spec`]; any secrets-refs in `params` are left **unresolved**
/// here and dereferenced only at bring-up time in `thegn-svc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareSpec {
    pub provider: ShareProviderKind,
    /// A short, DNS/filesystem-safe id for the worktree (e.g. `app-feat`), used
    /// for stable per-worktree subdomains and the state dir.
    pub label: String,
    /// The worktree-local TCP port to expose (e.g. a dev server on 3000).
    pub local_port: u16,
    /// Who can reach the share (resolved against provider capability).
    pub visibility: ShareVisibility,
    pub on_error: ShareOnError,
    /// How long to wait for the share's URL to appear before applying `on_error`.
    pub ready_timeout: Duration,
    /// The selected provider's configuration (still carrying secrets-refs).
    pub params: ShareParams,
}

/// Provider-specific share parameters, mirroring the `[share.<provider>]`
/// sub-tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareParams {
    Bore(BoreConfig),
    Frp(FrpConfig),
    Tailscale(TailscaleShareConfig),
    Iroh(IrohShareConfig),
}

/// Sanitize a worktree path/name into a DNS/filesystem-safe label.
pub fn label_for(worktree: &str) -> String {
    let base = worktree.rsplit('/').next().unwrap_or(worktree);
    let s: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').to_ascii_lowercase()
}

/// Resolve a `[share]` config block into a [`ShareSpec`] for `local_port` on the
/// worktree identified by `label` (see [`label_for`]).
///
/// `reach` selects the provider via the `[share] public`/`team`/`peer` keys
/// (intent-first); `None` falls back to the single `[share] provider`. Returns
/// `None` when the resolved provider is `none` (sharing disabled, or that reach
/// is unset). Reconciles the requested visibility with provider capability.
pub fn build_share_spec(
    cfg: &ShareConfig,
    label: &str,
    local_port: u16,
    reach: Option<ShareReach>,
) -> Option<ShareSpec> {
    let provider = match reach {
        Some(r) => cfg.reach_provider(r),
        None => cfg.provider,
    };
    let params = match provider {
        ShareProviderKind::None => return None,
        ShareProviderKind::Bore => ShareParams::Bore(cfg.bore.clone()),
        ShareProviderKind::Frp => ShareParams::Frp(cfg.frp.clone()),
        ShareProviderKind::Tailscale => ShareParams::Tailscale(cfg.tailscale.clone()),
        ShareProviderKind::Iroh => ShareParams::Iroh(cfg.iroh.clone()),
    };
    let visibility = reconcile_visibility(cfg.visibility, &params);
    Some(ShareSpec {
        provider,
        label: label.to_string(),
        local_port,
        visibility,
        on_error: cfg.on_error,
        ready_timeout: Duration::from_secs(cfg.ready_timeout_secs),
        params,
    })
}

/// Resolve the effective visibility against provider capability. bore/frp are
/// public-only (a `private` request downgrades with a warning). tailscale is
/// authoritative: `funnel` ⇒ public, `serve` ⇒ private, regardless of the
/// requested visibility.
fn reconcile_visibility(requested: ShareVisibility, params: &ShareParams) -> ShareVisibility {
    match (requested, params) {
        (_, ShareParams::Tailscale(ts)) => {
            if ts.funnel {
                ShareVisibility::Public
            } else {
                ShareVisibility::Private
            }
        }
        // iroh is peer-to-peer: a ticket holder connects, never the public web.
        (_, ShareParams::Iroh(_)) => ShareVisibility::Private,
        (ShareVisibility::Private, ShareParams::Bore(_) | ShareParams::Frp(_)) => {
            msg::warn(
                "share: this backend only supports public shares; using 'public' \
                 (use the tailscale/iroh backend for private shares)",
            );
            ShareVisibility::Public
        }
        (v, _) => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cfg() -> ShareConfig {
        ShareConfig {
            provider: ShareProviderKind::Bore,
            ..ShareConfig::default()
        }
    }

    #[test]
    fn disabled_provider_yields_none() {
        let cfg = ShareConfig::default(); // provider = none
        assert!(build_share_spec(&cfg, "wt", 3000, None).is_none());
    }

    #[test]
    fn bore_resolves_with_port_and_timeout() {
        let mut cfg = enabled_cfg();
        cfg.ready_timeout_secs = 7;
        let spec = build_share_spec(&cfg, "wt", 8080, None).expect("enabled");
        assert_eq!(spec.provider, ShareProviderKind::Bore);
        assert_eq!(spec.local_port, 8080);
        assert_eq!(spec.ready_timeout, Duration::from_secs(7));
        assert!(matches!(spec.params, ShareParams::Bore(_)));
    }

    #[test]
    fn bore_downgrades_private_to_public() {
        let mut cfg = enabled_cfg();
        cfg.visibility = ShareVisibility::Private;
        let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
        assert_eq!(spec.visibility, ShareVisibility::Public);
    }

    #[test]
    fn bore_keeps_explicit_public() {
        let mut cfg = enabled_cfg();
        cfg.visibility = ShareVisibility::Public;
        let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
        assert_eq!(spec.visibility, ShareVisibility::Public);
    }

    #[test]
    fn config_enums_round_trip() {
        use crate::config::{ShareOnError, ShareVisibility};
        assert_eq!(
            ShareProviderKind::from_str_validated("bore"),
            Ok(ShareProviderKind::Bore)
        );
        assert_eq!(
            ShareProviderKind::from_str_validated("off"),
            Ok(ShareProviderKind::None)
        );
        assert_eq!(ShareProviderKind::Bore.as_str(), "bore");
        assert!(ShareProviderKind::from_str_validated("nope").is_err());

        assert_eq!(
            ShareVisibility::from_str_validated("private"),
            Ok(ShareVisibility::Private)
        );
        assert_eq!(ShareVisibility::Public.as_str(), "public");

        assert_eq!(
            ShareOnError::from_str_validated("warn"),
            Ok(ShareOnError::Warn)
        );
        assert_eq!(ShareOnError::Fail.as_str(), "fail");

        // Unknown values are infallible at deserialize time → default.
        assert!(ShareConfig::default().provider == ShareProviderKind::None);
    }

    #[test]
    fn params_carry_bore_config_verbatim() {
        let mut cfg = enabled_cfg();
        cfg.bore.to = "relay.example.com".into();
        cfg.bore.remote_port = 9000;
        let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
        let ShareParams::Bore(b) = spec.params else {
            panic!("expected bore params");
        };
        assert_eq!(b.to, "relay.example.com");
        assert_eq!(b.remote_port, 9000);
    }

    #[test]
    fn frp_resolves_and_carries_config() {
        let mut cfg = ShareConfig {
            provider: ShareProviderKind::Frp,
            ..ShareConfig::default()
        };
        cfg.frp.server_addr = "frps.example.com".into();
        cfg.frp.subdomain_host = "share.example.com".into();
        let spec = build_share_spec(&cfg, "app-feat", 3000, None).expect("enabled");
        assert_eq!(spec.provider, ShareProviderKind::Frp);
        assert_eq!(spec.label, "app-feat");
        let ShareParams::Frp(f) = spec.params else {
            panic!("expected frp params");
        };
        assert_eq!(f.server_addr, "frps.example.com");
    }

    #[test]
    fn label_for_sanitizes_worktree_path() {
        assert_eq!(label_for("/home/u/code/app/Feat_1"), "feat-1");
        assert_eq!(label_for("plain"), "plain");
    }

    #[test]
    fn tailscale_visibility_follows_serve_vs_funnel() {
        // serve ⇒ private (tailnet), regardless of requested visibility.
        let mut cfg = ShareConfig {
            provider: ShareProviderKind::Tailscale,
            visibility: ShareVisibility::Public,
            ..ShareConfig::default()
        };
        let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
        assert_eq!(spec.visibility, ShareVisibility::Private);
        assert!(matches!(spec.params, ShareParams::Tailscale(_)));

        // funnel ⇒ public.
        cfg.tailscale.funnel = true;
        let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
        assert_eq!(spec.visibility, ShareVisibility::Public);
    }

    #[test]
    fn iroh_is_always_peer_private() {
        let cfg = ShareConfig {
            provider: ShareProviderKind::Iroh,
            visibility: ShareVisibility::Public, // requested public…
            ..ShareConfig::default()
        };
        let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
        assert_eq!(spec.visibility, ShareVisibility::Private); // …resolved private
        assert!(matches!(spec.params, ShareParams::Iroh(_)));
    }

    #[test]
    fn reach_round_trip_and_resolution() {
        assert_eq!(ShareReach::from_str_validated("team"), Ok(ShareReach::Team));
        assert_eq!(ShareReach::Peer.as_str(), "peer");

        // Reach keys map to providers; an unset reach yields `none`.
        let cfg = ShareConfig {
            public: ShareProviderKind::Frp,
            team: ShareProviderKind::Tailscale,
            ..ShareConfig::default()
        };
        // build with reach=team resolves to tailscale.
        let mut tcfg = cfg.clone();
        tcfg.tailscale.funnel = false;
        let spec = build_share_spec(&tcfg, "wt", 3000, Some(ShareReach::Team)).expect("team");
        assert_eq!(spec.provider, ShareProviderKind::Tailscale);
        // peer is unset → no spec.
        assert!(build_share_spec(&cfg, "wt", 3000, Some(ShareReach::Peer)).is_none());
        // reach=None falls back to `provider` (none here) → no spec.
        assert!(build_share_spec(&cfg, "wt", 3000, None).is_none());
    }

    #[test]
    fn configured_reaches_orders_and_respects_allow_public() {
        let mut cfg = ShareConfig {
            public: ShareProviderKind::Frp,
            team: ShareProviderKind::Tailscale,
            peer: ShareProviderKind::Iroh,
            ..ShareConfig::default()
        };
        assert_eq!(
            cfg.configured_reaches(),
            vec![ShareReach::Public, ShareReach::Team, ShareReach::Peer]
        );
        // allow_public=false drops the public reach (the guard would refuse it).
        cfg.allow_public = false;
        assert_eq!(
            cfg.configured_reaches(),
            vec![ShareReach::Team, ShareReach::Peer]
        );
        // Only `provider` set ⇒ no configured reaches (no picker).
        let only_provider = ShareConfig {
            provider: ShareProviderKind::Bore,
            ..ShareConfig::default()
        };
        assert!(only_provider.configured_reaches().is_empty());
    }

    #[test]
    fn new_share_enums_round_trip() {
        use crate::config::FrpProxyType;
        assert_eq!(
            ShareProviderKind::from_str_validated("frp"),
            Ok(ShareProviderKind::Frp)
        );
        assert_eq!(
            ShareProviderKind::from_str_validated("ts"),
            Ok(ShareProviderKind::Tailscale)
        );
        assert_eq!(
            ShareProviderKind::from_str_validated("dumbpipe"),
            Ok(ShareProviderKind::Iroh)
        );
        assert_eq!(ShareProviderKind::Frp.as_str(), "frp");
        assert_eq!(
            FrpProxyType::from_str_validated("tcp"),
            Ok(FrpProxyType::Tcp)
        );
        assert_eq!(FrpProxyType::Https.as_str(), "https");
        assert!(FrpProxyType::from_str_validated("nope").is_err());
    }
}
