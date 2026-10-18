use crate::compositor::Rect;

/// Returns the right-aligned portion of `area` needed for `len` samples.
/// Braille cells hold two samples; other raster tables hold one per cell.
pub(crate) fn graph_rect(area: Rect, len: usize, braille: bool) -> Rect {
    let used = if braille { len.div_ceil(2) } else { len };
    let cols = area.cols.min(used);
    Rect {
        x: area.x + area.cols.saturating_sub(cols),
        cols,
        ..area
    }
}

#[cfg(test)]
mod tests {
    use super::graph_rect;
    use crate::compositor::Rect;

    #[test]
    fn graph_rect_right_aligns_clamped_sample_width() {
        let area = Rect {
            x: 7,
            y: 3,
            cols: 5,
            rows: 4,
        };
        for (len, braille, x, cols) in [
            (0, true, 12, 0),
            (1, true, 11, 1),
            (3, true, 10, 2),
            (10, true, 7, 5),
            (3, false, 9, 3),
            (8, false, 7, 5),
        ] {
            assert_eq!(
                graph_rect(area, len, braille),
                Rect { x, cols, ..area },
                "len={len}, braille={braille}"
            );
        }

        assert_eq!(
            graph_rect(
                Rect {
                    x: 2,
                    y: 1,
                    cols: 0,
                    rows: 6,
                },
                9,
                true,
            ),
            Rect {
                x: 2,
                y: 1,
                cols: 0,
                rows: 6,
            }
        );
    }
}
