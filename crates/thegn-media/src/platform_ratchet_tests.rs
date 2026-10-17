//! Platform-cfg ratchet for `thegn-media` — a per-OS leaf crate by design, so the
//! pinned files in `test/platform-cfg-media-ratchet.txt` *are* the platform
//! tables; the ratchet still makes a new per-OS file a deliberate, reasoned
//! addition rather than drift. `ratchet.rs` is a verbatim copy of
//! `thegn_core::test_support::ratchet` (this crate is core-free).

use crate::ratchet::{file_ratchet, has_platform_cfg};

#[test]
fn platform_cfgs_are_pinned() {
    file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "platform-cfg-media-ratchet.txt",
        &[],
        |_, body| has_platform_cfg(body),
        "This crate is a per-OS leaf: per-platform code is expected, but each \
         file carrying it is pinned so a new one is a reasoned addition.",
    );
}

fn production_thread_spawn(body: &str) -> bool {
    let code = crate::ratchet::code_only(body);
    let lines: Vec<&str> = code.lines().collect();
    let production_end = lines
        .windows(2)
        .position(|pair| {
            pair[0].trim() == "#[cfg(test)]" && pair[1].trim_start().starts_with("mod ")
        })
        .map_or(code.len(), |line| {
            lines[..line].iter().map(|part| part.len() + 1).sum()
        });
    let code = &code[..production_end];
    regex::Regex::new(r"(?:^|[^A-Za-z0-9_])thread::spawn\s*\(")
        .expect("thread spawn predicate regex is valid")
        .is_match(code)
}

#[test]
fn platform_ratchet_thread_names_are_frozen_empty() {
    crate::ratchet::file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-name-media-ratchet.txt",
        &[],
        |key, body| {
            let file_name = key.rsplit('/').next().unwrap_or(key);
            file_name != "tests.rs"
                && !file_name.ends_with("_tests.rs")
                && production_thread_spawn(body)
        },
        "Name production threads with the crate's stable task attribution prefix.",
    );
}

#[test]
fn platform_ratchet_thread_name_predicate_ignores_test_code() {
    let cases = [
        ("thread::spawn(|| {});", true),
        ("#[cfg(test)]\nmod tests { thread::spawn(|| {}); }", false),
        ("thread::Builder::new().spawn(|| {});", false),
    ];

    for (source, expected) in cases {
        assert_eq!(production_thread_spawn(source), expected, "{source}");
    }
}
