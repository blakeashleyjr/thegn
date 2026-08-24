//! Platform-cfg ratchet for `thegn-metrics` — a per-OS leaf crate by design, so the
//! pinned files in `test/platform-cfg-metrics-ratchet.txt` *are* the platform
//! tables; the ratchet still makes a new per-OS file a deliberate, reasoned
//! addition rather than drift. `ratchet.rs` is a verbatim copy of
//! `thegn_core::test_support::ratchet` (this crate is core-free).

use crate::ratchet::{file_ratchet, has_platform_cfg};

#[test]
fn platform_cfgs_are_pinned() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "platform-cfg-metrics-ratchet.txt",
        &[],
        |_, body| has_platform_cfg(body),
        "This crate is a per-OS leaf: per-platform code is expected, but each \
         file carrying it is pinned so a new one is a reasoned addition.",
    );
}
