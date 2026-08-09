//! Wacom-style pen pressure response curve.
//!
//! Raw tablet pressure `[0, 1]` is reshaped before injection:
//!
//! ```text
//!   out(p) = 0                                    for p <= threshold
//!   out(p) = (min(1, (p - threshold) / (max - threshold)))^gamma
//! ```
//!
//! The three parameters map one-to-one onto the classic Wacom "Pen Feel"
//! dialog:
//!
//!   - `threshold` — "Click threshold": pressure below this is treated as
//!     hover. Filters accidental feather-light contacts.
//!   - `max` — "Max pressure": the input level at which output saturates.
//!     Lowering it means you reach full pressure without pressing hard.
//!   - `gamma` — "Sensitivity": bends the curve. gamma < 1 makes light
//!     touches count for more (soft feel); gamma > 1 demands more force
//!     for the same output (firm feel); 1.0 is linear.
//!
//! Applied once, PC-side, where `PenSample` is built — so both injection
//! backends (VMulti HID and the synthetic-pointer fallback) see identical
//! response, and the Android client stays untouched.
//!
//! When the curve outputs 0 while the tip physically touches, the sample
//! is demoted to hover — that IS the click-threshold behaviour: a graze
//! that doesn't cross the threshold must not click, exactly as Wacom's
//! driver does it.

/// Reshapes raw pressure. Construct via [`PressureCurve::new`], which
/// clamps parameters into sane ranges so a corrupted settings file can
/// never produce a dead pen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PressureCurve {
    threshold: f32,
    max: f32,
    gamma: f32,
}

pub const MIN_THRESHOLD: f32 = 0.0;
pub const MAX_THRESHOLD: f32 = 0.5;
pub const MIN_MAX: f32 = 0.5;
pub const MAX_MAX: f32 = 1.0;
pub const MIN_GAMMA: f32 = 0.2;
pub const MAX_GAMMA: f32 = 5.0;

impl Default for PressureCurve {
    /// Identity: threshold 0, max 1, gamma 1 — byte-for-byte the pre-curve
    /// behaviour.
    fn default() -> Self {
        Self {
            threshold: 0.0,
            max: 1.0,
            gamma: 1.0,
        }
    }
}

impl PressureCurve {
    pub fn new(threshold: f32, max: f32, gamma: f32) -> Self {
        // NaN slips through `clamp` (NaN.clamp(..) == NaN), so sanitize
        // non-finite params to the identity values first.
        let threshold = if threshold.is_finite() { threshold } else { 0.0 };
        let max = if max.is_finite() { max } else { 1.0 };
        let threshold = threshold.clamp(MIN_THRESHOLD, MAX_THRESHOLD);
        // Keep a usable ramp: max must sit meaningfully above threshold.
        let max = max.clamp((threshold + 0.05).max(MIN_MAX), MAX_MAX);
        let gamma = if gamma.is_finite() {
            gamma.clamp(MIN_GAMMA, MAX_GAMMA)
        } else {
            1.0
        };
        Self {
            threshold,
            max,
            gamma,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.threshold == 0.0 && self.max == 1.0 && self.gamma == 1.0
    }

    /// Reshape one raw pressure sample. Output is `[0, 1]`; monotonic in
    /// the input; exactly 0 at/below the threshold and exactly 1 at/above
    /// `max`.
    pub fn apply(&self, raw: f32) -> f32 {
        let p = if raw.is_finite() {
            raw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if p <= self.threshold {
            return 0.0;
        }
        let ramp = ((p - self.threshold) / (self.max - self.threshold)).min(1.0);
        if self.gamma == 1.0 {
            ramp
        } else {
            ramp.powf(self.gamma)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passes_through() {
        let c = PressureCurve::default();
        assert!(c.is_identity());
        for i in 0..=100 {
            let p = i as f32 / 100.0;
            assert!((c.apply(p) - p).abs() < 1e-6, "identity broke at {p}");
        }
    }

    #[test]
    fn threshold_cuts_light_touches() {
        let c = PressureCurve::new(0.15, 1.0, 1.0);
        assert_eq!(c.apply(0.0), 0.0);
        assert_eq!(c.apply(0.15), 0.0);
        assert!(c.apply(0.16) > 0.0);
        // Full pressure still reaches 1.
        assert!((c.apply(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn max_saturates_early() {
        let c = PressureCurve::new(0.0, 0.7, 1.0);
        assert!((c.apply(0.7) - 1.0).abs() < 1e-6);
        assert!((c.apply(0.9) - 1.0).abs() < 1e-6);
        assert!((c.apply(0.35) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn gamma_bends_but_keeps_endpoints() {
        for g in [0.4, 0.7, 1.5, 3.0] {
            let c = PressureCurve::new(0.0, 1.0, g);
            assert_eq!(c.apply(0.0), 0.0);
            assert!((c.apply(1.0) - 1.0).abs() < 1e-6);
            let mid = c.apply(0.5);
            if g < 1.0 {
                assert!(mid > 0.5, "soft gamma {g} should lift mid, got {mid}");
            } else {
                assert!(mid < 0.5, "firm gamma {g} should lower mid, got {mid}");
            }
        }
    }

    #[test]
    fn monotonic_for_all_parameter_combos() {
        for &t in &[0.0, 0.1, 0.3, 0.5] {
            for &m in &[0.5, 0.7, 1.0] {
                for &g in &[0.2, 0.5, 1.0, 2.0, 5.0] {
                    let c = PressureCurve::new(t, m, g);
                    let mut prev = -1.0f32;
                    for i in 0..=200 {
                        let out = c.apply(i as f32 / 200.0);
                        assert!(
                            out >= prev - 1e-6,
                            "non-monotonic at t={t} m={m} g={g}"
                        );
                        assert!((0.0..=1.0).contains(&out));
                        prev = out;
                    }
                }
            }
        }
    }

    #[test]
    fn hostile_params_are_clamped_never_dead() {
        // A corrupted settings file must not produce a pen that can't
        // reach full pressure or never leaves zero.
        for c in [
            PressureCurve::new(9.0, -3.0, 0.0),
            PressureCurve::new(f32::NAN, f32::INFINITY, f32::NAN),
            PressureCurve::new(0.5, 0.5, 100.0),
        ] {
            assert_eq!(c.apply(0.0), 0.0);
            assert!((c.apply(1.0) - 1.0).abs() < 1e-6, "{c:?} cannot saturate");
        }
        // Hostile input samples.
        let c = PressureCurve::default();
        assert_eq!(c.apply(f32::NAN), 0.0);
        assert_eq!(c.apply(-5.0), 0.0);
        assert!((c.apply(7.0) - 1.0).abs() < 1e-6);
    }
}
