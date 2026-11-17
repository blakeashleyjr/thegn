//! Vercel spend policy: create sizing, the timeout-extension rule, the
//! lifetime reaper decision and the ownership tags. Pure.

use std::collections::BTreeMap;

use crate::vercel::admit::AdmitError;
use crate::vercel::api::SessionStatus;

/// Everything a create needs, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePlan {
    pub name: String,
    pub vcpus: u32,
    pub image: Option<String>,
    pub region: Option<String>,
    pub timeout_ms: u64,
    pub tags: BTreeMap<String, String>,
    pub egress: Vec<String>,
}

/// The session timeout a sandbox is created with.
pub const INITIAL_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// How often a live `vercel-exec` bridge checks the remaining session time.
pub const EXTEND_CHECK_SECS: u64 = 300;
/// Extend only when less than this remains.
pub const EXTEND_WHEN_REMAINING_MS: u64 = 10 * 60 * 1000;
/// One extension step.
pub const EXTEND_STEP_MS: u64 = 15 * 60 * 1000;
/// `max_instances = 0` resolves to this for vercel.
pub const DEFAULT_MAX_INSTANCES: u32 = 2;
/// `max_lifetime_secs = 0` resolves to this for vercel (never unbounded).
pub const DEFAULT_MAX_LIFETIME_SECS: u64 = 24 * 3600;
/// A stopped-at-ceiling sandbox is deleted this long after the ceiling.
pub const DELETE_GRACE_SECS: u64 = 7 * 24 * 3600;

/// The vCPU count for a configured `size` (`1|2|4|8|16|32`; `auto`/empty ⇒
/// `min_vcpu` rounded up to a legal value, else 2).
pub fn vcpus_for(size: &str, min_vcpu: u32) -> Result<u32, AdmitError> {
    const LEGAL: [u32; 6] = [1, 2, 4, 8, 16, 32];
    let size = size.trim();
    if size.is_empty() || size.eq_ignore_ascii_case("auto") {
        if min_vcpu == 0 {
            return Ok(2);
        }
        return LEGAL
            .iter()
            .copied()
            .find(|v| *v >= min_vcpu)
            .ok_or(AdmitError {
                field: "min_vcpu",
                reason: "exceeds the largest vercel size (32)",
            });
    }
    match size {
        "1" | "2" | "4" | "8" | "16" | "32" => Ok(size.parse().unwrap_or(2)),
        _ => Err(AdmitError {
            field: "size",
            reason: "must be one of 1, 2, 4, 8, 16, 32 or auto",
        }),
    }
}

/// The effective instance cap (`0` ⇒ [`DEFAULT_MAX_INSTANCES`]).
pub fn max_instances(configured: u32) -> u32 {
    if configured == 0 {
        DEFAULT_MAX_INSTANCES
    } else {
        configured
    }
}

/// The effective lifetime ceiling (`0` ⇒ [`DEFAULT_MAX_LIFETIME_SECS`]).
pub fn max_lifetime_secs(configured: u64) -> u64 {
    if configured == 0 {
        DEFAULT_MAX_LIFETIME_SECS
    } else {
        configured
    }
}

/// How many ms to extend a session by, `None` = don't. Extends only when less
/// than [`EXTEND_WHEN_REMAINING_MS`] remains, never past
/// `created_at + lifetime_cap`. All arithmetic is checked.
pub fn next_extension(
    now_ms: i64,
    expires_at_ms: i64,
    created_at_ms: i64,
    lifetime_cap_secs: u64,
) -> Option<u64> {
    if now_ms < 0 || expires_at_ms < 0 || created_at_ms < 0 {
        return None;
    }
    let remaining = expires_at_ms.checked_sub(now_ms)?;
    if remaining < 0 || remaining >= i64::try_from(EXTEND_WHEN_REMAINING_MS).ok()? {
        return None;
    }
    let cap_ms = i64::try_from(lifetime_cap_secs.checked_mul(1000)?).ok()?;
    let ceiling = created_at_ms.checked_add(cap_ms)?;
    let room = ceiling.checked_sub(expires_at_ms)?;
    if room <= 0 {
        return None;
    }
    Some(EXTEND_STEP_MS.min(u64::try_from(room).ok()?))
}

/// What the lifetime reaper does with one sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapAction {
    Keep,
    Stop,
    Delete,
    Quarantine(&'static str),
}

/// The reaper decision for one sandbox. Unknown age is quarantined (never
/// deleted blind); an owner-less sandbox is deleted; past the ceiling a billing
/// session is stopped first, and a non-billing one past ceiling + grace is deleted.
pub fn reap_action(
    now_ms: i64,
    created_at_ms: Option<i64>,
    status: SessionStatus,
    owner_alive: bool,
    lifetime_cap_secs: u64,
) -> ReapAction {
    let Some(created) = created_at_ms else {
        return ReapAction::Quarantine("unknown age");
    };
    if !owner_alive {
        return ReapAction::Delete;
    }
    let age_secs = u64::try_from(now_ms.saturating_sub(created) / 1000).unwrap_or(0);
    if age_secs >= lifetime_cap_secs && is_billing(status) {
        return ReapAction::Stop;
    }
    if age_secs >= lifetime_cap_secs.saturating_add(DELETE_GRACE_SECS) {
        return ReapAction::Delete;
    }
    ReapAction::Keep
}

fn tag_value(s: &str) -> String {
    s.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}

/// The tag key carrying the sanitized controller id.
pub const CONTROLLER_TAG: &str = "thegn-controller";
/// The tag key carrying the sanitized ledger owner.
pub const OWNER_TAG: &str = "thegn-owner";
/// The tag key carrying the sanitized host label (informational only).
pub const HOST_TAG: &str = "thegn-host";

/// The ownership tags (keys `thegn-*`; values sanitized to `[a-z0-9-]`, at
/// most 64 chars; the tag charset is UNVERIFIED so this stays conservative).
pub fn tags_for(controller: &str, owner: &str, host_label: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (CONTROLLER_TAG.to_string(), tag_value(controller)),
        (OWNER_TAG.to_string(), tag_value(owner)),
        (HOST_TAG.to_string(), tag_value(host_label)),
    ])
}

/// The one ownership proof every stop/delete path reads (the reaper and the
/// provider's delete): `tags` carry [`CONTROLLER_TAG`] equal to the sanitized
/// `controller` exactly as [`tags_for`] wrote it, and, when `owner` is
/// non-empty, [`OWNER_TAG`] equal to the sanitized `owner`. An empty
/// controller proves nothing. Sanitizing is idempotent, so a caller may pass
/// either the raw id or an already sanitized tag value.
pub fn tags_prove_ours(tags: &BTreeMap<String, String>, controller: &str, owner: &str) -> bool {
    let want = tag_value(controller);
    if want.is_empty() || tags.get(CONTROLLER_TAG) != Some(&want) {
        return false;
    }
    owner.is_empty() || tags.get(OWNER_TAG) == Some(&tag_value(owner))
}

/// The fleet_cost size key: `"<n>vcpu"`.
pub fn size_key(vcpus: u32) -> String {
    format!("{vcpus}vcpu")
}

/// Whether a session in `status` bills compute. An unrecognised status
/// (`Unknown`) fails safe for spend: it counts as billing (possibly running),
/// so the reaper stops it at the ceiling and the cost rows price it as running.
pub fn is_billing(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Pending
            | SessionStatus::Running
            | SessionStatus::Stopping
            | SessionStatus::Snapshotting
            | SessionStatus::Unknown
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    #[test]
    fn vcpus() {
        for n in [1u32, 2, 4, 8, 16, 32] {
            assert_eq!(vcpus_for(&n.to_string(), 0).unwrap(), n);
        }
        assert_eq!(vcpus_for("", 0).unwrap(), 2);
        assert_eq!(vcpus_for("auto", 0).unwrap(), 2);
        assert_eq!(vcpus_for("AUTO", 3).unwrap(), 4);
        assert_eq!(vcpus_for("", 1).unwrap(), 1);
        assert_eq!(vcpus_for("", 9).unwrap(), 16);
        assert_eq!(vcpus_for("", 32).unwrap(), 32);
        assert_eq!(vcpus_for("", 33).unwrap_err().field, "min_vcpu");
        assert_eq!(vcpus_for("3", 0).unwrap_err().field, "size");
        assert_eq!(vcpus_for("large", 0).unwrap_err().field, "size");
    }

    #[test]
    fn effective_caps() {
        assert_eq!(max_instances(0), 2);
        assert_eq!(max_instances(7), 7);
        assert_eq!(max_lifetime_secs(0), 86_400);
        assert_eq!(max_lifetime_secs(5), 5);
    }

    #[test]
    fn extension_boundaries() {
        let cap = 24 * 3600;
        // Plenty of time left, exactly the threshold: no extension.
        assert_eq!(
            next_extension(0, EXTEND_WHEN_REMAINING_MS as i64, 0, cap),
            None
        );
        // One ms under the threshold: a full step.
        assert_eq!(
            next_extension(0, EXTEND_WHEN_REMAINING_MS as i64 - 1, 0, cap),
            Some(EXTEND_STEP_MS)
        );
        // Already expired is not extended.
        assert_eq!(next_extension(10, 5, 0, cap), None);
        // Clamped to the ceiling.
        let created = 0;
        let expires = cap as i64 * 1000 - 60_000;
        assert_eq!(
            next_extension(expires - 1000, expires, created, cap),
            Some(60_000)
        );
        // At / past the ceiling.
        let ceil = cap as i64 * 1000;
        assert_eq!(next_extension(ceil - 1000, ceil, 0, cap), None);
        assert_eq!(next_extension(ceil, ceil + 5, 0, cap), None);
    }

    #[test]
    fn extension_negative_and_overflow() {
        assert_eq!(next_extension(-1, 5, 0, 100), None);
        assert_eq!(next_extension(0, -5, 0, 100), None);
        assert_eq!(next_extension(0, 5, -1, 100), None);
        assert_eq!(next_extension(i64::MAX, i64::MIN, 0, 100), None);
        assert_eq!(next_extension(0, 5, i64::MAX, 100), None);
        assert_eq!(next_extension(0, 5, 0, u64::MAX), None);
        assert_eq!(next_extension(0, 5, 0, u64::MAX / 1000 + 1), None);
    }

    #[test]
    fn reap_matrix() {
        let cap = 3600;
        let now = 100 * H * 24;
        let run = SessionStatus::Running;
        let stopped = SessionStatus::Stopped;
        let aged = |secs: u64| Some(now - secs as i64 * 1000);
        assert_eq!(
            reap_action(now, None, run, true, cap),
            ReapAction::Quarantine("unknown age")
        );
        assert_eq!(
            reap_action(now, None, run, false, cap),
            ReapAction::Quarantine("unknown age")
        );
        assert_eq!(
            reap_action(now, aged(10), run, false, cap),
            ReapAction::Delete
        );
        assert_eq!(reap_action(now, aged(10), run, true, cap), ReapAction::Keep);
        assert_eq!(
            reap_action(now, aged(cap - 1), run, true, cap),
            ReapAction::Keep
        );
        assert_eq!(
            reap_action(now, aged(cap), run, true, cap),
            ReapAction::Stop
        );
        assert_eq!(
            reap_action(now, aged(cap), stopped, true, cap),
            ReapAction::Keep
        );
        assert_eq!(
            reap_action(now, aged(cap + DELETE_GRACE_SECS - 1), stopped, true, cap),
            ReapAction::Keep
        );
        assert_eq!(
            reap_action(now, aged(cap + DELETE_GRACE_SECS), stopped, true, cap),
            ReapAction::Delete
        );
        assert_eq!(
            reap_action(now, aged(cap + DELETE_GRACE_SECS), run, true, cap),
            ReapAction::Stop
        );
        // A created_at in the future is age 0.
        assert_eq!(
            reap_action(now, Some(now + 5000), run, true, cap),
            ReapAction::Keep
        );
    }

    #[test]
    fn tags() {
        let t = tags_for("Thegn_Host", "wt/Feature X", &"h".repeat(100));
        assert_eq!(t.len(), 3);
        assert_eq!(t["thegn-controller"], "thegn-host");
        assert_eq!(t["thegn-owner"], "wt-feature-x");
        assert_eq!(t["thegn-host"].len(), 64);
        assert!(t.values().all(|v| {
            v.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }));
    }

    #[test]
    fn written_tags_prove_ownership_raw_or_sanitized() {
        // A controller id that sanitizing changes: the proof must still match
        // what `tags_for` wrote, whether the caller passes raw or sanitized.
        let t = tags_for("Ctl_ABC", "wt/Owner", "host");
        assert!(tags_prove_ours(&t, "Ctl_ABC", ""));
        assert!(tags_prove_ours(&t, "ctl-abc", ""));
        assert!(tags_prove_ours(&t, "Ctl_ABC", "wt/Owner"));
        assert!(tags_prove_ours(&t, &t[CONTROLLER_TAG], &t[OWNER_TAG]));
        assert!(!tags_prove_ours(&t, "other", ""));
        assert!(!tags_prove_ours(&t, "Ctl_ABC", "wt/Other"));
        assert!(!tags_prove_ours(&t, "", ""));
        assert!(!tags_prove_ours(&BTreeMap::new(), "Ctl_ABC", ""));
    }

    #[test]
    fn billing_and_size_key() {
        assert_eq!(size_key(4), "4vcpu");
        for s in [
            SessionStatus::Pending,
            SessionStatus::Running,
            SessionStatus::Stopping,
            SessionStatus::Snapshotting,
            SessionStatus::Unknown,
        ] {
            assert!(is_billing(s));
        }
        for s in [
            SessionStatus::Stopped,
            SessionStatus::Failed,
            SessionStatus::Aborted,
        ] {
            assert!(!is_billing(s));
        }
    }
}
