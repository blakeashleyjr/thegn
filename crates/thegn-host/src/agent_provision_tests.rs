use super::{ProvisionedSandbox, provision_guard_for_ensure_result, static_tunnel_refusal};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use thegn_core::config::{SandboxProfile, VpnConfig, VpnProviderKind};
use thegn_core::sandbox::{RefusalClass, TUNNEL_ONLY_REFUSAL};

#[test]
fn provision_guard_destroys_once_on_drop() {
    let calls = Arc::new(AtomicUsize::new(0));
    let destroyed_name = Arc::new(Mutex::new(None));
    let calls_for_destroy = Arc::clone(&calls);
    let name_for_destroy = Arc::clone(&destroyed_name);
    let guard = ProvisionedSandbox::new(
        "sandbox-created-here".to_string(),
        Box::new(move |name| {
            calls_for_destroy.fetch_add(1, Ordering::SeqCst);
            *name_for_destroy
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name.to_string());
            Ok(())
        }),
    );

    drop(guard);

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *destroyed_name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("sandbox-created-here".to_string())
    );
}

#[test]
fn provision_guard_commit_never_destroys() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_destroy = Arc::clone(&calls);
    let guard = ProvisionedSandbox::new(
        "sandbox-created-here".to_string(),
        Box::new(move |_| {
            calls_for_destroy.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    );

    guard.commit();

    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn provision_guard_destroy_failure_is_logged_not_panicking() {
    let guard = ProvisionedSandbox::new(
        "sandbox-created-here".to_string(),
        Box::new(|_| anyhow::bail!("destroy failed")),
    );

    drop(guard);
}

#[test]
fn provision_preexisting_sandbox_is_not_destroyed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_destroy = Arc::clone(&calls);
    let guard = provision_guard_for_ensure_result(
        false,
        "sandbox-already-existed".to_string(),
        Box::new(move |_| {
            calls_for_destroy.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    );

    assert!(guard.is_none());
    drop(guard);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn provision_static_tunnel_refusal_only_for_sealed_tunnel_provider_with_vpn() {
    let profiles = [
        SandboxProfile::Open,
        SandboxProfile::Hardened,
        SandboxProfile::Sealed,
        SandboxProfile::SealedTunnel,
    ];
    let vpn_providers = [VpnProviderKind::None, VpnProviderKind::Tailscale];

    for is_provider in [false, true] {
        for profile in profiles {
            for provider in vpn_providers {
                let vpn = VpnConfig {
                    provider,
                    ..VpnConfig::default()
                };
                let refusal = static_tunnel_refusal(is_provider, profile, &vpn, "provider:test");
                let expected = is_provider
                    && profile == SandboxProfile::SealedTunnel
                    && provider == VpnProviderKind::Tailscale;

                if expected {
                    match refusal {
                        Some(thegn_core::sandbox::EnterError::Refused(refusal)) => {
                            assert_eq!(refusal.class, RefusalClass::TunnelUnenforced);
                            assert_eq!(refusal.message, TUNNEL_ONLY_REFUSAL);
                        }
                        other => panic!("expected tunnel refusal, got {other:?}"),
                    }
                } else {
                    assert!(refusal.is_none());
                }
            }
        }
    }
}
