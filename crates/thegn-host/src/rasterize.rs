//! Off-loop rasterization of document previews to RGBA (AF 775 graphics path).
//!
//! Called only from `spawn_blocking` (all of these read files and, for PDF /
//! Mermaid, shell out to an external renderer). The pixels are handed to
//! [`crate::graphics::kitty_image`] for display. Every path degrades to a text
//! fallback upstream when it errors, so a missing `pdftoppm` / `mmdc` (or a
//! terminal without graphics) never breaks the preview — it just shows text.
//! No AI/agent dependency anywhere.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// A decoded raster ready for [`crate::graphics::kitty_image`].
pub struct Raster {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// Cap the longest side so the kitty payload (and base64 of it) stays bounded —
/// a preview pane is a few hundred pixels at most.
const MAX_DIM: u32 = 800;

/// Decode encoded image `bytes` to RGBA, downscaling to fit [`MAX_DIM`].
fn decode_and_fit(bytes: &[u8]) -> Result<Raster, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("image decode failed: {e}"))?;
    let img = if img.width() > MAX_DIM || img.height() > MAX_DIM {
        img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Raster {
        rgba: rgba.into_raw(),
        w,
        h,
    })
}

/// Rasterize an image file (PNG/JPEG) to RGBA. Blocking.
pub fn image_file(path: &Path) -> Result<Raster, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    decode_and_fit(&bytes)
}

/// Decode encoded image `bytes` (e.g. cover art fetched over HTTP) to RGBA.
pub fn image_bytes(bytes: &[u8]) -> Result<Raster, String> {
    decode_and_fit(bytes)
}

/// Rasterize page 1 of a PDF via `pdftoppm` (poppler) → PNG on stdout → RGBA.
/// Errs (→ text fallback) when the tool is absent or fails. Blocking.
#[expect(clippy::disallowed_methods)] // off-loop: called from spawn_blocking
pub fn pdf_page1(path: &Path) -> Result<Raster, String> {
    let out = Command::new("pdftoppm")
        .args(["-png", "-f", "1", "-l", "1", "-singlefile", "-r", "96"])
        .arg(path)
        .arg("-") // write PNG to stdout
        .output()
        .map_err(|e| format!("pdftoppm unavailable: {e}"))?;
    if !out.status.success() {
        return Err("pdftoppm failed".to_string());
    }
    decode_and_fit(&out.stdout)
}

/// Extract a PDF's text via `pdftotext` (the no-graphics fallback). `None` when
/// the tool is absent or fails. Blocking.
#[expect(clippy::disallowed_methods)] // off-loop: called from spawn_blocking
pub fn pdf_text(path: &Path) -> Option<String> {
    let out = Command::new("pdftotext")
        .arg(path)
        .arg("-") // text to stdout
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Render a Mermaid file via `mmdc` (mermaid-cli) → PNG → RGBA. Errs (→ source
/// fallback) when the tool is absent or fails. Blocking.
#[expect(clippy::disallowed_methods)] // off-loop: called from spawn_blocking
pub fn mermaid(path: &Path) -> Result<Raster, String> {
    let tmp = unique_temp("mmd", "png");
    let status = Command::new("mmdc")
        .arg("-i")
        .arg(path)
        .arg("-o")
        .arg(&tmp)
        .arg("-b")
        .arg("transparent")
        .status()
        .map_err(|e| format!("mmdc unavailable: {e}"))?;
    let result = if status.success() {
        std::fs::read(&tmp)
            .map_err(|e| format!("mmdc output read failed: {e}"))
            .and_then(|bytes| decode_and_fit(&bytes))
    } else {
        Err("mmdc failed".to_string())
    };
    let _ = std::fs::remove_file(&tmp); // best-effort: temp cleanup
    result
}

/// A process-unique temp path `<tmpdir>/thegn-<pid>-<n>.<ext>`. Avoids a
/// tempfile dependency; the counter makes concurrent rasterizations distinct.
fn unique_temp(tag: &str, ext: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("thegn-{tag}-{}-{n}.{ext}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_generated_png_to_rgba() {
        // Encode a 2x3 red PNG in-memory, then round-trip through decode_and_fit.
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let r = decode_and_fit(buf.get_ref()).unwrap();
        assert_eq!((r.w, r.h), (2, 3));
        assert_eq!(r.rgba.len(), 2 * 3 * 4);
        assert_eq!(&r.rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn downscales_oversized_images_under_the_cap() {
        let img = image::RgbaImage::from_pixel(2000, 1000, image::Rgba([0, 0, 0, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let r = decode_and_fit(buf.get_ref()).unwrap();
        assert!(r.w <= MAX_DIM && r.h <= MAX_DIM);
        assert!(
            r.w == MAX_DIM || r.h == MAX_DIM,
            "longest side hits the cap"
        );
    }

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode_and_fit(b"not an image").is_err());
    }

    #[test]
    fn unique_temp_paths_differ() {
        assert_ne!(unique_temp("x", "png"), unique_temp("x", "png"));
    }
}
