//! Penflow capture + encode + inject engine.
//!
//! See `docs/design.md` §6 for the architecture. The public surface is
//! deliberately minimal at this stage — concrete implementations land
//! incrementally as work proceeds.

/// Returns a build identifier string. Used as a cross-crate sanity test
/// until the real `Engine` API lands.
pub fn build_id() -> &'static str {
    "penflow-core v0.1.0 (pre-engine)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_present() {
        assert!(build_id().starts_with("penflow-core"));
    }
}
