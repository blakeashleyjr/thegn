//! UI-side yield to the daemon's headless producers (THE-2208).
//!
//! Each UI producer body (off-loop, on `sched::spawn_bg`) first asks
//! [`daemon_owns`]; while the daemon holds that kind's live lease the UI skips
//! the network fetch and only reloads its panel from the DB cache. Off-loop
//! only: it reads the DB.
//!
//! Stub: filled by b161 lane 5. Until then the UI always polls itself.
#![allow(dead_code, reason = "filled by b161 lane 5")]

use thegn_core::producer_lease::ProducerKind;

/// True while the daemon holds `kind`'s live lease. Stub: never.
pub(crate) fn daemon_owns(_kind: ProducerKind) -> bool {
    false
}
