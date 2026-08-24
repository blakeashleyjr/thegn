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
//! Both allowlists are shrink-only (see `thegn_core::test_support::ratchet`):
//! existing debt is frozen, new debt is impossible. Removing an entry is always
//! welcome; a new entry needs a reason in the file.

use thegn_core::test_support::ratchet::file_ratchet;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn every_floating_box_registers_a_caret_cover() {
    file_ratchet(
        MANIFEST,
        "caret-cover-ratchet.txt",
        &[],
        |_, body| body.contains("draw_card("),
        "These files draw a card outside `layer::open_layer`, so whatever they \
         paint never registers a caret cover and the focused pane's cursor will \
         blink on top of it. Draw the box with `layer::open_layer` (it covers for \
         you), or call `caret::cover(rect)` explicitly.",
    );
}

#[test]
fn every_caret_glyph_claims_the_real_cursor() {
    // The bars a text field uses for its caret. `█` also draws graphs and the
    // splash wordmark, hence the allowlist rather than a blanket ban.
    const GLYPHS: [char; 2] = ['\u{258f}', '\u{2588}'];
    file_ratchet(
        MANIFEST,
        "caret-glyph-ratchet.txt",
        // seg.rs defines the sanctioned constructor.
        &["seg.rs"],
        |_, body| GLYPHS.iter().any(|g| body.contains(*g)),
        "These files write a caret glyph by hand. A hand-drawn bar is only a \
         picture of a caret — the real terminal cursor stays parked in the pane \
         behind the popup. Use `seg::caret()` (or `.into_caret()` to keep a \
         different glyph), which claims the cursor as the line is laid out. If \
         the glyph is not a caret (a graph bar, the wordmark), pin the file.",
    );
}
