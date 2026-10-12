//! First-party terminal graphics: encode an RGBA raster as a kitty
//! graphics-protocol APC stream, placed at a cell rect (AF 399/775).
//!
//! This is the *emit* half of the document-viewer graphics path: the preview
//! pane rasterizes an image / Mermaid diagram / PDF page to RGBA off the loop
//! (see [`crate::rasterize`]), and this module turns those pixels into the bytes
//! the outer terminal draws — the same kitty protocol the corner relay
//! ([`crate::kitty_relay`]) forwards, but transmitted first-party rather than
//! relayed from a child. Terminals without kitty support fall back to a text
//! representation upstream, so this module is only reached on a capable
//! terminal. Pure over its inputs (unit-tested on the byte structure); the
//! actual pixels can only be confirmed on a live kitty/ghostty/wezterm terminal.

/// String Terminator (`ESC \`).
const ST: &[u8] = b"\x1b\\";
/// Max base64 payload per kitty chunk (protocol limit is 4096).
const CHUNK: usize = 4096;

/// Reserved outer-terminal image ID for first-party preview graphics.
/// The corner relay owns the immediately following bounded ID block.
pub const PREVIEW_IMAGE_ID: u32 = crate::kitty_relay::CORNER_ID_BASE - 1;

const _: () = assert!(
    PREVIEW_IMAGE_ID != 0
        && (PREVIEW_IMAGE_ID < crate::kitty_relay::CORNER_ID_BASE
            || PREVIEW_IMAGE_ID
                >= crate::kitty_relay::CORNER_ID_BASE + crate::kitty_relay::CORNER_IDS as u32)
);

/// A placed image: where (0-based screen cell origin) and how big (in cells) to
/// draw the raster, so kitty scales the pixels to the preview rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub origin_row: u16,
    pub origin_col: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Encode `rgba` (`px_w`×`px_h`, 4 bytes/pixel) as a kitty transmit-and-display
/// APC stream, preceded by an absolute CUP to `place.origin` and scaled to
/// `place.cols`×`place.rows` cells. `C=1` keeps the cursor put; `q=2` silences
/// the terminal's ack. Fails (nothing to emit) on zero dimensions, a length
/// that doesn't match `px_w*px_h*4`, or dimensions whose byte size overflows.
pub fn kitty_image(
    rgba: &[u8],
    px_w: u32,
    px_h: u32,
    place: Placement,
) -> Result<Vec<u8>, RasterError> {
    if px_w == 0 || px_h == 0 {
        return Err(RasterError::ZeroDimension);
    }
    let expected = (px_w as usize)
        .checked_mul(px_h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(RasterError::SizeOverflow)?;
    if rgba.len() != expected {
        return Err(RasterError::LengthMismatch {
            expected,
            actual: rgba.len(),
        });
    }
    let payload = base64(rgba);
    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(CHUNK).collect();

    // Place the image at its cell origin (1-based CUP), like the corner relay.
    let mut out = format!(
        "\x1b[{};{}H",
        place.origin_row as usize + 1,
        place.origin_col as usize + 1
    )
    .into_bytes();

    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        out.extend_from_slice(b"\x1b_G");
        if i == 0 {
            // First chunk carries the full control set.
            let ctrl = format!(
                "a=T,f=32,s={px_w},v={px_h},c={},r={},i={PREVIEW_IMAGE_ID},C=1,q=2,m={}",
                place.cols,
                place.rows,
                u8::from(!last)
            );
            out.extend_from_slice(ctrl.as_bytes());
        } else {
            out.extend_from_slice(format!("m={}", u8::from(!last)).as_bytes());
        }
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(ST);
    }
    Ok(out)
}

/// Why a raster cannot be encoded as a kitty image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterError {
    /// Width or height is zero.
    ZeroDimension,
    /// `width*height*4` does not fit in `usize`.
    SizeOverflow,
    /// The pixel buffer length differs from `width*height*4`.
    LengthMismatch { expected: usize, actual: usize },
}

/// Delete only the first-party preview image from the outer terminal (on
/// dismiss / switch / occlusion), leaving corner-relay images intact.
pub fn delete_preview() -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={PREVIEW_IMAGE_ID},q=2;\x1b\\").into_bytes()
}

/// Standard base64 (padded). Small local encoder — the same reason
/// [`crate::copymode`] rolls its own: not worth a dependency.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place() -> Placement {
        Placement {
            origin_row: 3,
            origin_col: 10,
            cols: 20,
            rows: 8,
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_mismatched_buffer() {
        assert_eq!(
            kitty_image(&[0; 3], 1, 1, place()),
            Err(RasterError::LengthMismatch {
                expected: 4,
                actual: 3
            })
        ); // need 4 bytes
        assert_eq!(
            kitty_image(&[], 0, 0, place()),
            Err(RasterError::ZeroDimension)
        );
        assert_eq!(
            kitty_image(&[0; 4], 0, 1, place()),
            Err(RasterError::ZeroDimension)
        );
    }

    #[test]
    fn rejects_overflowing_dimensions_without_panicking() {
        // u32::MAX x u32::MAX x 4 overflows usize on every target width.
        assert_eq!(
            kitty_image(&[0; 4], u32::MAX, u32::MAX, place()),
            Err(RasterError::SizeOverflow)
        );
    }

    #[test]
    fn single_pixel_has_cup_control_and_terminated_apc() {
        let px = [1u8, 2, 3, 4]; // 1x1 RGBA
        let out = kitty_image(&px, 1, 1, place()).unwrap();
        // Leading CUP to (origin_row+1, origin_col+1) = 4;11H
        assert!(out.starts_with(b"\x1b[4;11H"), "leads with CUP to origin");
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(&format!(
            "a=T,f=32,s=1,v=1,c=20,r=8,i={PREVIEW_IMAGE_ID},C=1,q=2,m=0"
        )));
        assert!(out.ends_with(ST), "APC is ST-terminated");
        // exactly one APC (m=0 only, no continuation)
        assert_eq!(s.matches("\x1b_G").count(), 1);
    }

    #[test]
    fn large_image_is_chunked_with_continuation_flags() {
        // 40x40 RGBA = 6400 bytes → base64 ~8536 chars → ≥3 chunks of 4096.
        let rgba = vec![7u8; 40 * 40 * 4];
        let out = kitty_image(&rgba, 40, 40, place()).unwrap();
        let s = String::from_utf8_lossy(&out);
        let apc_count = s.matches("\x1b_G").count();
        assert!(
            apc_count >= 3,
            "chunked into multiple APCs (got {apc_count})"
        );
        // first chunk opens with m=1 (more to come), full control set present
        assert!(s.contains("a=T,f=32,s=40,v=40"));
        assert!(s.contains(",m=1;"), "non-final chunks carry m=1");
        // exactly one final chunk with m=0
        assert_eq!(s.matches("m=0;").count(), 1, "one terminating chunk");
        // continuation chunks carry only m=, not the control set
        assert!(s.contains("\x1b_Gm=1;") || s.contains("\x1b_Gm=0;"));
    }

    #[test]
    fn preview_delete_is_scoped_and_quiet() {
        assert_eq!(
            delete_preview(),
            format!("\x1b_Ga=d,d=I,i={PREVIEW_IMAGE_ID},q=2;\x1b\\").into_bytes()
        );
        assert!(!delete_preview().windows(5).any(|w| w == b"a=d\x1b\\"));
        assert_ne!(delete_preview(), b"\x1b_Ga=d\x1b\\");
        assert_ne!(PREVIEW_IMAGE_ID, 0);
    }
}
