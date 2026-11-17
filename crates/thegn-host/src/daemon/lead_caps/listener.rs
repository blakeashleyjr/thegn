//! The Lead's dedicated loopback control listener (THE-2171): bearer tokens
//! only (`local_admin = false`), started only when `[lead]` is configured.
//! With `[lead]` absent nothing runs: no listener, thread, timer or spawn.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 9.

/// Spawn the Lead listener when `[lead]` is configured. Stub: starts nothing.
pub(crate) fn spawn(
    _cfg: &thegn_core::config::Config,
    state: thegn_svc::control::http::ControlState,
) {
    drop(state);
}
