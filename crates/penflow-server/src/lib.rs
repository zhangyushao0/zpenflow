//! Penflow session orchestrator.
//!
//! See `docs/design.md` §9. This crate ties together the engine
//! ([`penflow_core::Engine`]), the transport
//! ([`penflow_transport::Transport`]), and the wire protocol
//! ([`penflow_protocol`]) into a single tokio-driven session loop.

#![deny(missing_docs)]

#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod vdd;

#[cfg(windows)]
pub use session::{Session, SessionConfig, SessionError, SessionEvent};
#[cfg(windows)]
pub use vdd::{VddController, VddError};

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn session_config_default_round_trips() {
        let cfg = super::SessionConfig::default();
        // 120 fps matches the MovinkPad's panel; if you bump the default
        // again, update this assertion too.
        assert_eq!(cfg.fps, 120);
        assert_eq!(cfg.codec, penflow_core::encoder::Codec::Hevc);
        assert!(cfg.bitrate_bps >= 1_000_000);
    }
}
