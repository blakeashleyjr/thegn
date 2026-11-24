//! Off-loop Files-preview fetch + content-type routing (AF 775).
//!
//! Extracted from `run.rs`: the event loop calls [`spawn_fetch`] when a file is
//! opened in the inline preview, and drains two channels — text lines for the
//! `FilePreview` pane (the plain-text route plus CSV tables, Jupyter cells,
//! PDF-extracted text, and Mermaid source) and, on a graphics-capable terminal,
//! a decoded raster ([`crate::rasterize::Raster`]) that [`crate::preview_gfx`]
//! draws over the panel via the kitty path. Every fetch runs on the bounded preview supervisor
//! worker and pulses the waker; nothing here touches the loop or the AI layer.

use std::fmt::Write as _;
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
/// Maximum prefix retained for a binary hex preview.
const HEX_PREVIEW_BYTES: usize = 4096;
const UNCONTAINED_MARKER: &str = "(renderer ran uncontained)";
const CONTAINMENT_UNAVAILABLE_NOTE: &str = "(containment unavailable)";

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
            // Prepare the potentially multi-megabyte base64 payload before
            // deliver takes the supervisor mutex. Deliver closures must stay
            // O(1): submit and cancel_fetches need this same lock on the loop.
            let prepared = raster
                .as_ref()
                .filter(|_| kitty && !ctx.cancelled().load(std::sync::atomic::Ordering::Relaxed))
                .and_then(|(r, w, h)| crate::graphics::kitty_payload(r, *w, *h).ok())
                .map(std::sync::Arc::new);
            let delivered = ctx.deliver(|| {
                let mut delivered = false;
                if let Some((r, w, h)) = raster {
                    // The display path uses the prepared payload; keep RGBA
                    // only when preparation was skipped or failed, for its
                    // legacy fallback encoder.
                    let rgba = if prepared.is_some() { Vec::new() } else { r };
                    delivered |= img_tx
                        .try_send((
                            rel.clone(),
                            Raster {
                                rgba,
                                w,
                                h,
                                kitty: prepared,
                            },
                        ))
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
            || crate::rasterize::image_file(abs).map(|r| (r, false)),
            |_| Err("image preview needs a graphics-capable terminal".to_string()),
        ),
        PreviewRoute::Mermaid => graphical(
            abs,
            kitty,
            || {
                crate::rasterize::mermaid_with_status(abs, cancel)
                    .map(|rendered| (rendered.raster, rendered.uncontained))
            },
            |reason| {
                mermaid_source_fallback(
                    abs,
                    reason.is_some_and(|reason| {
                        reason == crate::rasterize::RendererRefusal::ContainmentUnavailable.note()
                    }) || (!kitty
                        && matches!(
                            crate::rasterize::containment_policy(
                                std::env::consts::OS == "linux",
                                false
                            ),
                            crate::rasterize::Policy::Refuse
                        )),
                )
            },
        ),
        PreviewRoute::Pdf => graphical(
            abs,
            kitty,
            || {
                crate::rasterize::pdf_page1_with_status(abs, cancel)
                    .map(|rendered| (rendered.raster, rendered.uncontained))
            },
            |_| {
                // Fallback: extracted text, or a truthful note on why there is none.
                Ok(match crate::rasterize::pdf_text_with_status(abs, cancel) {
                    Ok(rendered) => {
                        let mut lines = bounded_lines(&rendered.text);
                        if rendered.uncontained {
                            lines.truncate(PREVIEW_MAX_LINES.saturating_sub(1));
                            lines.push(UNCONTAINED_MARKER.to_string());
                        }
                        lines
                    }
                    Err(why) => vec![bounded_line(&format!("(PDF — {why})"))],
                })
            },
        ),
        PreviewRoute::Archive => (read_archive_listing(abs, cancel), None),
        PreviewRoute::Docx => (read_docx_text(abs, cancel), None),
        PreviewRoute::Unknown => (read_hex_preview(abs), None),
    }
}

/// Shared handling for the graphical routes (image / Mermaid / PDF): when the
/// terminal is kitty-capable, rasterize and show a caption under the image;
/// otherwise fall back to text via `text_fallback`. On a rasterization error we
/// also fall back to text, so a missing renderer never breaks the preview.
fn graphical(
    abs: &std::path::Path,
    kitty: bool,
    rasterize: impl FnOnce() -> Result<(Raster, bool), String>,
    text_fallback: impl FnOnce(Option<&str>) -> Result<Vec<String>, String>,
) -> Rendered {
    if kitty {
        match rasterize() {
            Ok((r, uncontained)) => {
                let mut caption = vec![caption(abs, r.w, r.h), String::new()];
                if uncontained {
                    caption.push(UNCONTAINED_MARKER.to_string());
                }
                caption.push("(shown via terminal graphics)".to_string());
                return (Ok(caption), Some((r.rgba, r.w, r.h)));
            }
            Err(e) => {
                tracing::debug!("preview rasterize failed, using text: {e}");
                return (text_fallback(Some(&e)), None);
            }
        }
    }
    (text_fallback(None), None)
}

fn mermaid_source_fallback(
    abs: &std::path::Path,
    renderer_refused: bool,
) -> Result<Vec<String>, String> {
    let mut lines = read_text(abs)?;
    if renderer_refused {
        lines.truncate(PREVIEW_MAX_LINES.saturating_sub(1));
        lines.push(CONTAINMENT_UNAVAILABLE_NOTE.to_string());
    }
    Ok(lines)
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

/// Read + decode a file for the plain-text route, falling back to hex for NUL
/// bytes. The decode/limits live in [`prepare_preview`] (pure).
fn read_text(abs: &std::path::Path) -> Result<Vec<String>, String> {
    let bytes = read_bytes(abs)?;
    if bytes.contains(&0) {
        return Ok(hex_dump_lines(
            &bytes[..bytes.len().min(HEX_PREVIEW_BYTES)],
            bytes.len() > HEX_PREVIEW_BYTES,
        ));
    }
    prepare_preview(&bytes)
}

/// Scan cap for the (decompressed) tar stream.
const ARCHIVE_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// List a zip / tar / tar.gz for the preview pane. Bounded and sanitized; any
/// parse error falls back to the hex dump with a one-line note on top.
fn read_archive_listing(
    abs: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<String>, String> {
    use thegn_core::archive_list::{self as al, EntryKind};
    let max_entries = PREVIEW_MAX_LINES.saturating_sub(3);
    let file = crate::rasterize::open_regular_nonblocking(abs)
        .map_err(|e| format!("cannot preview file: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot preview file: {e}"))?;
    if !meta.is_file() {
        return Err("cannot preview file: not a regular file".into());
    }
    let lower = abs
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let gz = lower.ends_with(".gz") || lower.ends_with(".tgz");
    let is_tar = gz || lower.ends_with(".tar");
    let mut file = file;
    let (label, result) = if is_tar {
        let r: Result<al::Listing, String> = if gz {
            let mut dec = flate2::read::MultiGzDecoder::new(file);
            al::tar_listing(&mut dec, max_entries, ARCHIVE_SCAN_BYTES, cancel)
        } else {
            al::tar_listing(&mut file, max_entries, ARCHIVE_SCAN_BYTES, cancel)
        };
        ("tar archive", r)
    } else {
        let sniffed_tar = {
            use std::io::{Read, Seek, SeekFrom};
            let mut probe = [0u8; 512];
            let n = file.read(&mut probe).unwrap_or(0);
            // best-effort: a failed rewind surfaces as a parse error below.
            let _ = file.seek(SeekFrom::Start(0));
            n >= 262 && &probe[257..262] == b"ustar"
        };
        if sniffed_tar {
            (
                "tar archive",
                al::tar_listing(&mut file, max_entries, ARCHIVE_SCAN_BYTES, cancel),
            )
        } else {
            (
                "zip archive",
                al::zip_listing(&mut file, meta.len(), max_entries),
            )
        }
    };
    let listing = match result {
        Ok(l) => l,
        Err(why) => {
            let mut lines = vec![bounded_line(&format!(
                "(archive listing unavailable: {})",
                thegn_core::msg::terminal_safe(&why)
            ))];
            lines.extend(read_hex_preview(abs)?);
            return Ok(lines);
        }
    };
    let mut lines: Vec<String> = Vec::new();
    let mut retained = 0usize;
    let mut byte_capped = false;
    for e in &listing.entries {
        let size = e.size.map(|s| s.to_string()).unwrap_or_default();
        let name = thegn_core::msg::terminal_safe(&e.name);
        let text = match &e.kind {
            EntryKind::Symlink(t) => {
                format!(
                    "{size:>10}  {name} -> {}",
                    thegn_core::msg::terminal_safe(t)
                )
            }
            _ if e.encrypted => format!("{size:>10}  {name} (encrypted)"),
            _ => format!("{size:>10}  {name}"),
        };
        let line = bounded_line(&text.replace('\n', " "));
        if retained.saturating_add(line.len()) > PREVIEW_MAX_TOTAL_BYTES {
            byte_capped = true;
            break;
        }
        retained += line.len();
        lines.push(line);
    }
    let count = listing.total_hint.unwrap_or(listing.entries.len() as u64);
    let mut header = format!("{label}, {count} entries");
    if listing.truncated.is_some() || byte_capped {
        header.push_str(", listing truncated");
    }
    lines.splice(0..0, [header, String::new()]);
    Ok(lines)
}

/// Cap on a docx entry's stored (compressed) size.
const DOCX_MAX_COMPRESSED: u64 = 16 * 1024 * 1024;
/// Cap on the inflated `word/document.xml` bytes we scan.
const DOCX_MAX_XML: u64 = 8 * 1024 * 1024;

/// Extract the paragraph text of a `.docx` for the preview pane. Any failure
/// falls back to the zip part listing with a one-line note on top.
fn read_docx_text(
    abs: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<String>, String> {
    match docx_text_lines(abs, cancel) {
        Ok(lines) => Ok(lines),
        Err(why) => {
            let mut lines = vec![bounded_line(&format!(
                "(docx text unavailable: {}; showing parts)",
                thegn_core::msg::terminal_safe(&why)
            ))];
            lines.extend(read_archive_listing(abs, cancel)?);
            Ok(lines)
        }
    }
}

fn docx_text_lines(
    abs: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<String>, String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::Ordering;
    use thegn_core::archive_list as al;
    let mut file = crate::rasterize::open_regular_nonblocking(abs)
        .map_err(|e| format!("cannot preview file: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot preview file: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let len = meta.len();
    let loc = al::zip_find_entry(&mut file, len, "word/document.xml")?
        .ok_or_else(|| "word/document.xml not found".to_string())?;
    if loc.flags & 1 != 0 {
        return Err("document is encrypted".into());
    }
    if loc.method != 0 && loc.method != 8 {
        return Err(format!("unsupported compression method {}", loc.method));
    }
    if loc.compressed > DOCX_MAX_COMPRESSED {
        return Err("document.xml too large".into());
    }
    let data = al::zip_entry_data_offset(&mut file, len, &loc)?;
    file.seek(SeekFrom::Start(data))
        .map_err(|e| format!("seek failed: {e}"))?;
    let stored = file.take(loc.compressed);
    let mut src: Box<dyn Read> = if loc.method == 8 {
        Box::new(flate2::read::DeflateDecoder::new(stored))
    } else {
        Box::new(stored)
    };
    let mut xml: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut xml_truncated = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = src
            .read(&mut buf)
            .map_err(|e| format!("inflate failed: {e}"))?;
        if n == 0 {
            break;
        }
        let room = usize::try_from(DOCX_MAX_XML)
            .unwrap_or(usize::MAX)
            .saturating_sub(xml.len());
        if n > room {
            xml.extend_from_slice(buf.get(..room).unwrap_or(&[]));
            xml_truncated = true;
            break;
        }
        xml.extend_from_slice(buf.get(..n).unwrap_or(&[]));
    }
    let max_lines = PREVIEW_MAX_LINES.saturating_sub(3);
    let text = thegn_core::docx_text::document_xml_lines(&xml, max_lines, PREVIEW_MAX_LINE_BYTES)?;
    let mut header = String::from("docx document");
    if text.truncated || xml_truncated {
        header.push_str(", text truncated");
    }
    let mut lines = vec![header, String::new()];
    let mut retained = 0usize;
    for l in &text.lines {
        let line = bounded_line(&thegn_core::msg::terminal_safe(l));
        if retained.saturating_add(line.len()) > PREVIEW_MAX_TOTAL_BYTES {
            if !lines[0].ends_with(", text truncated") {
                lines[0].push_str(", text truncated");
            }
            break;
        }
        retained += line.len();
        lines.push(line);
    }
    Ok(lines)
}

fn read_hex_preview(abs: &std::path::Path) -> Result<Vec<String>, String> {
    let prefix = crate::rasterize::read_regular_prefix(abs, HEX_PREVIEW_BYTES + 1)
        .map_err(|e| format!("cannot preview file: {e}"))?;
    let shown = prefix.len().min(HEX_PREVIEW_BYTES);
    Ok(hex_dump_lines(
        &prefix[..shown],
        prefix.len() > HEX_PREVIEW_BYTES,
    ))
}

/// Format a bounded byte prefix as 16-byte hex rows with an ASCII gutter.
pub(crate) fn hex_dump_lines(bytes: &[u8], total_known_more: bool) -> Vec<String> {
    let truncated = total_known_more || bytes.len() > HEX_PREVIEW_BYTES;
    let bytes = &bytes[..bytes.len().min(HEX_PREVIEW_BYTES)];
    let mut lines = vec![
        format!(
            "binary file: first {} bytes shown as hex{}",
            bytes.len(),
            if truncated { " (truncated)" } else { "" }
        ),
        String::new(),
    ];
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let mut line = format!("{:08x}  ", row * 16);
        for column in 0..16 {
            if let Some(byte) = chunk.get(column) {
                write!(line, "{byte:02x} ").expect("writing to String cannot fail");
            } else {
                line.push_str("   ");
            }
            if column == 7 {
                line.push(' ');
            }
        }
        line.push('|');
        for byte in chunk {
            line.push(if (0x20..=0x7e).contains(byte) {
                char::from(*byte)
            } else {
                '.'
            });
        }
        line.push('|');
        lines.push(line);
    }
    lines
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
            && let Some(last) = lines.last_mut()
        {
            *last = marker.to_string();
        } else {
            lines.push(marker.to_string());
        }
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
    fn hex_dump_formats_offset_hex_and_ascii() {
        let lines = hex_dump_lines(b"Hello\0\x7f\x80 world!!!", false);
        assert_eq!(lines[0], "binary file: first 17 bytes shown as hex");
        assert_eq!(lines[1], "");
        assert_eq!(
            lines[2],
            "00000000  48 65 6c 6c 6f 00 7f 80  20 77 6f 72 6c 64 21 21 |Hello... world!!|"
        );
        assert_eq!(
            lines[3],
            "00000010  21                                               |!|"
        );
    }

    #[test]
    fn hex_dump_caps_at_limit_and_marks_truncation() {
        let bytes = vec![0x41; 5000];
        let lines = hex_dump_lines(&bytes, false);
        assert_eq!(
            lines[0],
            "binary file: first 4096 bytes shown as hex (truncated)"
        );
        assert_eq!(lines.len(), 258);
        assert!(lines[2].starts_with("00000000"));
        assert!(lines[257].starts_with("00000ff0"));
    }

    #[test]
    fn unknown_route_renders_hex_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"\0unknown binary").unwrap();

        let (text, raster) =
            route_and_render(&path, false, &std::sync::atomic::AtomicBool::new(false));
        assert!(raster.is_none());
        assert!(text.unwrap()[0].starts_with("binary file: first 15 bytes shown as hex"));
    }

    #[test]
    fn text_route_with_late_nul_falls_back_to_hex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        let mut bytes = vec![b'x'; 2000];
        bytes.push(0);
        std::fs::write(&path, bytes).unwrap();

        let lines = read_text(&path).unwrap();
        assert_eq!(lines[0], "binary file: first 2001 bytes shown as hex");
        assert!(lines[2].starts_with("00000000  78 78"));
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
    fn refused_mermaid_source_fallback_keeps_note_inside_shared_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many-lines.mmd");
        std::fs::write(&path, "x\n".repeat(60_000)).unwrap();

        let lines = mermaid_source_fallback(&path, true).unwrap();
        assert!(lines.len() <= PREVIEW_MAX_LINES);
        assert_eq!(
            lines.last(),
            Some(&CONTAINMENT_UNAVAILABLE_NOTE.to_string())
        );
    }

    #[test]
    fn graphical_preview_marks_an_uncontained_renderer() {
        let (text, raster) = graphical(
            std::path::Path::new("diagram.pdf"),
            true,
            || {
                Ok((
                    Raster {
                        rgba: vec![255, 0, 0, 255],
                        w: 1,
                        h: 1,
                        kitty: None,
                    },
                    true,
                ))
            },
            |_| Err("fallback should not run".to_string()),
        );
        assert!(raster.is_some());
        assert!(text.unwrap().iter().any(|line| line == UNCONTAINED_MARKER));
    }

    #[test]
    fn mermaid_linux_source_fallback_explains_containment_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagram.mmd");
        std::fs::write(&path, "graph TD\n  A-->B\n").unwrap();

        let lines = mermaid_source_fallback(
            &path,
            matches!(
                crate::rasterize::containment_policy(std::env::consts::OS == "linux", false),
                crate::rasterize::Policy::Refuse
            ),
        )
        .unwrap();
        let policy = crate::rasterize::containment_policy(std::env::consts::OS == "linux", false);
        assert_eq!(
            lines.last().map(String::as_str),
            if matches!(policy, crate::rasterize::Policy::Refuse) {
                Some(CONTAINMENT_UNAVAILABLE_NOTE)
            } else {
                Some("  A-->B")
            }
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

    fn zip_fixture(name: &str) -> Vec<u8> {
        let mut out = b"PK\x03\x04".to_vec();
        out.extend_from_slice(&[0u8; 26]);
        out.extend_from_slice(name.as_bytes());
        let mut cd = Vec::new();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&[0u8; 24]);
        cd[24..28].copy_from_slice(&7u32.to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0u8; 16]);
        cd.extend_from_slice(name.as_bytes());
        let off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn tar_fixture(name: &str) -> Vec<u8> {
        let mut b = [0u8; 512];
        b[..name.len()].copy_from_slice(name.as_bytes());
        b[124..136].copy_from_slice(b"00000000003\0");
        b[156] = b'0';
        b[148..156].fill(b' ');
        let sum: u32 = b.iter().map(|x| u32::from(*x)).sum();
        b[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut v = b.to_vec();
        v.extend_from_slice(b"abc");
        v.resize(1024, 0);
        v.extend_from_slice(&[0u8; 1024]);
        v
    }

    fn no_cancel() -> std::sync::atomic::AtomicBool {
        std::sync::atomic::AtomicBool::new(false)
    }

    #[test]
    fn read_archive_listing_lists_zip_and_sanitizes_names() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.zip");
        std::fs::write(&f, zip_fixture("evil\x1b[31m.txt")).unwrap();
        let lines = read_archive_listing(&f, &no_cancel()).unwrap();
        assert_eq!(lines[0], "zip archive, 1 entries");
        assert_eq!(lines[1], "");
        assert!(lines[2].contains("evil"), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\x1b')), "{lines:?}");
    }

    #[test]
    fn read_archive_listing_lists_tgz() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.tar.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tar_fixture("hello.txt")).unwrap();
        std::fs::write(&f, enc.finish().unwrap()).unwrap();
        let lines = read_archive_listing(&f, &no_cancel()).unwrap();
        assert_eq!(lines[0], "tar archive, 1 entries");
        assert_eq!(lines[2], "         3  hello.txt");
    }

    #[test]
    fn read_archive_listing_corrupt_zip_falls_back_to_hex_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bad.zip");
        std::fs::write(&f, b"PK\x03\x04 not really a zip archive at all").unwrap();
        let lines = read_archive_listing(&f, &no_cancel()).unwrap();
        assert!(
            lines[0].starts_with("(archive listing unavailable"),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("binary file: first"), "{lines:?}");
    }

    /// Single-entry zip (`word/document.xml`) with explicit method and flags.
    fn docx_fixture(data: &[u8], method: u16, flags: u16) -> Vec<u8> {
        let name = b"word/document.xml";
        let mut out = b"PK\x03\x04".to_vec();
        out.extend_from_slice(&[0u8; 22]);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 2]);
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        let mut cd = Vec::new();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&[0u8; 4]);
        cd.extend_from_slice(&flags.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&[0u8; 8]);
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0u8; 12]);
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(name);
        let off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn deflate(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    const DOC_XML: &str = "<w:document><w:body><w:p><w:r><w:t>First para</w:t></w:r></w:p>\
        <w:p><w:r><w:t>Second &amp; last</w:t></w:r></w:p></w:body></w:document>";

    #[test]
    fn read_docx_text_renders_stored_and_deflated() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.docx");
        for z in [
            docx_fixture(DOC_XML.as_bytes(), 0, 0),
            docx_fixture(&deflate(DOC_XML.as_bytes()), 8, 0),
        ] {
            std::fs::write(&f, z).unwrap();
            let lines = read_docx_text(&f, &no_cancel()).unwrap();
            assert_eq!(lines[0], "docx document", "{lines:?}");
            assert_eq!(lines[1], "");
            assert_eq!(lines[2], "First para");
            assert_eq!(lines[3], "Second & last");
        }
    }

    #[test]
    fn read_docx_text_is_escape_free() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.docx");
        let xml = "<w:p><w:t>red\x1b[31m text \x07</w:t></w:p>";
        std::fs::write(&f, docx_fixture(xml.as_bytes(), 0, 0)).unwrap();
        let lines = read_docx_text(&f, &no_cancel()).unwrap();
        assert!(lines[2].contains("red"), "{lines:?}");
        assert!(
            lines
                .iter()
                .all(|l| !l.contains('\x1b') && !l.contains('\x07')),
            "{lines:?}"
        );
    }

    #[test]
    fn read_docx_text_bad_inputs_fall_back_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.docx");
        let cases = [
            docx_fixture(b"xx", 0, 1),                   // encrypted
            docx_fixture(b"xx", 12, 0),                  // bzip2
            docx_fixture(b"\xff\xff\xff garbage", 8, 0), // corrupt deflate
            docx_fixture(b"not xml at all", 0, 0),
            b"PK\x03\x04 junk".to_vec(),
        ];
        for z in cases {
            std::fs::write(&f, z).unwrap();
            let lines = read_docx_text(&f, &no_cancel()).unwrap();
            assert!(
                lines[0].starts_with("(docx text unavailable: ")
                    && lines[0].ends_with("; showing parts)"),
                "{lines:?}"
            );
        }
        // Cancelled mid-read falls back too (never panics).
        std::fs::write(&f, docx_fixture(DOC_XML.as_bytes(), 0, 0)).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let lines = read_docx_text(&f, &cancel).unwrap();
        assert!(lines[0].contains("cancelled"), "{lines:?}");
    }

    #[test]
    fn read_docx_text_inflate_bomb_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.docx");
        let mut xml = b"<w:p><w:t>x</w:t></w:p>".to_vec();
        xml.resize(DOCX_MAX_XML as usize + 4096, b' ');
        std::fs::write(&f, docx_fixture(&deflate(&xml), 8, 0)).unwrap();
        let lines = read_docx_text(&f, &no_cancel()).unwrap();
        assert_eq!(lines[0], "docx document, text truncated", "{lines:?}");
        assert_eq!(lines[2], "x");
    }
}
