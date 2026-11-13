//! Harness selection.
//!
//! The daemon talks to [`h4rn_harness::Harness`] only; it never knows how a
//! vendor protocol works. Adapters are supplied through an injectable catalog so
//! tests use `FakeHarness` and production can plug in the registry the harness
//! crate grows. When the real registry lands this module is the single place
//! that adapts it.

use std::collections::HashMap;
use std::sync::Arc;

use h4rn_harness::{Harness, HarnessInstall};
use h4rn_protocol::HarnessKind;

/// The set of adapters this daemon can start.
pub trait HarnessCatalog: Send + Sync {
    /// Returns the adapter for `kind` when it is registered.
    fn get(&self, kind: HarnessKind) -> Option<Arc<dyn Harness>>;

    /// Harness kinds this build can run.
    fn available(&self) -> Vec<HarnessKind>;

    /// Detection information for one harness, when registered and installed.
    fn detect(&self, kind: HarnessKind) -> Option<HarnessInstall>;
}

/// A deterministic catalog built from registered adapters.
#[derive(Clone, Default)]
pub struct StaticHarnessCatalog {
    harnesses: HashMap<HarnessKind, Arc<dyn Harness>>,
}

impl StaticHarnessCatalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// A catalog with one adapter.
    pub fn with(harness: Arc<dyn Harness>) -> Self {
        let mut catalog = Self::new();
        catalog.register(harness);
        catalog
    }

    /// Registers (or replaces) an adapter.
    pub fn register(&mut self, harness: Arc<dyn Harness>) {
        self.harnesses.insert(harness.kind(), harness);
    }

    /// Builder form of [`StaticHarnessCatalog::register`].
    pub fn and(mut self, harness: Arc<dyn Harness>) -> Self {
        self.register(harness);
        self
    }
}

impl HarnessCatalog for StaticHarnessCatalog {
    fn get(&self, kind: HarnessKind) -> Option<Arc<dyn Harness>> {
        self.harnesses.get(&kind).cloned()
    }

    fn available(&self) -> Vec<HarnessKind> {
        self.harnesses.keys().copied().collect()
    }

    fn detect(&self, kind: HarnessKind) -> Option<HarnessInstall> {
        self.harnesses
            .get(&kind)
            .and_then(|harness| harness.detect())
    }
}

/// Stable iteration order for harness kinds in diagnostics and host summaries.
pub fn all_harness_kinds() -> [HarnessKind; 4] {
    [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Opencode,
        HarnessKind::Antigravity,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_harness::FakeHarness;

    #[test]
    fn catalog_exposes_registered_harnesses() {
        let catalog = StaticHarnessCatalog::new().and(Arc::new(FakeHarness::new()));
        assert_eq!(catalog.available(), vec![HarnessKind::ClaudeCode]);
        assert!(catalog.get(HarnessKind::ClaudeCode).is_some());
        assert!(catalog.get(HarnessKind::Codex).is_none());
        assert!(catalog.detect(HarnessKind::ClaudeCode).is_some());
    }

    #[test]
    fn undetected_harness_reports_none() {
        let catalog = StaticHarnessCatalog::with(Arc::new(FakeHarness::undetected()));
        assert!(catalog.get(HarnessKind::ClaudeCode).is_some());
        assert!(catalog.detect(HarnessKind::ClaudeCode).is_none());
    }
}
