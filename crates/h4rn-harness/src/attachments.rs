//! Per-harness attachment delivery (ADR-008).
//!
//! Context addressing is daemon-side; what the model actually receives is the
//! harness's business. This module owns that matrix so each adapter can ask
//! "how should this attachment be delivered?" instead of re-deriving media
//! types and prompt lines. The rule that never changes is the path line: every
//! attachment tells the model the absolute path it was materialised at, even
//! when a native block also carries the bytes.
//!
//! | Harness | Images | PDFs and other files |
//! | --- | --- | --- |
//! | Claude Code | native `image` block (base64) | path reference |
//! | Codex | native `localImage` path item | path reference |
//! | OpenCode | `file://` part for local servers | path reference |
//! | Antigravity (ACP) | image block when the capability negotiated | path reference |

use std::path::Path;

use h4rn_protocol::HarnessKind;

use crate::AttachmentRef;

/// What kind of content an attachment holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentClass {
    /// Raster image; a harness may accept it natively.
    Image,
    /// PDF or another document that some harnesses accept natively.
    Document,
    /// Everything else, always delivered by path.
    Other,
}

impl AttachmentClass {
    /// Classifies a media type, falling back to the file extension.
    pub fn of(media_type: &str, path: &Path) -> Self {
        let media_type = if media_type.is_empty() {
            media_type_from_path(path).to_owned()
        } else {
            media_type.to_ascii_lowercase()
        };
        if media_type.starts_with("image/") {
            Self::Image
        } else if media_type == "application/pdf" {
            Self::Document
        } else {
            Self::Other
        }
    }
}

/// How one attachment reaches one harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentDelivery {
    /// Native base64 image content block (Claude Code).
    NativeImageBlock,
    /// Native `localImage` path item (Codex).
    LocalImagePath,
    /// ACP image block, only after the capability was negotiated.
    AcpImageBlock,
    /// `file://` prompt part (OpenCode, local servers only).
    FileUrl,
    /// A line in the prompt naming the absolute path.
    PathReference,
}

/// The delivery strategy for a harness and attachment class.
pub fn strategy(harness: HarnessKind, class: AttachmentClass) -> AttachmentDelivery {
    if class != AttachmentClass::Image {
        // Phase 2 deliberately keeps PDFs and documents on the path fallback;
        // native document blocks land with the per-harness capability work.
        return AttachmentDelivery::PathReference;
    }
    match harness {
        HarnessKind::ClaudeCode => AttachmentDelivery::NativeImageBlock,
        HarnessKind::Codex => AttachmentDelivery::LocalImagePath,
        HarnessKind::Opencode => AttachmentDelivery::FileUrl,
        HarnessKind::Antigravity => AttachmentDelivery::AcpImageBlock,
    }
}

/// Resolves the definitive media type for an attachment.
///
/// Adapters that read bytes must not guess differently from the daemon, so the
/// declared type wins and the extension is only a fallback when the blob has
/// no usable type.
pub fn media_type(attachment: &AttachmentRef) -> String {
    if !attachment.media_type.is_empty() {
        return attachment.media_type.clone();
    }
    media_type_from_path(&attachment.path).to_owned()
}

/// The prompt line that tells the model where the bytes live.
///
/// It is intentionally identical across harnesses: the model can always fall
/// back to reading the file when a native block was not negotiated.
pub fn path_line(attachment: &AttachmentRef) -> String {
    let name = attachment
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| attachment.id.to_string());
    let kind = match AttachmentClass::of(&media_type(attachment), &attachment.path) {
        AttachmentClass::Image => "image",
        AttachmentClass::Document => "document",
        AttachmentClass::Other => "file",
    };
    format!(
        "[Attached {kind} \"{name}\" is saved at: {}]",
        attachment.path.display()
    )
}

/// Reads a text attachment, decoding UTF-8 (with or without a BOM) and
/// UTF-16LE/UTF-16BE with a byte-order mark.
///
/// Returns `None` when the bytes are not valid text in a supported encoding so
/// the caller can keep delivering the file by path instead of inlining it. A
/// UTF-8 BOM is stripped; it is an encoding marker, not content.
pub fn read_text(path: &Path) -> Option<String> {
    decode_text(&std::fs::read(path).ok()?)
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    // UTF-32LE starts with the UTF-16LE BOM bytes followed by two NULs, and
    // would otherwise decode as NUL-filled text. Neither UTF-32 order is
    // inlineable, so the file stays a link.
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
    {
        return None;
    }
    let text = if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        std::str::from_utf8(body).ok().map(str::to_owned)
    } else if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        decode_utf16(body, u16::from_le_bytes)
    } else if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        decode_utf16(body, u16::from_be_bytes)
    } else {
        std::str::from_utf8(bytes).ok().map(str::to_owned)
    }?;
    // A NUL means the bytes were not the text they appeared to be (for
    // example a binary file that happens to decode). Keep it a link.
    (!text.contains('\0')).then_some(text)
}

fn decode_utf16(bytes: &[u8], decode: fn([u8; 2]) -> u16) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| decode([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

fn media_type_from_path(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("md") => "text/markdown",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use h4rn_protocol::AttachmentId;

    use super::*;

    fn attachment(path: &str, media_type: &str) -> AttachmentRef {
        AttachmentRef {
            id: AttachmentId::from_content(path.as_bytes()),
            path: PathBuf::from(path),
            media_type: media_type.into(),
        }
    }

    #[test]
    fn images_use_the_native_strategy_for_each_harness() {
        assert_eq!(
            strategy(HarnessKind::ClaudeCode, AttachmentClass::Image),
            AttachmentDelivery::NativeImageBlock
        );
        assert_eq!(
            strategy(HarnessKind::Codex, AttachmentClass::Image),
            AttachmentDelivery::LocalImagePath
        );
        assert_eq!(
            strategy(HarnessKind::Opencode, AttachmentClass::Image),
            AttachmentDelivery::FileUrl
        );
        assert_eq!(
            strategy(HarnessKind::Antigravity, AttachmentClass::Image),
            AttachmentDelivery::AcpImageBlock
        );
    }

    #[test]
    fn documents_and_other_files_always_fall_back_to_a_path() {
        for harness in [
            HarnessKind::ClaudeCode,
            HarnessKind::Codex,
            HarnessKind::Opencode,
            HarnessKind::Antigravity,
        ] {
            assert_eq!(
                strategy(harness, AttachmentClass::Document),
                AttachmentDelivery::PathReference
            );
            assert_eq!(
                strategy(harness, AttachmentClass::Other),
                AttachmentDelivery::PathReference
            );
        }
    }

    #[test]
    fn classes_fall_back_to_the_extension() {
        assert_eq!(
            AttachmentClass::of("", Path::new("/tmp/a.PNG")),
            AttachmentClass::Image
        );
        assert_eq!(
            AttachmentClass::of("", Path::new("/tmp/a.pdf")),
            AttachmentClass::Document
        );
        assert_eq!(
            AttachmentClass::of("", Path::new("/tmp/a.bin")),
            AttachmentClass::Other
        );
    }

    #[test]
    fn text_attachments_decode_utf8_and_both_utf16_byte_orders() {
        let utf8 = "Aim: 🚀\n".as_bytes();
        assert_eq!(decode_text(utf8).as_deref(), Some("Aim: 🚀\n"));

        let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
        utf8_bom.extend_from_slice(utf8);
        assert_eq!(decode_text(&utf8_bom).as_deref(), Some("Aim: 🚀\n"));

        let text = "日本語 notes\n";
        let mut utf16_le = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            utf16_le.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_text(&utf16_le).as_deref(), Some(text));

        let mut utf16_be = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            utf16_be.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(decode_text(&utf16_be).as_deref(), Some(text));
    }

    #[test]
    fn binary_attachments_stay_unreadable() {
        assert_eq!(decode_text(&[0xFF, 0x00, 0xFE, 0x01]), None);
        assert_eq!(decode_text(&[0xFF, 0xFE, 0x41]), None);
        assert_eq!(decode_text(&[0x89, b'P', b'N', b'G']), None);
    }

    #[test]
    fn utf32_files_are_links_not_nul_filled_text() {
        // UTF-32LE starts with the UTF-16LE BOM followed by two NULs.
        let mut utf32_le = vec![0xFF, 0xFE, 0x00, 0x00];
        for ch in "notes\n".chars() {
            utf32_le.extend_from_slice(&(ch as u32).to_le_bytes());
        }
        assert_eq!(decode_text(&utf32_le), None);

        let mut utf32_be = vec![0x00, 0x00, 0xFE, 0xFF];
        for ch in "notes\n".chars() {
            utf32_be.extend_from_slice(&(ch as u32).to_be_bytes());
        }
        assert_eq!(decode_text(&utf32_be), None);
    }

    #[test]
    fn decoded_text_with_a_nul_is_not_inlined() {
        // UTF-16LE "A\0" decodes cleanly but is not text worth embedding.
        assert_eq!(decode_text(&[0xFF, 0xFE, 0x41, 0x00, 0x00, 0x00]), None);
        assert_eq!(decode_text(b"a\0b"), None);
    }

    #[test]
    fn read_text_uses_the_file_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        let mut encoded = vec![0xFF, 0xFE];
        for unit in "héllo".encode_utf16() {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, encoded).expect("write");
        assert_eq!(read_text(&path).as_deref(), Some("héllo"));
    }

    #[test]
    fn path_lines_name_the_kind_and_absolute_path() {
        assert_eq!(
            path_line(&attachment("/tmp/shot.png", "image/png")),
            "[Attached image \"shot.png\" is saved at: /tmp/shot.png]"
        );
        assert_eq!(
            path_line(&attachment("/tmp/spec.pdf", "application/pdf")),
            "[Attached document \"spec.pdf\" is saved at: /tmp/spec.pdf]"
        );
        assert_eq!(
            path_line(&attachment("/tmp/notes.csv", "")),
            "[Attached file \"notes.csv\" is saved at: /tmp/notes.csv]"
        );
    }
}
