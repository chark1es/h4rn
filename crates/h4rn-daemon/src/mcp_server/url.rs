//! OAuth redirect and PKCE helpers for outside agents.
//!
//! An outside agent finishes OAuth by sending the browser to a redirect URI it
//! registered. That URI is the one place a malicious client could send an
//! authorization code somewhere else, so the daemon admits only `https` and
//! loopback `http` addresses and never follows a client's own scheme.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use url::Url;

/// Whether `value` is an acceptable OAuth redirect URI.
///
/// The rule is deliberately narrow: `https` anywhere, or plain `http` to
/// `localhost`, `127.0.0.1` or `[::1]`. A custom scheme, a LAN address or an
/// embedded credential is refused.
pub fn redirect_allowed(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(url) = Url::parse(trimmed) else {
        return false;
    };
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host == "127.0.0.1"
                || host.trim_matches(['[', ']']) == "::1"
        }),
        _ => false,
    }
}

/// Whether a PKCE challenge is a base64url-encoded SHA-256 digest.
///
/// S256 is the only method h4rn accepts; a 43-character base64url string is
/// exactly one SHA-256 digest without padding.
pub fn valid_s256_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Whether a code verifier is in the RFC 7636 length range.
pub fn valid_verifier(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

/// The S256 transformation of a verifier, as base64url without padding.
pub fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Constant-time-ish comparison for PKCE challenges.
pub fn challenge_matches(verifier: &str, challenge: &str) -> bool {
    let computed = s256(verifier);
    computed.len() == challenge.len()
        && computed
            .bytes()
            .zip(challenge.bytes())
            .fold(0u8, |acc, (left, right)| acc | (left ^ right))
            == 0
}

/// Escapes text for an HTML text node or a double-quoted attribute.
pub fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_admit_https_and_loopback_only() {
        assert!(redirect_allowed("https://chatgpt.com/a/callback"));
        assert!(redirect_allowed("http://localhost:1455/callback"));
        assert!(redirect_allowed("http://127.0.0.1:1455/callback"));
        assert!(redirect_allowed("http://[::1]:1455/callback"));
        assert!(!redirect_allowed("http://example.com/callback"));
        assert!(!redirect_allowed("http://192.168.1.2/callback"));
        assert!(!redirect_allowed("h4rn://callback"));
        assert!(!redirect_allowed("https://user:pass@example.com/cb"));
        assert!(!redirect_allowed("https://example.com/cb#fragment"));
        assert!(!redirect_allowed(""));
    }

    #[test]
    fn s256_matches_the_rfc_vector() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(valid_verifier(verifier));
        assert!(valid_s256_challenge(challenge));
        assert_eq!(s256(verifier), challenge);
        assert!(challenge_matches(verifier, challenge));
        assert!(!challenge_matches(
            verifier,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ));
    }

    #[test]
    fn html_escapes_client_names() {
        assert_eq!(
            html_escape(r#"<script>"x"&'"#),
            "&lt;script&gt;&quot;x&quot;&amp;&#39;"
        );
    }
}
