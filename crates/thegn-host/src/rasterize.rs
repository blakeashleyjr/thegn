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
use std::sync::atomic::AtomicBool;

/// A decoded raster ready for [`crate::graphics::kitty_image`].
pub struct Raster {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// Cap the longest side so the kitty payload (and base64 of it) stays bounded —
/// a preview pane is a few hundred pixels at most.
const MAX_DIM: u32 = 800;

/// Hard bounds on the SOURCE image, checked from the header before any pixel
/// buffer is allocated. [`MAX_DIM`] only caps the final raster; without these a
/// tiny compressed file declaring enormous dimensions would still be fully
/// decoded first.
#[derive(Clone, Copy)]
pub(crate) struct DecodeLimits {
    max_dim: u32,
    max_pixels: u64,
    max_alloc: u64,
}

/// Keep player supplied cover art to a modest decode budget.
pub(crate) const MEDIA_ART_LIMITS: DecodeLimits = DecodeLimits {
    max_dim: 8192,
    max_pixels: 32 * 1024 * 1024,
    max_alloc: 192 * 1024 * 1024,
};
/// Bound preview source dimensions while allowing high resolution photos.
const MAX_PREVIEW_INPUT_DIM: u32 = 16384;
/// Bound preview pixel count to 256 MP before decoder allocation.
const MAX_PREVIEW_INPUT_PIXELS: u64 = 256 * 1024 * 1024;
/// Larger camera images remain previewable while the pixel buffer stays bounded to 1 GiB.
const PREVIEW_LIMITS: DecodeLimits = DecodeLimits {
    max_dim: MAX_PREVIEW_INPUT_DIM,
    max_pixels: MAX_PREVIEW_INPUT_PIXELS,
    max_alloc: 1024 * 1024 * 1024,
};
/// Bound preview file reads to 256 MiB while covering large camera JPEGs.
const MAX_PREVIEW_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Bound generated Mermaid PNGs independently of the decoder's pixel budget.
const MAX_MERMAID_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;

/// Whether declared source dimensions are within the decode budget. Pure.
fn check_dimensions(w: u32, h: u32, limits: DecodeLimits) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err("image has no pixels".to_string());
    }
    if w > limits.max_dim || h > limits.max_dim {
        return Err(format!("image dimensions {w}x{h} exceed the decode limit"));
    }
    if u64::from(w) * u64::from(h) > limits.max_pixels {
        return Err(format!("image {w}x{h} exceeds the pixel limit"));
    }
    Ok(())
}

/// Decode encoded image `bytes` to RGBA, downscaling to fit [`MAX_DIM`]. The
/// header dimensions are validated and decoder limits installed first.
fn decode_and_fit(bytes: &[u8], limits: DecodeLimits) -> Result<Raster, String> {
    let reader = || {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| format!("image decode failed: {e}"))
    };
    let (w, h) = reader()?
        .into_dimensions()
        .map_err(|e| format!("image decode failed: {e}"))?;
    check_dimensions(w, h, limits)?;
    let mut r = reader()?;
    let mut decoder_limits = image::Limits::default();
    decoder_limits.max_image_width = Some(limits.max_dim);
    decoder_limits.max_image_height = Some(limits.max_dim);
    decoder_limits.max_alloc = Some(limits.max_alloc);
    r.limits(decoder_limits);
    let img = r
        .decode()
        .map_err(|e| format!("image decode failed: {e}"))?;
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

/// Read at most `cap` bytes from a REGULAR file. The kind is verified on the
/// opened handle (not just the path), the resolved final target is opened
/// without following a symlink, the open cannot block on a FIFO, and `take`
/// bounds a file that grows mid-read.
pub(crate) fn read_regular_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let file = open_regular_nonblocking(path)?;
    read_open_regular_capped(file, cap)
}

/// Read a caller-admitted path exactly as supplied, without resolving it
/// again. The final path component is opened without following a symlink.
pub(crate) fn read_regular_capped_exact(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let file = open_regular_nonblocking_exact(path)?;
    read_open_regular_capped(file, cap)
}

fn read_open_regular_capped(file: std::fs::File, cap: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let meta = file.metadata().map_err(|e| format!("read failed: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > cap {
        return Err(format!(
            "file exceeds the {cap} byte preview cap ({} bytes)",
            meta.len()
        ));
    }
    let mut buf = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    if buf.len() as u64 > cap {
        return Err(format!(
            "file exceeds the {cap} byte preview cap (more than {cap} bytes)"
        ));
    }
    Ok(buf)
}

/// Resolve a caller path (allowing a final symlink), then open the resolved
/// path without following a final symlink. Validate the opened handle too.
/// Nonblocking open prevents a path swapped to a FIFO from parking the caller.
fn open_regular_nonblocking(path: &Path) -> Result<std::fs::File, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("read failed: {e}"))?;
    match std::fs::metadata(&canonical) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("not a regular file".to_string()),
        Err(e) => return Err(format!("read failed: {e}")),
    }
    let file = crate::platform::open_read_nonblocking_nofollow(&canonical)
        .map_err(|e| format!("read failed: {e}"))?;
    if !file
        .metadata()
        .map_err(|e| format!("read failed: {e}"))?
        .is_file()
    {
        return Err("not a regular file".to_string());
    }
    Ok(file)
}

/// Open the exact path without following a final-component symlink. Callers
/// that enforce a path policy must pass the admitted path directly here.
fn open_regular_nonblocking_exact(path: &Path) -> Result<std::fs::File, String> {
    let file = crate::platform::open_read_nonblocking_nofollow(path)
        .map_err(|e| format!("read failed: {e}"))?;
    if !file
        .metadata()
        .map_err(|e| format!("read failed: {e}"))?
        .is_file()
    {
        return Err("not a regular file".to_string());
    }
    Ok(file)
}

/// Read a small prefix from the opened regular file. Used only for
/// extension-less content sniffing.
pub(crate) fn read_regular_prefix(path: &Path, cap: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = open_regular_nonblocking(path)?;
    let mut prefix = Vec::with_capacity(cap);
    file.take(cap as u64)
        .read_to_end(&mut prefix)
        .map_err(|e| format!("read failed: {e}"))?;
    Ok(prefix)
}

/// Rasterize an image file (PNG/JPEG) to RGBA. Blocking.
pub fn image_file(path: &Path) -> Result<Raster, String> {
    image_file_with_limits(path, MAX_PREVIEW_FILE_BYTES, PREVIEW_LIMITS)
}

/// Rasterize a file with caller-chosen byte and decode limits. Blocking.
pub(crate) fn image_file_with_limits(
    path: &Path,
    cap: u64,
    limits: DecodeLimits,
) -> Result<Raster, String> {
    let bytes = read_regular_capped(path, cap)?;
    decode_and_fit(&bytes, limits)
}

/// Rasterize an already-admitted image path without resolving it again.
pub(crate) fn image_file_exact_with_limits(
    path: &Path,
    cap: u64,
    limits: DecodeLimits,
) -> Result<Raster, String> {
    let bytes = read_regular_capped_exact(path, cap)?;
    decode_and_fit(&bytes, limits)
}

/// Decode bytes with caller-chosen limits (cover art uses the tighter budget).
pub(crate) fn image_bytes_with_limits(
    bytes: &[u8],
    limits: DecodeLimits,
) -> Result<Raster, String> {
    decode_and_fit(bytes, limits)
}

/// Rasterize page 1 of a PDF via `pdftoppm` (poppler) → PNG on stdout → RGBA.
/// Errs (→ text fallback) when the tool is absent, fails, exceeds the renderer
/// deadline or `cancel` is raised; the whole process group is killed and
/// reaped on every path. Blocking.
pub fn pdf_page1(path: &Path, cancel: &AtomicBool) -> Result<Raster, String> {
    let mut cmd = Command::new("pdftoppm");
    cmd.args(["-png", "-f", "1", "-l", "1", "-singlefile", "-r", "96"])
        .arg(path)
        .arg("-"); // write PNG to stdout
    let out = crate::preview_jobs::run_renderer(&mut cmd, cancel)
        .map_err(|e| format!("pdftoppm: {}", capture_err(&e)))?;
    decode_and_fit(&out, PREVIEW_LIMITS)
}

/// Extract a PDF's text via `pdftotext` (the no-graphics fallback). Output cut
/// at the byte cap is returned truncated (fine for text); `Err` carries a
/// truthful reason (absent tool, failure, timeout, cancel). Blocking.
pub fn pdf_text(path: &Path, cancel: &AtomicBool) -> Result<String, String> {
    use crate::preview_jobs::CaptureError as C;
    let mut cmd = Command::new("pdftotext");
    cmd.arg(path).arg("-"); // text to stdout
    match crate::preview_jobs::run_renderer(&mut cmd, cancel) {
        Ok(out) | Err(C::Capped(out)) => Ok(String::from_utf8_lossy(&out).into_owned()),
        Err(C::Spawn(_)) => Err("no text extractor available".to_string()),
        Err(e) => Err(format!("text extraction {}", capture_err(&e))),
    }
}

/// Render a Mermaid file via `mmdc` (mermaid-cli) → PNG → RGBA. Errs (→ source
/// fallback) when the tool is absent, fails, times out or is cancelled; the
/// renderer's whole process tree (mmdc spawns a browser) is killed and reaped.
/// Blocking.
pub fn mermaid(path: &Path, cancel: &AtomicBool) -> Result<Raster, String> {
    // Keep the renderer's output in a private, unpredictable directory so an
    // untrusted user of the shared temp directory cannot pre-place a symlink.
    let tmp_dir = tempfile::tempdir().map_err(|e| format!("mmd temp directory failed: {e}"))?;
    let tmp = tmp_dir.path().join("render.png");
    let mut cmd = Command::new("mmdc");
    cmd.arg("-i")
        .arg(path)
        .arg("-o")
        .arg(&tmp)
        .arg("-b")
        .arg("transparent");
    match crate::preview_jobs::run_renderer_quiet(&mut cmd, cancel) {
        Ok(()) => read_mermaid_output(&tmp, MAX_MERMAID_OUTPUT_BYTES),
        Err(e) => Err(format!("mmdc: {}", capture_err(&e))),
    }
}

fn read_mermaid_output(path: &Path, cap: u64) -> Result<Raster, String> {
    read_regular_capped(path, cap)
        .map_err(|e| format!("mmdc output read failed: {e}"))
        .and_then(|bytes| decode_and_fit(&bytes, PREVIEW_LIMITS))
}

fn capture_err(e: &crate::preview_jobs::CaptureError) -> String {
    use crate::preview_jobs::CaptureError as C;
    match e {
        C::Spawn(err) => format!("unavailable: {err}"),
        C::Cancelled => "cancelled".to_string(),
        C::Timeout => "timed out".to_string(),
        C::Failed => "failed".to_string(),
        C::Capped(_) => "output exceeded the size cap".to_string(),
    }
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
        let r = decode_and_fit(buf.get_ref(), PREVIEW_LIMITS).unwrap();
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
        let r = decode_and_fit(buf.get_ref(), PREVIEW_LIMITS).unwrap();
        assert!(r.w <= MAX_DIM && r.h <= MAX_DIM);
        assert!(
            r.w == MAX_DIM || r.h == MAX_DIM,
            "longest side hits the cap"
        );
    }

    #[test]
    fn dimension_guard_bounds_width_height_and_pixels() {
        assert!(check_dimensions(800, 800, MEDIA_ART_LIMITS).is_ok());
        assert!(check_dimensions(MEDIA_ART_LIMITS.max_dim, 1, MEDIA_ART_LIMITS).is_ok());
        assert!(check_dimensions(0, 10, MEDIA_ART_LIMITS).is_err());
        assert!(check_dimensions(MEDIA_ART_LIMITS.max_dim + 1, 1, MEDIA_ART_LIMITS).is_err());
        assert!(check_dimensions(1, MEDIA_ART_LIMITS.max_dim + 1, MEDIA_ART_LIMITS).is_err());
        // Each side is legal but the area is not.
        assert!(
            check_dimensions(
                MEDIA_ART_LIMITS.max_dim,
                MEDIA_ART_LIMITS.max_dim,
                MEDIA_ART_LIMITS
            )
            .is_err()
        );
        assert!(check_dimensions(MAX_PREVIEW_INPUT_DIM, 1, PREVIEW_LIMITS).is_ok());
        assert!(
            check_dimensions(
                MAX_PREVIEW_INPUT_DIM,
                MAX_PREVIEW_INPUT_PIXELS as u32 / MAX_PREVIEW_INPUT_DIM,
                PREVIEW_LIMITS
            )
            .is_ok()
        );
    }

    #[test]
    fn dimension_bomb_png_is_refused_before_decode() {
        // 1x1 PNG with the IHDR rewritten to 60000x60000: tiny file, huge claim.
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let mut png = buf.into_inner();
        png[16..20].copy_from_slice(&60000u32.to_be_bytes());
        png[20..24].copy_from_slice(&60000u32.to_be_bytes());
        let crc = crc32fast::hash(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let err = decode_and_fit(&png, MEDIA_ART_LIMITS)
            .err()
            .expect("must refuse");
        assert!(err.contains("dimensions 60000x60000 exceed the decode limit"));
    }

    #[test]
    fn read_regular_capped_enforces_cap_and_kind() {
        let dir = std::env::temp_dir().join(format!("thegn-rast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f.bin");
        std::fs::write(&f, [7u8; 100]).unwrap();
        assert_eq!(read_regular_capped(&f, 100).unwrap().len(), 100);
        assert!(read_regular_capped(&f, 99).is_err());
        assert!(read_regular_capped(&dir, 100).is_err(), "directory refused");
        if crate::platform::test_symlink_supported() {
            let target = dir.join("target");
            let link = dir.join("link");
            std::fs::write(&target, b"secret").unwrap();
            crate::platform::test_symlink(&target, &link).unwrap();
            assert_eq!(read_regular_capped(&link, 100).unwrap(), b"secret");
        }
        if crate::platform::test_fifo_supported() {
            let fifo = dir.join("p");
            crate::platform::test_fifo(&fifo).unwrap();
            // Must return promptly instead of blocking on the FIFO.
            assert!(read_regular_capped(&fifo, 100).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir); // best-effort: temp cleanup
    }

    #[test]
    fn exact_read_refuses_a_final_symlink_while_preview_read_resolves_it() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.png");
        let inside = dir.path().join("inside.png");
        std::fs::write(&outside, b"bounded fixture").unwrap();
        crate::platform::test_symlink(&outside, &inside).unwrap();

        assert!(read_regular_capped_exact(&inside, 100).is_err());
        assert_eq!(
            read_regular_capped(&inside, 100).unwrap(),
            b"bounded fixture"
        );
    }

    #[test]
    fn exact_read_refuses_fifo_without_blocking() {
        if !crate::platform::test_fifo_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("cover.fifo");
        crate::platform::test_fifo(&fifo).unwrap();
        assert!(read_regular_capped_exact(&fifo, 100).is_err());
    }

    #[test]
    fn mermaid_output_is_refused_at_its_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("render.png");
        std::fs::write(&output, [0u8; 33]).unwrap();
        let error = read_mermaid_output(&output, 32).err().expect("must refuse");
        assert!(error.contains("mmdc output read failed: file exceeds the 32 byte preview cap"));
    }

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode_and_fit(b"not an image", PREVIEW_LIMITS).is_err());
    }
}
