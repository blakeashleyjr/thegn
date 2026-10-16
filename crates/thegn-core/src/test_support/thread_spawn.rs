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

/// Return whether a source file is a whole-file test module excluded from
/// production thread scans.
pub fn is_test_module_file(key: &str) -> bool {
    let file_name = key.rsplit('/').next().unwrap_or(key);
    file_name == "tests.rs" || file_name.ends_with("_tests.rs")
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
    use super::{is_test_module_file, spawns_unnamed_thread};

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
}
