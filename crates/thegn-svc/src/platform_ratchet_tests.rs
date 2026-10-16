//! Platform-cfg ratchet for `thegn-svc` (see
//! `thegn_core::test_support::ratchet`). Pins in
//! `test/platform-cfg-svc-ratchet.txt`; `ipc.rs` (the unix-socket / named-pipe
//! seam) is the intended home for per-OS code here.

use thegn_core::test_support::ratchet::{
    file_ratchet, has_platform_cfg, sets_state_env_directly, state_env_test_body,
};

#[test]
fn platform_cfgs_are_pinned() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "platform-cfg-svc-ratchet.txt",
        &["plugin/platform/"],
        |_, body| has_platform_cfg(body),
        "Per-OS code in thegn-svc belongs in ipc.rs (the transport seam) or plugin/platform (owned resident lifecycle) or behind \
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

#[test]
fn state_env_ratchet_tracks_direct_test_environment_writes() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "state-env-svc-ratchet.txt",
        &["testenv.rs", "test_support/ratchet.rs"],
        |key, body| state_env_test_body(key, body).is_some_and(sets_state_env_directly),
        "Tests that mutate HOME/XDG or Windows user-state variables should use \
         testenv::IsolatedState and state_env_for so isolation works on every OS.",
    );
}
