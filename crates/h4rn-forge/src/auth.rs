//! Token acquisition for forge accounts.
//!
//! Two paths exist, and both are explicit:
//!
//! - **OAuth device flow** (RFC 8628), implemented for GitHub because it is the
//!   only supported forge that publishes a device authorization grant. The
//!   daemon asks the user to visit a short URL and enter a code; the token is
//!   returned to the daemon and stored in its secret store. GitLab, Gitea and
//!   Forgejo use a personal access token instead (see `docs/forges.md`).
//! - **CLI import** — [`import_gh_token`] and [`import_glab_token`] read an
//!   existing token from the `gh`/`glab` CLIs. They are only called after the
//!   user explicitly chooses "import from CLI"; nothing is imported at startup
//!   and no token is ever logged.
//!
//! This module never persists a token. The daemon owns storage (ADR-016).

use std::process::Command;

use serde_json::Value;
use tokio::time::sleep;

use crate::error::{ForgeError, Result};

/// Environment variable that holds a GitHub OAuth app client id.
///
/// The device grant needs a client id, not a secret. Users register an OAuth
/// app (or reuse an existing public app) and set this variable before starting
/// the daemon; `docs/forges.md` explains the required callback-free setup.
pub const GITHUB_CLIENT_ID_ENV: &str = "H4RN_GITHUB_CLIENT_ID";

/// Least-privilege scopes requested by the GitHub device flow.
///
/// `repo` is needed for private repositories and review writes; `read:user`
/// resolves the account login. Public-only use could narrow this to
/// `public_repo read:user`, so the daemon lets the caller pass scopes.
pub const GITHUB_DEVICE_SCOPES: &str = "repo read:user";

const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A started device authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFlowStart {
    /// Opaque code the daemon polls with. Never shown to the user.
    pub device_code: String,
    /// Short code the user types at [`verification_uri`](Self::verification_uri).
    pub user_code: String,
    /// URL the user visits.
    pub verification_uri: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
    /// Minimum seconds between polls.
    pub interval: u64,
}

/// A token obtained from the device flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceToken {
    /// Access token. Callers must hand this straight to the secret store.
    pub access_token: String,
    /// Scopes the forge reports, split on whitespace and commas.
    pub scopes: Vec<String>,
    /// Token type, usually `bearer`.
    pub token_type: String,
}

/// Reads the configured GitHub OAuth app client id.
pub fn github_client_id() -> Option<String> {
    std::env::var(GITHUB_CLIENT_ID_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Starts GitHub's device authorization grant.
pub async fn start_github_device_flow(client_id: &str, scopes: &str) -> Result<DeviceFlowStart> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return Err(ForgeError::NotConfigured(format!(
            "no GitHub OAuth client id; set {GITHUB_CLIENT_ID_ENV}"
        )));
    }
    let response = oauth_client()?
        .post(GITHUB_DEVICE_CODE_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body(&[("client_id", client_id), ("scope", scopes)]))
        .send()
        .await
        .map_err(|error| ForgeError::Network(error.to_string()))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| ForgeError::Network(error.to_string()))?;
    if !status.is_success() {
        return Err(ForgeError::Http {
            status: status.as_u16(),
            message: oauth_message(&body),
        });
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| ForgeError::Parse(format!("device code response: {error}")))?;
    let device_code = string_field(&value, "device_code")?;
    let user_code = string_field(&value, "user_code")?;
    let verification_uri = value
        .get("verification_uri")
        .and_then(Value::as_str)
        .or_else(|| value.get("verification_url").and_then(Value::as_str))
        .ok_or_else(|| ForgeError::Parse("device code response has no verification URL".into()))?
        .to_owned();
    Ok(DeviceFlowStart {
        device_code,
        user_code,
        verification_uri,
        expires_in: value
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
        interval: value
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
    })
}

/// Polls until the user approves, denies, or the code expires.
///
/// RFC 8628 error handling is honoured: `authorization_pending` keeps polling,
/// `slow_down` increases the interval by five seconds, and terminal errors are
/// returned as [`ForgeError::Unauthorized`].
pub async fn poll_github_device_flow(
    client_id: &str,
    start: &DeviceFlowStart,
) -> Result<DeviceToken> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return Err(ForgeError::NotConfigured(format!(
            "no GitHub OAuth client id; set {GITHUB_CLIENT_ID_ENV}"
        )));
    }
    let client = oauth_client()?;
    let deadline = start.expires_in;
    let mut waited = 0u64;
    let mut interval = start.interval;

    loop {
        if waited >= deadline {
            return Err(ForgeError::Unauthorized {
                forge: h4rn_protocol::ForgeKind::GitHub,
                message: "the device code expired before it was approved".into(),
            });
        }
        sleep(std::time::Duration::from_secs(interval)).await;
        waited = waited.saturating_add(interval);

        let response = client
            .post(GITHUB_TOKEN_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(&[
                ("client_id", client_id),
                ("device_code", start.device_code.as_str()),
                ("grant_type", DEVICE_GRANT_TYPE),
            ]))
            .send()
            .await
            .map_err(|error| ForgeError::Network(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ForgeError::Network(error.to_string()))?;
        if !status.is_success() {
            return Err(ForgeError::Http {
                status: status.as_u16(),
                message: oauth_message(&body),
            });
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| ForgeError::Parse(format!("device token response: {error}")))?;

        if let Some(token) = value.get("access_token").and_then(Value::as_str) {
            if token.trim().is_empty() {
                return Err(ForgeError::Parse(
                    "device flow returned an empty token".into(),
                ));
            }
            return Ok(DeviceToken {
                access_token: token.to_owned(),
                scopes: parse_scopes(value.get("scope").and_then(Value::as_str).unwrap_or("")),
                token_type: value
                    .get("token_type")
                    .and_then(Value::as_str)
                    .unwrap_or("bearer")
                    .to_owned(),
            });
        }

        match value.get("error").and_then(Value::as_str).unwrap_or("") {
            "authorization_pending" => continue,
            "slow_down" => {
                interval = interval.saturating_add(5);
                continue;
            }
            "expired_token" => {
                return Err(ForgeError::Unauthorized {
                    forge: h4rn_protocol::ForgeKind::GitHub,
                    message: "the device code expired; start again".into(),
                });
            }
            "access_denied" => {
                return Err(ForgeError::Unauthorized {
                    forge: h4rn_protocol::ForgeKind::GitHub,
                    message: "the device authorization was denied".into(),
                });
            }
            other if !other.is_empty() => {
                return Err(ForgeError::Unauthorized {
                    forge: h4rn_protocol::ForgeKind::GitHub,
                    message: oauth_message(&body),
                });
            }
            _ => {
                return Err(ForgeError::Parse(
                    "device token response had no token and no error".into(),
                ));
            }
        }
    }
}

/// Imports a token from the GitHub CLI.
///
/// Only call this after the user explicitly asks to import. `hostname` selects
/// a GHES host; `None` reads the default host.
pub fn import_gh_token(hostname: Option<&str>) -> Result<String> {
    let mut command = Command::new("gh");
    command.args(["auth", "token"]);
    if let Some(hostname) = hostname {
        command.args(["--hostname", hostname]);
    }
    let output = command
        .output()
        .map_err(|error| ForgeError::Local(format!("run gh auth token: {error}")))?;
    if !output.status.success() {
        return Err(ForgeError::Local(format!(
            "gh auth token failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() {
        return Err(ForgeError::Local(
            "gh is installed but has no token; run `gh auth login`".into(),
        ));
    }
    Ok(token)
}

/// Imports a token from the GitLab CLI.
///
/// Only call this after the user explicitly asks to import. `hostname` selects
/// a self-hosted GitLab; `None` reads the default host.
pub fn import_glab_token(hostname: Option<&str>) -> Result<String> {
    let mut command = Command::new("glab");
    command.args(["auth", "status", "--show-token"]);
    if let Some(hostname) = hostname {
        command.args(["--hostname", hostname]);
    }
    let output = command
        .output()
        .map_err(|error| ForgeError::Local(format!("run glab auth status: {error}")))?;
    if !output.status.success() {
        return Err(ForgeError::Local(format!(
            "glab auth status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let token = text
        .lines()
        .filter_map(|line| line.split_once("Token:"))
        .map(|(_, token)| token.trim())
        .find(|token| !token.is_empty());
    match token {
        Some(token) => Ok(token.to_owned()),
        None => Err(ForgeError::Local(
            "glab is installed but reported no token; run `glab auth login`".into(),
        )),
    }
}

/// Splits the scopes string from a GitHub token response.
pub fn parse_scopes(value: &str) -> Vec<String> {
    value
        .split([',', ' '])
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn oauth_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("h4rn-forge/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| ForgeError::Network(error.to_string()))
}

fn string_field(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| ForgeError::Parse(format!("device code response has no {key}")))
}

fn oauth_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error_description")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| body.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_split_on_spaces_and_commas() {
        assert_eq!(
            parse_scopes("repo, read:user  workflow"),
            vec!["repo", "read:user", "workflow"]
        );
        assert!(parse_scopes("").is_empty());
    }

    #[test]
    fn oauth_messages_prefer_the_description() {
        assert_eq!(
            oauth_message(r#"{"error":"bad_verification_code","error_description":"bad code"}"#),
            "bad code"
        );
        assert_eq!(oauth_message("plain text"), "plain text");
    }

    #[test]
    fn github_client_id_requires_a_value() {
        // The environment is process-wide; only assert the negative when unset.
        if std::env::var(GITHUB_CLIENT_ID_ENV).is_err() {
            assert!(github_client_id().is_none());
        }
    }
}
