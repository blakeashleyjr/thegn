//! Cover-art rendering for the Now-Playing overlay (optional `[media]` feature).
//!
//! Art is fetched + decoded **off the loop** (`file://` reads and HTTP GETs are
//! blocking / slow) and folded into a half-block **mosaic**: each cell is a `▀`
//! glyph whose foreground is the upper pixel and background the lower pixel, so
//! one text cell shows two vertical pixels. This renders in any truecolor
//! terminal — no kitty/sixel required — which is the safe default the spec calls
//! for (graphics protocols stay gated / best-effort elsewhere). When the
//! terminal can't do truecolor the seg color layer quantizes it down like every
//! other frame.

use termwiz::terminal::TerminalWaker;

use crate::seg::{Line, Tok, seg};

/// One decoded cover as mosaic rows, ready for `seg::draw_line`. `cols`/`rows`
/// are the cell dimensions (rows = pixel-rows / 2).
#[derive(Debug, Clone)]
pub(crate) struct ArtMosaic {
    /// The source URL this was decoded from — so a stale delivery for a
    /// now-different track can be dropped.
    pub url: String,
    pub lines: Vec<Line>,
}

/// Build a `cols × rows`-cell mosaic from a decoded RGBA raster by nearest-
/// neighbour sampling two vertical pixels per cell into a `▀` (upper-half block).
pub(crate) fn mosaic(raster: &crate::rasterize::Raster, cols: usize, rows: usize) -> Vec<Line> {
    let (iw, ih) = (raster.w.max(1), raster.h.max(1));
    let px = |cx: usize, py: usize| -> (u8, u8, u8) {
        // Map cell/sub-pixel coords into the source image (nearest neighbour).
        let sx = ((cx as u32) * iw / cols.max(1) as u32).min(iw - 1);
        let sy = ((py as u32) * ih / (rows.max(1) as u32 * 2)).min(ih - 1);
        let idx = ((sy * iw + sx) * 4) as usize;
        match raster.rgba.get(idx..idx + 3) {
            Some(p) => (p[0], p[1], p[2]),
            None => (0, 0, 0),
        }
    };
    (0..rows)
        .map(|row| {
            let mut segs = Vec::with_capacity(cols);
            for cx in 0..cols {
                let top = px(cx, row * 2);
                let bot = px(cx, row * 2 + 1);
                segs.push(
                    seg(Tok::Rgb(top.0, top.1, top.2), "\u{2580}") // ▀
                        .bg(Tok::Rgb(bot.0, bot.1, bot.2)),
                );
            }
            Line::segs(segs)
        })
        .collect()
}

/// Fetch + decode `art_url` into an [`ArtMosaic`] off-thread, then deliver it and
/// pulse the waker. Supports `file://`/bare paths and `http(s)://`. Silent on
/// any failure (the overlay just shows its placeholder).
pub(crate) fn spawn_fetch(
    art_url: String,
    cols: usize,
    rows: usize,
    tx: tokio::sync::mpsc::UnboundedSender<ArtMosaic>,
    waker: TerminalWaker,
) {
    tokio::spawn(async move {
        let raster = if let Some(path) = local_path(&art_url) {
            match tokio::task::spawn_blocking(move || crate::rasterize::image_file(&path)).await {
                Ok(Ok(r)) => r,
                _ => return,
            }
        } else if art_url.starts_with("http://") || art_url.starts_with("https://") {
            let Ok(resp) = reqwest::get(&art_url).await else {
                return;
            };
            let Ok(bytes) = resp.bytes().await else {
                return;
            };
            match tokio::task::spawn_blocking(move || crate::rasterize::image_bytes(&bytes)).await {
                Ok(Ok(r)) => r,
                _ => return,
            }
        } else {
            return;
        };
        let lines = mosaic(&raster, cols, rows);
        let _ = tx.send(ArtMosaic {
            url: art_url,
            lines,
        });
        let _ = waker.wake();
    });
}

/// A local filesystem path for a `file://` URL or a bare absolute path; `None`
/// for remote schemes.
fn local_path(url: &str) -> Option<std::path::PathBuf> {
    if let Some(rest) = url.strip_prefix("file://") {
        // `file:///home/…` → strip the (empty) host component.
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        return Some(std::path::PathBuf::from(path));
    }
    if url.starts_with('/') {
        return Some(std::path::PathBuf::from(url));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_parses_file_urls() {
        assert_eq!(
            local_path("file:///home/u/art.png"),
            Some(std::path::PathBuf::from("/home/u/art.png"))
        );
        assert_eq!(
            local_path("/tmp/cover.jpg"),
            Some(std::path::PathBuf::from("/tmp/cover.jpg"))
        );
        assert_eq!(local_path("https://i.scdn.co/x"), None);
        assert_eq!(local_path("data:image/png;base64,AAAA"), None);
    }

    #[test]
    fn mosaic_dims() {
        let raster = crate::rasterize::Raster {
            rgba: vec![255u8; 8 * 8 * 4],
            w: 8,
            h: 8,
        };
        let lines = mosaic(&raster, 6, 3);
        assert_eq!(lines.len(), 3);
    }
}
