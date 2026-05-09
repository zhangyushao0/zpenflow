//! Cursor shape cache for the DDA-side compositor.
//!
//! `IDXGIOutputDuplication::GetFramePointerShape` only returns shape data
//! when the OS reports a shape change; most frames just update position.
//! `decode_shape` normalises the three DDA shape kinds into a single BGRA
//! buffer the GPU compositor can sample uniformly:
//!
//!   - `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR` (0x2): BGRA premultiplied
//!     alpha, copied through.
//!   - `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR` (0x4): legacy XOR/key
//!     format. Pixels with alpha=0x00 are opaque (BGR shown as-is, alpha
//!     forced to 0xFF). Pixels with alpha=0xFF are XOR-with-screen and we
//!     render them transparent — losing the overlay effect (rare in modern
//!     cursors) but keeping the silhouette right.
//!   - `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME` (0x1): 1bpp AND mask
//!     stacked on top of 1bpp XOR mask. The DXGI buffer's effective image
//!     height is `info.Height / 2` (the lower half is the XOR mask). Same
//!     XOR caveat as above — XORed pixels render transparent.

use crate::error::EngineResult;

/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*` values we recognise. Numeric layout
/// matches the Microsoft constants exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    Monochrome = 1,
    Color = 2,
    MaskedColor = 4,
}

impl ShapeKind {
    fn from_raw(t: u32) -> Option<Self> {
        match t {
            1 => Some(Self::Monochrome),
            2 => Some(Self::Color),
            4 => Some(Self::MaskedColor),
            _ => None,
        }
    }
}

/// One decoded cursor shape, normalised to premultiplied BGRA.
///
/// `width × height` is the *visible* image size — for monochrome that is
/// the original `info.Height / 2`, not the raw buffer height. `pixels`
/// is `width * height * 4` bytes, tightly packed (pitch = width*4).
pub struct CursorShape {
    pub kind: ShapeKind,
    pub width: u32,
    pub height: u32,
    /// Hot-spot offset into the bitmap (`info.HotSpot`). Caller subtracts
    /// these from the cursor screen position to find the bitmap's top-left.
    pub hot_x: i32,
    pub hot_y: i32,
    /// Tightly-packed BGRA (B, G, R, A) bytes, premultiplied alpha.
    pub pixels: Vec<u8>,
}

/// Decode one DDA shape buffer into `CursorShape`.
///
/// `raw` is the buffer whose layout depends on `kind`:
/// - Color/MaskedColor: `info.Pitch` bytes per row, `info.Height` rows.
/// - Monochrome: `info.Pitch` bytes per row, `info.Height` rows TOTAL,
///   where the bottom half is the XOR mask.
pub fn decode_shape(
    kind_raw: u32,
    width: u32,
    height_raw: u32,
    pitch: u32,
    hot_x: i32,
    hot_y: i32,
    raw: &[u8],
) -> EngineResult<CursorShape> {
    use crate::error::EngineError;
    let kind = ShapeKind::from_raw(kind_raw).ok_or_else(|| {
        EngineError::Win32(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ))
    })?;
    if width == 0 || height_raw == 0 {
        return Err(EngineError::Win32(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        )));
    }

    match kind {
        ShapeKind::Color => decode_color(width, height_raw, pitch, hot_x, hot_y, raw),
        ShapeKind::MaskedColor => decode_masked_color(width, height_raw, pitch, hot_x, hot_y, raw),
        ShapeKind::Monochrome => decode_monochrome(width, height_raw, pitch, hot_x, hot_y, raw),
    }
}

fn decode_color(
    width: u32,
    height: u32,
    pitch: u32,
    hot_x: i32,
    hot_y: i32,
    raw: &[u8],
) -> EngineResult<CursorShape> {
    let row_bytes = (width as usize) * 4;
    let pitch = pitch as usize;
    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let off = y * pitch;
        let end = off
            .checked_add(row_bytes)
            .filter(|e| *e <= raw.len())
            .ok_or_else(|| short_buffer_err(raw.len(), off + row_bytes))?;
        pixels.extend_from_slice(&raw[off..end]);
    }
    Ok(CursorShape {
        kind: ShapeKind::Color,
        width,
        height,
        hot_x,
        hot_y,
        pixels,
    })
}

fn decode_masked_color(
    width: u32,
    height: u32,
    pitch: u32,
    hot_x: i32,
    hot_y: i32,
    raw: &[u8],
) -> EngineResult<CursorShape> {
    let row_bytes = (width as usize) * 4;
    let pitch = pitch as usize;
    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let off = y * pitch;
        let end = off
            .checked_add(row_bytes)
            .filter(|e| *e <= raw.len())
            .ok_or_else(|| short_buffer_err(raw.len(), off + row_bytes))?;
        let row = &raw[off..end];
        for chunk in row.chunks_exact(4) {
            let (b, g, r, a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
            // alpha=0x00: opaque, draw the BGR with full opacity.
            // alpha=0xFF: XOR with screen — we approximate as transparent.
            // PMA: opaque pixels become (B, G, R, 255).
            if a == 0x00 {
                pixels.extend_from_slice(&[b, g, r, 0xFF]);
            } else {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Ok(CursorShape {
        kind: ShapeKind::MaskedColor,
        width,
        height,
        hot_x,
        hot_y,
        pixels,
    })
}

fn decode_monochrome(
    width: u32,
    height_raw: u32,
    pitch: u32,
    hot_x: i32,
    hot_y: i32,
    raw: &[u8],
) -> EngineResult<CursorShape> {
    // Buffer holds `Height` rows TOTAL; first half is AND mask, second
    // half is XOR mask. The presented image height is therefore Height/2.
    if height_raw % 2 != 0 {
        return Err(short_buffer_err(0, 0)); // malformed; height must be even
    }
    let height = height_raw / 2;
    let pitch = pitch as usize;
    // AND mask occupies `height * pitch` bytes from offset 0.
    // XOR mask occupies `height * pitch` bytes from offset `height * pitch`.
    let xor_off = height as usize * pitch;
    if raw.len() < xor_off * 2 {
        return Err(short_buffer_err(raw.len(), xor_off * 2));
    }
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let byte_idx = x / 8;
            let bit_idx = 7 - (x % 8);
            let mask = 1u8 << bit_idx;
            let and_bit = (raw[y * pitch + byte_idx] & mask) != 0;
            let xor_bit = (raw[xor_off + y * pitch + byte_idx] & mask) != 0;
            // Mapping (matches GDI semantics, with XOR=transparent fallback):
            //   AND=0, XOR=0 → black, opaque
            //   AND=0, XOR=1 → white, opaque
            //   AND=1, XOR=0 → transparent (background)
            //   AND=1, XOR=1 → XOR with screen (we render transparent v1)
            let bgra = match (and_bit, xor_bit) {
                (false, false) => [0, 0, 0, 0xFF],
                (false, true) => [0xFF, 0xFF, 0xFF, 0xFF],
                (true, false) => [0, 0, 0, 0],
                (true, true) => [0, 0, 0, 0],
            };
            pixels.extend_from_slice(&bgra);
        }
    }
    Ok(CursorShape {
        kind: ShapeKind::Monochrome,
        width,
        height,
        hot_x,
        hot_y,
        pixels,
    })
}

fn short_buffer_err(have: usize, need: usize) -> crate::error::EngineError {
    let _ = (have, need);
    crate::error::EngineError::Win32(windows::core::Error::from_hresult(
        windows::Win32::Foundation::E_INVALIDARG,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_color_strips_pitch_padding() {
        // 2×2 image with pitch=12 (4 px worth) — only first 8 bytes per row valid.
        let raw = [
            0xAA, 0xBB, 0xCC, 0xFF, 0x11, 0x22, 0x33, 0xFF, 0, 0, 0, 0, // row 0 padded
            0x44, 0x55, 0x66, 0xFF, 0x77, 0x88, 0x99, 0xFF, 0, 0, 0, 0, // row 1 padded
        ];
        let shape = decode_shape(2, 2, 2, 12, 0, 0, &raw).unwrap();
        assert_eq!(shape.pixels.len(), 16);
        assert_eq!(&shape.pixels[..4], &[0xAA, 0xBB, 0xCC, 0xFF]);
        assert_eq!(&shape.pixels[4..8], &[0x11, 0x22, 0x33, 0xFF]);
        assert_eq!(&shape.pixels[8..12], &[0x44, 0x55, 0x66, 0xFF]);
        assert_eq!(&shape.pixels[12..16], &[0x77, 0x88, 0x99, 0xFF]);
    }

    #[test]
    fn decode_masked_color_alpha_semantics() {
        // 1×1 with alpha=0x00 → opaque red.
        let raw = [0x00, 0x00, 0xFF, 0x00];
        let shape = decode_shape(4, 1, 1, 4, 0, 0, &raw).unwrap();
        assert_eq!(&shape.pixels, &[0x00, 0x00, 0xFF, 0xFF]);
        // 1×1 with alpha=0xFF → transparent (XOR fallback).
        let raw = [0x00, 0x00, 0xFF, 0xFF];
        let shape = decode_shape(4, 1, 1, 4, 0, 0, &raw).unwrap();
        assert_eq!(&shape.pixels, &[0, 0, 0, 0]);
    }

    #[test]
    fn decode_monochrome_classic_arrow_corner() {
        // 8×4 image: AND mask (4 rows) + XOR mask (4 rows). Pitch=1 byte/row.
        // AND row 0: 0b00000000 → all opaque
        // XOR row 0: 0b11110000 → first 4 white, last 4 black
        // AND row 1: 0b11111111 → all transparent (background)
        let raw = [
            0x00, 0x00, 0x00, 0x00, // AND
            0xF0, 0x00, 0x00, 0x00, // XOR
        ];
        let shape = decode_shape(1, 8, 8, 1, 0, 0, &raw).unwrap();
        assert_eq!(shape.height, 4);
        // Row 0: 4 white, 4 black, all opaque
        assert_eq!(&shape.pixels[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&shape.pixels[12..16], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&shape.pixels[16..20], &[0, 0, 0, 0xFF]);
        assert_eq!(&shape.pixels[28..32], &[0, 0, 0, 0xFF]);
    }
}
