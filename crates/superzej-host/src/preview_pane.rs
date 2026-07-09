//! Off-loop Files-preview fetch + content-type routing (AF 775).
//!
//! Extracted from `run.rs`: the event loop calls [`spawn_fetch`] when a file is
//! opened in the inline preview, and drains two channels — text lines for the
//! `FilePreview` pane (the plain-text route plus CSV tables, Jupyter cells,
//! PDF-extracted text, and Mermaid source) and, on a graphics-capable terminal,
//! a decoded raster ([`crate::rasterize::Raster`]) that [`crate::preview_gfx`]
//! draws over the panel via the kitty path. Every fetch runs on `spawn_blocking`
//! and pulses the waker; nothing here touches the loop or the AI layer.

use tokio::sync::mpsc as tokio_mpsc;

use superzej_core::preview::{self, CsvTable, Notebook, PreviewRoute};

use crate::rasterize::Raster;

/// `(rel_path, Ok(lines) | Err(reason))` for the `FilePreview` text pane.
pub type TextMsg = (String, Result<Vec<String>, String>);
/// `(rel_path, raster)` for the graphics overlay (kitty path).
pub type ImageMsg = (String, Raster);

/// A routed preview: text lines for the pane, plus an optional decoded raster
/// `(rgba, w, h)` for the graphics path. Kept as a tuple to stay `Send`-simple.
type Rendered = (Result<Vec<String>, String>, Option<(Vec<u8>, u32, u32)>);

/// Read a file off the loop for the inline Files preview and route it by
/// content type, sending text lines (always) and — when `kitty` and the route
/// is graphical — a decoded raster. `rel` tags both results so a fast
/// esc/reopen drops strays. Pulses the waker on delivery.
pub fn spawn_fetch(
    rel: String,
    abs: std::path::PathBuf,
    text_tx: tokio_mpsc::UnboundedSender<TextMsg>,
    img_tx: tokio_mpsc::UnboundedSender<ImageMsg>,
    waker: termwiz::terminal::TerminalWaker,
    kitty: bool,
) {
    tokio::task::spawn_blocking(move || {
        let (text, raster) = route_and_render(&abs, kitty);
        let mut delivered = false;
        if let Some((r, w, h)) = raster {
            delivered |= img_tx.send((rel.clone(), Raster { rgba: r, w, h })).is_ok();
        }
        delivered |= text_tx.send((rel, text)).is_ok();
        if delivered {
            let _ = waker.wake();
        }
    });
}

/// Route `abs` and produce `(text_lines, optional_raster)`. Pure-ish (does file
/// I/O + optional subprocess rasterization); split out so it stays readable.
/// The raster is returned as `(rgba, w, h)` to keep the return `Send`-simple.
fn route_and_render(abs: &std::path::Path, kitty: bool) -> Rendered {
    match route(abs) {
        PreviewRoute::Text => (read_text(abs), None),
        PreviewRoute::Csv => (
            read_capped(abs).map(|s| crate::preview_render::csv_lines(&CsvTable::parse(abs, &s))),
            None,
        ),
        PreviewRoute::Jupyter => (
            read_capped(abs).and_then(|s| {
                Notebook::parse(&s).map(|nb| crate::preview_render::notebook_lines(&nb))
            }),
            None,
        ),
        PreviewRoute::Image => graphical(
            abs,
            kitty,
            || crate::rasterize::image_file(abs),
            || Err("image preview needs a graphics-capable terminal".to_string()),
        ),
        PreviewRoute::Mermaid => graphical(
            abs,
            kitty,
            || crate::rasterize::mermaid(abs),
            || {
                // Fallback: show the Mermaid source as text.
                read_capped(abs).map(|s| s.lines().map(str::to_string).collect())
            },
        ),
        PreviewRoute::Pdf => graphical(
            abs,
            kitty,
            || crate::rasterize::pdf_page1(abs),
            || {
                // Fallback: extracted text, or a clear note when no extractor exists.
                Ok(crate::rasterize::pdf_text(abs)
                    .map(|t| t.lines().map(str::to_string).collect())
                    .unwrap_or_else(|| vec!["(PDF — no text extractor available)".to_string()]))
            },
        ),
        PreviewRoute::Unknown => (Err("binary file".to_string()), None),
    }
}

/// Shared handling for the graphical routes (image / Mermaid / PDF): when the
/// terminal is kitty-capable, rasterize and show a caption under the image;
/// otherwise fall back to text via `text_fallback`. On a rasterization error we
/// also fall back to text, so a missing renderer never breaks the preview.
fn graphical(
    abs: &std::path::Path,
    kitty: bool,
    rasterize: impl FnOnce() -> Result<Raster, String>,
    text_fallback: impl FnOnce() -> Result<Vec<String>, String>,
) -> Rendered {
    if kitty {
        match rasterize() {
            Ok(r) => {
                let caption = vec![
                    caption(abs, r.w, r.h),
                    String::new(),
                    "(shown via terminal graphics)".to_string(),
                ];
                return (Ok(caption), Some((r.rgba, r.w, r.h)));
            }
            Err(e) => tracing::debug!("preview rasterize failed, using text: {e}"),
        }
    }
    (text_fallback(), None)
}

/// A one-line caption for a graphical preview.
fn caption(abs: &std::path::Path, w: u32, h: u32) -> String {
    let name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    format!("🖼 {name} — {w}×{h}")
}

/// Detect a file's [`PreviewRoute`] from its extension, sniffing a small head
/// only for the extension-less / unknown fallback.
fn route(abs: &std::path::Path) -> PreviewRoute {
    let mut head = [0u8; 1024];
    let n = std::fs::File::open(abs)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    preview::route_for(abs, &head[..n])
}

/// Read + decode a file for the plain-text route, rejecting oversized/binary
/// content. The decode/limits live in [`prepare_preview`] (pure).
fn read_text(abs: &std::path::Path) -> Result<Vec<String>, String> {
    prepare_preview(&read_bytes(abs)?)
}

/// Read a file as UTF-8 (lossy) for the CSV/Jupyter/Mermaid text routes.
fn read_capped(abs: &std::path::Path) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&read_bytes(abs)?).into_owned())
}

/// Read a file, rejecting ones larger than the preview cap before reading.
fn read_bytes(abs: &std::path::Path) -> Result<Vec<u8>, String> {
    const MAX_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB
    if let Ok(meta) = std::fs::metadata(abs)
        && meta.len() > MAX_BYTES
    {
        return Err(format!("file too large ({} KiB)", meta.len() / 1024));
    }
    std::fs::read(abs).map_err(|e| format!("cannot read: {e}"))
}

/// Decode bytes into preview lines: reject binary content (a NUL byte), expand
/// tabs to 4 spaces, strip CRs, and cap the line count. Pure + unit-tested.
pub fn prepare_preview(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.contains(&0) {
        return Err("binary file".into());
    }
    const MAX_LINES: usize = 50_000;
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text
        .split('\n')
        .take(MAX_LINES)
        .map(|l| l.trim_end_matches('\r').replace('\t', "    "))
        .collect();
    // A trailing newline splits into a spurious empty final element — drop it,
    // but keep a single empty line for a genuinely empty file.
    if text.ends_with('\n') && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_preview_expands_tabs_and_drops_trailing_newline() {
        let lines = prepare_preview(b"a\tb\nc\n").unwrap();
        assert_eq!(lines, vec!["a    b".to_string(), "c".to_string()]);
    }

    #[test]
    fn prepare_preview_rejects_binary() {
        assert_eq!(prepare_preview(b"abc\0def"), Err("binary file".into()));
    }

    #[test]
    fn prepare_preview_strips_cr_and_keeps_interior_blank_lines() {
        let lines = prepare_preview(b"one\r\n\r\nthree").unwrap();
        assert_eq!(
            lines,
            vec!["one".to_string(), String::new(), "three".to_string()]
        );
    }

    #[test]
    fn prepare_preview_empty_file_is_one_blank_line() {
        assert_eq!(prepare_preview(b"").unwrap(), vec![String::new()]);
    }

    #[test]
    fn caption_includes_name_and_dimensions() {
        assert_eq!(
            caption(std::path::Path::new("a/b/pic.png"), 640, 480),
            "🖼 pic.png — 640×480"
        );
    }
}
