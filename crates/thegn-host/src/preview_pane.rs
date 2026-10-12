//! Off-loop Files-preview fetch + content-type routing (AF 775).
//!
//! Extracted from `run.rs`: the event loop calls [`spawn_fetch`] when a file is
//! opened in the inline preview, and drains two channels — text lines for the
//! `FilePreview` pane (the plain-text route plus CSV tables, Jupyter cells,
//! PDF-extracted text, and Mermaid source) and, on a graphics-capable terminal,
//! a decoded raster ([`crate::rasterize::Raster`]) that [`crate::preview_gfx`]
//! draws over the panel via the kitty path. Every fetch runs on the bounded preview supervisor
//! worker and pulses the waker; nothing here touches the loop or the AI layer.

use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::preview::{self, CsvTable, Notebook, PreviewRoute};

use crate::rasterize::Raster;

/// `(rel_path, Ok(lines) | Err(reason))` for the `FilePreview` text pane.
pub type TextMsg = (String, Result<Vec<String>, String>);
/// `(rel_path, raster)` for the graphics overlay (kitty path).
pub type ImageMsg = (String, Raster);

/// A routed preview: text lines for the pane, plus an optional decoded raster
/// `(rgba, w, h)` for the graphics path. Kept as a tuple to stay `Send`-simple.
type Rendered = (Result<Vec<String>, String>, Option<(Vec<u8>, u32, u32)>);

/// Capacity of each result channel. The supervisor delivers at most one result
/// per generation (one text + one image message) and only the newest
/// generation, so a handful of slots is never exceeded while the loop drains.
pub const RESULT_CAP: usize = 4;

/// Maximum lines retained by any text preview route.
pub(crate) const PREVIEW_MAX_LINES: usize = 50_000;
/// Maximum UTF-8 bytes retained for a rendered preview line, including its
/// truncation marker when present.
pub(crate) const PREVIEW_MAX_LINE_BYTES: usize = 4096;
/// Maximum UTF-8 bytes retained across all rendered preview lines.
pub(crate) const PREVIEW_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// Read a file off the loop for the inline Files preview and route it by
/// content type, sending text lines (always) and — when `kitty` and the route
/// is graphical — a decoded raster. `rel` tags both results so a fast
/// esc/reopen drops strays. Pulses the waker on delivery.
///
/// Jobs go through the process-wide [`crate::preview_jobs`] supervisor: one
/// active job plus the latest pending one; a newer fetch cancels the active
/// job's renderers and replaces the pending one, and a superseded job can never
/// deliver.
pub fn spawn_fetch(
    rel: String,
    abs: std::path::PathBuf,
    text_tx: tokio_mpsc::Sender<TextMsg>,
    img_tx: tokio_mpsc::Sender<ImageMsg>,
    waker: termwiz::terminal::TerminalWaker,
    kitty: bool,
) {
    // On a panic the fallback still answers this generation, so the preview
    // shows an error instead of sticking at "loading".
    let fb_tx = text_tx.clone();
    let fb_rel = rel.clone();
    let fb_waker = waker.clone();
    let fallback = move |ctx: &crate::preview_jobs::JobCtx| {
        let delivered = ctx.deliver(|| {
            fb_tx
                .try_send((fb_rel, Err("preview failed unexpectedly".to_string())))
                .is_ok()
        });
        if delivered {
            let _ = fb_waker.wake(); // best-effort: waker pulse
        }
    };
    crate::preview_jobs::global().submit_guarded(
        move |ctx| {
            let (text, raster) = route_and_render(&abs, kitty, ctx.cancelled());
            let delivered = ctx.deliver(|| {
                let mut delivered = false;
                if let Some((r, w, h)) = raster {
                    delivered |= img_tx
                        .try_send((rel.clone(), Raster { rgba: r, w, h }))
                        .is_ok();
                }
                delivered |= text_tx.try_send((rel, text)).is_ok();
                delivered
            });
            if delivered {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        },
        fallback,
    );
}

/// The preview was closed or replaced by something that is not a file preview:
/// drop queued work and kill the active job's renderers.
pub fn cancel_fetches() {
    crate::preview_jobs::global().cancel_all();
}

/// Close the inline file preview AND cancel its in-flight fetch: the one place
/// the loop drops `file_preview`, so a renderer can never outlive its pane.
pub fn close_file_preview(panel_ui: &mut crate::panel::PanelUi) {
    panel_ui.file_preview = None;
    cancel_fetches();
}

/// Route `abs` and produce `(text_lines, optional_raster)`. Pure-ish (does file
/// I/O + optional subprocess rasterization); split out so it stays readable.
/// The raster is returned as `(rgba, w, h)` to keep the return `Send`-simple.
fn route_and_render(
    abs: &std::path::Path,
    kitty: bool,
    cancel: &std::sync::atomic::AtomicBool,
) -> Rendered {
    match route(abs) {
        PreviewRoute::Text => (read_text(abs), None),
        PreviewRoute::Csv => (
            read_capped(abs).map(|s| {
                crate::preview_render::csv_lines(&CsvTable::parse(abs, &s))
                    .into_iter()
                    .map(|line| bounded_line(&line))
                    .collect()
            }),
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
            || crate::rasterize::mermaid(abs, cancel),
            || {
                // Fallback: show the Mermaid source as text.
                read_text(abs)
            },
        ),
        PreviewRoute::Pdf => graphical(
            abs,
            kitty,
            || crate::rasterize::pdf_page1(abs, cancel),
            || {
                // Fallback: extracted text, or a truthful note on why there is none.
                Ok(match crate::rasterize::pdf_text(abs, cancel) {
                    Ok(t) => bounded_lines(&t),
                    Err(why) => vec![bounded_line(&format!("(PDF — {why})"))],
                })
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
    let head = crate::rasterize::read_regular_prefix(abs, 1024).unwrap_or_default();
    preview::route_for(abs, &head)
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
    crate::rasterize::read_regular_capped(abs, MAX_BYTES)
        .map_err(|e| format!("cannot preview file: {e}"))
}

/// Decode bytes into preview lines: reject binary content (a NUL byte), expand
/// tabs to 4 spaces, strip CRs, and cap the line count. Pure + unit-tested.
pub fn prepare_preview(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.contains(&0) {
        return Err("binary file".into());
    }
    let text = String::from_utf8_lossy(bytes);
    let mut lines = bounded_lines(&text);
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

/// Split text lazily under the shared line-count, rendered-line, and total
/// rendered-byte limits.
pub(crate) fn bounded_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut retained_bytes = 0;
    for line in text.split('\n') {
        if lines.len() == PREVIEW_MAX_LINES {
            break;
        }

        let mut rendered = bounded_line(line);
        let remaining = PREVIEW_MAX_TOTAL_BYTES.saturating_sub(retained_bytes);
        if rendered.len() <= remaining {
            retained_bytes += rendered.len();
            lines.push(rendered);
            continue;
        }

        let marker = crate::caps::active_glyphs().ellipsis;
        let marker_bytes = marker.len();
        let mut content_limit = remaining.saturating_sub(marker_bytes).min(rendered.len());
        while !rendered.is_char_boundary(content_limit) {
            content_limit -= 1;
        }
        rendered.truncate(content_limit);
        retained_bytes += rendered.len();
        lines.push(rendered);

        // If the limit lands exactly on a line boundary, make room for the
        // marker by trimming retained content from the end.
        while retained_bytes.saturating_add(marker_bytes) > PREVIEW_MAX_TOTAL_BYTES {
            let Some(last) = lines.last_mut() else {
                break;
            };
            if last.is_empty() {
                lines.pop();
                continue;
            }
            let excess = retained_bytes + marker_bytes - PREVIEW_MAX_TOTAL_BYTES;
            let target = last.len().saturating_sub(excess);
            let boundary = (0..=target)
                .rev()
                .find(|index| last.is_char_boundary(*index))
                .unwrap_or(0);
            retained_bytes -= last.len() - boundary;
            last.truncate(boundary);
        }
        if lines.len() == PREVIEW_MAX_LINES
            && let Some(last) = lines.pop()
        {
            retained_bytes -= last.len();
        }
        lines.push(marker.to_string());
        break;
    }

    lines
}

/// Normalize and cap one line, keeping UTF-8 intact and reserving room for the
/// active terminal's ellipsis only when truncation is needed.
pub(crate) fn bounded_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.trim_end_matches('\r').chars();
    let mut truncated = false;
    for ch in chars.by_ref() {
        let mut encoded = [0; 4];
        let rendered = if ch == '\t' {
            "    "
        } else {
            ch.encode_utf8(&mut encoded)
        };
        if out.len().saturating_add(rendered.len()) > PREVIEW_MAX_LINE_BYTES {
            truncated = true;
            break;
        }
        out.push_str(rendered);
    }
    if truncated {
        let ellipsis = crate::caps::active_glyphs().ellipsis;
        let content_limit = PREVIEW_MAX_LINE_BYTES.saturating_sub(ellipsis.len());
        while out.len() > content_limit {
            out.pop();
        }
        out.push_str(ellipsis);
    }
    out
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
    fn bounded_lines_truncates_a_single_long_line_within_byte_cap() {
        let lines = bounded_lines(&"x".repeat(100_000));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].len() <= PREVIEW_MAX_LINE_BYTES);
        assert!(lines[0].ends_with(crate::caps::active_glyphs().ellipsis));
    }

    #[test]
    fn bounded_lines_caps_total_bytes_after_expanding_tabs() {
        let tab_line = "\t".repeat(1024);
        let input = std::iter::repeat_n(tab_line.as_str(), 1100)
            .collect::<Vec<_>>()
            .join("\n");

        let lines = bounded_lines(&input);
        assert!(lines.iter().map(String::len).sum::<usize>() <= PREVIEW_MAX_TOTAL_BYTES);
        assert_eq!(
            lines.last().map(String::as_str),
            Some(crate::caps::active_glyphs().ellipsis)
        );
    }

    #[test]
    fn mermaid_source_fallback_obeys_shared_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many-lines.mmd");
        let contents = "x\n".repeat(60_000);
        std::fs::write(&path, contents).unwrap();

        let (text, raster) =
            route_and_render(&path, false, &std::sync::atomic::AtomicBool::new(false));
        assert!(raster.is_none());
        let lines = text.unwrap();
        assert!(
            lines.len() <= PREVIEW_MAX_LINES,
            "retained {} lines",
            lines.len()
        );
    }

    #[test]
    fn caption_includes_name_and_dimensions() {
        assert_eq!(
            caption(std::path::Path::new("a/b/pic.png"), 640, 480),
            "🖼 pic.png — 640×480"
        );
    }

    #[test]
    fn read_bytes_refuses_oversized_input_with_a_limit_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("large.txt");
        std::fs::write(&file, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
        let error = read_bytes(&file).unwrap_err();
        assert!(
            error.contains("2097152 byte preview cap") && error.contains("2097153 bytes"),
            "unexpected diagnostic: {error}"
        );
        // The direct opened-handle cap check, including a file larger than
        // its cap, is covered by rasterize::tests::read_regular_capped_enforces_cap_and_kind.
    }

    #[test]
    fn read_bytes_previews_a_final_symlink_to_a_regular_file() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("AGENTS.md");
        let link = dir.path().join("CLAUDE.md");
        std::fs::write(&target, b"workspace instructions").unwrap();
        crate::platform::test_symlink(&target, &link).unwrap();

        assert_eq!(read_bytes(&link).unwrap(), b"workspace instructions");
    }
}
