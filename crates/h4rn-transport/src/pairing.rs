//! The pairing QR payload (ADR-010).
//!
//! A pairing payload is what the daemon prints as a QR code (or hands to a
//! deep link). It deliberately carries the **daemon's** static public key, the
//! endpoint to dial, and a one-time PSK:
//!
//! ```text
//! h4rn://pair?e=192.168.1.20%3A47800&a=100.64.1.5%3A47800&d=<x25519 public, hex>&p=<psk, hex>&l=Studio
//! ```
//!
//! `e` is the primary endpoint and each `a` is a further candidate (LAN,
//! tailnet or a `wss://` relay). A client probes them in the order given.
//!
//! The client pins `d`, so `IK` authenticates the daemon on the first message
//! and a stranger who photographs the screen still learns nothing that lets
//! them impersonate it. `p` is the one-time secret for a LAN/Tailscale pairing
//! made without h4rn-connect: it becomes the `IKpsk2` PSK, so the daemon also
//! authenticates the *client* and can add its public key to the allowlist.
//!
//! The PSK is single-use. A payload is rotated after one device pairs, and a
//! stale payload fails the handshake rather than granting access.

use crate::keys::{KEY_LEN, KeyStoreError};

/// Digits in a short authentication string.
pub const SAS_DIGITS: usize = 12;

/// The short authentication string for a typed-code pairing.
///
/// A typed code carries no key, so the daemon's Noise key reaches the new
/// device from the relay, which could substitute its own. Both ends therefore
/// derive this string from the two static keys *they* hold and the person
/// compares them on the two screens: a relay in the middle makes the two sides
/// disagree.
///
/// `client_public` is the new device's key as the daemon holds it,
/// `daemon_public` the daemon's key as the new device holds it. The result is
/// [`SAS_DIGITS`] decimal digits in groups of four (about 40 bits). That is
/// deliberately longer than the usual six: a short string can be ground against
/// in real time (an attacker generating key pairs until two strings collide),
/// 40 bits cannot.
pub fn short_auth_string(client_public: &[u8; KEY_LEN], daemon_public: &[u8; KEY_LEN]) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("h4rn pairing short authentication string v1");
    hasher.update(client_public);
    hasher.update(daemon_public);
    let mut bytes = [0u8; 8];
    hasher.finalize_xof().fill(&mut bytes);
    let value = u64::from_le_bytes(bytes) % 10u64.pow(SAS_DIGITS as u32);
    let digits = format!("{value:0width$}", width = SAS_DIGITS);
    digits
        .as_bytes()
        .chunks(4)
        .map(|group| std::str::from_utf8(group).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Scheme prefix of an encoded payload.
pub const SCHEME: &str = "h4rn://pair";

/// A decoded pairing payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingPayload {
    /// Primary advertised endpoint, `host:port` or a `wss://` URL.
    pub endpoint: String,
    /// Further candidate endpoints, in preference order (LAN, tailnet, relay).
    pub endpoints: Vec<String>,
    /// Daemon X25519 static public key.
    pub daemon_public: [u8; KEY_LEN],
    /// One-time pairing secret.
    pub psk: [u8; KEY_LEN],
    /// Human-readable machine label.
    pub label: Option<String>,
}

impl PairingPayload {
    /// Builds a payload from one primary endpoint plus candidates.
    pub fn new(
        endpoint: impl Into<String>,
        endpoints: impl IntoIterator<Item = String>,
        daemon_public: [u8; KEY_LEN],
        psk: [u8; KEY_LEN],
        label: Option<String>,
    ) -> Self {
        let endpoint = endpoint.into();
        let mut unique = Vec::new();
        for candidate in endpoints {
            if candidate != endpoint && !unique.contains(&candidate) {
                unique.push(candidate);
            }
        }
        Self {
            endpoint,
            endpoints: unique,
            daemon_public,
            psk,
            label,
        }
    }

    /// Every candidate endpoint, primary first.
    pub fn candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.endpoint.as_str()).chain(self.endpoints.iter().map(String::as_str))
    }

    /// Encodes the payload as a single URI.
    pub fn encode(&self) -> String {
        let mut out = format!(
            "{SCHEME}?e={}&d={}&p={}",
            percent_encode(&self.endpoint),
            hex::encode(self.daemon_public),
            hex::encode(self.psk)
        );
        for endpoint in &self.endpoints {
            out.push_str("&a=");
            out.push_str(&percent_encode(endpoint));
        }
        if let Some(label) = &self.label {
            out.push_str("&l=");
            out.push_str(&percent_encode(label));
        }
        out
    }

    /// Decodes a payload, rejecting anything that is not a complete pairing URI.
    pub fn decode(value: &str) -> Result<Self, KeyStoreError> {
        let value = value.trim();
        let rest = value
            .strip_prefix(SCHEME)
            .and_then(|rest| rest.strip_prefix('?'))
            .ok_or_else(|| KeyStoreError::Malformed("not a pairing URI".into()))?;
        let mut endpoint = None;
        let mut endpoints = Vec::new();
        let mut daemon = None;
        let mut psk = None;
        let mut label = None;
        for pair in rest.split('&') {
            let (key, raw) = pair
                .split_once('=')
                .ok_or_else(|| KeyStoreError::Malformed("pairing field without a value".into()))?;
            match key {
                "e" => endpoint = Some(percent_decode(raw)?),
                "a" => endpoints.push(percent_decode(raw)?),
                "d" => daemon = Some(parse_hex_key(raw)?),
                "p" => psk = Some(parse_hex_key(raw)?),
                "l" => label = Some(percent_decode(raw)?),
                _ => {}
            }
        }
        let endpoint =
            endpoint.ok_or_else(|| KeyStoreError::Malformed("missing endpoint".into()))?;
        if endpoint.is_empty() {
            return Err(KeyStoreError::Malformed("empty endpoint".into()));
        }
        let mut unique = Vec::new();
        for candidate in endpoints {
            if candidate != endpoint && !unique.contains(&candidate) {
                unique.push(candidate);
            }
        }
        Ok(Self {
            endpoint,
            endpoints: unique,
            daemon_public: daemon
                .ok_or_else(|| KeyStoreError::Malformed("missing daemon key".into()))?,
            psk: psk.ok_or_else(|| KeyStoreError::Malformed("missing pairing secret".into()))?,
            label,
        })
    }
}

fn parse_hex_key(value: &str) -> Result<[u8; KEY_LEN], KeyStoreError> {
    let bytes = hex::decode(value).map_err(|error| KeyStoreError::Malformed(error.to_string()))?;
    if bytes.len() != KEY_LEN {
        return Err(KeyStoreError::Malformed(format!(
            "key is {} bytes, expected {KEY_LEN}",
            bytes.len()
        )));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    if key == [0u8; KEY_LEN] {
        return Err(KeyStoreError::Malformed("key is all zeroes".into()));
    }
    Ok(key)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> Result<String, KeyStoreError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(KeyStoreError::Malformed("truncated percent escape".into()));
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|_| KeyStoreError::Malformed("bad percent escape".into()))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| KeyStoreError::Malformed("bad percent escape".into()))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| KeyStoreError::Malformed("endpoint is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> PairingPayload {
        PairingPayload::new(
            "192.168.1.20:47800",
            vec![
                "100.64.1.5:47800".into(),
                "wss://relay.example/secure".into(),
            ],
            [5u8; KEY_LEN],
            [6u8; KEY_LEN],
            Some("Studio Mac".into()),
        )
    }

    #[test]
    fn payload_round_trips() {
        let payload = payload();
        assert_eq!(PairingPayload::decode(&payload.encode()).unwrap(), payload);
    }

    #[test]
    fn payload_without_label_round_trips() {
        let payload = PairingPayload {
            label: None,
            ..payload()
        };
        assert_eq!(PairingPayload::decode(&payload.encode()).unwrap(), payload);
    }

    #[test]
    fn candidates_round_trip_in_priority_order() {
        let payload = payload();
        let encoded = payload.encode();
        assert!(encoded.contains("&a=100.64.1.5:47800"));
        let decoded = PairingPayload::decode(&encoded).unwrap();
        assert_eq!(
            decoded.candidates().collect::<Vec<_>>(),
            vec![
                "192.168.1.20:47800",
                "100.64.1.5:47800",
                "wss://relay.example/secure"
            ]
        );
    }

    #[test]
    fn non_pairing_input_is_rejected() {
        for bad in [
            "",
            "https://example.com",
            "h4rn://pair",
            "h4rn://pair?e=x",
            "h4rn://pair?e=x&d=00&p=00",
        ] {
            assert!(PairingPayload::decode(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn all_zero_keys_are_rejected() {
        let encoded = format!("{SCHEME}?e=x&d={}&p={}", "00".repeat(32), "11".repeat(32));
        assert!(PairingPayload::decode(&encoded).is_err());
    }

    #[test]
    fn the_short_auth_string_is_twelve_digits_in_groups_of_four() {
        let sas = short_auth_string(&[1u8; KEY_LEN], &[2u8; KEY_LEN]);
        let groups: Vec<&str> = sas.split(' ').collect();
        assert_eq!(groups.len(), 3, "{sas}");
        assert!(
            groups
                .iter()
                .all(|g| g.len() == 4 && g.bytes().all(|b| b.is_ascii_digit()))
        );
        // Stable: the phone (wasm) and the daemon (native) must agree.
        assert_eq!(sas, short_auth_string(&[1u8; KEY_LEN], &[2u8; KEY_LEN]));
    }

    #[test]
    fn a_substituted_key_changes_the_string_on_that_side() {
        let phone = [1u8; KEY_LEN];
        let daemon = [2u8; KEY_LEN];
        let relay = [3u8; KEY_LEN];
        // Honest relay: both screens agree.
        assert_eq!(
            short_auth_string(&phone, &daemon),
            short_auth_string(&phone, &daemon)
        );
        // MITM: the phone holds the relay's key for the daemon, the daemon
        // holds the relay's key for the phone.
        let on_phone = short_auth_string(&phone, &relay);
        let on_daemon = short_auth_string(&relay, &daemon);
        assert_ne!(on_phone, on_daemon);
        // The roles are not interchangeable.
        assert_ne!(
            short_auth_string(&phone, &daemon),
            short_auth_string(&daemon, &phone)
        );
    }
}
