//! The persisted device registry: who may reach this daemon, and as what.
//!
//! The transport allowlist only says a Noise key may complete a handshake. This
//! registry says what that key is allowed to *do*, and it is the single source
//! of authorisation for `/secure` (client devices) and `/secure-mcp` (peer
//! daemons). Every entry records:
//!
//! - the key, which is the only identity the daemon trusts (it is proven by the
//!   Noise handshake; a device id or machine name from the relay or from a
//!   frame is a claim, never an identity);
//! - the [`Role`]: a `Client` device holds scopes, a `Host` peer daemon holds
//!   none and may only use the MCP tunnel;
//! - the [`Provenance`]: `Connect` entries are managed by h4rn-connect (its
//!   snapshots are authoritative), `Direct` ones came from a LAN/tailnet pairing
//!   and are only removed by an explicit revoke.
//!
//! A key with no entry is denied. The file survives restarts so pairing state
//! and scopes are not forgotten, and the transport allowlist is rebuilt from it.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use h4rn_protocol::{HostId, PairScope, PairingConfirmation};
use h4rn_transport::keys::{KEY_LEN, read_secret_file, write_secret_file};
use serde::{Deserialize, Serialize};

/// What a registered key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A user device (phone, browser, desktop) that holds [`PairScope`]s.
    Client,
    /// A peer daemon allowed to open the MCP tunnel, nothing else.
    Host,
}

/// How a device got its entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Paired through h4rn-connect; connect snapshots are authoritative.
    Connect,
    /// Paired over LAN/tailnet with a one-time secret.
    Direct,
}

/// One registered key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Noise static key, hex.
    pub key: String,
    /// Connect device id for `Connect` clients; the daemon's device label for
    /// hosts, when known.
    pub device_id: Option<String>,
    /// Client or peer daemon.
    pub role: Role,
    /// Scopes held. Always contains `view` for a client; empty for a host.
    pub scopes: Vec<PairScope>,
    /// Pairing provenance.
    pub provenance: Provenance,
    /// The machine a `Host` entry stands for.
    pub machine: Option<String>,
    /// A relay-provisioned device whose key was announced but not admitted
    /// until the host confirms the pairing. Pending entries have no effective
    /// scopes and are kept off the transport allowlist.
    #[serde(default)]
    pub pending: bool,
    /// Confirmed by this run's authoritative source: a connect snapshot for a
    /// connect client, a peer report for a peer daemon, or the pairing itself
    /// for a direct one. Never serialised: every connect entry starts a restart
    /// unverified, so a revocation that happened while the daemon could not
    /// reach connect cannot outlive it.
    #[serde(skip)]
    pub verified: bool,
}

impl Entry {
    fn key_bytes(&self) -> Option<[u8; KEY_LEN]> {
        hex::decode(&self.key).ok()?.as_slice().try_into().ok()
    }

    fn admits(&self) -> bool {
        !self.pending && self.verified
    }
}

/// Why a registry change could not be made durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistError(String);

impl PersistError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PersistError {}

/// What the registry knows about an authenticated key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorization {
    /// Client or host.
    pub role: Role,
    /// Scopes held (clients only).
    pub scopes: Vec<PairScope>,
    /// Connect device id or host label.
    pub device_id: Option<String>,
    /// Pairing provenance.
    pub provenance: Provenance,
    /// Machine a host entry stands for.
    pub machine: Option<HostId>,
}

/// A device as reported by connect, for reconciliation.
#[derive(Clone, Debug)]
pub struct ConnectEntry {
    /// Connect device id.
    pub device_id: String,
    /// Noise static key.
    pub key: [u8; KEY_LEN],
    /// Granted scopes; an empty list is read as `view` only.
    pub scopes: Vec<PairScope>,
}

/// The persisted registry.
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    entries: RwLock<HashMap<[u8; KEY_LEN], Entry>>,
    path: Option<PathBuf>,
    /// Scopes the next direct pairing grants.
    pending_direct: Mutex<Option<Vec<PairScope>>>,
    /// Typed-code pairings waiting for a human to compare the SAS.
    confirmations: Mutex<Vec<PairingConfirmation>>,
    /// Fired when the relay reports that the account's machines or devices may
    /// have changed (snapshot, presence, pairing, revocation), so the peer
    /// directory refreshes on a push instead of polling connect.
    topology: tokio::sync::Notify,
    /// Test-only failure injection for the durable write.
    #[cfg(test)]
    fail_persist: std::sync::atomic::AtomicBool,
}

impl DeviceRegistry {
    /// An in-memory registry (tests, signed-out tools).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Opens (or creates) the registry stored at `path`.
    ///
    /// A missing file is an empty registry. A corrupt file is *also* an empty
    /// registry, never an open one: every device must pair again.
    ///
    /// Connect entries load unverified: connect is the authority for them, and
    /// until this run's first snapshot (or peer report) arrives, a device that
    /// was revoked while the daemon was offline must not be admitted. Direct
    /// pairings do not depend on connect, so they load admitted.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut entries = HashMap::new();
        match read_secret_file(&path) {
            Ok(Some(bytes)) => match serde_json::from_slice::<Vec<Entry>>(&bytes) {
                Ok(list) => {
                    for mut entry in list {
                        entry.verified = entry.provenance == Provenance::Direct;
                        if let Some(key) = entry.key_bytes() {
                            entries.insert(key, entry);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "the device registry is unreadable; devices must pair again")
                }
            },
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "could not read the device registry; devices must pair again")
            }
        }
        let registry = Self {
            entries: RwLock::new(entries),
            path: Some(path),
            pending_direct: Mutex::new(None),
            confirmations: Mutex::new(Vec::new()),
            topology: tokio::sync::Notify::new(),
            #[cfg(test)]
            fail_persist: std::sync::atomic::AtomicBool::new(false),
        };
        registry.apply_pending_revocations();
        registry
    }

    /// The topology-change signal; see the field docs.
    pub fn topology(&self) -> &tokio::sync::Notify {
        &self.topology
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<[u8; KEY_LEN], Entry>> {
        self.entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Applies `change`, then persists. When `rollback` is set, a failed write
    /// restores the previous entries so an admission the file cannot record is
    /// not in effect for this run either. A revocation passes `false`: staying
    /// revoked in memory is safer than putting a revoked key back.
    fn mutate<R>(
        &self,
        rollback: bool,
        change: impl FnOnce(&mut HashMap<[u8; KEY_LEN], Entry>) -> R,
    ) -> (R, Result<(), PersistError>) {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = rollback.then(|| guard.clone());
        let result = change(&mut guard);
        let persisted = self.persist(&guard);
        if persisted.is_err()
            && let Some(before) = before
        {
            *guard = before;
        }
        (result, persisted)
    }

    fn persist(&self, entries: &HashMap<[u8; KEY_LEN], Entry>) -> Result<(), PersistError> {
        #[cfg(test)]
        if self
            .fail_persist
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(PersistError::new("injected persist failure"));
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut list: Vec<&Entry> = entries.values().collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        let body = serde_json::to_vec(&list).map_err(|error| {
            PersistError::new(format!("serialise the device registry: {error}"))
        })?;
        write_secret_file(path, &body)
            .map_err(|error| PersistError::new(format!("write the device registry: {error}")))
    }

    /// The sidecar listing keys whose revocation could not be written into the
    /// registry file. It is tiny, so it is the retry that survives a crash.
    fn revocation_path(&self) -> Option<PathBuf> {
        self.path
            .as_ref()
            .map(|path| path.with_file_name("devices.revoked"))
    }

    fn read_revocations(path: &Path) -> BTreeSet<[u8; KEY_LEN]> {
        let Ok(Some(bytes)) = read_secret_file(path) else {
            return BTreeSet::new();
        };
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| hex::decode(line.trim()).ok())
            .filter_map(|key| key.as_slice().try_into().ok())
            .collect()
    }

    /// Records keys that must not be admitted even if the registry file still
    /// lists them. Called only after the main write failed.
    fn remember_revocations(&self, keys: &[[u8; KEY_LEN]]) {
        let Some(path) = self.revocation_path() else {
            return;
        };
        let mut revoked = Self::read_revocations(&path);
        revoked.extend(keys.iter().copied());
        let body = revoked
            .iter()
            .map(hex::encode)
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(error) = write_secret_file(&path, body.as_bytes()) {
            tracing::warn!(
                %error,
                keys = keys.len(),
                "could not record a pending device revocation; a restart may re-admit it"
            );
        }
    }

    /// Drops keys a previous run failed to remove from the registry file, then
    /// rewrites the file and clears the sidecar once that succeeds.
    fn apply_pending_revocations(&self) {
        let Some(path) = self.revocation_path() else {
            return;
        };
        let revoked = Self::read_revocations(&path);
        if revoked.is_empty() {
            return;
        }
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|key, _| !revoked.contains(key));
        match self.persist(&guard) {
            Ok(()) => {
                if let Err(error) = std::fs::remove_file(&path) {
                    tracing::warn!(%error, "could not clear the device revocation sidecar");
                }
            }
            Err(error) => {
                // Keep the sidecar: the next start retries, and this run keeps
                // the keys out of the registry regardless.
                tracing::warn!(%error, "could not persist the applied device revocations");
            }
        }
    }

    /// Registers a connect-paired client. Pairing again replaces its scopes.
    /// An empty scope list is read as `view` only: missing authority is never
    /// widened.
    pub fn insert_scoped(
        &self,
        device_id: impl Into<String>,
        key: [u8; KEY_LEN],
        scopes: Vec<PairScope>,
    ) -> Result<(), PersistError> {
        self.upsert_connect(device_id.into(), key, scopes, Some(false))
    }

    /// Registers a relay-announced device that is not admitted yet.
    ///
    /// A key connect announced but nobody compared a code for has no scopes
    /// and stays off the transport allowlist. A snapshot that still lists it
    /// must never promote it: only [`Self::decide_pending`] may.
    pub fn provision_scoped(
        &self,
        device_id: impl Into<String>,
        key: [u8; KEY_LEN],
        scopes: Vec<PairScope>,
    ) -> Result<(), PersistError> {
        self.upsert_connect(device_id.into(), key, scopes, None)
    }

    /// Registers a connect-paired client with `view` only.
    pub fn insert(
        &self,
        device_id: impl Into<String>,
        key: [u8; KEY_LEN],
    ) -> Result<(), PersistError> {
        self.insert_scoped(device_id, key, Vec::new())
    }

    /// Inserts or updates a connect-managed client without ever re-rolling a
    /// peer daemon key. `pending` is `Some` for an explicit decision and
    /// `None` to keep an existing decision (new entries stay pending).
    fn upsert_connect(
        &self,
        device_id: String,
        key: [u8; KEY_LEN],
        scopes: Vec<PairScope>,
        pending: Option<bool>,
    ) -> Result<(), PersistError> {
        let scopes = PairScope::normalize(scopes);
        let entry = Entry {
            key: hex::encode(key),
            device_id: Some(device_id.clone()),
            role: Role::Client,
            scopes: scopes.clone(),
            provenance: Provenance::Connect,
            machine: None,
            pending: pending.unwrap_or(true),
            verified: true,
        };
        let ((), persisted) = self.mutate(true, |entries| {
            // A peer daemon key is never silently re-roled into a client: MCP
            // peering depends on the host entry staying a host.
            if entries
                .get(&key)
                .is_some_and(|existing| existing.role == Role::Host)
            {
                tracing::warn!(
                    device = %device_id,
                    "refusing to replace a peer daemon entry with a client"
                );
                return;
            }
            // One connect device id maps to one key: a re-paired device with a
            // fresh key replaces its old entry.
            entries.retain(|existing_key, existing| {
                *existing_key == key
                    || !(existing.provenance == Provenance::Connect
                        && existing.role == Role::Client
                        && existing.device_id.as_deref() == Some(device_id.as_str()))
            });
            match entries.get_mut(&key) {
                Some(existing) => {
                    // A snapshot refreshes the requested scopes but keeps a
                    // pending device pending and a confirmed one confirmed.
                    existing.device_id = Some(device_id);
                    existing.scopes = scopes;
                    existing.provenance = Provenance::Connect;
                    existing.machine = None;
                    existing.pending = pending.unwrap_or(existing.pending);
                    existing.verified = true;
                }
                None => {
                    entries.insert(key, entry);
                }
            }
        });
        persisted
    }

    /// Registers a direct (LAN/tailnet) pairing with explicit scopes.
    pub fn insert_direct(
        &self,
        key: [u8; KEY_LEN],
        scopes: Vec<PairScope>,
    ) -> Result<(), PersistError> {
        let entry = Entry {
            key: hex::encode(key),
            device_id: None,
            role: Role::Client,
            scopes: PairScope::normalize(scopes),
            provenance: Provenance::Direct,
            machine: None,
            pending: false,
            verified: true,
        };
        let ((), persisted) = self.mutate(true, |entries| {
            entries.insert(key, entry);
        });
        persisted
    }

    /// Registers a peer daemon, identified by its Noise key, as the host for
    /// `machine`. It may use the MCP tunnel and nothing else.
    pub fn register_host(
        &self,
        machine: HostId,
        key: [u8; KEY_LEN],
        device_id: Option<String>,
    ) -> Result<(), PersistError> {
        let entry = Entry {
            key: hex::encode(key),
            device_id,
            role: Role::Host,
            scopes: Vec::new(),
            provenance: Provenance::Connect,
            machine: Some(machine.to_string()),
            pending: false,
            verified: true,
        };
        let ((), persisted) = self.mutate(true, |entries| {
            entries.insert(key, entry);
        });
        persisted
    }

    /// Scopes the next direct pairing will grant. The caller arms this before
    /// the one-time secret; the transport's pairing hook consumes it.
    pub fn arm_direct(&self, scopes: Vec<PairScope>) {
        *self
            .pending_direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PairScope::normalize(scopes));
    }

    /// Drops the armed scopes without recording a device.
    ///
    /// Called with the secret: an armed window must not outlive the
    /// `direct-pairing.json` that opened it, or a later arm-less pairing would
    /// inherit the wrong authority.
    pub fn clear_direct(&self) {
        *self
            .pending_direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// Records the device that just paired directly with the armed scopes.
    ///
    /// With nothing armed (an operator-provisioned `H4RN_PAIRING_PSK`) the
    /// device gets `view` and `approve`: the same safe default as a CLI direct
    /// pairing without `--scope`. Wider authority needs an explicit arm.
    pub fn complete_direct(&self, key: [u8; KEY_LEN]) -> Result<(), PersistError> {
        let scopes = self
            .pending_direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| PairScope::MINIMAL.to_vec());
        self.insert_direct(key, scopes)
    }

    /// Records a newly connect-paired device so the person can confirm the
    /// short authentication string.
    ///
    /// The string is computed here, in Rust, from the device key connect gave
    /// this daemon and this daemon's own key. A relay that substituted the
    /// daemon key shown to the device cannot make the two agree.
    pub fn note_confirmation(
        &self,
        device_id: &str,
        device_name: &str,
        device_key: &[u8; KEY_LEN],
        daemon_key: &[u8; KEY_LEN],
    ) {
        let sas = h4rn_transport::pairing::short_auth_string(device_key, daemon_key);
        let mut list = self
            .confirmations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        list.retain(|entry| entry.device_id != device_id);
        list.push(PairingConfirmation {
            device_id: device_id.to_owned(),
            device_name: device_name.to_owned(),
            sas,
            paired_at: chrono::Utc::now(),
        });
    }

    /// Confirmations still worth showing, newest first. Entries that were
    /// confirmed, rejected or removed since are dropped.
    ///
    /// A pending device does not expire: until the host decides, it stays
    /// blocked and listed rather than silently becoming an orphan.
    pub fn pending_confirmations(&self) -> Vec<PairingConfirmation> {
        let live: std::collections::HashSet<String> = self
            .read()
            .values()
            .filter(|entry| entry.pending)
            .filter_map(|entry| entry.device_id.clone())
            .collect();
        let mut list = self
            .confirmations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        list.retain(|entry| live.contains(&entry.device_id));
        let mut out = list.clone();
        out.sort_by_key(|entry| std::cmp::Reverse(entry.paired_at));
        out
    }

    /// Records the host's decision on a pending relay pairing.
    ///
    /// Returns the key and the decision so the caller can admit or revoke it
    /// on the transport. `None` means the device is no longer pending, which
    /// makes the request safe to retry. The paired [`Result`] says whether the
    /// decision was written to disk:
    ///
    /// - a confirmation that cannot be persisted is rolled back, so the device
    ///   stays pending and the caller must not admit it;
    /// - a rejection that cannot be persisted stays revoked in memory and is
    ///   recorded in the revocation sidecar, so a restart still refuses it.
    pub fn decide_pending(
        &self,
        device_id: &str,
        confirm: bool,
    ) -> (Option<([u8; KEY_LEN], bool)>, Result<(), PersistError>) {
        let (decided, persisted) = self.mutate(confirm, |entries| {
            let key = entries
                .iter()
                .find(|(_, entry)| {
                    entry.pending
                        && entry.role == Role::Client
                        && entry.device_id.as_deref() == Some(device_id)
                })
                .map(|(key, _)| *key)?;
            if confirm {
                let entry = entries.get_mut(&key)?;
                entry.pending = false;
                entry.verified = true;
            } else {
                entries.remove(&key);
            }
            Some(key)
        });
        self.confirmations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|entry| entry.device_id != device_id || (confirm && persisted.is_err()));
        let decided = decided.map(|key| (key, confirm));
        // A confirmation whose write failed was rolled back: the device is
        // still pending, so the caller must not admit it.
        let decided = if confirm && persisted.is_err() {
            None
        } else {
            decided
        };
        if let Err(error) = &persisted
            && let Some((key, false)) = decided
        {
            self.remember_revocations(&[key]);
            tracing::warn!(device = %device_id, %error, "a pairing rejection could not be saved");
        }
        (decided, persisted)
    }

    /// What `key` is authorised as, or `None` when it has no entry, is still
    /// waiting for the host to confirm its pairing, or has not been confirmed
    /// by this run's authoritative connect or peer source.
    pub fn authorization(&self, key: &[u8; KEY_LEN]) -> Option<Authorization> {
        self.read()
            .get(key)
            .filter(|entry| entry.admits())
            .map(|entry| Authorization {
                role: entry.role,
                scopes: entry.scopes.clone(),
                device_id: entry.device_id.clone(),
                provenance: entry.provenance,
                machine: entry.machine.clone().map(HostId::new),
            })
    }

    /// The connect device id owning `key`, for audit attribution.
    pub fn device_for_key(&self, key: &[u8; KEY_LEN]) -> Option<String> {
        self.read()
            .get(key)
            .filter(|entry| entry.admits())
            .and_then(|entry| entry.device_id.clone())
    }

    /// The Noise key of a connect device id.
    pub fn key_for_device(&self, device_id: &str) -> Option<[u8; KEY_LEN]> {
        self.read()
            .iter()
            .find(|(_, entry)| entry.device_id.as_deref() == Some(device_id))
            .map(|(key, _)| *key)
    }

    /// The scopes a client key holds; `None` when unregistered or not a client.
    pub fn scopes_for_key(&self, key: &[u8; KEY_LEN]) -> Option<Vec<PairScope>> {
        self.authorization(key)
            .filter(|auth| auth.role == Role::Client)
            .map(|auth| auth.scopes)
    }

    /// Removes a connect device by id, returning its key.
    ///
    /// A failed write keeps the removal in memory and records it in the
    /// revocation sidecar: a restart must not bring a revoked device back.
    pub fn remove(&self, device_id: &str) -> (Option<[u8; KEY_LEN]>, Result<(), PersistError>) {
        let (removed, persisted) = self.mutate(false, |entries| {
            let key = entries
                .iter()
                .find(|(_, entry)| entry.device_id.as_deref() == Some(device_id))
                .map(|(key, _)| *key)?;
            entries.remove(&key);
            Some(key)
        });
        if let Err(error) = &persisted
            && let Some(key) = removed
        {
            self.remember_revocations(&[key]);
            tracing::warn!(device = %device_id, %error, "a device removal could not be saved");
        }
        (removed, persisted)
    }

    /// Removes one key, whatever its provenance (an explicit revoke).
    pub fn remove_key(&self, key: &[u8; KEY_LEN]) -> (bool, Result<(), PersistError>) {
        let (removed, persisted) = self.mutate(false, |entries| entries.remove(key).is_some());
        if let Err(error) = &persisted
            && removed
        {
            self.remember_revocations(&[*key]);
            tracing::warn!(%error, "a key removal could not be saved");
        }
        (removed, persisted)
    }

    /// Makes a connect snapshot authoritative for connect-managed clients.
    ///
    /// Every device in `devices` is inserted or updated and marked verified;
    /// every connect-managed client entry that is *absent* is removed and its
    /// key returned so the caller can terminate its sessions. Direct pairings
    /// and hosts are kept. The [`Result`] reports the first write failure; a
    /// removal stays in memory and is recorded in the revocation sidecar.
    pub fn reconcile_connect(
        &self,
        devices: &[ConnectEntry],
    ) -> (Vec<[u8; KEY_LEN]>, Result<(), PersistError>) {
        let present: std::collections::HashSet<[u8; KEY_LEN]> =
            devices.iter().map(|device| device.key).collect();
        let (removed, mut persisted) = self.mutate(false, |entries| {
            let stale: Vec<[u8; KEY_LEN]> = entries
                .iter()
                .filter(|(key, entry)| {
                    entry.provenance == Provenance::Connect
                        && entry.role == Role::Client
                        && !present.contains(*key)
                })
                .map(|(key, _)| *key)
                .collect();
            for key in &stale {
                entries.remove(key);
            }
            stale
        });
        if let Err(error) = &persisted
            && !removed.is_empty()
        {
            self.remember_revocations(&removed);
            tracing::warn!(%error, devices = removed.len(), "a snapshot revocation could not be saved");
        }
        for device in devices {
            // A snapshot may refresh scopes, but it must never promote a
            // device the host has not confirmed.
            if let Err(error) =
                self.provision_scoped(device.device_id.clone(), device.key, device.scopes.clone())
                && persisted.is_ok()
            {
                persisted = Err(error);
            }
        }
        (removed, persisted)
    }

    /// Every registered key, pending ones included.
    pub fn keys(&self) -> Vec<[u8; KEY_LEN]> {
        self.read().keys().copied().collect()
    }

    /// Keys the transport may admit: every registered entry whose pairing the
    /// host has confirmed *and* whose connect source has verified it this run.
    /// A pending relay device and an unverified connect entry are deliberately
    /// absent.
    pub fn admitted_keys(&self) -> Vec<[u8; KEY_LEN]> {
        self.read()
            .iter()
            .filter(|(_, entry)| entry.admits())
            .map(|(key, _)| *key)
            .collect()
    }

    /// Whether `key` may be admitted to the transport right now.
    pub fn is_admitted(&self, key: &[u8; KEY_LEN]) -> bool {
        self.read().get(key).is_some_and(Entry::admits)
    }

    /// Whether `key` is registered but still waiting for the host to confirm
    /// the pairing.
    pub fn is_pending(&self, key: &[u8; KEY_LEN]) -> bool {
        self.read().get(key).is_some_and(|entry| entry.pending)
    }

    /// The role a key is registered with, ignoring whether this run has
    /// verified it. Used to keep a re-role from overwriting an entry that is
    /// merely waiting for its authoritative source.
    pub fn registered_role(&self, key: &[u8; KEY_LEN]) -> Option<Role> {
        self.read().get(key).map(|entry| entry.role)
    }

    /// Every connect-managed peer-daemon key, for reconciliation after a
    /// restart when the in-memory peer directory is empty.
    pub fn connect_host_keys(&self) -> Vec<[u8; KEY_LEN]> {
        self.read()
            .iter()
            .filter(|(_, entry)| {
                entry.role == Role::Host && entry.provenance == Provenance::Connect
            })
            .map(|(key, _)| *key)
            .collect()
    }

    /// Number of registered keys.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Makes the next durable write fail, for the rollback tests.
    #[cfg(test)]
    fn fail_persist_once(&self) {
        self.fail_persist
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_registry(path: &Path) -> DeviceRegistry {
        DeviceRegistry::open(path)
    }

    #[test]
    fn a_pending_pairing_survives_a_restart_without_admitting_the_key() {
        // A relay-announced key must stay blocked across restarts until the
        // host decides; neither reloading nor an old registry defaults it open.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        {
            let devices = open_registry(&path);
            devices
                .provision_scoped("d1", [7u8; KEY_LEN], vec![PairScope::View])
                .expect("persist");
        }
        let devices = open_registry(&path);
        assert!(devices.is_pending(&[7u8; KEY_LEN]));
        assert!(devices.authorization(&[7u8; KEY_LEN]).is_none());
        assert!(!devices.admitted_keys().contains(&[7u8; KEY_LEN]));
        let (key, persisted) = devices.decide_pending("d1", true);
        persisted.expect("persist");
        let (key, confirm) = key.expect("pending");
        assert_eq!(key, [7u8; KEY_LEN]);
        assert!(confirm);
        assert!(devices.is_admitted(&[7u8; KEY_LEN]));
        assert_eq!(
            devices.scopes_for_key(&[7u8; KEY_LEN]),
            Some(vec![PairScope::View])
        );
    }

    #[test]
    fn a_restart_does_not_admit_unverified_connect_clients_or_hosts() {
        // H2: connect is the authority for its entries. A device revoked while
        // the daemon was offline from connect must not be admitted by a restart
        // before the first snapshot or peer report reconciles it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        {
            let devices = open_registry(&path);
            devices
                .insert_scoped("phone", [1u8; KEY_LEN], vec![PairScope::View])
                .expect("persist");
            devices
                .register_host(HostId::new("host_b"), [2u8; KEY_LEN], None)
                .expect("persist");
            devices
                .insert_direct([3u8; KEY_LEN], vec![PairScope::View])
                .expect("persist");
            assert!(devices.is_admitted(&[1u8; KEY_LEN]));
            assert!(devices.is_admitted(&[2u8; KEY_LEN]));
        }
        let devices = open_registry(&path);
        assert!(!devices.is_admitted(&[1u8; KEY_LEN]), "connect client");
        assert!(devices.authorization(&[1u8; KEY_LEN]).is_none());
        assert!(devices.scopes_for_key(&[1u8; KEY_LEN]).is_none());
        assert!(!devices.is_admitted(&[2u8; KEY_LEN]), "peer host");
        assert!(devices.authorization(&[2u8; KEY_LEN]).is_none());
        assert!(
            devices.is_admitted(&[3u8; KEY_LEN]),
            "a direct pairing does not depend on connect"
        );
        assert_eq!(
            devices.admitted_keys(),
            vec![[3u8; KEY_LEN]],
            "only the direct pairing may reach the transport"
        );
    }

    #[test]
    fn a_restart_snapshot_omitting_a_client_revokes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        {
            let devices = open_registry(&path);
            devices
                .insert_scoped("gone", [1u8; KEY_LEN], vec![PairScope::View])
                .expect("persist");
        }
        let devices = open_registry(&path);
        let (removed, persisted) = devices.reconcile_connect(&[]);
        persisted.expect("persist");
        assert_eq!(removed, vec![[1u8; KEY_LEN]]);
        assert!(devices.authorization(&[1u8; KEY_LEN]).is_none());
        assert!(!devices.is_admitted(&[1u8; KEY_LEN]));
    }

    #[test]
    fn a_restart_snapshot_including_a_client_admits_it_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        {
            let devices = open_registry(&path);
            devices
                .insert_scoped("kept", [1u8; KEY_LEN], vec![PairScope::View])
                .expect("persist");
        }
        let devices = open_registry(&path);
        assert!(!devices.is_admitted(&[1u8; KEY_LEN]));
        let (removed, persisted) = devices.reconcile_connect(&[ConnectEntry {
            device_id: "kept".into(),
            key: [1u8; KEY_LEN],
            scopes: vec![PairScope::View, PairScope::Approve],
        }]);
        persisted.expect("persist");
        assert!(removed.is_empty());
        assert!(devices.is_admitted(&[1u8; KEY_LEN]));
        assert_eq!(
            devices.scopes_for_key(&[1u8; KEY_LEN]),
            Some(vec![PairScope::View, PairScope::Approve])
        );
    }

    #[test]
    fn an_unpersistable_confirmation_rolls_back_and_reports() {
        // H4: a confirm the file cannot record must leave the device pending
        // and tell the caller, not report success and admit it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        let devices = open_registry(&path);
        devices
            .provision_scoped("d1", [7u8; KEY_LEN], vec![PairScope::View])
            .expect("persist");
        devices.fail_persist_once();
        let (decided, persisted) = devices.decide_pending("d1", true);
        assert!(
            decided.is_none(),
            "a rolled-back confirmation admits nothing"
        );
        assert!(persisted.is_err());
        assert!(devices.is_pending(&[7u8; KEY_LEN]));
        assert!(devices.authorization(&[7u8; KEY_LEN]).is_none());
        // The in-memory card is still there for a retry, which persists.
        assert_eq!(devices.pending_confirmations().len(), 0);
        let (decided, persisted) = devices.decide_pending("d1", true);
        persisted.expect("persist");
        assert!(decided.is_some());
        assert!(devices.is_admitted(&[7u8; KEY_LEN]));
    }

    #[test]
    fn an_unpersistable_rejection_stays_revoked_and_survives_a_restart() {
        // H4: a revoke is safer in memory than on disk, so it stays revoked
        // and the sidecar keeps a restart from re-admitting it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        let devices = open_registry(&path);
        devices
            .insert_direct([7u8; KEY_LEN], vec![PairScope::View])
            .expect("persist");
        devices.fail_persist_once();
        let (removed, persisted) = devices.remove_key(&[7u8; KEY_LEN]);
        assert!(removed);
        assert!(persisted.is_err());
        assert!(!devices.is_admitted(&[7u8; KEY_LEN]));
        assert!(
            path.with_file_name("devices.revoked").exists(),
            "the revocation sidecar is the retry that survives a crash"
        );
        drop(devices);
        let reopened = open_registry(&path);
        assert!(reopened.authorization(&[7u8; KEY_LEN]).is_none());
        assert!(reopened.keys().is_empty());
        assert!(
            !path.with_file_name("devices.revoked").exists(),
            "a successful rewrite clears the sidecar"
        );
    }

    #[test]
    fn an_unpersistable_pairing_rejection_is_recorded_in_the_sidecar() {
        // The exact path `ConfirmPairing { confirm: false }` takes.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        let devices = open_registry(&path);
        devices
            .provision_scoped("d1", [7u8; KEY_LEN], vec![PairScope::View])
            .expect("persist");
        devices.fail_persist_once();
        let (decided, persisted) = devices.decide_pending("d1", false);
        assert_eq!(decided, Some(([7u8; KEY_LEN], false)));
        assert!(persisted.is_err());
        assert!(!devices.is_pending(&[7u8; KEY_LEN]));
        assert!(!devices.authorization(&[7u8; KEY_LEN]).is_some());
        drop(devices);
        let reopened = open_registry(&path);
        assert!(reopened.keys().is_empty());
    }
}
