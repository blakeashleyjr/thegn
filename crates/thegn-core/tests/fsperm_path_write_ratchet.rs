//! Shrink-only ratchet: production callers of the PATH-based `fsperm::write_owner_only_atomic(&Path, ..)` (THE-523).

use thegn_core::test_support::ratchet::{code_only, file_ratchet};

const NAME: &str = "write_owner_only_atomic";
const WHY: &str = "New code must publish owner-only files through a handle-based PrivateDir / RootedDir (`dir.write_owner_only_atomic(name, bytes)` or `publish_file`), not the path-based `fsperm::write_owner_only_atomic(&Path, ..)`, which re-resolves its parent through mutable pathnames (THE-523). This list is shrink-only: migrate the caller and delete the entry.";

/// True when `code` calls the free function, not the handle method or its definition.
fn calls_path_based_atomic_write(code: &str) -> bool {
    code.match_indices(NAME).any(|(at, _)| {
        let before = &code[..at];
        let after = code[at + NAME.len()..].trim_start();
        let ident_tail = before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let trimmed = before.trim_end();
        let is_method = trimmed.ends_with('.');
        let is_definition = trimmed
            .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            == Some("fn");
        after.starts_with('(') && !ident_tail && !is_method && !is_definition
    })
}

#[test]
fn fsperm_path_write_ratchet_pins_core() {
    file_ratchet(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../thegn-core"),
        "fsperm-path-write-core-ratchet.txt",
        &["fsperm.rs"],
        |_, body| calls_path_based_atomic_write(body),
        WHY,
    );
}

#[test]
fn fsperm_path_write_ratchet_pins_host() {
    file_ratchet(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../thegn-host"),
        "fsperm-path-write-host-ratchet.txt",
        &[],
        |_, body| calls_path_based_atomic_write(body),
        WHY,
    );
}

#[test]
fn fsperm_path_write_ratchet_pins_svc() {
    file_ratchet(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../thegn-svc"),
        "fsperm-path-write-svc-ratchet.txt",
        &[],
        |_, body| calls_path_based_atomic_write(body),
        WHY,
    );
}

#[test]
fn fsperm_path_write_ratchet_predicate_flags_only_path_based_calls() {
    for call in [
        "thegn_core::fsperm::write_owner_only_atomic(&path, &bytes)?;",
        "crate::fsperm::write_owner_only_atomic(\n    &path,\n    b)",
        "write_owner_only_atomic (path, bytes)",
    ] {
        assert!(
            calls_path_based_atomic_write(call),
            "expected path call: {call}"
        );
    }

    for non_call in [
        "dir.write_owner_only_atomic(name, bytes)",
        "private\n        .write_owner_only_atomic(&name, b)",
        "pub fn write_owner_only_atomic(path: &Path, bytes: &[u8])",
        "my_write_owner_only_atomic(x)",
        "use thegn_core::fsperm::write_owner_only_atomic;",
    ] {
        assert!(
            !calls_path_based_atomic_write(non_call),
            "unexpected path call: {non_call}"
        );
    }

    let comment = code_only("// fsperm::write_owner_only_atomic(&p, b)\nlet x = 1;");
    assert!(
        !calls_path_based_atomic_write(&comment),
        "comment-only mention must not be a call"
    );
}
