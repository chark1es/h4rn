//! CLIProxyAPI feature detection and per-harness routing (ADR-015, ADR-016).
//!
//! Capability is probed, never inferred from a version string: `HEAD /healthz`
//! for liveness, a keyless management request for the capability headers, then
//! authenticated route probes. The per-harness base-URL recipes live in
//! [`overrides`].
//!
//! The daemon owns one [`ProxyService`]. Profiles and their metadata live in a
//! JSON file under the daemon data directory; management and client keys live
//! in the daemon secret store and never appear in that file, a log line or a
//! response.

#![deny(missing_docs)]

mod error;
mod failover;
mod limits;
mod models;
mod overrides;
mod performance;
mod probe;
mod profiles;
mod secret;
mod service;

pub use error::{ProxyError, ProxyErrorKind};
pub use failover::pick_failover;
pub use limits::read_limits;
pub use models::list_models;
pub use overrides::{
    CODEX_KEY_ENV, HarnessOverride, ProxyTarget, anthropic_base, for_harness, gemini_base,
    openai_base, provider_id, resolve_model, root_base,
};
pub use performance::{PerformanceCall, read_performance};
pub use probe::{ProbeTarget, probe};
pub use profiles::{
    ActiveOverride, ProfileStore, ResolvedProfile, harness_kinds, recipe_note, recipe_verified,
    validate_base_url,
};
pub use secret::{MemorySecretStore, SecretStore, SecretValue, secret_backend_error};
pub use service::{AppliedOverride, DEFAULT_CAPABILITY_TTL, ProxyService, proxy_only_model};

/// Crate name, used by startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-proxy";
