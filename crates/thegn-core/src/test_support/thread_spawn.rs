//! Helpers for ratcheting production thread naming.

use regex::Regex;

/// Return whether production code contains a bare `thread::spawn` call.
///
/// The scan stops at the first inline `#[cfg(test)]` or
/// `#[cfg(all(test, ..))]` module. Consequently production code placed after
/// such a module is not scanned. A direct `use std::thread::spawn; spawn(..)`
/// import is also outside this lexical check.
pub fn spawns_unnamed_thread(code: &str) -> bool {
    let mut code = crate::test_support::ratchet::code_only(code);
    if let Some(test_module) = inline_test_module_start(&code) {
        code.truncate(test_module);
    }

    Regex::new(r"(?:^|[^A-Za-z0-9_])thread::spawn\s*\(")
        .expect("thread spawn predicate regex is valid")
        .is_match(&code)
}

/// Return whether production code constructs a thread with `thread::Builder`.
///
/// Production threads go through `thegn_core::thread_name::spawn_named` or
/// `try_spawn_named`; a direct Builder is debt because those helpers are the
/// naming chokepoint (and `thread_name.rs` is excluded from this ratchet).
pub fn builds_thread_directly(code: &str) -> bool {
    let mut code = crate::test_support::ratchet::code_only(code);
    if let Some(test_module) = inline_test_module_start(&code) {
        code.truncate(test_module);
    }

    Regex::new(r"(?:^|[^A-Za-z0-9_])thread::Builder::new\s*\(")
        .expect("thread builder predicate regex is valid")
        .is_match(&code)
}

/// Return whether a source file is a whole-file test module excluded from
/// production thread scans.
pub fn is_test_module_file(key: &str) -> bool {
    let file_name = key.rsplit('/').next().unwrap_or(key);
    file_name == "tests.rs" || file_name.ends_with("_tests.rs")
}

/// Return thread-name literals that lack the `thegn-` prefix or exceed Linux's
/// 15-byte comm limit. For `format!` names, the literal prefix before the
/// first `{` is checked. Variables, constants and helper-returned names remain
/// unchecked because a lexical scan cannot evaluate them.
pub fn bad_thread_name_literals(code: &str) -> Vec<String> {
    let mut code = crate::test_support::ratchet::code_only(code);
    if let Some(test_module) = inline_test_module_start(&code) {
        code.truncate(test_module);
    }

    Regex::new(
        r#"(?:\bthread::Builder::new\(\)\s*\.name|\.thread_name|\bspawn_named|\btry_spawn_named)\s*\(\s*(?:"([^"\\]*)"|&?\s*format!\s*\(\s*"([^"\\]*)")"#,
    )
    .expect("thread name literal regex is valid")
    .captures_iter(&code)
    .filter_map(|captures| {
        captures
            .get(1)
            .or_else(|| captures.get(2))
            .map(|name| name.as_str())
    })
    .filter(|name| {
        let prefix = name.split_once('{').map_or(*name, |(prefix, _)| prefix);
        prefix.len() > 15 || !prefix.starts_with("thegn-")
    })
    .map(str::to_owned)
    .collect()
}

fn inline_test_module_start(code: &str) -> Option<usize> {
    let header = Regex::new(
        r"(?m)((?:^[ \t]*#[^\n]*\n)+)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{",
    )
    .expect("inline module header regex is valid");
    let test_cfg = Regex::new(r"#\[cfg\((?:test|all\([^)]*\btest\b[^)]*\))\)\]")
        .expect("test cfg regex is valid");

    header.captures_iter(code).find_map(|captures| {
        let attributes = captures.get(1)?.as_str();
        if test_cfg.is_match(attributes) {
            captures.get(0).map(|capture| capture.start())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        bad_thread_name_literals, builds_thread_directly, is_test_module_file,
        spawns_unnamed_thread,
    };

    #[test]
    fn builds_thread_directly_finds_production_builders() {
        let cases = [
            ("std::thread::Builder::new().spawn(f);", true),
            (
                "#[cfg(test)]\nmod tests { std::thread::Builder::new(); }",
                false,
            ),
            ("// thread::Builder::new()", false),
            ("spawn_named(\"thegn-worker\", f);", false),
            ("tokio::runtime::Builder::new_multi_thread();", false),
        ];

        for (source, expected) in cases {
            assert_eq!(builds_thread_directly(source), expected, "{source}");
        }
    }

    #[test]
    fn test_module_files_are_recognized_by_basename() {
        let cases = [
            ("db_tests.rs", true),
            ("plugin/session/tests.rs", true),
            ("issue/gh_run/unix_tests.rs", true),
            ("tests_util.rs", false),
            ("run.rs", false),
        ];

        for (key, expected) in cases {
            assert_eq!(is_test_module_file(key), expected, "{key}");
        }
    }

    #[test]
    fn predicate_distinguishes_bare_and_named_spawn_calls() {
        let cases = [
            ("std::thread::spawn(|| {});", true),
            ("use std::thread; thread::spawn (|| {});", true),
            ("thread::Builder::new().name(\"x\").spawn(|| {});", false),
            ("spawn_named(\"thegn-x\", || {});", false),
            ("named_thread::spawn(|| {});", false),
            (
                "tokio::spawn(async {}); tokio::task::spawn_blocking(|| {});",
                false,
            ),
            ("#[cfg(test)]\nmod tests { thread::spawn(|| {}); }", false),
            (
                "#[cfg(all(test, unix))]\n#[allow(dead_code)]\nmod t { thread::spawn(|| {}); }",
                false,
            ),
            ("#[cfg(not(test))] mod real { thread::spawn(|| {}); }", true),
            ("#[cfg(test)] mod tests;\nthread::spawn(|| {});", true),
            ("// thread::spawn(|| {});", false),
        ];

        for (source, expected) in cases {
            assert_eq!(spawns_unnamed_thread(source), expected, "{source}");
        }
    }

    #[test]
    fn bad_thread_name_literals_cover_builder_and_spawn_named_shapes() {
        let cases: [(&str, &[&str]); 14] = [
            (
                r#"std::thread::Builder::new().name("thegn-plugin-once".into()).spawn(f);"#,
                &["thegn-plugin-once"],
            ),
            (
                "std::thread::Builder::new()\n    .name(\"bridge-fs-flush\".into())\n    .spawn(f);",
                &["bridge-fs-flush"],
            ),
            (
                r#"thegn_core::thread_name::spawn_named("thegn-plug-once", f);"#,
                &[],
            ),
            (
                r#"try_spawn_named("thegn-123456789a", f);"#,
                &["thegn-123456789a"],
            ),
            (r#"spawn_named("thegn-12345678a", f);"#, &[]),
            (
                r#"tokio::runtime::Builder::new_multi_thread().thread_name("thegn-rt");"#,
                &[],
            ),
            (
                r#".thread_name("tokio-runtime-worker")"#,
                &["tokio-runtime-worker"],
            ),
            (
                r#"std::thread::Builder::new().name(format!("mcp-up-{n}"))"#,
                &["mcp-up-{n}"],
            ),
            (r#"spawn_named(format!("thegn-x-{n}"), f)"#, &[]),
            (r#"spawn_named(format!("thegn-mcp-{n}"), f)"#, &[]),
            (
                r#"try_spawn_named(&format!("thegn-a-very-long-{n}"), f)"#,
                &["thegn-a-very-long-{n}"],
            ),
            (
                r#"clap::Arg::new("x").name("a-very-long-argument-name")"#,
                &[],
            ),
            (r#"// std::thread::Builder::new().name("bad-name")"#, &[]),
            (
                r##"#[cfg(test)]
mod tests { fn t() { std::thread::Builder::new().name("private-late-resource"); } }"##,
                &[],
            ),
        ];

        for (source, expected) in cases {
            let names: Vec<String> = bad_thread_name_literals(source);
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            assert_eq!(names, expected, "{source}");
        }
    }
}
