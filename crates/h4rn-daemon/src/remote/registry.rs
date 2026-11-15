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

use std::collections::HashMap;
use std::path::PathBuf;
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
}

impl Entry {
    fn key_bytes(&self) -> Option<[u8; KEY_LEN]> {
        hex::decode(&self.key).ok()?.as_slice().try_into().ok()
    }
}

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
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut entries = HashMap::new();
        match read_secret_file(&path) {
            Ok(Some(bytes)) => match serde_json::from_slice::<Vec<Entry>>(&bytes) {
                Ok(list) => {
                    for entry in list {
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
        Self {
            entries: RwLock::new(entries),
            path: Some(path),
            pending_direct: Mutex::new(None),
            confirmations: Mutex::new(Vec::new()),
            topology: tokio::sync::Notify::new(),
        }
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

    fn mutate<R>(&self, change: impl FnOnce(&mut HashMap<[u8; KEY_LEN], Entry>) -> R) -> R {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = change(&mut guard);
        self.persist(&guard);
        result
    }

    fn persist(&self, entries: &HashMap<[u8; KEY_LEN], Entry>) {
        let Some(path) = &self.path else { return };
        let mut list: Vec<&Entry> = entries.values().collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        match serde_json::to_vec(&list) {
            Ok(body) => {
                if let Err(error) = write_secret_file(path, &body) {
                    tracing::warn!(%error, "could not persist the device registry");
                }
            }
            Err(error) => tracing::warn!(%error, "could not serialise the device registry"),
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
    ) {
        self.upsert_connect(device_id.into(), key, scopes, Some(false));
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
    ) {
        self.upsert_connect(device_id.into(), key, scopes, None);
    }

    /// Registers a connect-paired client with `view` only.
    pub fn insert(&self, device_id: impl Into<String>, key: [u8; KEY_LEN]) {
        self.insert_scoped(device_id, key, Vec::new());
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
    ) {
        let scopes = PairScope::normalize(scopes);
        let entry = Entry {
            key: hex::encode(key),
            device_id: Some(device_id.clone()),
            role: Role::Client,
            scopes: scopes.clone(),
            provenance: Provenance::Connect,
            machine: None,
            pending: pending.unwrap_or(true),
        };
        self.mutate(|entries| {
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
                }
                None => {
                    entries.insert(key, entry);
                }
            }
        });
    }

    /// Registers a direct (LAN/tailnet) pairing with explicit scopes.
    pub fn insert_direct(&self, key: [u8; KEY_LEN], scopes: Vec<PairScope>) {
        let entry = Entry {
            key: hex::encode(key),
            device_id: None,
            role: Role::Client,
            scopes: PairScope::normalize(scopes),
            provenance: Provenance::Direct,
            machine: None,
            pending: false,
        };
        self.mutate(|entries| {
            entries.insert(key, entry);
        });
    }

    /// Registers a peer daemon, identified by its Noise key, as the host for
    /// `machine`. It may use the MCP tunnel and nothing else.
    pub fn register_host(&self, machine: HostId, key: [u8; KEY_LEN], device_id: Option<String>) {
        let entry = Entry {
            key: hex::encode(key),
            device_id,
            role: Role::Host,
            scopes: Vec::new(),
            provenance: Provenance::Connect,
            machine: Some(machine.to_string()),
            pending: false,
        };
        self.mutate(|entries| {
            entries.insert(key, entry);
        });
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
    pub fn complete_direct(&self, key: [u8; KEY_LEN]) {
        let scopes = self
            .pending_direct
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| PairScope::MINIMAL.to_vec());
        self.insert_direct(key, scopes);
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
    /// makes the request safe to retry.
    pub fn decide_pending(&self, device_id: &str, confirm: bool) -> Option<([u8; KEY_LEN], bool)> {
        let decided = self.mutate(|entries| {
            let key = entries
                .iter()
                .find(|(_, entry)| {
                    entry.pending
                        && entry.role == Role::Client
                        && entry.device_id.as_deref() == Some(device_id)
                })
                .map(|(key, _)| *key)?;
            if confirm {
                entries.get_mut(&key)?.pending = false;
            } else {
                entries.remove(&key);
            }
            Some(key)
        });
        self.confirmations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|entry| entry.device_id != device_id);
        decided.map(|key| (key, confirm))
    }

    /// What `key` is authorised as, or `None` when it has no entry or is still
    /// waiting for the host to confirm its pairing.
    pub fn authorization(&self, key: &[u8; KEY_LEN]) -> Option<Authorization> {
        self.read()
            .get(key)
            .filter(|entry| !entry.pending)
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
            .filter(|entry| !entry.pending)
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
    pub fn remove(&self, device_id: &str) -> Option<[u8; KEY_LEN]> {
        self.mutate(|entries| {
            let key = entries
                .iter()
                .find(|(_, entry)| entry.device_id.as_deref() == Some(device_id))
                .map(|(key, _)| *key)?;
            entries.remove(&key);
            Some(key)
        })
    }

    /// Removes one key, whatever its provenance (an explicit revoke).
    pub fn remove_key(&self, key: &[u8; KEY_LEN]) -> bool {
        self.mutate(|entries| entries.remove(key).is_some())
    }

    /// Makes a connect snapshot authoritative for connect-managed clients.
    ///
    /// Every device in `devices` is inserted or updated; every connect-managed
    /// client entry that is *absent* is removed and its key returned so the
    /// caller can terminate its sessions. Direct pairings and hosts are kept.
    pub fn reconcile_connect(&self, devices: &[ConnectEntry]) -> Vec<[u8; KEY_LEN]> {
        let present: std::collections::HashSet<[u8; KEY_LEN]> =
            devices.iter().map(|device| device.key).collect();
        let removed = self.mutate(|entries| {
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
        for device in devices {
            // A snapshot may refresh scopes, but it must never promote a
            // device the host has not confirmed.
            self.provision_scoped(device.device_id.clone(), device.key, device.scopes.clone());
        }
        removed
    }

    /// Every registered key, pending ones included.
    pub fn keys(&self) -> Vec<[u8; KEY_LEN]> {
        self.read().keys().copied().collect()
    }

    /// Keys the transport may admit: every registered entry whose pairing the
    /// host has confirmed. A pending relay device is deliberately absent.
    pub fn admitted_keys(&self) -> Vec<[u8; KEY_LEN]> {
        self.read()
            .iter()
            .filter(|(_, entry)| !entry.pending)
            .map(|(key, _)| *key)
            .collect()
    }

    /// Whether `key` may be admitted to the transport right now.
    pub fn is_admitted(&self, key: &[u8; KEY_LEN]) -> bool {
        self.read().get(key).is_some_and(|entry| !entry.pending)
    }

    /// Whether `key` is registered but still waiting for the host to confirm
    /// the pairing.
    pub fn is_pending(&self, key: &[u8; KEY_LEN]) -> bool {
        self.read().get(key).is_some_and(|entry| entry.pending)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_pairing_survives_a_restart_without_admitting_the_key() {
        // A relay-announced key must stay blocked across restarts until the
        // host decides; neither reloading nor an old registry defaults it open.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("devices.json");
        {
            let devices = DeviceRegistry::open(&path);
            devices.provision_scoped("d1", [7u8; KEY_LEN], vec![PairScope::View]);
        }
        let devices = DeviceRegistry::open(&path);
        assert!(devices.is_pending(&[7u8; KEY_LEN]));
        assert!(devices.authorization(&[7u8; KEY_LEN]).is_none());
        assert!(!devices.admitted_keys().contains(&[7u8; KEY_LEN]));
        let (key, confirm) = devices.decide_pending("d1", true).expect("pending");
        assert_eq!(key, [7u8; KEY_LEN]);
        assert!(confirm);
        assert!(devices.is_admitted(&[7u8; KEY_LEN]));
        assert_eq!(
            devices.scopes_for_key(&[7u8; KEY_LEN]),
            Some(vec![PairScope::View])
        );
    }
}
