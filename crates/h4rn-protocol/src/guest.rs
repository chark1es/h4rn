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

/// Largest message text a guest may submit, in bytes.
///
/// Text is stored in the journal and shown to the owner, so it shares the same
/// 20 MB pending budget as the files rather than being unbounded.
pub const GUEST_MAX_TEXT_BYTES: usize = 32 * 1024;

/// Most messages one guest link may have waiting for the owner at once.
pub const GUEST_MAX_PENDING_REQUESTS: usize = 8;

/// Largest guest HTTP exchange the relay carries, in raw bytes.
///
/// A relayed body is split into [`GUEST_CHUNK_BYTES`] frames so one large
/// upload cannot grow a shared WebSocket frame past the transport's limit.
pub const GUEST_MAX_EXCHANGE_BYTES: usize = GUEST_MAX_PENDING_BYTES as usize + 1024 * 1024;

/// Raw bytes per relayed guest chunk. Base64 and JSON keep each frame far
/// below the relay's frame limit, and a full exchange fits the relay queue.
pub const GUEST_CHUNK_BYTES: usize = 512 * 1024;

/// Name of the cookie that binds a link to its first browser.
pub const GUEST_COOKIE: &str = "h4rn_guest";

/// The fixed guest page.
///
/// Connect serves this page from its own origin and the daemon serves it from
/// its listener, so one tested page is used in both places. It contains no
/// token and fetches only JSON from the path it was served from.
pub const GUEST_PAGE_HTML: &str = include_str!("guest-page.html");

/// Renders the page with its initial link state.
pub fn guest_page_html(state: &str) -> String {
    GUEST_PAGE_HTML.replace("__STATE__", state)
}

/// A content-security policy that admits only the page's own inline style and
/// script by hash.
///
/// The page is fixed and trusted, but hashing it means an injected string can
/// never become script: the browser runs only these exact bytes.
pub fn guest_page_csp() -> String {
    let script = csp_hash("script");
    let style = csp_hash("style");
    format!(
        "default-src 'none'; style-src {style}; script-src {script}; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
}

/// SHA-256 of the one inline `<script>` or `<style>` block, as a CSP source.
fn csp_hash(tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let body = GUEST_PAGE_HTML
        .split_once(&open)
        .and_then(|(_, rest)| rest.split_once(&close))
        .map(|(body, _)| body)
        .unwrap_or("");
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(body.as_bytes());
    format!(
        "'sha256-{}'",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, digest)
    )
}

/// The route template for a guest path, safe to log.
///
/// Guest paths carry a bearer token in the path, so tracing must name a fixed
/// template and never the raw URI. A relay path (`/guest/<machine>/<token>/…`)
/// and a direct path are both reported as guest templates; the exact shape is
/// not worth leaking a machine id for.
pub fn redact_guest_path(path: &str) -> String {
    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    match segments.as_slice() {
        ["guest"] => "/guest".to_owned(),
        ["guest", _] => "/guest/{token}".to_owned(),
        ["guest", _, _] => "/guest/{token}/{…}".to_owned(),
        ["guest", _, _, "state"] => "/guest/{token}/state".to_owned(),
        ["guest", _, _, "messages"] => "/guest/{token}/messages".to_owned(),
        ["guest", _, _, "blob", _] => "/guest/{token}/blob/{id}".to_owned(),
        ["guest", ..] => "/guest/{token}/…".to_owned(),
        _ => path.to_owned(),
    }
}

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
    /// Whether this machine has no address the link can open on.
    ///
    /// A link can exist while the daemon only listens on concrete network
    /// addresses: its guest routes answer on no address this machine can
    /// reach. The share dialog then explains the remedy instead of calling
    /// the link usable.
    #[serde(default)]
    pub no_serving_listener: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_csp_admits_only_the_pages_own_script_and_style_by_hash() {
        let csp = guest_page_csp();
        assert!(csp.contains("script-src 'sha256-"), "{csp}");
        assert!(csp.contains("style-src 'sha256-"), "{csp}");
        assert!(!csp.contains("unsafe-inline"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("connect-src 'self'"));
        // The two hashes are the script's and the style's, not the same one.
        assert_ne!(
            csp.split("script-src")
                .nth(1)
                .and_then(|rest| rest.split(';').next()),
            csp.split("style-src")
                .nth(1)
                .and_then(|rest| rest.split(';').next())
        );
    }

    #[test]
    fn the_page_is_static_and_carries_no_token_placeholder() {
        let page = guest_page_html("active");
        assert!(page.contains("data-state=\"active\""));
        assert!(!page.contains("__STATE__"));
        assert!(!page.contains("h4rn_guest="));
    }

    #[test]
    fn a_logged_guest_route_never_contains_the_token() {
        for path in [
            "/guest/tok_123",
            "/guest/tok_123/state",
            "/guest/tok_123/state/7",
            "/guest/tok_123/messages",
            "/guest/tok_123/blob/abc",
            "/guest/machine_1/tok_123/state/7",
        ] {
            let redacted = redact_guest_path(path);
            assert!(!redacted.contains("tok_123"), "{path} -> {redacted}");
            assert!(redacted.starts_with("/guest"), "{path} -> {redacted}");
        }
        assert_eq!(redact_guest_path("/healthz"), "/healthz");
    }
}
