//! Render-loop emission of the first-party preview image (AF 775 graphics path).
//!
//! The preview fetch ([`crate::preview_pane`]) rasterizes an image / Mermaid /
//! PDF page off the loop and hands the pixels here via [`PreviewGfx::set`]. The
//! event loop then calls [`PreviewGfx::frame`] right after the cell frame is
//! flushed (the same spot the corner relay emits) to draw the image over the
//! panel rect with the kitty protocol, or to delete it when the preview is
//! dismissed, occluded by an overlay, or scrolled/resized away. Images bypass
//! the cell diff entirely, so this never forces a chrome recompose. Only the
//! byte plumbing is unit-tested; the pixels need a live kitty terminal.

use crate::compositor::Rect;
use crate::graphics::{self, Placement};
use crate::rasterize::Raster;

/// Holds the current preview raster and whether/where it is drawn on the outer
/// terminal, so the loop emits it once and deletes it exactly when it should
/// disappear.
#[derive(Default)]
pub struct PreviewGfx {
    /// The rasterized image and the preview path it belongs to.
    img: Option<(String, Raster)>,
    /// The placement it is currently drawn at, or `None` when nothing is drawn.
    drawn: Option<Placement>,
}

impl PreviewGfx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a freshly rasterized preview image for `rel`, forcing a redraw on
    /// the next [`frame`](Self::frame). Dismissal needs no explicit call: once
    /// the open preview path no longer matches (or the preview closes, so
    /// `open_path` is `None`), [`frame`](Self::frame) deletes the stale image.
    pub fn set(&mut self, rel: String, raster: Raster) {
        self.img = Some((rel, raster));
    }

    /// Bytes to write to the outer terminal this frame: draw the image once at
    /// the panel rect when it should be visible, delete it when it should not
    /// (dismissed, occluded, path changed, panel gone, or placement changed),
    /// or nothing when already correct.
    pub fn frame(
        &mut self,
        panel: Option<Rect>,
        open_path: Option<&str>,
        occluded: bool,
        kitty: bool,
    ) -> Vec<u8> {
        let want: Option<(Placement, &Raster)> = if !kitty || occluded {
            None
        } else {
            match (&self.img, panel, open_path) {
                (Some((rel, raster)), Some(rect), Some(open)) if rel == open => {
                    Some((placement(rect), raster))
                }
                _ => None,
            }
        };
        match want {
            Some((place, raster)) if self.drawn != Some(place) => {
                // (Re)draw: delete any stale image first, then transmit + display.
                let mut out = Vec::new();
                if self.drawn.is_some() {
                    out.extend_from_slice(graphics::delete_all());
                }
                out.extend_from_slice(&graphics::kitty_image(
                    &raster.rgba,
                    raster.w,
                    raster.h,
                    place,
                ));
                self.drawn = Some(place);
                out
            }
            Some(_) => Vec::new(), // already drawn at this placement
            None if self.drawn.is_some() => {
                self.drawn = None;
                graphics::delete_all().to_vec()
            }
            None => Vec::new(),
        }
    }
}

/// Placement inside the panel rect: inset for the panel border and the caption
/// row, scaling the raster to the remaining cell area.
fn placement(panel: Rect) -> Placement {
    Placement {
        origin_col: panel.x.saturating_add(1) as u16,
        origin_row: panel.y.saturating_add(2) as u16, // border + caption line
        cols: panel.cols.saturating_sub(2).max(1) as u16,
        rows: panel.rows.saturating_sub(3).max(1) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster() -> Raster {
        Raster {
            rgba: vec![1, 2, 3, 4],
            w: 1,
            h: 1,
        }
    }

    fn panel() -> Rect {
        Rect {
            x: 50,
            y: 3,
            cols: 30,
            rows: 20,
        }
    }

    #[test]
    fn nothing_drawn_when_no_image() {
        let mut g = PreviewGfx::new();
        assert!(
            g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
    }

    #[test]
    fn draws_once_then_stays_quiet() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let first = g.frame(Some(panel()), Some("a.png"), false, true);
        assert!(!first.is_empty(), "first frame transmits the image");
        assert!(
            first.windows(3).any(|w| w == b"\x1b_G"),
            "carries a kitty APC"
        );
        // Steady state: same placement → no repeated emission.
        assert!(
            g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
    }

    #[test]
    fn deletes_on_dismiss() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let _ = g.frame(Some(panel()), Some("a.png"), false, true);
        // Preview closed → no open path → the image is deleted next frame.
        assert_eq!(
            g.frame(Some(panel()), None, false, true),
            graphics::delete_all().to_vec()
        );
    }

    #[test]
    fn deletes_when_occluded_then_redraws_when_clear() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let _ = g.frame(Some(panel()), Some("a.png"), false, true);
        // Overlay comes up → delete.
        assert_eq!(
            g.frame(Some(panel()), Some("a.png"), true, true),
            graphics::delete_all().to_vec()
        );
        // Overlay clears → redraw.
        assert!(
            !g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
    }

    #[test]
    fn deletes_when_preview_path_changes() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let _ = g.frame(Some(panel()), Some("a.png"), false, true);
        // A different file is open now → the stale image is deleted.
        assert_eq!(
            g.frame(Some(panel()), Some("b.rs"), false, true),
            graphics::delete_all().to_vec()
        );
    }

    #[test]
    fn no_graphics_terminal_never_emits() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        assert!(
            g.frame(Some(panel()), Some("a.png"), false, false)
                .is_empty()
        );
    }
}
