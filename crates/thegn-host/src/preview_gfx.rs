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
    /// Content revision of `img`, bumped by every [`set`](PreviewGfx::set) so a
    /// replacement at the same path and placement still redraws.
    rev: u64,
    /// Revision whose invalid raster has already been logged.
    invalid_logged_rev: Option<u64>,
    /// The placement and content revision currently drawn, or `None` when
    /// nothing is drawn.
    drawn: Option<(Placement, u64)>,
    /// Number of image encodes, exposed only for retry regression tests.
    #[cfg(test)]
    encodes: usize,
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
        self.rev = self.rev.wrapping_add(1);
    }

    /// Forget the terminal placement after a full screen clear. The clear
    /// already erased the image, so the next frame must transmit it again.
    pub fn invalidate(&mut self) {
        self.drawn = None;
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
            Some((place, raster)) if self.drawn != Some((place, self.rev)) => {
                // (Re)draw: encode first, and commit `drawn` only once there are
                // real bytes to emit. An invalid raster draws nothing.
                #[cfg(test)]
                {
                    self.encodes += 1;
                }
                match graphics::kitty_image(&raster.rgba, raster.w, raster.h, place) {
                    Ok(image) => {
                        // Delete any stale image first, then transmit + display.
                        let mut out = Vec::new();
                        if self.drawn.is_some() {
                            out.extend_from_slice(&graphics::delete_preview());
                        }
                        out.extend_from_slice(&image);
                        self.drawn = Some((place, self.rev));
                        out
                    }
                    Err(e) => {
                        if self.invalid_logged_rev != Some(self.rev) {
                            tracing::warn!(
                                target: "thegn::preview",
                                error = ?e,
                                "invalid preview raster"
                            );
                            self.invalid_logged_rev = Some(self.rev);
                        }
                        // Invalid raster: never claim it is drawn; clear any
                        // stale image that is still on screen.
                        if self.drawn.take().is_some() {
                            graphics::delete_preview()
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            Some(_) => Vec::new(), // already drawn at this placement and revision
            None if self.drawn.is_some() => {
                self.drawn = None;
                graphics::delete_preview()
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
    fn draw_then_occlude_emits_only_preview_scoped_graphics() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let mut bytes = g.frame(Some(panel()), Some("a.png"), false, true);
        bytes.extend(g.frame(Some(panel()), Some("a.png"), true, true));

        let wire = String::from_utf8_lossy(&bytes);
        let ids: Vec<u32> = wire
            .match_indices("i=")
            .map(|(start, _)| {
                let digits = wire[start + 2..]
                    .split(|ch: char| !ch.is_ascii_digit())
                    .next()
                    .expect("ID field has a value");
                digits.parse().expect("ID field is numeric")
            })
            .collect();
        assert_eq!(
            ids,
            [graphics::PREVIEW_IMAGE_ID, graphics::PREVIEW_IMAGE_ID]
        );
        assert!(ids.iter().all(|id| {
            !(crate::kitty_relay::CORNER_ID_BASE
                ..crate::kitty_relay::CORNER_ID_BASE + crate::kitty_relay::CORNER_IDS as u32)
                .contains(id)
        }));
        assert!(
            !bytes
                .windows(b"\x1b_Ga=d\x1b\\".len())
                .any(|window| { window == b"\x1b_Ga=d\x1b\\" })
        );
    }

    #[test]
    fn invalid_raster_draws_nothing_and_is_not_marked_drawn() {
        let mut g = PreviewGfx::new();
        g.set(
            "a.png".to_string(),
            Raster {
                rgba: vec![1, 2, 3], // 1x1 needs 4 bytes
                w: 1,
                h: 1,
            },
        );
        assert!(
            g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
        assert!(g.drawn.is_none(), "invalid raster must not set drawn");
        // A valid raster afterwards still draws.
        g.set("a.png".to_string(), raster());
        assert!(
            !g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
    }

    #[test]
    fn invalid_replacement_clears_the_stale_drawn_image() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let _ = g.frame(Some(panel()), Some("a.png"), false, true); // best-effort: test: the frame's effect is the point; the assert below checks it
        g.set(
            "a.png".to_string(),
            Raster {
                rgba: Vec::new(),
                w: 0,
                h: 0,
            },
        );
        assert_eq!(
            g.frame(Some(panel()), Some("a.png"), false, true),
            graphics::delete_preview()
        );
        assert!(g.drawn.is_none());
    }

    #[test]
    fn same_path_same_placement_replacement_redraws() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let first = g.frame(Some(panel()), Some("a.png"), false, true);
        assert!(!first.is_empty());
        // Re-rasterized (file changed on disk) at the same path and placement.
        g.set(
            "a.png".to_string(),
            Raster {
                rgba: vec![9, 9, 9, 9],
                w: 1,
                h: 1,
            },
        );
        let second = g.frame(Some(panel()), Some("a.png"), false, true);
        assert!(
            second.starts_with(&graphics::delete_preview()),
            "stale image deleted"
        );
        assert!(second.len() > graphics::delete_preview().len());
        assert_ne!(first, second[graphics::delete_preview().len()..].to_vec());
        // And steady state is quiet again.
        assert!(
            g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
    }

    #[test]
    fn deletes_on_dismiss() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let _ = g.frame(Some(panel()), Some("a.png"), false, true); // best-effort: test: the frame's effect is the point; the assert below checks it
        // Preview closed → no open path → the image is deleted next frame.
        assert_eq!(
            g.frame(Some(panel()), None, false, true),
            graphics::delete_preview()
        );
    }

    #[test]
    fn deletes_when_occluded_then_redraws_when_clear() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let _ = g.frame(Some(panel()), Some("a.png"), false, true); // best-effort: test: the frame's effect is the point; the assert below checks it
        // Overlay comes up → delete.
        assert_eq!(
            g.frame(Some(panel()), Some("a.png"), true, true),
            graphics::delete_preview()
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
        let _ = g.frame(Some(panel()), Some("a.png"), false, true); // best-effort: test: the frame's effect is the point; the assert below checks it
        // A different file is open now → the stale image is deleted.
        assert_eq!(
            g.frame(Some(panel()), Some("b.rs"), false, true),
            graphics::delete_preview()
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

    #[test]
    fn full_repaint_invalidate_redraws_once_then_quiet() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let first = g.frame(Some(panel()), Some("a.png"), false, true);
        assert!(!first.is_empty());
        assert_eq!(g.encodes, 1);

        g.invalidate();
        let redrawn = g.frame(Some(panel()), Some("a.png"), false, true);
        assert!(!redrawn.is_empty());
        assert_eq!(g.encodes, 2);
        let quiet = g.frame(Some(panel()), Some("a.png"), false, true);
        assert!(quiet.is_empty());
        assert_eq!(g.encodes, 2);
    }

    #[test]
    fn refused_draw_encodes_once_across_retries() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let encoded = g.frame(Some(panel()), Some("a.png"), false, true);
        assert_eq!(g.encodes, 1);

        let mut pending = crate::preview_retry::PendingPreviewOob::default();
        let mut refusals = 5;
        let mut admitted = Vec::new();
        pending.offer(encoded.clone(), |bytes| {
            if refusals > 0 {
                refusals -= 1;
                Err(bytes)
            } else {
                admitted.push(bytes);
                Ok(())
            }
        });
        for _ in 0..5 {
            let repeated = g.frame(Some(panel()), Some("a.png"), false, true);
            assert!(repeated.is_empty());
            pending.offer(repeated, |_| Ok(()));
            pending.retry(|bytes| {
                if refusals > 0 {
                    refusals -= 1;
                    Err(bytes)
                } else {
                    admitted.push(bytes);
                    Ok(())
                }
            });
        }
        assert_eq!(g.encodes, 1);
        assert_eq!(admitted, vec![encoded]);
        assert!(!pending.is_pending());
    }

    #[test]
    fn new_revision_while_refused_replaces_pending_and_encodes_once() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let stale = g.frame(Some(panel()), Some("a.png"), false, true);
        let mut pending = crate::preview_retry::PendingPreviewOob::default();
        pending.offer(stale.clone(), Err);

        g.set(
            "a.png".to_string(),
            Raster {
                rgba: vec![9, 9, 9, 9],
                w: 1,
                h: 1,
            },
        );
        let latest = g.frame(Some(panel()), Some("a.png"), false, true);
        assert_eq!(g.encodes, 2);
        assert!(latest.starts_with(&graphics::delete_preview()));
        let mut admitted = Vec::new();
        pending.offer(latest.clone(), |bytes| {
            admitted.push(bytes);
            Ok(())
        });
        assert_eq!(admitted, vec![latest]);
        assert!(!admitted[0].starts_with(&stale));
    }

    #[test]
    fn hide_while_refused_replaces_draw_with_delete() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let draw = g.frame(Some(panel()), Some("a.png"), false, true);
        let mut pending = crate::preview_retry::PendingPreviewOob::default();
        pending.offer(draw.clone(), Err);

        let delete = g.frame(Some(panel()), Some("a.png"), true, true);
        assert_eq!(delete, graphics::delete_preview());
        pending.offer(delete.clone(), Err);
        let mut admitted = Vec::new();
        pending.retry(|bytes| {
            admitted.push(bytes);
            Ok(())
        });
        assert_eq!(admitted, vec![delete]);
        assert_eq!(admitted[0], graphics::delete_preview());
        assert!(!admitted.iter().any(|bytes| bytes == &draw));
    }
}
