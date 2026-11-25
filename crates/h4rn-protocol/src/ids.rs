//! Typed identifiers used by the wire protocol.
//!
//! Identifiers are newtypes over `String` so a `HostId` can never be passed
//! where a `ThreadId` is expected. They serialise transparently as JSON
//! strings (ADR-002). New ids use UUID v7 so they sort by creation time.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;
use uuid::Uuid;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
        pub struct $name(String);

        impl $name {
            /// Wraps an existing identifier.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Creates a time-sortable UUID v7 identifier with this type's prefix.
            pub fn new_uuid_v7() -> Self {
                Self(format!("{}{}", $prefix, Uuid::now_v7()))
            }

            /// Borrows the identifier as a string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the identifier and returns the inner string.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(value.to_owned()))
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// Identifies a daemon host (a laptop, server, WSL distro or remote box).
    HostId,
    "host_"
);
string_id!(
    /// Identifies a conversation thread and its event log.
    ThreadId,
    "th_"
);
string_id!(
    /// Identifies one user turn inside a thread.
    TurnId,
    "turn_"
);
string_id!(
    /// Identifies a subagent thread spawned by a parent thread.
    SubagentId,
    "sa_"
);
string_id!(
    /// Identifies a paired client device.
    DeviceId,
    "dev_"
);
string_id!(
    /// Identifies one daemon-owned terminal (PTY) session.
    TerminalId,
    "term_"
);
string_id!(
    /// Identifies one scheduled task.
    ScheduleTaskId,
    "sch_"
);
string_id!(
    /// Identifies one message waiting in a thread's follow-up queue.
    QueuedMessageId,
    "qm_"
);

/// The BLAKE3 content address of an attachment blob (ADR-008).
///
/// The wire form is 64 lowercase hexadecimal characters: the plain BLAKE3
/// digest, with no type prefix, so identical bytes dedupe across threads.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
pub struct AttachmentId(String);

impl AttachmentId {
    /// Wraps an already-computed content address.
    ///
    /// Prefer [`AttachmentId::from_content`] when the bytes are available; use
    /// [`AttachmentId::parse`] when the id arrives from the wire.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Computes the BLAKE3 content address of `bytes`.
    pub fn from_content(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// Parses and validates a content address from the wire.
    pub fn parse(value: impl Into<String>) -> Result<Self, AttachmentIdError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            Ok(Self(value))
        } else {
            Err(AttachmentIdError::InvalidHex { value })
        }
    }

    /// Borrows the content address as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AttachmentId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for AttachmentId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl FromStr for AttachmentId {
    type Err = AttachmentIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl AsRef<str> for AttachmentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Errors returned when an attachment id is not a BLAKE3 hex digest.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttachmentIdError {
    /// The value was not 64 lowercase hexadecimal characters.
    #[error("attachment id must be 64 lowercase hex characters, got {value:?}")]
    InvalidHex {
        /// The rejected value.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_address_is_stable_and_lowercase_hex() {
        let empty = AttachmentId::from_content(b"");
        assert_eq!(
            empty.as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert!(empty.as_str().bytes().all(|b| !b.is_ascii_uppercase()));
        assert_eq!(empty, AttachmentId::parse(empty.as_str()).unwrap());
    }

    #[test]
    fn parse_rejects_bad_length_and_uppercase() {
        assert!(AttachmentId::parse("abc").is_err());
        assert!(AttachmentId::parse("A".repeat(64)).is_err());
        assert!(AttachmentId::parse("g".repeat(64)).is_err());
    }

    #[test]
    fn ids_are_typed_strings_with_stable_prefixes() {
        let thread = ThreadId::new_uuid_v7();
        assert!(thread.as_str().starts_with("th_"));
        assert_eq!(thread.to_string(), thread.as_str());

        let host = HostId::from("atlas");
        assert_eq!(host.as_ref(), "atlas");
    }

    #[test]
    fn ids_serialise_transparently() {
        let id = ThreadId::new("th_123");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"th_123\"");
    }
}
