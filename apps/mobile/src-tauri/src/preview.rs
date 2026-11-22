//! Preview documents on their own origin.
//!
//! A `srcdoc` frame inherits the window's CSP, and the shell's policy is
//! `script-src 'self'`: page scripts and the link bridge would never run in a
//! packaged build. The UI therefore hands the prepared HTML to the shell, gets
//! back a URL on the `h4rn-preview` scheme, and the shell serves the document
//! from memory with its own CSP header. The iframe keeps
//! `sandbox="allow-scripts"` without `allow-same-origin`, so the page still has
//! an opaque origin and cannot read the app.
//!
//! Entries are one-shot in spirit: the preview releases a URL when it closes
//! or reloads, and the store keeps only a few documents in case a release is
//! missed.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;
use tauri::http::{Response, StatusCode, header};

/// The URI scheme both shells register.
pub const PREVIEW_SCHEME: &str = "h4rn-preview";

/// The policy the document is served with. The page may run scripts and load
/// images, styles, scripts and fonts over `https:`; nothing talks to h4rn.
///
/// This header is also why page scripts work on Android at all: the document's
/// own `<meta>` policy is enough on its own, but Android WebViews without
/// `DOCUMENT_START_SCRIPT` make Wry inject the shell's initialization scripts
/// into this HTML and prepend their hashes to `script-src`, and a CSP with
/// hashes ignores `'unsafe-inline'`. A hash in that header cannot come from
/// here, so `preview_capabilities` reports the WebView's support before a
/// frame is created (see `docs/ux.md`).
const PREVIEW_CSP: &str = "default-src 'none'; img-src data: https:; media-src data: https:; object-src data:; style-src 'unsafe-inline' https:; script-src 'unsafe-inline' https:; font-src data: https:; connect-src https:";

/// Largest prepared document the shell stores, in bytes. The UI caps the
/// embedded workspace at 10 MB; base64 expansion and markup need headroom.
const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Documents kept at once, so a missed release cannot grow memory without
/// bound. A new preview evicts the oldest.
const MAX_DOCUMENTS: usize = 4;

#[derive(Default)]
struct Documents {
    entries: HashMap<String, String>,
    order: VecDeque<String>,
}

/// The in-memory map of prepared documents, keyed by a random token.
#[derive(Default)]
pub struct PreviewStore(Mutex<Documents>);

/// What the UI needs to frame one prepared document.
#[derive(Debug, Serialize)]
pub struct PreparedPreview {
    /// The URL to put in the iframe's `src`.
    ///
    /// The document already carries the bridge's own token; the shell does not
    /// mint a second one for the frame.
    pub url: String,
}

impl PreviewStore {
    fn insert(&self, html: String) -> Result<PreparedPreview, String> {
        if html.len() > MAX_DOCUMENT_BYTES {
            return Err(format!(
                "the prepared preview is larger than {} MiB",
                MAX_DOCUMENT_BYTES / 1_048_576
            ));
        }
        let token = new_token()?;
        let mut documents = self
            .0
            .lock()
            .map_err(|_| "the preview store is unavailable".to_owned())?;
        while documents.order.len() >= MAX_DOCUMENTS {
            let Some(oldest) = documents.order.pop_front() else {
                break;
            };
            documents.entries.remove(&oldest);
        }
        documents.entries.insert(token.clone(), html);
        documents.order.push_back(token.clone());
        Ok(PreparedPreview {
            url: preview_url(&token),
        })
    }

    fn remove(&self, token: &str) {
        if let Ok(mut documents) = self.0.lock() {
            documents.entries.remove(token);
            documents.order.retain(|entry| entry != token);
        }
    }

    fn get(&self, token: &str) -> Option<String> {
        self.0
            .lock()
            .ok()
            .and_then(|documents| documents.entries.get(token).cloned())
    }
}

/// The URL form the current platform's WebView uses for the scheme.
///
/// macOS, iOS and Linux serve `<scheme>://localhost/<path>`; Windows and
/// Android serve `http://<scheme>.localhost/<path>`.
pub fn preview_url(token: &str) -> String {
    if cfg!(any(windows, target_os = "android")) {
        format!("http://{PREVIEW_SCHEME}.localhost/{token}")
    } else {
        format!("{PREVIEW_SCHEME}://localhost/{token}")
    }
}

/// The token in a preview URL, if it is one of ours.
fn token_from_url(url: &str) -> Option<&str> {
    let path = url.split(['#', '?']).next()?;
    let token = path.trim_end_matches('/').rsplit('/').next()?;
    if token.is_empty() || !token.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(token)
}

/// 128 bits from the operating system's random source, so a page that learns
/// one token cannot guess another preview's URL.
fn new_token() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("no random source: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// What the preview's WebView can do, for the panel's honest notices.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewCapabilities {
    /// The shell platform: `android`, `ios` or `desktop` (a host test build).
    pub platform: &'static str,
    /// Whether the WebView runs the shell's scripts at document start.
    pub document_start_scripts: bool,
}

#[cfg(target_os = "android")]
const PLATFORM: &str = "android";
#[cfg(target_os = "ios")]
const PLATFORM: &str = "ios";
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const PLATFORM: &str = "desktop";

/// Reports the preview's WebView capabilities before a page is framed.
#[tauri::command]
pub fn preview_capabilities() -> PreviewCapabilities {
    PreviewCapabilities {
        platform: PLATFORM,
        document_start_scripts: document_start_scripts_supported(),
    }
}

/// Whether this WebView supports document-start scripts.
///
/// Wry checks `WebViewFeature.DOCUMENT_START_SCRIPT` on Android and, when it
/// is missing, injects the shell's initialization scripts into the HTML and
/// adds their hashes to `script-src`; a policy with hashes ignores
/// `'unsafe-inline'`, so the page's own scripts never run. That WebView
/// feature arrived in WebView 84, which is what the reported runtime version
/// is compared against here. The shell can answer before a frame is created,
/// so the panel never has to infer an Android limitation from a missing
/// bridge message on every platform.
#[cfg(target_os = "android")]
fn document_start_scripts_supported() -> bool {
    const DOCUMENT_START_WEBVIEW_MAJOR: u32 = 84;
    tauri::webview_version()
        .ok()
        .and_then(|version| version.split('.').next()?.parse::<u32>().ok())
        .is_none_or(|major| major >= DOCUMENT_START_WEBVIEW_MAJOR)
}

/// iOS WebViews (and a host test build of this crate) inject scripts at
/// document start.
#[cfg(not(target_os = "android"))]
fn document_start_scripts_supported() -> bool {
    true
}

/// Stores one prepared document; answers with the URL to frame it.
#[tauri::command]
pub fn preview_prepare(
    store: tauri::State<'_, PreviewStore>,
    html: String,
) -> Result<PreparedPreview, String> {
    store.insert(html)
}

/// Forgets a prepared document. A stale or unknown URL is not an error.
#[tauri::command]
pub fn preview_release(store: tauri::State<'_, PreviewStore>, url: String) -> Result<(), String> {
    if let Some(token) = token_from_url(&url) {
        store.remove(token);
    }
    Ok(())
}

/// The scheme handler: serves one stored document, or a plain 404.
pub fn respond(store: &PreviewStore, request: &tauri::http::Request<Vec<u8>>) -> Response<Vec<u8>> {
    let token = request.uri().path().trim_matches('/');
    match store.get(token) {
        Some(html) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CONTENT_SECURITY_POLICY, PREVIEW_CSP)
            .header(header::CACHE_CONTROL, "no-store")
            .header("x-content-type-options", "nosniff")
            .body(html.into_bytes())
            .unwrap_or_else(|_| not_found()),
        None => not_found(),
    }
}

fn not_found() -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(b"no such preview".to_vec())
        .expect("a static 404 response is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_of(prepared: &PreparedPreview) -> String {
        token_from_url(&prepared.url)
            .expect("a prepared URL carries its token")
            .to_owned()
    }

    fn request(path: &str) -> tauri::http::Request<Vec<u8>> {
        tauri::http::Request::builder()
            .uri(format!("{PREVIEW_SCHEME}://localhost{path}"))
            .body(Vec::new())
            .expect("request")
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn a_host_build_reports_desktop_webview_capabilities() {
        let capabilities = preview_capabilities();
        assert_eq!(capabilities.platform, "desktop");
        assert!(capabilities.document_start_scripts);
        // The UI reads camelCase; a snake_case field would silently default.
        let json = serde_json::to_value(capabilities).expect("capabilities json");
        assert_eq!(json["documentStartScripts"], true);
    }

    #[test]
    fn a_prepared_document_is_served_with_its_own_policy() {
        let store = PreviewStore::default();
        let prepared = store.insert("<p>hi</p>".into()).expect("store");
        let response = respond(&store, &request(&format!("/{}", token_of(&prepared))));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_SECURITY_POLICY),
            Some(&header::HeaderValue::from_static(PREVIEW_CSP))
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert_eq!(String::from_utf8_lossy(response.body()), "<p>hi</p>");
    }

    #[test]
    fn releasing_a_document_or_asking_for_an_unknown_token_is_plain() {
        let store = PreviewStore::default();
        let prepared = store.insert("<p>hi</p>".into()).expect("store");
        assert!(token_from_url(&prepared.url).is_some());

        store.remove(&token_of(&prepared));
        assert_eq!(
            respond(&store, &request(&format!("/{}", token_of(&prepared)))).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            respond(&store, &request("/not-a-token")).status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn the_header_policy_allows_inline_scripts_and_adds_no_hashes() {
        // Wry's Android fallback for WebViews without document-start scripts
        // rewrites `script-src` with hashes, which makes browsers ignore
        // `'unsafe-inline'`. This server must never do that itself: the policy
        // it serves is the documented one, and the only place hashes can come
        // from is that external fallback (which `preview_capabilities`
        // reports).
        let store = PreviewStore::default();
        let prepared = store.insert("<p>hi</p>".into()).expect("store");
        let response = respond(&store, &request(&format!("/{}", token_of(&prepared))));
        let csp = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .expect("policy header");
        assert_eq!(csp, PREVIEW_CSP);
        let script_src = csp
            .split(';')
            .map(str::trim)
            .find(|part| part.starts_with("script-src"))
            .expect("script-src present");
        assert!(script_src.contains("'unsafe-inline'"), "{script_src}");
        assert!(!csp.contains("'sha256-"), "{csp}");
        assert_eq!(String::from_utf8_lossy(response.body()), "<p>hi</p>");
    }

    #[test]
    fn a_new_document_evicts_the_oldest() {
        let store = PreviewStore::default();
        let first = store.insert("first".into()).expect("store");
        for index in 0..MAX_DOCUMENTS {
            store.insert(format!("doc {index}")).expect("store");
        }
        assert!(store.get(&token_of(&first)).is_none());
        assert!(store.get(&new_token().expect("random source")).is_none());
    }

    #[test]
    fn tokens_are_random_hex_and_urls_follow_the_platform_form() {
        let token = new_token().expect("random source");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(token, new_token().expect("random source"));
        assert!(preview_url(&token).contains(&token));
    }
}
