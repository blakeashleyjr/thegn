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

#[cfg(test)]
fn bad_thread_name_literal(body: &str) -> bool {
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
    regex::Regex::new(
        r#"thread::Builder::new\(\)\s*\.name\s*\(\s*(?:"([^"\\]*)"|format!\s*\(\s*"([^"\\]*)")"#,
    )
    .expect("thread name literal regex is valid")
    .captures_iter(code)
    .filter_map(|captures| captures.get(1).or_else(|| captures.get(2)))
    .any(|name| {
        let name = name.as_str();
        let prefix = name.split_once('{').map_or(name, |(prefix, _)| prefix);
        prefix.len() > 15 || !prefix.starts_with("thegn-")
    })
}

#[test]
fn platform_ratchet_thread_names_are_frozen_empty() {
    crate::ratchet::file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-name-metrics-ratchet.txt",
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
fn platform_ratchet_thread_names_fit_the_comm_limit() {
    crate::ratchet::file_ratchet(
        env!("CARGO_MANIFEST_DIR"),
        "thread-name-length-metrics-ratchet.txt",
        &[],
        |key, body| {
            let file_name = key.rsplit('/').next().unwrap_or(key);
            file_name != "tests.rs"
                && !file_name.ends_with("_tests.rs")
                && bad_thread_name_literal(body)
        },
        "Production thread names must use the `thegn-` prefix and fit in 15 bytes.",
    );
}

#[test]
fn platform_ratchet_thread_name_length_predicate_cases() {
    let cases = [
        (r#"thread::Builder::new().name("thegn-good")"#, false),
        (r#"thread::Builder::new().name("bad-name")"#, true),
        (
            r#"thread::Builder::new().name(format!("mcp-up-{n}"))"#,
            true,
        ),
        (
            "#[cfg(test)]\nmod tests { thread::Builder::new().name(\"bad-name\"); }",
            false,
        ),
    ];

    for (source, expected) in cases {
        assert_eq!(bad_thread_name_literal(source), expected, "{source}");
    }
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
