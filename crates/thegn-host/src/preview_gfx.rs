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
    /// A full terminal clear makes the actual image state unknown. Preserve
    /// that fact until a frame either draws or deletes the preview.
    cleared: bool,
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

    /// Mark the terminal image state unknown after a full screen clear. A
    /// later visible frame redraws it; a hidden frame sends a scoped delete so
    /// any refused draw cannot reappear after the clear.
    pub fn invalidate(&mut self) {
        self.cleared = true;
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
            Some((place, raster)) if self.cleared || self.drawn != Some((place, self.rev)) => {
                // (Re)draw: encode first, and commit `drawn` only once there are
                // real bytes to emit. An invalid raster draws nothing.
                let image = if let Some(payload) = &raster.kitty {
                    Ok(graphics::kitty_place(payload, place))
                } else {
                    #[cfg(test)]
                    {
                        self.encodes += 1;
                    }
                    graphics::kitty_image(&raster.rgba, raster.w, raster.h, place)
                };
                match image {
                    Ok(image) => {
                        // Delete any stale image first, then transmit + display.
                        let mut out = Vec::new();
                        if self.drawn.is_some() {
                            out.extend_from_slice(&graphics::delete_preview());
                        }
                        out.extend_from_slice(&image);
                        self.drawn = Some((place, self.rev));
                        self.cleared = false;
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
                        let should_delete = self.drawn.take().is_some() || self.cleared;
                        self.cleared = false;
                        if should_delete {
                            graphics::delete_preview()
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            Some(_) => Vec::new(), // already drawn at this placement and revision
            None if self.drawn.is_some() || self.cleared => {
                self.drawn = None;
                self.cleared = false;
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
            kitty: None,
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
        assert_eq!(
            g.encodes, 1,
            "raster without a payload uses the fallback encoder"
        );
    }

    #[test]
    fn prepared_payload_draws_without_loop_encode_and_matches_legacy_bytes() {
        let rgba = vec![1, 2, 3, 4];
        let raster = Raster {
            kitty: Some(std::sync::Arc::new(
                graphics::kitty_payload(&rgba, 1, 1).unwrap(),
            )),
            rgba: rgba.clone(),
            w: 1,
            h: 1,
        };
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster);

        let drawn = g.frame(Some(panel()), Some("a.png"), false, true);
        assert_eq!(g.encodes, 0);
        assert_eq!(
            drawn,
            graphics::kitty_image(&rgba, 1, 1, placement(panel())).unwrap()
        );
    }

    #[test]
    fn placement_change_reuses_prepared_payload_without_loop_encode() {
        let rgba = vec![5, 6, 7, 8];
        let raster = Raster {
            kitty: Some(std::sync::Arc::new(
                graphics::kitty_payload(&rgba, 1, 1).unwrap(),
            )),
            rgba: rgba.clone(),
            w: 1,
            h: 1,
        };
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster);
        assert!(
            !g.frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
        let resized = Rect {
            x: panel().x,
            y: panel().y,
            cols: panel().cols + 1,
            rows: panel().rows,
        };

        let redraw = g.frame(Some(resized), Some("a.png"), false, true);
        assert_eq!(g.encodes, 0);
        assert!(redraw.starts_with(&graphics::delete_preview()));
        assert_eq!(
            redraw[graphics::delete_preview().len()..].to_vec(),
            graphics::kitty_place(
                g.img.as_ref().unwrap().1.kitty.as_ref().unwrap(),
                placement(resized)
            )
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
                kitty: None,
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
    fn invalid_raster_after_invalidate_deletes_refused_draw() {
        let mut g = PreviewGfx::new();
        g.set(
            "a.png".to_string(),
            Raster {
                rgba: vec![1, 2, 3], // 1x1 needs 4 bytes
                w: 1,
                h: 1,
                kitty: None,
            },
        );
        assert!(g.drawn.is_none());
        g.invalidate();

        assert_eq!(
            g.frame(Some(panel()), Some("a.png"), false, true),
            graphics::delete_preview()
        );
        assert!(!g.cleared);
        assert!(
            g.frame(Some(panel()), None, false, true).is_empty(),
            "the clear marker is consumed after the delete"
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
                kitty: None,
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
                kitty: None,
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
                kitty: None,
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

    #[test]
    fn invalidate_then_hide_replaces_refused_draw_with_delete() {
        let mut g = PreviewGfx::new();
        g.set("a.png".to_string(), raster());
        let draw = g.frame(Some(panel()), Some("a.png"), false, true);
        let mut pending = crate::preview_retry::PendingPreviewOob::default();
        pending.offer(draw.clone(), Err);

        g.invalidate();
        let delete = g.frame(Some(panel()), Some("a.png"), true, true);
        assert_eq!(delete, graphics::delete_preview());
        pending.offer(delete, Err);

        let mut admitted = Vec::new();
        pending.retry(|bytes| {
            admitted.push(bytes);
            Ok(())
        });
        assert_eq!(admitted, vec![graphics::delete_preview()]);
        assert!(!admitted.iter().any(|bytes| bytes == &draw));
    }
}

#[cfg(test)]
mod cross_surface_tests {
    use super::*;
    use std::collections::BTreeSet;

    #[derive(Default)]
    struct OuterKitty {
        shown: BTreeSet<u32>,
        log: Vec<String>,
        global_delete: bool,
    }

    impl OuterKitty {
        fn apply(&mut self, bytes: &[u8]) {
            let mut rest = bytes;
            while let Some(start) = rest.windows(3).position(|window| window == b"\x1b_G") {
                rest = &rest[start + 3..];
                let Some(end) = rest.windows(2).position(|window| window == b"\x1b\\") else {
                    break;
                };
                let apc = &rest[..end];
                rest = &rest[end + 2..];
                let control_end = apc
                    .iter()
                    .position(|byte| *byte == b';')
                    .unwrap_or(apc.len());
                let control = std::str::from_utf8(&apc[..control_end])
                    .expect("kitty control fields are ASCII");
                let fields: Vec<(&str, &str)> = control
                    .split(',')
                    .filter_map(|field| field.split_once('='))
                    .collect();
                let value = |key: &str| {
                    fields
                        .iter()
                        .find_map(|(name, value)| (*name == key).then_some(*value))
                };
                match value("a") {
                    Some("T" | "p") => {
                        if let Some(id) = value("i").and_then(|id| id.parse::<u32>().ok()) {
                            self.shown.insert(id);
                            self.log.push(format!("draw id {id}"));
                        } else {
                            // Kitty continuation chunks have no action field and are ignored.
                            self.log.push("draw without image id".to_string());
                        }
                    }
                    Some("d") if value("d") == Some("I") => {
                        if let Some(id) = value("i").and_then(|id| id.parse::<u32>().ok()) {
                            self.shown.remove(&id);
                            self.log.push(format!("scoped delete id {id}"));
                        } else {
                            self.global_delete = true;
                            self.shown.clear();
                            self.log
                                .push("GLOBAL DELETE: image delete missing id".to_string());
                        }
                    }
                    Some("d") => {
                        self.global_delete = true;
                        self.shown.clear();
                        self.log.push(format!(
                            "GLOBAL DELETE: unsupported delete form {}",
                            control
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    #[derive(Default)]
    struct Wire {
        refuse: bool,
        admitted: Vec<Vec<u8>>,
    }

    impl Wire {
        fn submit(&mut self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
            if self.refuse {
                Err(bytes)
            } else {
                self.admitted.push(bytes);
                Ok(())
            }
        }

        fn flush(&mut self, outer: &mut OuterKitty) {
            for bytes in self.admitted.drain(..) {
                outer.apply(&bytes);
            }
        }
    }

    fn raster() -> Raster {
        Raster {
            rgba: vec![1, 2, 3, 4],
            w: 1,
            h: 1,
            kitty: None,
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

    fn relay(relay_state: &mut crate::kitty_relay::KittyRelay, child_apc: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        relay_state.feed_with_rect(child_apc, (8, 5), |piece| match piece {
            crate::kitty_relay::Piece::GfxDisplay(display) => {
                bytes.extend(crate::kitty_relay::cup((0, 0), (0, 0)));
                bytes.extend(display);
            }
            crate::kitty_relay::Piece::GfxOther(other) => bytes.extend(other),
            crate::kitty_relay::Piece::Emulator(_)
            | crate::kitty_relay::Piece::GfxAnswer(_)
            | crate::kitty_relay::Piece::Rejected => {}
        });
        bytes
    }

    fn corner_draw(relay_state: &mut crate::kitty_relay::KittyRelay, payload: &[u8]) -> Vec<u8> {
        relay(
            relay_state,
            &[
                b"\x1b_Ga=T,i=7,f=24,s=1,v=1;".as_slice(),
                payload,
                b"\x1b\\",
            ]
            .concat(),
        )
    }

    fn record_step(outer: &OuterKitty, step: &str, preview: bool, corner: bool) {
        let mut expected = BTreeSet::new();
        if preview {
            expected.insert(crate::graphics::PREVIEW_IMAGE_ID);
        }
        if corner {
            expected.insert(crate::kitty_relay::CORNER_ID_BASE + 1);
        }
        assert_eq!(
            outer.shown, expected,
            "{step}: shown IDs differ; full history: {:?}; actual IDs: {:?}",
            outer.log, outer.shown
        );
        assert!(
            !outer.global_delete,
            "{step}: global kitty delete observed; full history: {:?}; actual IDs: {:?}",
            outer.log, outer.shown
        );
    }

    fn refused_then_retry(
        wire: &mut Wire,
        outer: &mut OuterKitty,
        mut retry: impl FnMut(&mut Wire),
        step: &str,
        preview: bool,
        corner: bool,
    ) {
        wire.refuse = true;
        let before = wire.admitted.len();
        retry(wire);
        assert_eq!(
            wire.admitted.len(),
            before,
            "{step}: refused submission entered FIFO; history: {:?}; IDs: {:?}",
            outer.log,
            outer.shown
        );
        wire.refuse = false;
        retry(wire);
        wire.flush(outer);
        record_step(outer, step, preview, corner);
    }

    #[test]
    fn outer_model_flags_a_global_delete() {
        let mut outer = OuterKitty {
            shown: [11, 22].into_iter().collect(),
            ..OuterKitty::default()
        };
        outer.apply(b"\x1b_Ga=d\x1b\\");
        assert!(
            outer.global_delete,
            "global delete was not recorded; history: {:?}; IDs: {:?}",
            outer.log, outer.shown
        );
        assert!(
            outer.shown.is_empty(),
            "global delete did not clear IDs; history: {:?}; IDs: {:?}",
            outer.log,
            outer.shown
        );
    }

    #[test]
    fn outer_model_tracks_scoped_deletes_and_chunked_draws() {
        let rgba = vec![9; 300_000 * 4];
        let chunked = crate::graphics::kitty_image(
            &rgba,
            300_000,
            1,
            crate::graphics::Placement {
                origin_col: 0,
                origin_row: 0,
                cols: 1,
                rows: 1,
            },
        )
        .unwrap();
        let apc_count = chunked
            .windows(3)
            .filter(|window| *window == b"\x1b_G")
            .count();
        assert!(
            apc_count > 1,
            "expected a chunked image, found {apc_count} APC"
        );
        let mut outer = OuterKitty::default();
        outer.apply(&chunked);
        assert_eq!(
            outer.shown,
            BTreeSet::from([crate::graphics::PREVIEW_IMAGE_ID])
        );
        assert_eq!(
            outer.log,
            [format!("draw id {}", crate::graphics::PREVIEW_IMAGE_ID)]
        );
        outer.apply(&crate::graphics::delete_preview());
        assert!(
            outer.shown.is_empty(),
            "scoped preview delete left IDs; history: {:?}; IDs: {:?}",
            outer.log,
            outer.shown
        );
        assert!(!outer.global_delete, "history: {:?}", outer.log);
    }

    #[test]
    fn steady_preview_survives_every_corner_lifecycle_under_refusal() {
        let mut outer = OuterKitty::default();
        let mut wire = Wire::default();
        let mut preview = PreviewGfx::new();
        preview.set("a.png".to_string(), raster());
        let mut pending_preview = crate::preview_retry::PendingPreviewOob::default();
        let draw = preview.frame(Some(panel()), Some("a.png"), false, true);
        pending_preview.offer(draw, |bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "steady preview", true, false);

        let mut corner = crate::preview_retry::PendingCornerOob::default();
        let mut relay_state = crate::kitty_relay::KittyRelay::new();
        let first_draw = corner_draw(&mut relay_state, b"AQID");
        let mut first_offered = false;
        refused_then_retry(
            &mut wire,
            &mut outer,
            |wire| {
                if first_offered {
                    corner.retry(true, |bytes| wire.submit(bytes));
                } else {
                    first_offered = true;
                    corner.offer_draw(first_draw.clone(), true, |bytes| wire.submit(bytes));
                }
            },
            "corner draw",
            true,
            true,
        );

        let replacement = corner_draw(&mut relay_state, b"BAUG");
        let mut replacement_offered = false;
        refused_then_retry(
            &mut wire,
            &mut outer,
            |wire| {
                if replacement_offered {
                    corner.retry(true, |bytes| wire.submit(bytes));
                } else {
                    replacement_offered = true;
                    corner.offer_draw(replacement.clone(), true, |bytes| wire.submit(bytes));
                }
            },
            "corner replacement",
            true,
            true,
        );

        corner.require_delete();
        refused_then_retry(
            &mut wire,
            &mut outer,
            |wire| corner.retry(true, |bytes| wire.submit(bytes)),
            "corner geometry delete",
            true,
            false,
        );

        corner.offer_draw(replacement.clone(), true, |bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "corner restored before occlusion", true, true);
        corner.occlude();
        wire.refuse = true;
        corner.retry(false, |bytes| wire.submit(bytes));
        wire.refuse = false;
        corner.retry(true, |bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "corner occlusion and clear", true, true);

        corner.screen_cleared();
        wire.refuse = true;
        corner.retry(false, |bytes| wire.submit(bytes));
        wire.refuse = false;
        corner.retry(true, |bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "corner screen clear replay", true, true);

        let child_delete = relay(&mut relay_state, b"\x1b_Ga=d,i=7;\x1b\\");
        wire.refuse = true;
        assert!(wire.submit(child_delete.clone()).is_err());
        wire.refuse = false;
        // Child deletes relay directly through try_submit_oob, outside a retry slot.
        wire.submit(child_delete)
            .unwrap_or_else(|_| panic!("wire unexpectedly refused child delete"));
        wire.flush(&mut outer);
        record_step(&outer, "corner child delete", true, false);

        assert!(
            preview
                .frame(Some(panel()), Some("a.png"), false, true)
                .is_empty()
        );
    }

    #[test]
    fn steady_corner_survives_every_preview_lifecycle_under_refusal() {
        let mut outer = OuterKitty::default();
        let mut wire = Wire::default();
        let mut relay_state = crate::kitty_relay::KittyRelay::new();
        let corner_bytes = corner_draw(&mut relay_state, b"AQID");
        wire.submit(corner_bytes)
            .unwrap_or_else(|_| panic!("initial corner submission refused"));
        wire.flush(&mut outer);
        record_step(&outer, "steady corner", false, true);

        let mut preview = PreviewGfx::new();
        preview.set("a.png".to_string(), raster());
        let mut pending = crate::preview_retry::PendingPreviewOob::default();
        let initial = preview.frame(Some(panel()), Some("a.png"), false, true);
        wire.refuse = true;
        pending.offer(initial, |bytes| wire.submit(bytes));
        wire.refuse = false;
        pending.retry(|bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "preview draw", true, true);

        preview.set(
            "a.png".to_string(),
            Raster {
                rgba: vec![5, 6, 7, 8],
                w: 1,
                h: 1,
                kitty: None,
            },
        );
        let stale_replacement = preview.frame(Some(panel()), Some("a.png"), false, true);
        wire.refuse = true;
        pending.offer(stale_replacement, |bytes| wire.submit(bytes));
        preview.set(
            "a.png".to_string(),
            Raster {
                rgba: vec![9, 10, 11, 12],
                w: 1,
                h: 1,
                kitty: None,
            },
        );
        let latest_replacement = preview.frame(Some(panel()), Some("a.png"), false, true);
        pending.offer(latest_replacement, |bytes| wire.submit(bytes));
        wire.refuse = false;
        pending.retry(|bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "same-path latest-wins replacement", true, true);

        let dismissed = preview.frame(Some(panel()), None, false, true);
        wire.refuse = true;
        pending.offer(dismissed, |bytes| wire.submit(bytes));
        wire.refuse = false;
        pending.retry(|bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "preview dismiss", false, true);

        preview.set("b.png".to_string(), raster());
        let shown_again = preview.frame(Some(panel()), Some("b.png"), false, true);
        wire.refuse = true;
        pending.offer(shown_again, |bytes| wire.submit(bytes));
        wire.refuse = false;
        pending.retry(|bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "preview shown before occlusion", true, true);
        let occluded = preview.frame(Some(panel()), Some("b.png"), true, true);
        wire.refuse = true;
        pending.offer(occluded, |bytes| wire.submit(bytes));
        wire.refuse = false;
        pending.retry(|bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "preview occlusion", false, true);

        let un_occluded = preview.frame(Some(panel()), Some("b.png"), false, true);
        wire.refuse = true;
        pending.offer(un_occluded, |bytes| wire.submit(bytes));
        wire.refuse = false;
        pending.retry(|bytes| wire.submit(bytes));
        wire.flush(&mut outer);
        record_step(&outer, "preview un-occlude", true, true);
    }
}
