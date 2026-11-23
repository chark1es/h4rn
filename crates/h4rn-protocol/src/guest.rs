//! Guest links: one thread, one browser, no account (ADR-044).
//!
//! A guest link is a bearer token in a URL. The daemon stores only its hash,
//! binds it to one thread and one browser, and expires it after a day. These
//! types cross the wire in three places: the owner's sharing requests, the
//! event that queues a guest message for the owner, and the small HTTP surface
//! the guest page reads.
//!
//! A guest sees the thread's events, never a second thread, a diff, a file or a
//! terminal, and a guest message is a *request*: it reaches the harness only
//! after the owner sends it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::HarnessKind;
use crate::ids::ThreadId;

/// How long a new link stays valid.
pub const GUEST_LINK_TTL_HOURS: i64 = 24;

/// Largest number of files one guest message may carry.
pub const GUEST_MAX_FILES: usize = 4;

/// Largest total size of one guest message's files, in bytes.
pub const GUEST_MAX_MESSAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Largest total size of every file waiting for the owner, in bytes.
pub const GUEST_MAX_PENDING_BYTES: u64 = 20 * 1024 * 1024;

/// Name of the cookie that binds a link to its first browser.
pub const GUEST_COOKIE: &str = "h4rn_guest";

/// One live link the owner created for a thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestLink {
    /// Absolute URL to send the guest, while this daemon run still holds the
    /// token in memory. After a restart the link keeps working for the guest,
    /// but the owner must create a new one to copy it again.
    pub url: Option<String>,
    /// When the link stops working.
    pub expires_at: DateTime<Utc>,
    /// Whether the first browser has already claimed it.
    pub bound: bool,
}

/// Sharing state for one thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestSharing {
    /// Thread the state belongs to.
    pub thread_id: ThreadId,
    /// Whether Settings → Labs → Guest links is on for this machine.
    pub links_enabled: bool,
    /// The live link, when one exists.
    pub link: Option<GuestLink>,
}

/// The machine-wide guest-links setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestSettings {
    /// Whether guest links may be created and used on this machine.
    pub enabled: bool,
}

/// One file a guest attached to a message that is waiting for the owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestAttachment {
    /// Sanitized display name.
    pub name: String,
    /// Sniffed media type.
    pub mime: String,
    /// Size in bytes.
    #[ts(type = "number")]
    pub size: u64,
}

/// A message a guest submitted and the owner has not sent or dismissed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestRequest {
    /// Opaque request id, stable across reconnects.
    pub request_id: String,
    /// Thread the message belongs to.
    pub thread_id: ThreadId,
    /// Message text.
    pub text: String,
    /// Files attached to the message.
    pub attachments: Vec<GuestAttachment>,
    /// When the guest submitted it.
    pub created_at: DateTime<Utc>,
}

/// One file in a guest message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestAttachmentInput {
    /// Original file name.
    pub name: String,
    /// The browser's media-type guess; the daemon sniffs the bytes too.
    pub mime: String,
    /// Base64 of the file bytes.
    pub data: String,
}

/// A guest's submitted message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestMessageInput {
    /// Message text.
    pub text: String,
    /// Attached files, at most [`GUEST_MAX_FILES`].
    #[serde(default)]
    pub attachments: Vec<GuestAttachmentInput>,
}

/// Why a guest link no longer works, or that it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum GuestLinkState {
    /// The link is live.
    Active,
    /// The 24 hours passed.
    Expired,
    /// The owner stopped sharing, settled the thread or made a new link.
    Revoked,
    /// Settings → Labs → Guest links is off on the machine.
    Disabled,
    /// This browser is not the one the link was claimed by.
    Unbound,
}

/// The read-only thread view a guest sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GuestThreadView {
    /// Thread id, so a refreshed page asks for the same log.
    pub thread_id: ThreadId,
    /// Thread title.
    pub title: String,
    /// Harness that runs it.
    pub harness: HarnessKind,
    /// Model slug, shown read-only.
    pub model: String,
    /// Last activity, for the "live" line.
    pub updated_at: DateTime<Utc>,
}

/// What the guest page reads on each poll.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct GuestSnapshot {
    /// Whether the link still works.
    pub state: GuestLinkState,
    /// The thread, when the link is live or still readable.
    #[serde(default)]
    pub thread: Option<GuestThreadView>,
    /// Events after the sequence the page asked for.
    pub events: Vec<crate::SeqEvent>,
    /// Next sequence to ask for.
    #[ts(type = "number")]
    pub next_seq: u64,
    /// A plain sentence for a link that no longer works.
    #[serde(default)]
    pub notice: Option<String>,
}
