//! Text detection, encoding and content addressing for the editor.
//!
//! The editor only ever edits text. This module decides what counts as text,
//! which encoding it arrived in, and how to write it back without silently
//! changing a file's bytes (a UTF-16 file must stay UTF-16). Anything that is
//! not valid UTF-8 or BOM-marked UTF-16 is reported as binary and refused with
//! a hint rather than guessed at.

/// A decoded text file and the encoding it used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// Stable encoding name used on the wire.
    pub encoding: &'static str,
    /// Decoded text, without the BOM.
    pub text: String,
}

/// BLAKE3 content address of the raw file bytes.
pub fn etag(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Decodes a file, or returns `None` when it is binary.
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    if bytes.iter().take(8 * 1024).any(|byte| *byte == 0) {
        // UTF-16 is checked first because it legitimately contains NUL bytes.
        if let Some(decoded) = decode_utf16(bytes) {
            return Some(decoded);
        }
        return None;
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let text = std::str::from_utf8(&bytes[3..]).ok()?.to_owned();
        return Some(Decoded {
            encoding: "utf-8-bom",
            text,
        });
    }
    if let Some(decoded) = decode_utf16(bytes) {
        return Some(decoded);
    }
    let text = std::str::from_utf8(bytes).ok()?.to_owned();
    Some(Decoded {
        encoding: "utf-8",
        text,
    })
}

fn decode_utf16(bytes: &[u8]) -> Option<Decoded> {
    let (encoding, body) = if bytes.starts_with(&[0xFF, 0xFE]) {
        ("utf-16le", &bytes[2..])
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        ("utf-16be", &bytes[2..])
    } else {
        return None;
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| {
            if encoding == "utf-16le" {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    let text = String::from_utf16(&units).ok()?;
    Some(Decoded { encoding, text })
}

/// Encodes text for writing, preserving the file's existing encoding.
///
/// Unknown or binary encodings fall back to plain UTF-8. A BOM-marked encoding
/// gets its BOM back so the next reader sees the same file.
pub fn encode(text: &str, encoding: &str) -> Vec<u8> {
    match encoding {
        "utf-16le" => {
            let mut bytes = vec![0xFF, 0xFE];
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        }
        "utf-16be" => {
            let mut bytes = vec![0xFE, 0xFF];
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_be_bytes());
            }
            bytes
        }
        "utf-8-bom" => {
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
        _ => text.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_utf8_round_trips() {
        let bytes = b"fn main() {}\n";
        let decoded = decode(bytes).unwrap();
        assert_eq!(decoded.encoding, "utf-8");
        assert_eq!(encode(&decoded.text, decoded.encoding), bytes);
        assert_eq!(etag(bytes), etag(bytes));
    }

    #[test]
    fn bom_and_utf16_are_preserved() {
        let utf8 = [0xEF, 0xBB, 0xBF, b'h', b'i'];
        let decoded = decode(&utf8).unwrap();
        assert_eq!(decoded.encoding, "utf-8-bom");
        assert_eq!(decoded.text, "hi");
        assert_eq!(encode("hi", "utf-8-bom"), utf8);

        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "hi".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode(&utf16).unwrap();
        assert_eq!(decoded.encoding, "utf-16le");
        assert_eq!(decoded.text, "hi");
        assert_eq!(encode("hi", "utf-16le"), utf16);
    }

    #[test]
    fn binary_is_refused() {
        assert!(decode(&[0x00, 0x01, 0x02, 0xFF]).is_none());
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0xE0]).is_none());
    }
}
