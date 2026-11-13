//! The adapter registry the daemon resolves harnesses through.
//!
//! Phase 1b ships the Claude Code adapter and Phase 2 adds Antigravity. Codex
//! and OpenCode become entries here as their adapters land; the daemon must
//! never match on a concrete adapter type, only on
//! [`h4rn_protocol::HarnessKind`].

use std::sync::Arc;

use h4rn_protocol::HarnessKind;

use crate::{
    AntigravityHarness, ClaudeCodeHarness, CodexHarness, Harness, HarnessInstall, OpenCodeHarness,
};

/// Returns one adapter instance per built-in harness.
///
/// Detected installs are resolved lazily by [`Harness::detect`], so the
/// registry is cheap to build at daemon start even when nothing is installed.
pub fn builtin_adapters() -> Vec<Arc<dyn Harness>> {
    vec![
        Arc::new(ClaudeCodeHarness::new()),
        Arc::new(CodexHarness::new()),
        Arc::new(OpenCodeHarness::new()),
        Arc::new(AntigravityHarness::new()),
    ]
}

/// The set of adapters the daemon can pick from.
#[derive(Clone)]
pub struct HarnessRegistry {
    adapters: Vec<Arc<dyn Harness>>,
}

impl HarnessRegistry {
    /// Builds a registry containing every built-in adapter.
    pub fn new() -> Self {
        Self {
            adapters: builtin_adapters(),
        }
    }

    /// Builds a registry from an explicit adapter list (tests, plugins).
    pub fn with_adapters(adapters: Vec<Arc<dyn Harness>>) -> Self {
        Self { adapters }
    }

    /// All registered adapters, in registration order.
    pub fn all(&self) -> &[Arc<dyn Harness>] {
        &self.adapters
    }

    /// Looks up the adapter for one harness kind.
    pub fn get(&self, kind: HarnessKind) -> Option<Arc<dyn Harness>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.kind() == kind)
            .cloned()
    }

    /// The harness kinds with a usable installation on this host.
    pub fn detected(&self) -> Vec<(HarnessKind, HarnessInstall)> {
        self.adapters
            .iter()
            .filter_map(|adapter| adapter.detect().map(|install| (adapter.kind(), install)))
            .collect()
    }

    /// The adapter for `kind`, but only when it is installed.
    pub fn detected_adapter(&self, kind: HarnessKind) -> Option<Arc<dyn Harness>> {
        self.get(kind).filter(|adapter| adapter.detect().is_some())
    }
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_exposes_claude_code() {
        let registry = HarnessRegistry::new();
        let adapter = registry
            .get(HarnessKind::ClaudeCode)
            .expect("claude adapter");
        assert_eq!(adapter.kind(), HarnessKind::ClaudeCode);
        assert!(registry.get(HarnessKind::Antigravity).is_some());
    }

    #[test]
    fn the_registry_exposes_codex() {
        let registry = HarnessRegistry::new();
        let adapter = registry.get(HarnessKind::Codex).expect("codex adapter");
        assert_eq!(adapter.kind(), HarnessKind::Codex);
    }

    #[test]
    fn the_registry_exposes_opencode() {
        let registry = HarnessRegistry::new();
        let adapter = registry
            .get(HarnessKind::Opencode)
            .expect("opencode adapter");
        assert_eq!(adapter.kind(), HarnessKind::Opencode);
    }
}
