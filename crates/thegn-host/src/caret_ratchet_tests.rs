//! The caret ratchet: the two ways a popup can silently reintroduce "the pane's
//! cursor blinks on top of me".
//!
//! Both halves of [`crate::caret`] work by being recorded at the point of
//! painting, so they only hold as long as popups keep painting through the
//! shared entry points. These tests fail the build when something starts
//! hand-rolling instead:
//!
//! 1. **Covers** — a floating box must come from `layer::open_layer`, which
//!    registers the cover. Drawing a card directly bypasses that, so
//!    `borders::draw_card` has a pinned caller allowlist
//!    (`test/caret-cover-ratchet.txt`).
//! 2. **Claims** — a text field's caret must be `seg::caret()` /
//!    `Seg::into_caret()`, which claims the real cursor. A hand-written glyph
//!    renders a bar with no cursor behind it, so caret glyph literals have a
//!    pinned allowlist too (`test/caret-glyph-ratchet.txt`).
//!
//! Both allowlists are shrink-only, like `test/help-ratchet.txt`: existing debt
//! is frozen, new debt is impossible. Removing an entry is always welcome; a new
//! entry needs a reason in the file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn allowlist(name: &str) -> BTreeSet<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test")
        .join(name);
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Every `.rs` file under the crate's `src/`, as (repo-relative-ish key, body).
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = src_dir();
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let key = p.strip_prefix(&root).ok()?.to_string_lossy().to_string();
            // This file names both patterns in its own assertion messages.
            if key == "caret_ratchet_tests.rs" {
                return None;
            }
            let body = std::fs::read_to_string(&p).ok()?;
            Some((key, body))
        })
        .collect()
}

/// Strip `//`-comments so prose mentioning a glyph or an API doesn't trip the
/// scan. Crude but sufficient: these are line comments in normal Rust source.
fn code_only(body: &str) -> String {
    body.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_floating_box_registers_a_caret_cover() {
    let allow = allowlist("caret-cover-ratchet.txt");
    let found: BTreeSet<String> = sources()
        .into_iter()
        .filter(|(_, body)| code_only(body).contains("draw_card("))
        .map(|(key, _)| key)
        .collect();

    let unpinned: Vec<&String> = found.difference(&allow).collect();
    assert!(
        unpinned.is_empty(),
        "these files draw a card outside `layer::open_layer`, so whatever they \
         paint never registers a caret cover and the focused pane's cursor will \
         blink on top of it: {unpinned:?}\n\
         Draw the box with `layer::open_layer` (it covers for you), or call \
         `caret::cover(rect)` explicitly and pin the file in \
         test/caret-cover-ratchet.txt with a note saying why."
    );

    let stale: Vec<&String> = allow.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "test/caret-cover-ratchet.txt pins files that no longer draw a card — \
         the list is shrink-only, so delete these entries: {stale:?}"
    );
}

#[test]
fn every_caret_glyph_claims_the_real_cursor() {
    // The bars a text field uses for its caret. `█` also draws graphs and the
    // splash wordmark, hence the allowlist rather than a blanket ban.
    const GLYPHS: [char; 2] = ['\u{258f}', '\u{2588}'];
    let allow = allowlist("caret-glyph-ratchet.txt");
    let found: BTreeSet<String> = sources()
        .into_iter()
        .filter(|(key, body)| {
            // seg.rs defines the sanctioned constructor.
            key != "seg.rs" && GLYPHS.iter().any(|g| code_only(body).contains(*g))
        })
        .map(|(key, _)| key)
        .collect();

    let unpinned: Vec<&String> = found.difference(&allow).collect();
    assert!(
        unpinned.is_empty(),
        "these files write a caret glyph by hand: {unpinned:?}\n\
         A hand-drawn bar is only a picture of a caret — the real terminal \
         cursor stays parked in the pane behind the popup. Use \
         `seg::caret()` (or `.into_caret()` to keep a different glyph), which \
         claims the cursor as the line is laid out. If the glyph is not a \
         caret (a graph bar, the wordmark), pin the file in \
         test/caret-glyph-ratchet.txt."
    );

    let stale: Vec<&String> = allow.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "test/caret-glyph-ratchet.txt pins files with no caret glyph left — \
         the list is shrink-only, so delete these entries: {stale:?}"
    );
}
