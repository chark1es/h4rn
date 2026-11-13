//! The daemon-facing proxy service.
//!
//! Owns profile metadata, capability and catalogue caches, and the event
//! broadcast. Capabilities are cached for a short TTL and invalidated whenever
//! a request fails, so a proxy restart or a flipped management flag is noticed
//! without polling.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use h4rn_protocol::{
    HarnessKind, ProxyCapabilities, ProxyEvent, ProxyFailoverChoice, ProxyFailoverPolicy,
    ProxyLimits, ProxyModel, ProxyModelCatalog, ProxyProfile, ProxyProfileDraft, ProxyProfileId,
    ProxyProfilePatch, ProxyTestReport,
};
use reqwest::Client;
use tokio::sync::broadcast;

use crate::error::{ProxyError, ProxyErrorKind};
use crate::failover::pick_failover;
use crate::limits::read_limits;
use crate::models::list_models;
use crate::overrides::{HarnessOverride, ProxyTarget, for_harness};
use crate::probe::ProbeTarget;
use crate::profiles::{ProfileStore, ResolvedProfile};
use crate::secret::SecretStore;

/// Default capability cache lifetime.
pub const DEFAULT_CAPABILITY_TTL: Duration = Duration::from_secs(5 * 60);

const EVENT_CAPACITY: usize = 64;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of applying the proxy to one harness session.
#[derive(Clone, PartialEq, Eq)]
pub struct AppliedOverride {
    /// Profile that matched.
    pub profile_id: ProxyProfileId,
    /// Environment variables for the child.
    pub env: std::collections::BTreeMap<String, String>,
    /// Model id the adapter should use.
    pub model: String,
    /// Whether the recipe is verified.
    pub verified: bool,
    /// Caveat, if any.
    pub note: Option<String>,
}

impl std::fmt::Debug for AppliedOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values may be keys; only names are safe to log.
        f.debug_struct("AppliedOverride")
            .field("profile_id", &self.profile_id)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("model", &self.model)
            .field("verified", &self.verified)
            .field("note", &self.note)
            .finish()
    }
}

impl AppliedOverride {
    fn from_recipe(profile: &ResolvedProfile, recipe: HarnessOverride) -> Self {
        Self {
            profile_id: profile.id.clone(),
            env: recipe.env,
            model: recipe.model,
            verified: recipe.verified,
            note: recipe.note,
        }
    }
}

/// CLIProxyAPI profiles and probes.
pub struct ProxyService {
    store: Arc<ProfileStore>,
    http: Client,
    capabilities: Mutex<HashMap<String, ProxyCapabilities>>,
    catalogs: Mutex<HashMap<String, ProxyModelCatalog>>,
    ttl: Duration,
    events: broadcast::Sender<ProxyEvent>,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService")
            .field("store", &self.store)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl ProxyService {
    /// Opens the profile store under `dir` and builds a service.
    pub fn open(
        dir: impl AsRef<Path>,
        secrets: Arc<dyn SecretStore>,
        ttl: Duration,
    ) -> Result<Self, ProxyError> {
        let store = Arc::new(ProfileStore::open(dir, secrets)?);
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("h4rn-proxy/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Ok(Self {
            store,
            http,
            capabilities: Mutex::new(HashMap::new()),
            catalogs: Mutex::new(HashMap::new()),
            ttl,
            events,
        })
    }

    /// Subscribes to profile, capability and catalogue changes.
    pub fn subscribe(&self) -> broadcast::Receiver<ProxyEvent> {
        self.events.subscribe()
    }

    /// Every profile, without secrets.
    pub fn profiles(&self) -> Vec<ProxyProfile> {
        self.store.public_profiles(&self.capabilities_cache())
    }

    /// One profile, without secrets.
    pub fn profile(&self, id: &ProxyProfileId) -> Result<ProxyProfile, ProxyError> {
        let caps = self.capabilities_cache().get(id.as_str()).cloned();
        let resolved = self.store.get(id).ok_or_else(|| not_found(id))?;
        Ok(ProxyProfile {
            id: resolved.id.clone(),
            name: resolved.name,
            base_url: resolved.base_url,
            default: resolved.default,
            has_management_key: resolved.management_key.is_some(),
            has_api_key: resolved.api_key.is_some(),
            harnesses: resolved.harnesses,
            capabilities: caps,
        })
    }

    /// Adds a profile and announces the new list.
    pub fn add_profile(&self, draft: ProxyProfileDraft) -> Result<ProxyProfile, ProxyError> {
        let profile = self.store.add(draft)?;
        let public = self.profile(&profile.id)?;
        self.emit_profiles();
        Ok(public)
    }

    /// Updates a profile and invalidates its caches.
    pub fn update_profile(
        &self,
        id: &ProxyProfileId,
        patch: ProxyProfilePatch,
    ) -> Result<ProxyProfile, ProxyError> {
        self.invalidate(id);
        let profile = self.store.update(id, patch)?;
        let public = self.profile(&profile.id)?;
        self.emit_profiles();
        Ok(public)
    }

    /// Removes a profile and its secrets.
    pub fn remove_profile(&self, id: &ProxyProfileId) -> Result<(), ProxyError> {
        self.store.remove(id)?;
        self.invalidate(id);
        self.emit_profiles();
        Ok(())
    }

    /// Sets one harness toggle.
    pub fn set_harness_override(
        &self,
        id: &ProxyProfileId,
        harness: HarnessKind,
        enabled: bool,
        default_model: Option<String>,
    ) -> Result<ProxyProfile, ProxyError> {
        let profile = self
            .store
            .set_harness_override(id, harness, enabled, default_model)?;
        let public = self.profile(&profile.id)?;
        self.emit_profiles();
        Ok(public)
    }

    /// Probes one profile and refreshes its cached capabilities.
    pub async fn test(&self, id: &ProxyProfileId) -> Result<ProxyTestReport, ProxyError> {
        let profile = self.store.get(id).ok_or_else(|| not_found(id))?;
        let report = crate::probe::probe(&self.http, &target(&profile)).await;
        self.store_capabilities(id, report.capabilities.clone());
        let _ = self.events.send(ProxyEvent::ProfileTested {
            profile_id: id.clone(),
            report: report.clone(),
        });
        Ok(report)
    }

    /// Reads (and caches) the merged model catalogue.
    pub async fn models(&self, id: &ProxyProfileId) -> Result<ProxyModelCatalog, ProxyError> {
        let profile = self.store.get(id).ok_or_else(|| not_found(id))?;
        match list_models(&self.http, &profile).await {
            Ok(catalog) => {
                self.cache_catalog(id, catalog.clone());
                let _ = self.events.send(ProxyEvent::ModelsChanged {
                    profile_id: id.clone(),
                    catalog: catalog.clone(),
                });
                Ok(catalog)
            }
            Err(error) => {
                // A 401/404 can mean the proxy restarted with a different
                // configuration; drop capabilities so the next read re-probes.
                self.invalidate(id);
                Err(error)
            }
        }
    }

    /// Reads credential health and cooldowns.
    pub async fn limits(&self, id: &ProxyProfileId) -> Result<ProxyLimits, ProxyError> {
        let profile = self.store.get(id).ok_or_else(|| not_found(id))?;
        match read_limits(&self.http, &profile).await {
            Ok(limits) => {
                let _ = self.events.send(ProxyEvent::LimitsChanged {
                    profile_id: id.clone(),
                    limits: limits.clone(),
                });
                Ok(limits)
            }
            Err(error) => {
                self.invalidate(id);
                Err(error)
            }
        }
    }

    /// Picks a model the proxy can serve after a limit.
    pub async fn pick_failover(
        &self,
        id: &ProxyProfileId,
        current: &str,
        policy: &ProxyFailoverPolicy,
    ) -> Result<Option<ProxyFailoverChoice>, ProxyError> {
        let catalog = self.models(id).await?;
        let limits = self.limits(id).await?;
        Ok(pick_failover(current, policy, &catalog.models, &limits))
    }

    /// The override a session of `kind` should receive, if its profile is on.
    ///
    /// This is deliberately synchronous: a session start must not wait for a
    /// network probe. When no catalogue has been read yet, the thread's model
    /// (or the toggle's default) is used as-is.
    pub fn override_for(&self, kind: HarnessKind, model: &str) -> Option<AppliedOverride> {
        let active = self.store.active_for(kind)?;
        let models = self
            .catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(active.profile.id.as_str())
            .map(|catalog| {
                catalog
                    .models
                    .iter()
                    .map(|model| model.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let target = ProxyTarget {
            profile_id: active.profile.id.as_str(),
            base_url: &active.profile.base_url,
            api_key: active.profile.api_key.as_deref(),
            model,
            default_model: active.harness.default_model.as_deref(),
            models: &models,
        };
        let recipe = for_harness(kind, &target);
        Some(AppliedOverride::from_recipe(&active.profile, recipe))
    }

    /// The cached capabilities for one profile, re-probing after the TTL.
    pub async fn capabilities(&self, id: &ProxyProfileId) -> Result<ProxyCapabilities, ProxyError> {
        if let Some(caps) = self.fresh_capabilities(id) {
            return Ok(caps);
        }
        self.invalidate(id);
        Ok(self.test(id).await?.capabilities)
    }

    /// Drops cached capabilities and catalogue for one profile.
    pub fn invalidate(&self, id: &ProxyProfileId) {
        self.capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id.as_str());
        self.catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id.as_str());
    }

    /// The model catalogue cached from the last read, if any.
    pub fn cached_models(&self, id: &ProxyProfileId) -> Option<ProxyModelCatalog> {
        self.catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id.as_str())
            .cloned()
    }

    /// Cached model ids for pickers.
    pub fn model_ids(&self, id: &ProxyProfileId) -> Vec<String> {
        self.cached_models(id)
            .map(|catalog| catalog.models.into_iter().map(|model| model.id).collect())
            .unwrap_or_default()
    }

    fn capabilities_cache(&self) -> MutexGuard<'_, HashMap<String, ProxyCapabilities>> {
        self.capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn store_capabilities(&self, id: &ProxyProfileId, capabilities: ProxyCapabilities) {
        self.capabilities_cache()
            .insert(id.as_str().to_owned(), capabilities);
    }

    fn cache_catalog(&self, id: &ProxyProfileId, catalog: ProxyModelCatalog) {
        self.catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.as_str().to_owned(), catalog);
    }

    fn emit_profiles(&self) {
        let _ = self.events.send(ProxyEvent::ProfilesChanged {
            profiles: self.profiles(),
        });
    }

    /// Cached capabilities, re-probing values older than the TTL.
    ///
    /// Returns immediately; stale values are returned until an async caller
    /// refreshes them with [`Self::capabilities`]. The TTL still governs the
    /// cache because [`Self::capabilities`] removes stale entries first.
    fn fresh_capabilities(&self, id: &ProxyProfileId) -> Option<ProxyCapabilities> {
        let cached = self.capabilities_cache().get(id.as_str()).cloned()?;
        let age = Utc::now()
            .signed_duration_since(cached.detected_at)
            .to_std()
            .unwrap_or_default();
        (age < self.ttl).then_some(cached)
    }
}

fn not_found(id: &ProxyProfileId) -> ProxyError {
    ProxyError::new(
        ProxyErrorKind::NotFound,
        format!("proxy profile `{id}` does not exist"),
    )
    .with_hint("Refresh the profile list, then choose an existing profile")
}

fn target(profile: &ResolvedProfile) -> ProbeTarget {
    ProbeTarget::new(
        profile.base_url.clone(),
        profile.management_key.clone(),
        profile.api_key.clone(),
    )
}

/// A model id that is only served by the proxy, for live tests and diagnostics.
pub fn proxy_only_model(models: &[ProxyModel]) -> Option<&ProxyModel> {
    models
        .iter()
        .find(|model| model.source == h4rn_protocol::ProxyModelSource::OpenAi)
}
