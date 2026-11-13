//! Terminal QR rendering and the direct (`--direct`) pairing hand-off.
//!
//! `h4rnd pair` prints a QR for a new device. The normal flow creates a pairing
//! on h4rn-connect and renders the `h4rn://pair?...` URL connect returns. The
//! `--direct` flow skips connect: the daemon writes a short-lived PSK plus its
//! address candidates to `<data_dir>/direct-pairing.json`, the running daemon
//! picks that up, and the QR carries the LAN/tailnet endpoint, the daemon's
//! Noise key and the one-time secret (ADR-010).
//!
//! The PSK is single-use: the transport clears it after the first successful
//! pairing, and the daemon clears it when the file expires.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use h4rn_transport::keys::{KEY_LEN, read_secret_file, write_secret_file};
use h4rn_transport::pairing::PairingPayload;
use qrcode::{Color, QrCode};
use serde::{Deserialize, Serialize};

/// File the CLI writes and the daemon watches.
pub const DIRECT_PAIRING_FILE: &str = "direct-pairing.json";

/// How long a direct pairing stays open.
pub const DEFAULT_DIRECT_TTL_SECS: i64 = 300;

/// A pending direct pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectPairingFile {
    /// One-time PSK, hex.
    pub psk: String,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
    /// Advertised addresses, preferred first (`host:port`).
    pub endpoints: Vec<String>,
    /// Human label, normally the host name.
    pub label: Option<String>,
}

impl DirectPairingFile {
    /// Whether the pairing has expired.
    pub fn expired(&self) -> bool {
        self.expires_at <= Utc::now().timestamp()
    }

    /// Decodes the PSK, rejecting a malformed file.
    pub fn psk(&self) -> Result<[u8; KEY_LEN]> {
        let bytes = hex::decode(self.psk.trim()).context("direct pairing psk is not hex")?;
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("direct pairing psk must be {KEY_LEN} bytes"))
    }
}

/// Path of the direct pairing file.
pub fn direct_pairing_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DIRECT_PAIRING_FILE)
}

/// Writes the direct pairing file with private permissions.
pub fn write_direct_pairing(data_dir: &Path, pairing: &DirectPairingFile) -> Result<PathBuf> {
    let path = direct_pairing_path(data_dir);
    let body = serde_json::to_vec(pairing).context("serialise direct pairing")?;
    write_secret_file(&path, &body)
        .with_context(|| format!("write direct pairing {}", path.display()))?;
    Ok(path)
}

/// Reads the direct pairing file, returning `None` when absent.
pub fn read_direct_pairing(data_dir: &Path) -> Result<Option<DirectPairingFile>> {
    let path = direct_pairing_path(data_dir);
    let Some(bytes) = read_secret_file(&path)
        .with_context(|| format!("read direct pairing {}", path.display()))?
    else {
        return Ok(None);
    };
    let pairing: DirectPairingFile =
        serde_json::from_slice(&bytes).context("parse direct pairing file")?;
    Ok(Some(pairing))
}

/// Builds the QR payload for a direct pairing.
pub fn direct_payload(
    endpoints: &[String],
    daemon_public: [u8; KEY_LEN],
    psk: [u8; KEY_LEN],
    label: Option<String>,
) -> Result<PairingPayload> {
    let primary = endpoints
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("a direct pairing needs at least one endpoint"))?;
    Ok(PairingPayload::new(
        primary,
        endpoints.iter().skip(1).cloned(),
        daemon_public,
        psk,
        label,
    ))
}

/// Renders `text` as a QR code using Unicode half blocks.
///
/// Two module rows share one terminal row: `▀` is dark on top, `▄` dark on the
/// bottom, `█` both and a space neither. A two-module quiet zone keeps scanners
/// happy on a white-on-black terminal without doubling the output height.
pub fn render_qr(text: &str) -> Result<String> {
    let code = QrCode::new(text.as_bytes()).context("encode the QR code")?;
    let width = code.width();
    let colors = code.to_colors();
    let dark = |x: usize, y: usize| -> bool {
        if x >= width || y >= width {
            return false;
        }
        colors[y * width + x] == Color::Dark
    };
    let quiet = 2;
    let mut out = String::new();
    for row in 0..width.div_ceil(2) {
        let top = row * 2;
        let bottom = top + 1;
        for x in 0..(width + quiet * 2) {
            if x < quiet || x >= width + quiet {
                out.push(' ');
                continue;
            }
            let column = x - quiet;
            match (dark(column, top), dark(column, bottom)) {
                (true, true) => out.push('█'),
                (true, false) => out.push('▀'),
                (false, true) => out.push('▄'),
                (false, false) => out.push(' '),
            }
        }
        out.push('\n');
    }
    Ok(out.trim_end_matches('\n').to_owned())
}

/// Formats an expiry as a short local-time hint.
pub fn expiry_hint(expires_at: i64) -> String {
    DateTime::from_timestamp(expires_at, 0)
        .map(|at| at.format("%H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "soon".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_rendering_uses_half_blocks_and_a_quiet_zone() {
        let code = render_qr("h4rn://pair?e=192.168.1.20:47800").unwrap();
        assert!(code.contains('█') || code.contains('▀') || code.contains('▄'));
        assert!(code.lines().count() > 4);
        assert!(!code.ends_with('\n'));
    }

    #[test]
    fn direct_pairing_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = DirectPairingFile {
            psk: "11".repeat(KEY_LEN),
            expires_at: Utc::now().timestamp() + 60,
            endpoints: vec!["192.168.1.20:47800".into()],
            label: Some("studio".into()),
        };
        write_direct_pairing(dir.path(), &pairing).unwrap();
        assert_eq!(
            read_direct_pairing(dir.path()).unwrap(),
            Some(pairing.clone())
        );
        assert!(!pairing.expired());
        assert_eq!(pairing.psk().unwrap(), [0x11u8; KEY_LEN]);
    }

    #[test]
    fn payload_puts_the_first_endpoint_in_front() {
        let payload = direct_payload(
            &["100.64.1.5:47800".into(), "192.168.1.20:47800".into()],
            [1u8; KEY_LEN],
            [2u8; KEY_LEN],
            Some("studio".into()),
        )
        .unwrap();
        assert_eq!(
            payload.candidates().collect::<Vec<_>>(),
            vec!["100.64.1.5:47800", "192.168.1.20:47800"]
        );
    }
}
