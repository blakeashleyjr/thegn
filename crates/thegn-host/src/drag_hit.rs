//! Pure geometry for the compositor's mouse-drag **grab bands** — shared by the
//! sidebar-width and panel-width separator drags. Columns in, columns out: no
//! `Rect`, no model, no I/O, so every rule here is unit tested at the module.
//!
//! A separator is painted on exactly one column ([`crate::layout`]), which is
//! too fine a target for cell-quantized mouse reports. The band widens it to two
//! columns, and it always takes its extra cell from the **center** column's
//! outer frame cell — the second vertical rule the user already sees at that
//! boundary — never from the sidebar row or panel row beside it, which are live
//! click targets across their full width. A widened band must not cost a click
//! (design §3.1).

/// Which separator a grab band belongs to — the band always takes its extra
/// cell from the CENTER column's outer frame cell, never from the list beside
/// it (a sidebar row / panel row is a live click target across its full width).
///
/// `expect(dead_code)` is the wiring gate: chunk 2's `run.rs` is the non-test
/// caller — once it lands, the expectation stops being fulfilled and clippy
/// `-D warnings` forces this attribute off (the repo's transitional pattern,
/// cf. `host_provision::plan_summary`).
#[cfg_attr(not(test), expect(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SepSide {
    /// The sidebar|center separator: band is `{sep, sep + 1}`.
    Sidebar,
    /// The center|panel separator: band is `{sep - 1, sep}`.
    Panel,
}

/// Whether pointer column `mx` grabs the separator at `sep`.
#[cfg_attr(not(test), expect(dead_code))]
pub fn sep_grab(sep: Option<usize>, side: SepSide, mx: usize) -> bool {
    let Some(sep) = sep else {
        return false;
    };
    match side {
        SepSide::Sidebar => mx == sep || mx == sep + 1,
        SepSide::Panel => mx == sep || mx + 1 == sep,
    }
}

/// Whether `mx` is the separator column ITSELF (as opposed to the band's extra
/// furniture cell). Callers gate the extra cell on it not being pane/drawer
/// content; the separator column always grabs.
#[cfg_attr(not(test), expect(dead_code))]
pub fn sep_is_exact(sep: Option<usize>, mx: usize) -> bool {
    sep == Some(mx)
}

/// The separator column implied by pointer column `mx`, for a drag that pressed
/// at `press_x` while the separator sat at `sep`. The press offset is held for
/// the whole drag, so the divider tracks the cursor instead of jumping to it on
/// the first sample. Saturating: never underflows at column 0.
#[cfg_attr(not(test), expect(dead_code))]
pub fn sep_follow(press_x: usize, sep: usize, mx: usize) -> usize {
    if press_x >= sep {
        mx.saturating_sub(press_x - sep)
    } else {
        mx + (sep - press_x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_band_is_the_separator_and_the_cell_right_of_it() {
        assert!(sep_grab(Some(40), SepSide::Sidebar, 40));
        assert!(sep_grab(Some(40), SepSide::Sidebar, 41));
        // The sidebar's own last column stays a live row click.
        assert!(!sep_grab(Some(40), SepSide::Sidebar, 39));
        assert!(!sep_grab(Some(40), SepSide::Sidebar, 42));
    }

    #[test]
    fn panel_band_is_the_separator_and_the_cell_left_of_it() {
        assert!(sep_grab(Some(40), SepSide::Panel, 40));
        assert!(sep_grab(Some(40), SepSide::Panel, 39));
        // The panel's own first column stays a live row click.
        assert!(!sep_grab(Some(40), SepSide::Panel, 41));
        assert!(!sep_grab(Some(40), SepSide::Panel, 38));
    }

    #[test]
    fn no_separator_never_grabs() {
        for mx in [0, 1, 40, 41] {
            assert!(!sep_grab(None, SepSide::Sidebar, mx));
            assert!(!sep_grab(None, SepSide::Panel, mx));
        }
    }

    #[test]
    fn exact_is_only_the_separator_column() {
        assert!(sep_is_exact(Some(40), 40));
        // The band's extra cell is in the band but is not the separator — the
        // caller gates it on the cell not being pane/drawer content.
        assert!(!sep_is_exact(Some(40), 41));
        assert!(!sep_is_exact(Some(40), 39));
        assert!(!sep_is_exact(None, 40));
    }

    #[test]
    fn one_column_center_puts_a_column_in_both_bands() {
        // A center of exactly one column (`layout.rs:564-565`): sep_left = 40,
        // sep_right = 42, so column 41 is the center's only column and lies in
        // BOTH bands. The overlap is resolved by the caller checking the
        // sidebar first (`run.rs:12623` precedes `:12636`), so the sidebar drag
        // deterministically wins.
        assert!(sep_grab(Some(40), SepSide::Sidebar, 41));
        assert!(sep_grab(Some(42), SepSide::Panel, 41));
    }

    #[test]
    fn follow_holds_the_press_offset_for_the_whole_drag() {
        // Pressed on the separator: the divider is the cursor.
        assert_eq!(sep_follow(40, 40, 55), 55);
        // Pressed one cell right of it: the divider trails by one.
        assert_eq!(sep_follow(41, 40, 55), 54);
        // Pressed one cell left of it: the divider leads by one.
        assert_eq!(sep_follow(39, 40, 55), 56);
    }

    #[test]
    fn follow_saturates_at_column_zero() {
        assert_eq!(sep_follow(41, 40, 0), 0);
        assert_eq!(sep_follow(45, 40, 3), 0);
        assert_eq!(sep_follow(45, 40, 5), 0);
        assert_eq!(sep_follow(45, 40, 6), 1);
    }
}
