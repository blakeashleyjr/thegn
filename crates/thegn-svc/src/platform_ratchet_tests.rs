//! Platform-cfg ratchet for `thegn-svc` (see
//! `thegn_core::test_support::ratchet`). Pins in
//! `test/platform-cfg-svc-ratchet.txt`; `ipc.rs` (the unix-socket / named-pipe
//! seam) is the intended home for per-OS code here.

use thegn_core::test_support::ratchet::{file_ratchet, has_platform_cfg};

#[test]
fn platform_cfgs_are_pinned() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "platform-cfg-svc-ratchet.txt",
        &[],
        |_, body| has_platform_cfg(body),
        "Per-OS code in thegn-svc belongs in ipc.rs (the transport seam) or behind \
         a thegn-host platform function. Keep service logic platform-free.",
    );
}
