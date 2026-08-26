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

/// The managed-ssh call sites in thegn-svc (`vps/ssh_shim.rs`, `host/mod.rs`)
/// get their host-key options from the one chokepoint; no literal here. See
/// `thegn_core::hostkey`.
#[test]
fn host_key_literals_stay_in_the_chokepoint() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "hostkey-svc-ratchet.txt",
        &[],
        |_, body| thegn_core::hostkey::is_host_key_literal(body),
        "SSH host-key options belong to the one policy chokepoint \
         (`thegn_core::hostkey::host_key_args`): name a connection class and let \
         it build the `-o` args. Do not write a StrictHostKeyChecking / \
         UserKnownHostsFile / HostKeyAlias literal at a call site.",
    );
}
