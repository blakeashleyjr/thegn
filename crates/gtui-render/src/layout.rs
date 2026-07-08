//! Dashboard grid → terminal `Rect` layout.
//!
//! Panels place onto a 24-column Grafana-style grid (`GridPos` in grid cells).
//! We scale both axes proportionally so the dashboard fills the available tile
//! area: column width = `area.width / 24`, row height = `area.height /
//! total_grid_rows`. Every rect is clamped to `area` so an over-tall dashboard
//! degrades gracefully instead of drawing out of bounds.

use gtui_core::dashboard::{GridPos, Panel};
use ratatui::layout::Rect;

pub const GRID_COLS: u32 = 24;

/// Total grid rows a dashboard occupies (max `y + h`), min 1.
pub fn grid_rows(panels: &[Panel]) -> u32 {
    panels
        .iter()
        .map(|p| p.grid_pos.y + p.grid_pos.h)
        .max()
        .unwrap_or(1)
        .max(1)
}

/// Map one `GridPos` to a terminal `Rect` inside `area`, given the dashboard's
/// total grid-row span. Always within `area` bounds.
pub fn panel_rect(area: Rect, gp: &GridPos, total_rows: u32) -> Rect {
    let col_w = area.width as f32 / GRID_COLS as f32;
    let row_h = area.height as f32 / total_rows.max(1) as f32;

    let x = area
        .x
        .saturating_add((gp.x as f32 * col_w).round() as u16)
        .min(area.right());
    let y = area
        .y
        .saturating_add((gp.y as f32 * row_h).round() as u16)
        .min(area.bottom());
    let w = ((gp.w as f32 * col_w).round() as u16).min(area.right().saturating_sub(x));
    let h = ((gp.h as f32 * row_h).round() as u16).min(area.bottom().saturating_sub(y));
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtui_core::dashboard::{GridPos, Panel, Target};

    fn panel(x: u32, y: u32, w: u32, h: u32) -> Panel {
        Panel {
            id: 1,
            title: "p".into(),
            panel_type: "stat".into(),
            datasource: "host".into(),
            grid_pos: GridPos { x, y, w, h },
            targets: vec![Target {
                ref_id: "A".into(),
                expr: "x".into(),
            }],
        }
    }

    #[test]
    fn full_width_spans_the_area() {
        let area = Rect::new(0, 0, 48, 16);
        let r = panel_rect(
            area,
            &GridPos {
                x: 0,
                y: 0,
                w: 24,
                h: 16,
            },
            16,
        );
        assert_eq!(r.x, 0);
        assert_eq!(r.width, 48);
        assert_eq!(r.height, 16);
    }

    #[test]
    fn half_width_is_roughly_half() {
        let area = Rect::new(0, 0, 48, 16);
        let left = panel_rect(
            area,
            &GridPos {
                x: 0,
                y: 0,
                w: 12,
                h: 8,
            },
            16,
        );
        let right = panel_rect(
            area,
            &GridPos {
                x: 12,
                y: 0,
                w: 12,
                h: 8,
            },
            16,
        );
        assert_eq!(left.width, 24);
        assert_eq!(right.x, 24);
        assert_eq!(right.width, 24);
    }

    #[test]
    fn overflowing_position_clamps_to_zero_size_not_panic() {
        let area = Rect::new(0, 0, 48, 16);
        // y far past the grid ⇒ y clamps to the bottom, height 0.
        let r = panel_rect(
            area,
            &GridPos {
                x: 0,
                y: 100,
                w: 24,
                h: 8,
            },
            16,
        );
        assert!(r.y <= area.bottom());
        assert_eq!(r.height, 0);
    }

    #[test]
    fn zero_area_yields_zero_rect() {
        let area = Rect::new(0, 0, 0, 0);
        let r = panel_rect(
            area,
            &GridPos {
                x: 0,
                y: 0,
                w: 24,
                h: 8,
            },
            16,
        );
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
    }

    #[test]
    fn grid_rows_is_max_y_plus_h() {
        let panels = vec![panel(0, 0, 12, 8), panel(0, 8, 24, 6)];
        assert_eq!(grid_rows(&panels), 14);
        assert_eq!(grid_rows(&[]), 1);
    }
}
