//! Positive controls for the current path-derived worktree tag identity rules.
//! These tests pin invalid-byte, separator-alias, and lexical `..` behavior.
#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use thegn_core::log_trace::wt_slug;

#[test]
fn wt_tag_binds_invalid_utf8_names_by_their_bytes() {
    let a = PathBuf::from(OsStr::from_bytes(b"/repos/a/wt-\xff"));
    let b = PathBuf::from(OsStr::from_bytes(b"/repos/a/wt-\xfe"));
    let a_tag = wt_slug(&a);
    let b_tag = wt_slug(&b);

    assert_ne!(
        a_tag, b_tag,
        "distinct native path bytes need distinct tags"
    );
    assert_eq!(
        a_tag.split('.').next(),
        b_tag.split('.').next(),
        "invalid bytes share the same lossy display label"
    );
    assert_eq!(a_tag, wt_slug(&a.clone()));

    for tag in [&a_tag, &b_tag] {
        assert!(tag.is_ascii());
        assert!(!tag.chars().any(char::is_whitespace));
        assert!(!tag.contains('='));
        assert!(!tag.ends_with(".unbound"));
        assert!(tag.len() <= 32 + 1 + 64);
    }
}

#[test]
fn wt_tag_treats_separator_and_dot_spellings_as_one_worktree() {
    let canonical = wt_slug(Path::new("/repos/a/wt"));

    assert_eq!(canonical, wt_slug(Path::new("/repos/a/./wt")));
    assert_eq!(canonical, wt_slug(Path::new("/repos/a//wt")));
    assert_eq!(canonical, wt_slug(Path::new("/repos/a/wt/")));
    assert_eq!(canonical, wt_slug(Path::new("/repos/a/./wt/./")));
}

#[test]
fn wt_tag_never_collapses_dotdot_lexically() {
    assert_ne!(
        wt_slug(Path::new("/repos/a/x/../wt")),
        wt_slug(Path::new("/repos/a/wt")),
        "`..` is symlink-dependent, so lexical collapse could alias different checkouts"
    );
    assert_ne!(
        wt_slug(Path::new("/repos/a/wt")),
        wt_slug(Path::new("/repos/b/wt")),
        "same-basename worktrees in different repositories need distinct tags"
    );
}
