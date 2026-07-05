//! **Zones** — a named group of workspaces inside one profile providing a soft,
//! concurrent firewall (the two-freelance-clients case: both open side by side,
//! each worktree's panes/agents scoped to its zone).
//!
//! A zone is *policy in config* (`[zone.<name>]` — this module's [`ZoneConfig`])
//! plus *membership in the DB* (the [`crate::store::ZoneStore`] `workspaces.zone_id`).
//! The split is deliberate: policy is declarative/git-diffable, membership is
//! runtime state that a config typo must never silently re-zone.
//!
//! Zones plug into the config-resolution trust ladder at
//! [`crate::config_resolve::TrustLevel::Zone`] (between profile and repo): a
//! zone *clamps* its members — egress intersects down, budget caps roll up,
//! sandbox hardening can only rise. It never overlay-replaces. The interim
//! [`apply_zone_ceilings`] is the seam the general clamp engine will absorb.

use serde::{Deserialize, Serialize};

use crate::config::{SandboxConfig, SandboxProfile};

/// Zone policy (`[zone.<name>]`). Explicit clamp fields — deliberately NOT a
/// `SandboxOverlay`, whose semantics are replace/merge; a zone's are *clamp*.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ZoneConfig {
    /// Bundles bound at the zone scope layer (composed for every member
    /// worktree, below workspace/worktree bindings).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_bundles: Vec<String>,
    /// Egress **ceiling**: member `network_allow` intersects down into this set
    /// (empty ⇒ no zone ceiling). Enforced via the per-container DNS filter.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub network_allow: Vec<String>,
    /// Extra egress blocks appended to every member (deny wins).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub network_block: Vec<String>,
    /// Minimum sandbox hardening: a member profile may only be ≥ this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_floor: Option<SandboxProfile>,
    /// Spend cap for the zone scope (`zone:<name>`), rolled up in the proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<ZoneBudget>,
}

/// A zone's spend cap (`[zone.<name>.budget]`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ZoneBudget {
    /// Token ceiling for the period (`0`/absent ⇒ none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_tokens: Option<i64>,
    /// Cost ceiling in USD for the period (`0`/absent ⇒ none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_cost: Option<f64>,
}

/// The rank used by the sandbox-profile floor (higher = stricter). Kept in sync
/// with [`crate::config_resolve`]'s lattice.
fn profile_rank(p: SandboxProfile) -> u8 {
    match p {
        SandboxProfile::Open => 0,
        SandboxProfile::Hardened => 1,
        SandboxProfile::SealedTunnel => 2,
        SandboxProfile::Sealed => 3,
    }
}

/// A dropped egress entry reported when a member's `network_allow` is narrowed
/// by the zone ceiling.
#[derive(Debug, Clone, PartialEq)]
pub struct DroppedEgress {
    pub entry: String,
    pub zone: String,
}

/// Apply a zone's ceilings to a member's resolved sandbox: intersect egress
/// down, union blocks, raise the hardening floor. Returns the entries dropped by
/// the egress intersection (to surface). Interim engine — the general clamp
/// engine will absorb this at the `TrustLevel::Zone` slot.
pub fn apply_zone_ceilings(
    sb: &mut SandboxConfig,
    zone_name: &str,
    zc: &ZoneConfig,
) -> Vec<DroppedEgress> {
    let mut dropped = Vec::new();

    // Egress intersect-down. Empty zone ceiling ⇒ no narrowing.
    if !zc.network_allow.is_empty() {
        if sb.network_allow.is_empty() {
            // Member allowed everything (within the ceiling) ⇒ adopt the ceiling.
            sb.network_allow = zc.network_allow.clone();
        } else {
            let mut kept = Vec::new();
            for entry in std::mem::take(&mut sb.network_allow) {
                if zc
                    .network_allow
                    .iter()
                    .any(|z| z == &entry || crate::dns_filter::name_matches(&entry, z))
                {
                    kept.push(entry);
                } else {
                    dropped.push(DroppedEgress {
                        entry,
                        zone: zone_name.to_string(),
                    });
                }
            }
            sb.network_allow = kept;
        }
    }

    // Block union (deny wins).
    for b in &zc.network_block {
        if !sb.network_block.contains(b) {
            sb.network_block.push(b.clone());
        }
    }

    // Hardening floor.
    if let Some(floor) = zc.sandbox_floor {
        if profile_rank(floor) > profile_rank(sb.profile) {
            sb.profile = floor;
        }
        if profile_rank(floor) > profile_rank(sb.agent_profile) {
            sb.agent_profile = floor;
        }
    }

    dropped
}

/// Whether `bundle_zone` (a bundle's owning zone, empty = global) is composable
/// by a worktree in `worktree_zone` (its zone name, empty = unzoned). A global
/// bundle is composable everywhere; a zone-owned bundle only inside its zone.
pub fn bundle_visible(bundle_zone: &str, worktree_zone: &str) -> bool {
    bundle_zone.is_empty() || bundle_zone == worktree_zone
}

/// Push each `[zone.<name>.budget]` cap into the proxy's `zone:<name>` budget
/// scope (spend is preserved — the setter only touches the limits). Call at
/// host startup + config reload so the proxy's per-request rollup
/// ([`crate::store::ProxyStore`]) enforces the config-declared caps. Best-effort.
pub fn sync_budget_caps<S: crate::store::ProxyStore>(
    zones: &std::collections::BTreeMap<String, ZoneConfig>,
    db: &S,
) {
    for (name, zc) in zones {
        if let Some(budget) = &zc.budget {
            let scope = format!("zone:{name}");
            let _ = db.set_proxy_budget_limits(
                &scope,
                "monthly",
                budget.limit_tokens,
                budget.limit_cost,
                0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb() -> SandboxConfig {
        SandboxConfig::default()
    }

    #[test]
    fn egress_intersects_down_and_reports_drops() {
        let mut s = sb();
        s.network_allow = vec!["api.clienta.com".into(), "evil.com".into()];
        let zc = ZoneConfig {
            network_allow: vec!["*.clienta.com".into()],
            ..Default::default()
        };
        let dropped = apply_zone_ceilings(&mut s, "clientA", &zc);
        assert_eq!(s.network_allow, vec!["api.clienta.com".to_string()]);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].entry, "evil.com");
    }

    #[test]
    fn empty_member_allow_adopts_zone_ceiling() {
        let mut s = sb();
        assert!(s.network_allow.is_empty());
        let zc = ZoneConfig {
            network_allow: vec!["*.clienta.com".into()],
            ..Default::default()
        };
        apply_zone_ceilings(&mut s, "clientA", &zc);
        assert_eq!(s.network_allow, vec!["*.clienta.com".to_string()]);
    }

    #[test]
    fn empty_zone_ceiling_no_narrowing() {
        let mut s = sb();
        s.network_allow = vec!["anything.com".into()];
        let dropped = apply_zone_ceilings(&mut s, "z", &ZoneConfig::default());
        assert_eq!(s.network_allow, vec!["anything.com".to_string()]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn block_union_dedups() {
        let mut s = sb();
        s.network_block = vec!["a.com".into()];
        let zc = ZoneConfig {
            network_block: vec!["a.com".into(), "b.com".into()],
            ..Default::default()
        };
        apply_zone_ceilings(&mut s, "z", &zc);
        assert_eq!(
            s.network_block,
            vec!["a.com".to_string(), "b.com".to_string()]
        );
    }

    #[test]
    fn floor_raises_but_never_lowers() {
        let mut s = sb();
        s.profile = SandboxProfile::Open;
        let zc = ZoneConfig {
            sandbox_floor: Some(SandboxProfile::Sealed),
            ..Default::default()
        };
        apply_zone_ceilings(&mut s, "z", &zc);
        assert_eq!(s.profile, SandboxProfile::Sealed);

        // A lower floor never weakens an already-stricter member.
        let mut s2 = sb();
        s2.profile = SandboxProfile::Sealed;
        let zc2 = ZoneConfig {
            sandbox_floor: Some(SandboxProfile::Open),
            ..Default::default()
        };
        apply_zone_ceilings(&mut s2, "z", &zc2);
        assert_eq!(s2.profile, SandboxProfile::Sealed);
    }

    #[test]
    fn bundle_visibility() {
        assert!(bundle_visible("", "anything")); // global bundle
        assert!(bundle_visible("clientA", "clientA")); // own zone
        assert!(!bundle_visible("clientA", "clientB")); // foreign zone
        assert!(!bundle_visible("clientA", "")); // unzoned worktree
    }

    #[test]
    fn sync_budget_caps_sets_limits_without_clobbering_spend() {
        use crate::db::Db;
        use crate::store::ProxyStore;
        let db = Db::open_memory().unwrap();
        // Pre-existing spend on the zone scope.
        db.add_proxy_spend("zone:clientA", 500, 1.0, 1).unwrap();
        let mut zones = std::collections::BTreeMap::new();
        zones.insert(
            "clientA".to_string(),
            ZoneConfig {
                budget: Some(ZoneBudget {
                    limit_tokens: Some(10_000),
                    limit_cost: Some(50.0),
                }),
                ..Default::default()
            },
        );
        sync_budget_caps(&zones, &db);
        let b = db.proxy_budget("zone:clientA").unwrap().unwrap();
        assert_eq!(b.limit_tokens, Some(10_000));
        assert_eq!(b.limit_cost, Some(50.0));
        assert_eq!(b.spent_tokens, 500, "spend preserved");
        // Idempotent: re-syncing keeps spend.
        sync_budget_caps(&zones, &db);
        assert_eq!(
            db.proxy_budget("zone:clientA")
                .unwrap()
                .unwrap()
                .spent_tokens,
            500
        );
    }
}
