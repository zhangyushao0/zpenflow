//! Input-area → output-area coordinate transform.
//!
//! Replaces the predecessor's naive `left + norm * width` with a 2D affine
//! transform — design.md §6.6 says "Implemented as a single Matrix3x2 so
//! future 'rotate the tablet 90°' is one parameter". Using a hand-rolled
//! 6-float affine instead of pulling in `nalgebra` for one matrix, since
//! v1.0 doesn't ship rotation; the math is identical and the form swaps in
//! cleanly if/when rotation lands.
//!
//! Coordinate convention:
//!   - Pen samples arrive normalized to [0, 1] × [0, 1] over the **input
//!     area** (the Android tablet panel, after dead-zone trimming).
//!   - Output is virtual-screen pixels (after `SetProcessDpiAwarenessContext`
//!     so they're physical pixels, not DIPs — gate-2 finding §4.4b).

#[derive(Clone, Copy, Debug)]
pub struct AffineTransform {
    // 2D affine, row-major:
    //   [ a  c  e ]   [ x ]
    //   [ b  d  f ] * [ y ]
    //   [ 0  0  1 ]   [ 1 ]
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl AffineTransform {
    pub fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Build a transform that maps `[0, 1] × [0, 1]` (raw normalized pen
    /// coordinates) onto the output rectangle, with optional rotation in
    /// 90-degree steps applied to the input first.
    ///
    /// `rotation_deg` is one of 0 / 90 / 180 / 270; other values fall back
    /// to 0 (we don't do arbitrary rotation in v1.0 — the tablet ships in
    /// landscape and Krita's portrait path goes through Krita rotation, not
    /// ours).
    pub fn from_normalized_to_rect(
        output_left: i32,
        output_top: i32,
        output_w: u32,
        output_h: u32,
        rotation_deg: u32,
    ) -> Self {
        let ow = output_w as f32;
        let oh = output_h as f32;
        let ol = output_left as f32;
        let ot = output_top as f32;
        match rotation_deg % 360 {
            0 => Self {
                a: ow,
                b: 0.0,
                c: 0.0,
                d: oh,
                e: ol,
                f: ot,
            },
            90 => Self {
                // (x, y) → (oh - y * oh, x * ow) then translate. Equivalent
                // affine: x' = -oh * y + ol + ow ;  y' = ow * x + ot
                a: 0.0,
                b: ow,
                c: -ow,
                d: 0.0,
                e: ol + ow,
                f: ot,
            },
            180 => Self {
                a: -ow,
                b: 0.0,
                c: 0.0,
                d: -oh,
                e: ol + ow,
                f: ot + oh,
            },
            270 => Self {
                a: 0.0,
                b: -oh,
                c: ow,
                d: 0.0,
                e: ol,
                f: ot + oh,
            },
            _ => Self::from_normalized_to_rect(output_left, output_top, output_w, output_h, 0),
        }
    }

    /// Apply the transform to a single point.
    pub fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Apply and snap to integer pixels (the Win32 / WinRT injection APIs
    /// take `i32` coordinates).
    pub fn map_to_pixel(&self, x: f32, y: f32) -> (i32, i32) {
        let (fx, fy) = self.map(x, y);
        (fx.round() as i32, fy.round() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: (f32, f32), b: (f32, f32), eps: f32) -> bool {
        (a.0 - b.0).abs() < eps && (a.1 - b.1).abs() < eps
    }

    #[test]
    fn identity_is_passthrough() {
        let t = AffineTransform::identity();
        assert!(approx(t.map(0.5, 0.7), (0.5, 0.7), 1e-6));
    }

    #[test]
    fn maps_corners_for_rect() {
        let t = AffineTransform::from_normalized_to_rect(100, 200, 1920, 1080, 0);
        assert_eq!(t.map_to_pixel(0.0, 0.0), (100, 200));
        assert_eq!(t.map_to_pixel(1.0, 0.0), (2020, 200));
        assert_eq!(t.map_to_pixel(0.0, 1.0), (100, 1280));
        assert_eq!(t.map_to_pixel(1.0, 1.0), (2020, 1280));
        assert_eq!(t.map_to_pixel(0.5, 0.5), (1060, 740));
    }

    #[test]
    fn rotates_90_landscape_to_portrait() {
        // Input area [0,1]² rotated 90° onto a 100×200 output rect at origin.
        // Top-left of input (0,0) should land at top-right of output (100,0).
        let t = AffineTransform::from_normalized_to_rect(0, 0, 100, 200, 90);
        assert_eq!(t.map_to_pixel(0.0, 0.0), (100, 0));
        assert_eq!(t.map_to_pixel(1.0, 0.0), (100, 100));
        assert_eq!(t.map_to_pixel(0.0, 1.0), (0, 0));
        assert_eq!(t.map_to_pixel(1.0, 1.0), (0, 100));
    }
}
