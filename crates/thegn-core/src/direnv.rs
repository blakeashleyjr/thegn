//! Compatibility surface for the removed host-side `direnv` warm path.
//!
//! Thegn never evaluates a repository `.envrc` on the host, runs `direnv
//! allow`, writes `.direnv`, or starts a detached warm thread. Repository
//! environment setup is either an explicit user action or target-side work
//! performed inside the selected sandbox/provider.

use crate::config::WarmDirenv;

/// The old synchronous warm planner is retained only for source compatibility.
/// No configuration value authorizes host warming anymore.
pub const fn warm_now_plan(_mode: WarmDirenv) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::warm_now_plan;
    use crate::config::WarmDirenv;

    #[test]
    fn every_legacy_mode_is_inert() {
        for mode in [WarmDirenv::Auto, WarmDirenv::AllowedOnly, WarmDirenv::Off] {
            assert_eq!(warm_now_plan(mode), None);
            assert!(!mode.host_warming_enabled());
        }
    }
}
