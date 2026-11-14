//! Profile metadata, per-harness toggles and secret references.
//!
//! Secrets live in the [`SecretStore`]; the JSON file written here contains
//! only ids, labels, base URLs, the default flag and harness toggles.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use h4rn_protocol::{
    HarnessKind, ProxyCapabilities, ProxyHarnessOverride, ProxyProfile, ProxyProfileDraft,
    ProxyProfileId, ProxyProfilePatch,
};
use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, ProxyErrorKind};
use crate::secret::SecretStore;

const METADATA_FILE: &str = "proxy-profiles.json";
const MANAGEMENT_SECRET: &str = "management-key";
const API_SECRET: &str = "api-key";

/// One stored harness toggle.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredHarness {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    default_model: Option<String>,
}

/// One stored profile. No secrets.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredProfile {
    id: String,
    name: String,
    base_url: String,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    harnesses: BTreeMap<String, StoredHarness>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredState {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    profiles: Vec<StoredProfile>,
}

/// A profile with its secrets resolved, used internally.
#[derive(Clone)]
pub struct ResolvedProfile {
    /// Profile id.
    pub id: ProxyProfileId,
    /// Label.
    pub name: String,
    /// Root URL, no trailing slash.
    pub base_url: String,
    /// Default flag.
    pub default: bool,
    /// Management key, when stored.
    pub management_key: Option<String>,
    /// Client API key, when stored.
    pub api_key: Option<String>,
    /// Per-harness toggles.
    pub harnesses: Vec<ProxyHarnessOverride>,
}

impl std::fmt::Debug for ResolvedProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("default", &self.default)
            .field("has_management_key", &self.management_key.is_some())
            .field("has_api_key", &self.api_key.is_some())
            .field("harnesses", &self.harnesses)
            .finish()
    }
}

/// The harness a session should use a proxy for.
#[derive(Clone)]
pub struct ActiveOverride {
    /// Profile that owns the toggle.
    pub profile: ResolvedProfile,
    /// The harness toggle.
    pub harness: ProxyHarnessOverride,
}

/// Persists profile metadata and resolves secrets on demand.
#[derive(Debug)]
pub struct ProfileStore {
    path: PathBuf,
    state: RwLock<StoredState>,
    secrets: Arc<dyn SecretStore>,
}

impl ProfileStore {
    /// Opens the metadata file, creating an empty store when absent.
    pub fn open(dir: impl AsRef<Path>, secrets: Arc<dyn SecretStore>) -> Result<Self, ProxyError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|_| {
            ProxyError::new(
                ProxyErrorKind::Secret,
                "could not create the proxy profile directory",
            )
        })?;
        let path = dir.join(METADATA_FILE);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                ProxyError::new(
                    ProxyErrorKind::InvalidRequest,
                    format!("proxy profile metadata is invalid JSON: {error}"),
                )
                .with_hint("Remove or repair proxy-profiles.json in the daemon data directory")
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredState::default(),
            Err(_) => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Secret,
                    "could not read the proxy profile metadata file",
                ));
            }
        };
        Ok(Self {
            path,
            state: RwLock::new(state),
            secrets,
        })
    }

    fn read(&self) -> RwLockReadGuard<'_, StoredState> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, StoredState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persist(&self, state: &StoredState) -> Result<(), ProxyError> {
        let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
            ProxyError::new(
                ProxyErrorKind::Secret,
                format!("could not serialise proxy profiles: {error}"),
            )
        })?;
        let temporary = self.path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|_| {
            ProxyError::new(
                ProxyErrorKind::Secret,
                "could not write the proxy profile metadata file",
            )
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| {
                ProxyError::new(
                    ProxyErrorKind::Secret,
                    "could not write the proxy profile metadata file",
                )
            })?;
        std::fs::rename(&temporary, &self.path).map_err(|_| {
            ProxyError::new(
                ProxyErrorKind::Secret,
                "could not replace the proxy profile metadata file",
            )
        })?;
        Ok(())
    }

    /// All profiles in creation order.
    pub fn all(&self) -> Vec<ResolvedProfile> {
        self.read()
            .profiles
            .iter()
            .map(|profile| self.resolve(profile))
            .collect()
    }

    /// One profile by id.
    pub fn get(&self, id: &ProxyProfileId) -> Option<ResolvedProfile> {
        self.read()
            .profiles
            .iter()
            .find(|profile| profile.id == id.as_str())
            .map(|profile| self.resolve(profile))
    }

    /// The default profile.
    pub fn default_profile(&self) -> Option<ResolvedProfile> {
        self.read()
            .profiles
            .iter()
            .find(|profile| profile.default)
            .map(|profile| self.resolve(profile))
    }

    /// The profile and toggle that should serve `kind`.
    ///
    /// The default profile wins when its toggle is on; otherwise the first
    /// profile (in creation order) with the toggle on is used, so different
    /// harnesses can route through different profiles.
    pub fn active_for(&self, kind: HarnessKind) -> Option<ActiveOverride> {
        let state = self.read();
        let key = kind.slug();
        let default = state
            .profiles
            .iter()
            .find(|profile| profile.default && harness(&profile.harnesses, key).enabled);
        let chosen = default.or_else(|| {
            state
                .profiles
                .iter()
                .find(|profile| harness(&profile.harnesses, key).enabled)
        })?;
        let profile = self.resolve(chosen);
        let toggle = profile
            .harnesses
            .iter()
            .find(|entry| entry.harness == kind)
            .cloned()?;
        Some(ActiveOverride {
            profile,
            harness: toggle,
        })
    }

    /// Adds a profile.
    pub fn add(&self, draft: ProxyProfileDraft) -> Result<ResolvedProfile, ProxyError> {
        let name = validate_name(&draft.name)?;
        let base_url = validate_base_url(&draft.base_url)?;
        let management_key = normalise_secret(draft.management_key)?;
        let api_key = normalise_secret(draft.api_key)?;

        let mut state = self.write();
        let first = state.profiles.is_empty();
        let id = format!("proxy_{}", state.next_id);
        state.next_id += 1;
        if draft.default || first {
            for profile in &mut state.profiles {
                profile.default = false;
            }
        }
        let profile = StoredProfile {
            id,
            name,
            base_url,
            default: draft.default || first,
            harnesses: BTreeMap::new(),
        };
        state.profiles.push(profile.clone());
        let id = profile.id.clone();
        self.persist(&state)?;
        drop(state);

        self.write_secrets(&id, management_key, api_key)?;
        Ok(self.get(&ProxyProfileId::new(id)).expect("just inserted"))
    }

    /// Updates one profile. Missing fields are unchanged.
    pub fn update(
        &self,
        id: &ProxyProfileId,
        patch: ProxyProfilePatch,
    ) -> Result<ResolvedProfile, ProxyError> {
        let mut state = self.write();
        let index = state
            .profiles
            .iter()
            .position(|profile| profile.id == id.as_str())
            .ok_or_else(|| not_found(id))?;

        if let Some(name) = patch.name {
            state.profiles[index].name = validate_name(&name)?;
        }
        if let Some(base_url) = patch.base_url {
            state.profiles[index].base_url = validate_base_url(&base_url)?;
        }
        if patch.default == Some(true) {
            for profile in &mut state.profiles {
                profile.default = false;
            }
            state.profiles[index].default = true;
        } else if patch.default == Some(false) {
            state.profiles[index].default = false;
        }
        if !state.profiles.iter().any(|profile| profile.default)
            && let Some(first) = state.profiles.first_mut()
        {
            first.default = true;
        }
        self.persist(&state)?;
        drop(state);

        let management = normalise_secret(patch.management_key)?;
        let api = normalise_secret(patch.api_key)?;
        if patch.clear_management_key {
            self.secrets
                .delete(&secret_key(id.as_str(), MANAGEMENT_SECRET))?;
        }
        if let Some(value) = management {
            self.secrets
                .set(&secret_key(id.as_str(), MANAGEMENT_SECRET), &value)?;
        }
        if patch.clear_api_key {
            self.secrets.delete(&secret_key(id.as_str(), API_SECRET))?;
        }
        if let Some(value) = api {
            self.secrets
                .set(&secret_key(id.as_str(), API_SECRET), &value)?;
        }
        self.get(id).ok_or_else(|| not_found(id))
    }

    /// Removes a profile and both secrets.
    pub fn remove(&self, id: &ProxyProfileId) -> Result<(), ProxyError> {
        let mut state = self.write();
        let index = state
            .profiles
            .iter()
            .position(|profile| profile.id == id.as_str())
            .ok_or_else(|| not_found(id))?;
        let was_default = state.profiles[index].default;
        state.profiles.remove(index);
        if was_default && let Some(first) = state.profiles.first_mut() {
            first.default = true;
        }
        self.persist(&state)?;
        drop(state);
        let _ = self
            .secrets
            .delete(&secret_key(id.as_str(), MANAGEMENT_SECRET));
        let _ = self.secrets.delete(&secret_key(id.as_str(), API_SECRET));
        Ok(())
    }

    /// Enables or disables one harness on one profile.
    pub fn set_harness_override(
        &self,
        id: &ProxyProfileId,
        harness_kind: HarnessKind,
        enabled: bool,
        default_model: Option<String>,
    ) -> Result<ResolvedProfile, ProxyError> {
        let model = default_model
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty());
        let mut state = self.write();
        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == id.as_str())
            .ok_or_else(|| not_found(id))?;
        profile.harnesses.insert(
            harness_kind.slug().to_owned(),
            StoredHarness {
                enabled,
                default_model: model,
            },
        );
        self.persist(&state)?;
        drop(state);
        self.get(id).ok_or_else(|| not_found(id))
    }

    /// Builds the client-facing profile list, attaching cached capabilities.
    pub fn public_profiles(
        &self,
        capabilities: &HashMap<String, ProxyCapabilities>,
    ) -> Vec<ProxyProfile> {
        let mut profiles: Vec<ProxyProfile> = self
            .all()
            .into_iter()
            .map(|profile| {
                let caps = capabilities.get(profile.id.as_str()).cloned();
                ProxyProfile {
                    id: profile.id.clone(),
                    name: profile.name,
                    base_url: profile.base_url,
                    default: profile.default,
                    has_management_key: profile.management_key.is_some(),
                    has_api_key: profile.api_key.is_some(),
                    harnesses: profile.harnesses,
                    capabilities: caps,
                }
            })
            .collect();
        profiles.sort_by(|left, right| {
            right
                .default
                .cmp(&left.default)
                .then_with(|| left.id.cmp(&right.id))
        });
        profiles
    }

    fn resolve(&self, profile: &StoredProfile) -> ResolvedProfile {
        let id = ProxyProfileId::new(profile.id.clone());
        let harnesses = harness_kinds()
            .into_iter()
            .map(|kind| {
                let stored = profile
                    .harnesses
                    .get(kind.slug())
                    .cloned()
                    .unwrap_or_default();
                let preview = crate::overrides::preview(
                    kind,
                    id.as_str(),
                    &profile.base_url,
                    stored.default_model.as_deref(),
                );
                ProxyHarnessOverride {
                    harness: kind,
                    enabled: stored.enabled,
                    default_model: stored.default_model,
                    verified: recipe_verified(kind),
                    note: recipe_note(kind),
                    config_path: crate::overrides::config_path(kind).to_owned(),
                    preview,
                }
            })
            .collect();
        ResolvedProfile {
            id: id.clone(),
            name: profile.name.clone(),
            base_url: profile.base_url.clone(),
            default: profile.default,
            management_key: self
                .secrets
                .get(&secret_key(id.as_str(), MANAGEMENT_SECRET))
                .ok()
                .flatten(),
            api_key: self
                .secrets
                .get(&secret_key(id.as_str(), API_SECRET))
                .ok()
                .flatten(),
            harnesses,
        }
    }

    fn write_secrets(
        &self,
        id: &str,
        management_key: Option<String>,
        api_key: Option<String>,
    ) -> Result<(), ProxyError> {
        if let Some(value) = management_key {
            self.secrets
                .set(&secret_key(id, MANAGEMENT_SECRET), &value)?;
        }
        if let Some(value) = api_key {
            self.secrets.set(&secret_key(id, API_SECRET), &value)?;
        }
        Ok(())
    }
}

fn harness(map: &BTreeMap<String, StoredHarness>, key: &str) -> StoredHarness {
    map.get(key).cloned().unwrap_or_default()
}

fn secret_key(id: &str, kind: &str) -> String {
    format!("proxy/{id}/{kind}")
}

fn not_found(id: &ProxyProfileId) -> ProxyError {
    ProxyError::new(
        ProxyErrorKind::NotFound,
        format!("proxy profile `{id}` does not exist"),
    )
    .with_hint("Refresh the profile list, then choose an existing profile")
}

fn validate_name(name: &str) -> Result<String, ProxyError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ProxyError::new(
            ProxyErrorKind::InvalidRequest,
            "a proxy profile needs a name",
        ));
    }
    Ok(name.to_owned())
}

/// Validates and normalises a proxy root URL.
pub fn validate_base_url(input: &str) -> Result<String, ProxyError> {
    let trimmed = input.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(trimmed).map_err(|_| {
        ProxyError::new(
            ProxyErrorKind::InvalidRequest,
            "the proxy base URL is not a valid absolute URL",
        )
        .with_hint("Use a URL such as http://127.0.0.1:8317")
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ProxyError::new(
            ProxyErrorKind::InvalidRequest,
            "the proxy base URL must be http(s) with a host",
        )
        .with_hint("Use a URL such as http://127.0.0.1:8317"));
    }
    Ok(trimmed.to_owned())
}

fn normalise_secret(value: Option<String>) -> Result<Option<String>, ProxyError> {
    Ok(value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty()))
}

/// Every harness the integration knows.
pub fn harness_kinds() -> [HarnessKind; 4] {
    [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Opencode,
        HarnessKind::Antigravity,
    ]
}

/// Whether the recipe for `kind` is verified by a test or a live run.
pub fn recipe_verified(kind: HarnessKind) -> bool {
    !matches!(kind, HarnessKind::Antigravity)
}

/// A short caveat per harness.
pub fn recipe_note(kind: HarnessKind) -> Option<String> {
    match kind {
        HarnessKind::ClaudeCode => {
            Some("Base URL omits /v1; ANTHROPIC_AUTH_TOKEN carries the key".into())
        }
        HarnessKind::Codex => {
            Some("Base URL includes /v1; a model_providers entry names the key env".into())
        }
        HarnessKind::Opencode => {
            Some("Generated OPENCODE_CONFIG_CONTENT declares the provider and model list".into())
        }
        HarnessKind::Antigravity => Some(
            "Uses GEMINI_API_KEY + GOOGLE_GEMINI_BASE_URL; the path agy appends is unverified"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::MemorySecretStore;

    fn store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = Arc::new(MemorySecretStore::new());
        let store = ProfileStore::open(dir.path(), secrets).expect("store");
        (dir, store)
    }

    fn draft(name: &str) -> ProxyProfileDraft {
        ProxyProfileDraft {
            name: name.into(),
            base_url: "http://127.0.0.1:8317/".into(),
            management_key: Some("mgmt".into()),
            api_key: Some("api".into()),
            default: false,
        }
    }

    #[test]
    fn the_first_profile_is_default_and_urls_are_normalised() {
        let (_dir, store) = store();
        let profile = store.add(draft("local")).expect("add");
        assert!(profile.default);
        assert_eq!(profile.base_url, "http://127.0.0.1:8317");
        assert_eq!(profile.management_key.as_deref(), Some("mgmt"));
        assert_eq!(profile.api_key.as_deref(), Some("api"));
    }

    #[test]
    fn a_second_default_moves_the_flag() {
        let (_dir, store) = store();
        let first = store.add(draft("first")).expect("first");
        let mut second = draft("second");
        second.default = true;
        let second = store.add(second).expect("second");
        assert!(!store.get(&first.id).expect("first").default);
        assert!(store.get(&second.id).expect("second").default);
    }

    #[test]
    fn removing_the_default_promotes_the_next_profile() {
        let (_dir, store) = store();
        let first = store.add(draft("first")).expect("first");
        let second = store.add(draft("second")).expect("second");
        store.remove(&first.id).expect("remove");
        assert!(store.get(&second.id).expect("second").default);
    }

    #[test]
    fn toggles_are_per_profile_and_per_harness() {
        let (_dir, store) = store();
        let first = store.add(draft("first")).expect("first");
        let second = store.add(draft("second")).expect("second");
        store
            .set_harness_override(&first.id, HarnessKind::ClaudeCode, true, None)
            .expect("claude");
        store
            .set_harness_override(
                &second.id,
                HarnessKind::Codex,
                true,
                Some("gpt-6.1-sol".into()),
            )
            .expect("codex");

        let claude = store.active_for(HarnessKind::ClaudeCode).expect("claude");
        assert_eq!(claude.profile.id, first.id);
        let codex = store.active_for(HarnessKind::Codex).expect("codex");
        assert_eq!(codex.profile.id, second.id);
        assert_eq!(codex.harness.default_model.as_deref(), Some("gpt-6.1-sol"));
        assert!(store.active_for(HarnessKind::Opencode).is_none());
    }

    #[test]
    fn the_default_profile_wins_over_creation_order() {
        let (_dir, store) = store();
        let first = store.add(draft("first")).expect("first");
        let mut second = draft("second");
        second.default = true;
        let second = store.add(second).expect("second");
        store
            .set_harness_override(&first.id, HarnessKind::ClaudeCode, true, None)
            .expect("first toggle");
        store
            .set_harness_override(&second.id, HarnessKind::ClaudeCode, true, None)
            .expect("second toggle");
        assert_eq!(
            store
                .active_for(HarnessKind::ClaudeCode)
                .expect("active")
                .profile
                .id,
            second.id
        );
    }

    #[test]
    fn patches_replace_or_clear_secrets() {
        let (_dir, store) = store();
        let profile = store.add(draft("local")).expect("add");
        let patched = store
            .update(
                &profile.id,
                ProxyProfilePatch {
                    name: Some("renamed".into()),
                    base_url: None,
                    management_key: None,
                    clear_management_key: true,
                    api_key: Some("new".into()),
                    clear_api_key: false,
                    default: None,
                },
            )
            .expect("update");
        assert_eq!(patched.name, "renamed");
        assert!(patched.management_key.is_none());
        assert_eq!(patched.api_key.as_deref(), Some("new"));
    }

    #[test]
    fn metadata_never_contains_a_secret() {
        let (dir, store) = store();
        store.add(draft("local")).expect("add");
        let raw = std::fs::read_to_string(dir.path().join(METADATA_FILE)).expect("read");
        assert!(!raw.contains("mgmt"));
        assert!(!raw.contains("\"api\""));
        assert!(raw.contains("127.0.0.1:8317"));
    }

    #[test]
    fn invalid_urls_are_rejected() {
        let (_dir, store) = store();
        let mut bad = draft("bad");
        bad.base_url = "ftp://example.com".into();
        assert!(store.add(bad).is_err());
        let mut bad = draft("bad");
        bad.base_url = "not a url".into();
        assert!(store.add(bad).is_err());
    }
}
