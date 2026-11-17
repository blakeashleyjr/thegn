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
/// `min_vcpu` rounded up to a legal value, else 2). Spine stub, filled by
/// lane 2: always the default 2.
pub fn vcpus_for(_size: &str, _min_vcpu: u32) -> Result<u32, AdmitError> {
    Ok(2)
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

/// How many ms to extend a session by, `None` = don't. Spine stub, filled by
/// lane 2: never extends.
pub fn next_extension(
    _now_ms: i64,
    _expires_at_ms: i64,
    _created_at_ms: i64,
    _lifetime_cap_secs: u64,
) -> Option<u64> {
    None
}

/// What the lifetime reaper does with one sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapAction {
    Keep,
    Stop,
    Delete,
    Quarantine(&'static str),
}

/// The reaper decision for one sandbox. Spine stub, filled by lane 2: keeps.
pub fn reap_action(
    _now_ms: i64,
    _created_at_ms: Option<i64>,
    _status: SessionStatus,
    _owner_alive: bool,
    _lifetime_cap_secs: u64,
) -> ReapAction {
    ReapAction::Keep
}

/// The ownership tags (at most 5, keys `thegn-*`). Spine stub, filled by
/// lane 2.
pub fn tags_for(_controller: &str, _owner: &str, _host_label: &str) -> BTreeMap<String, String> {
    BTreeMap::new()
}

/// The fleet_cost size key: `"<n>vcpu"`.
pub fn size_key(vcpus: u32) -> String {
    format!("{vcpus}vcpu")
}

/// Whether a session in `status` bills compute. Spine stub, filled by lane 2.
pub fn is_billing(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Pending
            | SessionStatus::Running
            | SessionStatus::Stopping
            | SessionStatus::Snapshotting
    )
}
