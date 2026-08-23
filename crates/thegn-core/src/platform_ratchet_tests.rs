//! Platform-cfg ratchet for `thegn-core` (see `test_support::ratchet`).
//! `termcaps.rs` and the `sandbox*` family are platform *tables* by design and
//! are excluded; everything else pins in `test/platform-cfg-core-ratchet.txt`.

use crate::test_support::ratchet::{file_ratchet, has_platform_cfg};

#[test]
fn platform_cfgs_are_pinned() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "platform-cfg-core-ratchet.txt",
        &["termcaps.rs", "sandbox"],
        |_, body| has_platform_cfg(body),
        "thegn-core is substrate-agnostic; per-OS behaviour belongs in a platform \
         table (termcaps, sandbox) or in thegn-host/src/platform/. Prefer a pure \
         function that takes the OS as data.",
    );
}
