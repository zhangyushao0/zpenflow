//! Pen-activity idle governor.
//!
//! The pipeline encodes a frame every tick even when DDA reports no screen
//! change (the keepalive re-encode), so a 120 fps session keeps the hardware
//! encoder ~fully busy on a completely static desktop. That is wasted power
//! and, on laptops, real heat.
//!
//! This module lowers the tick rate when the pen has been out of range for a
//! while, and restores it the instant pen or touch traffic resumes:
//!
//!   - ACTIVE: any pen/touch event in the last `idle_after_ms` → the loop
//!     runs exactly as before (no behaviour change; the sleep helper returns
//!     `None`).
//!   - IDLE: no input for `idle_after_ms` → the loop is paced down to
//!     `idle_fps` by sleeping out the remainder of each frame period. The
//!     stream keeps flowing, so the tablet stays usable as a passive
//!     second screen — video still plays, just at the idle rate.
//!
//! Wake-up latency is bounded by one idle frame period (33 ms at 30 fps).
//! EMR pens report hover well before the tip lands, and hovering generates
//! a continuous event stream, so the session is back at full rate before
//! the first stroke starts.
//!
//! Config via env (read once at engine build):
//!   PENFLOW_IDLE_AFTER_MS  default 10000; 0 disables idling entirely.
//!   PENFLOW_IDLE_FPS       default 30;    0 disables idling entirely.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Shared "when did the user last touch/hover the tablet" signal.
///
/// The session's input read loop calls [`ActivityTracker::touch`] on every
/// pen and touch event; the pipeline thread polls
/// [`ActivityTracker::ms_since_activity`] once per tick. Millisecond
/// granularity in a `u64` is decades of uptime, and both sides measure
/// against the same private epoch, so no cross-thread clock agreement is
/// needed beyond the atomic.
pub struct ActivityTracker {
    epoch: Instant,
    last_ms: AtomicU64,
}

impl ActivityTracker {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            // Session start counts as activity: connect at full rate.
            last_ms: AtomicU64::new(0),
        }
    }

    /// Record input activity "now". Called from the session's read loop on
    /// every pen/touch event; cheap enough for per-event use (one atomic
    /// store).
    pub fn touch(&self) {
        self.last_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Release);
    }

    /// Milliseconds since the last recorded activity.
    pub fn ms_since_activity(&self) -> u64 {
        let now = self.epoch.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_ms.load(Ordering::Acquire))
    }
}

/// Idle-governor tunables, resolved once at engine build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleConfig {
    /// Input silence required before throttling. 0 = never idle.
    pub idle_after_ms: u64,
    /// Tick rate while idle. 0 = never idle.
    pub idle_fps: u32,
}

impl IdleConfig {
    pub const DEFAULT_AFTER_MS: u64 = 10_000;
    pub const DEFAULT_FPS: u32 = 30;

    pub fn disabled() -> Self {
        Self {
            idle_after_ms: 0,
            idle_fps: 0,
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_strings(
            std::env::var("PENFLOW_IDLE_AFTER_MS").ok().as_deref(),
            std::env::var("PENFLOW_IDLE_FPS").ok().as_deref(),
        )
    }

    /// Pure parsing core so it is unit-testable without process-global env
    /// mutation. Unparseable values fall back to the defaults rather than
    /// disabling the feature — a typo should not silently cost the user
    /// their battery.
    pub fn from_env_strings(after: Option<&str>, fps: Option<&str>) -> Self {
        let idle_after_ms = after
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(Self::DEFAULT_AFTER_MS);
        let idle_fps = fps
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(Self::DEFAULT_FPS);
        Self {
            idle_after_ms,
            idle_fps,
        }
    }

    pub fn enabled(&self) -> bool {
        self.idle_after_ms > 0 && self.idle_fps > 0
    }
}

/// Decide how long the pipeline should sleep after a tick.
///
/// Returns `None` when the loop must run at full rate (active, disabled, or
/// the tick already consumed the idle frame period), otherwise the number of
/// milliseconds to sleep so ticks pace out to `idle_fps`.
///
/// Pure function of its inputs so the policy is testable off-Windows.
pub fn throttle_sleep_ms(
    cfg: IdleConfig,
    ms_since_activity: u64,
    tick_elapsed_ms: u64,
) -> Option<u64> {
    if !cfg.enabled() {
        return None;
    }
    if ms_since_activity < cfg.idle_after_ms {
        return None; // recent input → full rate
    }
    let period_ms = 1000 / cfg.idle_fps.max(1) as u64;
    let remaining = period_ms.saturating_sub(tick_elapsed_ms);
    if remaining == 0 {
        None // tick was already slower than the idle rate
    } else {
        Some(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: IdleConfig = IdleConfig {
        idle_after_ms: 10_000,
        idle_fps: 30,
    };

    #[test]
    fn active_session_is_never_throttled() {
        assert_eq!(throttle_sleep_ms(CFG, 0, 2), None);
        assert_eq!(throttle_sleep_ms(CFG, 9_999, 2), None);
    }

    #[test]
    fn idle_session_paces_to_idle_fps() {
        // 30 fps → 33 ms period; an 8 ms tick sleeps the remaining 25 ms.
        assert_eq!(throttle_sleep_ms(CFG, 10_000, 8), Some(25));
        // Instant tick sleeps a full period.
        assert_eq!(throttle_sleep_ms(CFG, 60_000, 0), Some(33));
    }

    #[test]
    fn slow_tick_is_not_slept_further() {
        // Tick took longer than the idle period (heavy encode) → no sleep.
        assert_eq!(throttle_sleep_ms(CFG, 60_000, 40), None);
        assert_eq!(throttle_sleep_ms(CFG, 60_000, 33), None);
    }

    #[test]
    fn zero_disables() {
        let off_a = IdleConfig {
            idle_after_ms: 0,
            idle_fps: 30,
        };
        let off_b = IdleConfig {
            idle_after_ms: 10_000,
            idle_fps: 0,
        };
        assert_eq!(throttle_sleep_ms(off_a, u64::MAX, 0), None);
        assert_eq!(throttle_sleep_ms(off_b, u64::MAX, 0), None);
        assert_eq!(throttle_sleep_ms(IdleConfig::disabled(), u64::MAX, 0), None);
    }

    #[test]
    fn wake_is_immediate_after_touch() {
        // The tracker reports small ms_since_activity right after touch();
        // policy must return to full rate on the very next tick.
        let t = ActivityTracker::new();
        t.touch();
        assert!(t.ms_since_activity() < CFG.idle_after_ms);
        assert_eq!(throttle_sleep_ms(CFG, t.ms_since_activity(), 2), None);
    }

    #[test]
    fn env_parsing_defaults_and_overrides() {
        let d = IdleConfig::from_env_strings(None, None);
        assert_eq!(
            d,
            IdleConfig {
                idle_after_ms: IdleConfig::DEFAULT_AFTER_MS,
                idle_fps: IdleConfig::DEFAULT_FPS
            }
        );
        let c = IdleConfig::from_env_strings(Some("5000"), Some("20"));
        assert_eq!(
            c,
            IdleConfig {
                idle_after_ms: 5000,
                idle_fps: 20
            }
        );
        // 0 disables; garbage falls back to defaults instead of disabling.
        assert!(!IdleConfig::from_env_strings(Some("0"), Some("30")).enabled());
        let g = IdleConfig::from_env_strings(Some("banana"), Some(""));
        assert_eq!(g.idle_after_ms, IdleConfig::DEFAULT_AFTER_MS);
        assert_eq!(g.idle_fps, IdleConfig::DEFAULT_FPS);
    }

    #[test]
    fn tracker_counts_up_between_touches() {
        let t = ActivityTracker::new();
        let a = t.ms_since_activity();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let b = t.ms_since_activity();
        assert!(b >= a + 10, "elapsed should grow: {a} -> {b}");
        t.touch();
        assert!(t.ms_since_activity() < 10);
    }
}
