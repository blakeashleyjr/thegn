//! Platform-cfg ratchet for `thegn-core` (see `test_support::ratchet`).
//! `termcaps.rs` and the `sandbox*` family are platform *tables* by design and
//! are excluded; everything else pins in `test/platform-cfg-core-ratchet.txt`.

use crate::test_support::ratchet::{
    file_ratchet, has_platform_cfg, sets_state_env_directly, state_env_test_body,
};

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

#[test]
fn state_env_ratchet_tracks_direct_test_environment_writes() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "state-env-core-ratchet.txt",
        &["testenv.rs", "test_support/ratchet.rs"],
        |key, body| state_env_test_body(key, body).is_some_and(sets_state_env_directly),
        "Tests that mutate HOME/XDG or Windows user-state variables should use \
         testenv::IsolatedState and state_env_for so isolation works on every OS.",
    );
}

#[test]
fn threads_are_named_ratchet() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-name-core-ratchet.txt",
        &[],
        |key, body| {
            !crate::test_support::thread_spawn::is_test_module_file(key)
                && crate::test_support::thread_spawn::spawns_unnamed_thread(body)
        },
        "Name production threads with `crate::thread_name::spawn_named` so their \
         task comm identifies their role. Keep the allowlist empty.",
    );
}

#[test]
fn thread_builders_go_through_spawn_named_ratchet() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-builder-core-ratchet.txt",
        &["thread_name.rs"],
        |key, body| {
            !crate::test_support::thread_spawn::is_test_module_file(key)
                && crate::test_support::thread_spawn::builds_thread_directly(body)
        },
        "Spawn production threads with `thegn_core::thread_name::spawn_named` / \
         `try_spawn_named` (named, `thegn-` prefix, <= 15 bytes, QoS declared first); \
         the allowlist is seeded debt (THE-931) and shrink-only.",
    );
}

#[test]
fn thread_names_fit_the_comm_limit_ratchet() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-name-length-core-ratchet.txt",
        &[],
        |key, body| {
            !crate::test_support::thread_spawn::is_test_module_file(key)
                && !crate::test_support::thread_spawn::bad_thread_name_literals(body).is_empty()
        },
        "A thread name literal must start with `thegn-` and be at most 15 bytes (the Linux comm limit; longer names are truncated in /proc and break attribution). Use `thegn_core::thread_name::spawn_named`/`try_spawn_named`; the allowlist is seeded debt (THE-931) and shrink-only.",
    );
}
