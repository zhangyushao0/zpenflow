//! Penflow session orchestrator.
//!
//! See `docs/design.md` §9. This crate ties together the engine, transport,
//! and protocol layers into a single tokio-driven session loop.

#![deny(missing_docs)]

/// Returns a build identifier string. Used as a cross-crate sanity test
/// until the real session loop lands.
pub fn build_id() -> &'static str {
    "penflow-server v0.1.0 (pre-session)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_can_reference_engine_types() {
        assert!(penflow_core::build_id().contains("penflow-core"));
        assert!(build_id().contains("penflow-server"));
    }
}
