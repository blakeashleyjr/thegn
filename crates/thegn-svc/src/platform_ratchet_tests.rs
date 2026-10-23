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

#[test]
fn threads_are_named_ratchet() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-name-svc-ratchet.txt",
        &[],
        |key, body| {
            !thegn_core::test_support::thread_spawn::is_test_module_file(key)
                && thegn_core::test_support::thread_spawn::spawns_unnamed_thread(body)
        },
        "Name production threads with `thegn_core::thread_name::spawn_named` so \
         their task comm identifies their role. Keep the allowlist empty.",
    );
}

fn has_runtime_platform_guard(code: &str) -> bool {
    let mut remaining = code;
    while let Some(start) = remaining.find("Path::new(\"") {
        remaining = &remaining[start + "Path::new(\"".len()..];
        let Some(end_quote) = remaining.find('\"') else {
            return false;
        };
        let path = &remaining[..end_quote];
        if path != "/bin/sh" && !path.starts_with("/proc") {
            continue;
        }
        let after_path = remaining[end_quote + 1..].trim_start();
        let Some(after_close) = after_path.strip_prefix(')') else {
            continue;
        };
        let after_close = after_close.trim_start();
        let Some(after_dot) = after_close.strip_prefix('.') else {
            continue;
        };
        let after_dot = after_dot.trim_start();
        if ["exists", "is_file", "is_dir"].iter().any(|method| {
            after_dot
                .strip_prefix(method)
                .is_some_and(|tail| tail.trim_start().starts_with('('))
        }) {
            return true;
        }
    }
    false
}

#[test]
fn converted_runtime_guards_stay_compile_time() {
    const FILES: &[&str] = &["voice.rs", "git/native_diff.rs"];

    for file in FILES {
        let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
        let body = std::fs::read_to_string(path).expect("read production source file");
        let code = thegn_core::test_support::ratchet::code_only(&body);
        assert!(
            !has_runtime_platform_guard(&code),
            "{file} still probes /proc or /bin/sh at runtime"
        );
    }
}

#[test]
fn runtime_platform_guard_predicate_tracks_only_direct_probes() {
    let cases = [
        (r#"std::path::Path::new("/proc/self/stat").exists()"#, true),
        (r#"Path::new("/bin/sh").is_file()"#, true),
        (r#"Path::new("/proc").join(pid)"#, false),
        (r#"cfg!(target_os = "linux")"#, false),
        (r#"// Path::new("/proc/self/stat").exists()"#, false),
    ];

    for (source, expected) in cases {
        let code = thegn_core::test_support::ratchet::code_only(source);
        assert_eq!(has_runtime_platform_guard(&code), expected, "{source}");
    }
}

#[test]
fn thread_builders_go_through_spawn_named_ratchet() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-builder-svc-ratchet.txt",
        &[],
        |key, body| {
            !thegn_core::test_support::thread_spawn::is_test_module_file(key)
                && thegn_core::test_support::thread_spawn::builds_thread_directly(body)
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
        "thread-name-length-svc-ratchet.txt",
        &[],
        |key, body| {
            !thegn_core::test_support::thread_spawn::is_test_module_file(key)
                && !thegn_core::test_support::thread_spawn::bad_thread_name_literals(body)
                    .is_empty()
        },
        "A thread name literal must start with `thegn-` and be at most 15 bytes (the Linux comm limit; longer names are truncated in /proc and break attribution). Use `thegn_core::thread_name::spawn_named`/`try_spawn_named`; the allowlist is seeded debt (THE-931) and shrink-only.",
    );
}
