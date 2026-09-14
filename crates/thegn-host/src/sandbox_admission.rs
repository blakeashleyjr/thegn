//! Final launch admission, including bare host/remote outcomes after fallback.

use thegn_core::capabilities::IsolationClass;
use thegn_core::config::SandboxConfig;
use thegn_core::sandbox_floor::{FloorDecision, decide};

/// Check what the launch will actually enter. Candidate selection can remember
/// rejected weaker runtimes, but that does not prove the eventual host fallback
/// meets the floor (all runtimes may have been absent or failed preflight).
pub(crate) fn admit(
    cfg: &SandboxConfig,
    actual: IsolationClass,
    worktree: &str,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    match decide(cfg.isolation_floor, cfg.on_floor_miss, actual, actual) {
        FloorDecision::Ok | FloorDecision::BypassProvider => Ok(()),
        FloorDecision::Degrade(miss) => {
            let message = miss.message();
            thegn_core::msg::warn(&format!("{message} for {worktree}"));
            warnings.push(message);
            Ok(())
        }
        FloorDecision::Fail(miss) => anyhow::bail!("{} for {worktree}", miss.message()),
    }
}
