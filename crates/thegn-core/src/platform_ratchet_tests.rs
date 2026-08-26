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

/// Every ssh invocation gets its host-key options from the one policy
/// chokepoint (`hostkey::host_key_args`) by naming a connection class; a bare
/// `-o StrictHostKeyChecking=…` / `UserKnownHostsFile=…` / `HostKeyAlias=…`
/// literal anywhere else bypasses the policy table (and the ratchet that keeps
/// it the single source). See `crate::hostkey`.
#[test]
fn host_key_literals_stay_in_the_chokepoint() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "hostkey-core-ratchet.txt",
        // hostkey.rs IS the chokepoint — the only place the literals are built.
        &["hostkey.rs"],
        |_, body| crate::hostkey::is_host_key_literal(body),
        "SSH host-key options belong to the one policy chokepoint \
         (`thegn_core::hostkey::host_key_args`): name a connection class \
         (UserDeclared / ManagedFresh / LoopbackTunneled / SandboxBootstrap) and \
         let it build the `-o` args. Do not write a StrictHostKeyChecking / \
         UserKnownHostsFile / HostKeyAlias literal at a call site.",
    );
}
