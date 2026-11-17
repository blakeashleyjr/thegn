//! The Lead's reserved terminal group (THE-2171). There is no `GroupKind`
//! variant: the Lead is a `terminals` registry row with `kind = "lead"`.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 7.

/// The reserved terminal name of the Lead group.
#[allow(dead_code, reason = "filled by b152 lane 7")]
pub(crate) const LEAD_GROUP_NAME: &str = "lead";

/// The `terminals.kind` value that marks the Lead group.
#[allow(dead_code, reason = "filled by b152 lane 7")]
pub(crate) const LEAD_TERMINAL_KIND: &str = "lead";

/// Whether a terminal group name is the reserved Lead group.
#[allow(dead_code, reason = "filled by b152 lane 7")]
pub(crate) fn is_lead_group(name: &str) -> bool {
    name == LEAD_GROUP_NAME
}
