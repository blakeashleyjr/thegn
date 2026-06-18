//! Pure vertical-budget math for the accordion panel: how many rows the open
//! section's content gets, whether the airy spacing survives, and the
//! degradation ladder for short panels.
//!
//! Fixed rows = header + the numbered section rows + the blanks around the
//! open content; the open section's content gets the remainder, truncating to
//! an "… +N more · e expand" row on overflow.

/// Blank rows wrapped around the open section's content.
const OPEN_PADDING: usize = 2;

/// The resolved allocation for one panel height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Header rows to render (0..=requested; shrinks under pressure but the
    /// branch row survives longest).
    pub header_rows: usize,
    /// Content rows granted to the open section (0 = closed rows only).
    pub content_rows: usize,
    /// `Some(hidden)` when content overflows: the last granted row becomes
    /// the "… +hidden more" affordance.
    pub overflow: Option<usize>,
    /// Blank row after each closed section (only when there's room).
    pub airy: bool,
}

/// Allocate `rows` of panel height across header (`header_rows` requested),
/// `sections` one-line section rows, and the open section's `content_len`
/// rows.
pub fn allocate(rows: usize, header_rows: usize, content_len: usize, sections: usize) -> Plan {
    let mut header = header_rows;

    // The skeleton everything else must fit around.
    let fixed = |header: usize| header + sections + OPEN_PADDING;

    // Degradation ladder: shed header detail rows (keep one branch row) until
    // the skeleton fits.
    while header > 1 && fixed(header) > rows {
        header -= 1;
    }
    if fixed(header) > rows {
        // Even the skeleton doesn't fit: sections only, top-aligned.
        let header = header.min(rows.saturating_sub(sections).min(1));
        return Plan {
            header_rows: header,
            content_rows: 0,
            overflow: None,
            airy: false,
        };
    }

    let budget = rows - fixed(header);
    let (content_rows, overflow) = if content_len <= budget {
        (content_len, None)
    } else if budget == 0 {
        (0, None)
    } else {
        // budget-1 real rows + the "… +N more" row.
        (budget, Some(content_len - (budget - 1)))
    };
    // Breathing room after closed sections only when the leftover could hold
    // one blank per closed section.
    let airy = rows - (fixed(header) + content_rows) >= sections;
    Plan {
        header_rows: header,
        content_rows,
        overflow,
        airy,
    }
}

/// The resolved allocation for the full-width view: header, the horizontal
/// section rail, an optional rule + blank seam, and the body filling the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullPlan {
    pub header_rows: usize,
    pub rail_rows: usize,
    /// Whether the rule + blank seam between rail and body survives.
    pub seam: bool,
    /// Exact rows granted to the open section's body.
    pub body_rows: usize,
}

/// Allocate `rows` for the full view: header (`header_rows` requested) +
/// rail (`rail_rows` requested) + a 2-row seam (rule + blank), body = rest.
/// Degradation: shed header detail (keep the branch row), then the seam,
/// then rail rows down to one.
pub fn allocate_full(rows: usize, header_rows: usize, rail_rows: usize) -> FullPlan {
    let mut header = header_rows;
    let mut rail = rail_rows.max(1);
    let mut seam = true;
    let fixed = |h: usize, r: usize, s: bool| h + r + if s { 2 } else { 0 };
    while header > 1 && fixed(header, rail, seam) >= rows {
        header -= 1;
    }
    if seam && fixed(header, rail, seam) >= rows {
        seam = false;
    }
    while rail > 1 && fixed(header, rail, seam) >= rows {
        rail -= 1;
    }
    if fixed(header, rail, seam) >= rows {
        // Tiny: hand whatever exists to header→rail in that order.
        let header = header.min(rows.saturating_sub(1));
        let rail = rail.min(rows.saturating_sub(header));
        return FullPlan {
            header_rows: header,
            rail_rows: rail,
            seam: false,
            body_rows: 0,
        };
    }
    FullPlan {
        header_rows: header,
        rail_rows: rail,
        seam,
        body_rows: rows - fixed(header, rail, seam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in section count (the live order can be shorter via config).
    const SECTIONS: usize = super::super::SECTION_ORDER.len();

    /// Total rows a plan actually consumes (sanity for every case).
    fn consumed(p: &Plan) -> usize {
        p.header_rows
            + SECTIONS
            + OPEN_PADDING
            + p.content_rows
            + if p.airy { SECTIONS - 1 } else { 0 }
    }

    #[test]
    fn tall_panel_grants_all_content_with_air() {
        let p = allocate(50, 4, 12, SECTIONS);
        assert_eq!(p.header_rows, 4);
        assert_eq!(p.content_rows, 12);
        assert_eq!(p.overflow, None);
        assert!(p.airy);
        assert!(consumed(&p) <= 50);
    }

    #[test]
    fn overflow_reserves_the_more_row() {
        // fixed = 4 + 14 + 2 = 20; rows 24 → budget 4; content 20 → 4 rows
        // granted (3 real + the "+N more" row), hidden = 20 - 3 = 17.
        let p = allocate(24, 4, 20, SECTIONS);
        assert_eq!(p.content_rows, 4);
        assert_eq!(p.overflow, Some(17));
        assert!(!p.airy);
    }

    #[test]
    fn exact_fit_has_no_overflow() {
        let p = allocate(28, 4, 6, SECTIONS);
        assert_eq!(p.content_rows, 6);
        assert_eq!(p.overflow, None);
    }

    #[test]
    fn short_panel_sheds_header_detail_but_keeps_branch_row() {
        // rows 16: fixed(1)=17 > 16 → degenerate path, header=min(1,2)=1, zero
        // content budget.
        let p = allocate(16, 4, 10, SECTIONS);
        assert_eq!(p.header_rows, 1);
        assert_eq!(p.content_rows, 0);
        assert_eq!(p.overflow, None);
    }

    #[test]
    fn tiny_panel_degrades_to_sections_only() {
        let p = allocate(9, 4, 10, SECTIONS);
        assert_eq!(p.content_rows, 0);
        assert!(p.header_rows <= 1);
        let p = allocate(0, 4, 10, SECTIONS);
        assert_eq!(p.content_rows, 0);
        assert_eq!(p.header_rows, 0);
    }

    #[test]
    fn zero_content_never_overflows() {
        let p = allocate(40, 4, 0, SECTIONS);
        assert_eq!(p.content_rows, 0);
        assert_eq!(p.overflow, None);
    }

    #[test]
    fn budget_of_zero_with_content_shows_nothing_not_a_bare_more_row() {
        // rows exactly the skeleton: no content row at all (a lone "+N more"
        // row without context would be noise).
        let rows = 4 + SECTIONS + 2; // 19
        let p = allocate(rows, 4, 9, SECTIONS);
        assert_eq!(p.content_rows, 0);
        assert_eq!(p.overflow, None);
    }

    #[test]
    fn trimmed_orders_free_rows_for_content() {
        // Hiding sections (config) hands their rows to the open content:
        // 3 sections fixed = 4 + 3 + 2 = 9; rows 20 → budget 11.
        let p = allocate(20, 4, 11, 3);
        assert_eq!(p.content_rows, 11);
        assert_eq!(p.overflow, None);
    }

    #[test]
    fn full_plan_fills_the_body_and_degrades_in_order() {
        // Roomy: header 4 + rail 1 + seam 2 → body 33 of 40.
        let p = allocate_full(40, 4, 1);
        assert_eq!(
            p,
            FullPlan {
                header_rows: 4,
                rail_rows: 1,
                seam: true,
                body_rows: 33
            }
        );
        // Pressure sheds header detail first (branch row survives)…
        let p = allocate_full(6, 4, 2);
        assert_eq!(p.header_rows, 1);
        assert!(p.seam);
        assert!(p.body_rows > 0);
        // …then the seam, then rail rows down to one.
        let p = allocate_full(3, 4, 2);
        assert!(!p.seam);
        assert_eq!(p.rail_rows, 1);
        assert_eq!(p.body_rows, 1);
        // Tiny heights never go negative or overflow.
        for rows in [0usize, 1, 2] {
            let p = allocate_full(rows, 4, 2);
            assert!(
                p.header_rows + p.rail_rows + if p.seam { 2 } else { 0 } + p.body_rows <= rows,
                "rows={rows} {p:?}"
            );
        }
    }
}
