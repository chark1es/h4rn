//! Forge detection from git remotes and custom host configuration.
//!
//! A workspace's remotes are the source of truth for "which forge is this?".
//! Both ssh and https forms are understood, including the scp-like
//! `git@host:owner/repo.git` shorthand and self-hosted hosts reached through an
//! ssh alias. Unknown hosts are deliberately **not** guessed: a custom host
//! must be declared in `forge-hosts.json` so an ssh alias called `work` cannot
//! silently send a token to the wrong server.
//!
//! The configuration file is a small JSON document:
//!
//! ```json
//! {
//!   "hosts": [
//!     { "host": "ghe.example.com", "forge": "github", "base_url": "https://ghe.example.com" },
//!     { "host": "gitlab.example.com", "forge": "gitlab", "base_url": "https://gitlab.example.com" },
//!     { "host": "code.example.com", "forge": "forgejo", "base_url": "https://code.example.com" }
//!   ]
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use h4rn_protocol::{ForgeKind, ForgeRemote, ForgeRepoRef};
use serde::Deserialize;
use url::Url;

use crate::error::{ForgeError, Result};

/// One host mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeHost {
    /// Host as it appears in a remote URL or ssh alias, lowercased.
    pub host: String,
    /// Forge implementation.
    pub kind: ForgeKind,
    /// Public web base URL with no trailing slash.
    pub base_url: String,
}

/// Host-to-forge mappings, with the well-known public forges preloaded.
#[derive(Clone, Debug)]
pub struct ForgeHosts {
    hosts: BTreeMap<String, ForgeHost>,
}

impl Default for ForgeHosts {
    fn default() -> Self {
        let mut hosts = Self {
            hosts: BTreeMap::new(),
        };
        hosts.insert("github.com", ForgeKind::GitHub, "https://github.com");
        hosts.insert("gitlab.com", ForgeKind::GitLab, "https://gitlab.com");
        hosts.insert("codeberg.org", ForgeKind::Forgejo, "https://codeberg.org");
        hosts.insert("gitea.com", ForgeKind::Gitea, "https://gitea.com");
        hosts
    }
}

impl ForgeHosts {
    /// An empty mapping; every host is then unknown.
    pub fn empty() -> Self {
        Self {
            hosts: BTreeMap::new(),
        }
    }

    /// Adds or replaces one host mapping.
    pub fn insert(&mut self, host: &str, kind: ForgeKind, base_url: &str) {
        let host = host.trim().to_ascii_lowercase();
        let base_url = base_url.trim_end_matches('/').to_owned();
        self.hosts.insert(
            host.clone(),
            ForgeHost {
                host,
                kind,
                base_url,
            },
        );
    }

    /// Looks up one host.
    pub fn get(&self, host: &str) -> Option<&ForgeHost> {
        self.hosts.get(&host.trim().to_ascii_lowercase())
    }

    /// Every configured host, sorted by name.
    pub fn entries(&self) -> impl Iterator<Item = &ForgeHost> {
        self.hosts.values()
    }

    /// Reads `forge-hosts.json`. A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_json(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ForgeError::Local(format!(
                "read forge host config {}: {error}",
                path.display()
            ))),
        }
    }

    /// Parses the JSON configuration, merging over the defaults.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            hosts: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            host: String,
            forge: ForgeKind,
            base_url: Option<String>,
        }

        let file: File = serde_json::from_slice(bytes)
            .map_err(|error| ForgeError::Local(format!("parse forge host config: {error}")))?;
        let mut hosts = Self::default();
        for entry in file.hosts {
            let Some(base_url) = entry.base_url else {
                return Err(ForgeError::Local(format!(
                    "forge host {} needs a base_url",
                    entry.host
                )));
            };
            validate_base_url(&base_url)?;
            hosts.insert(&entry.host, entry.forge, &base_url);
        }
        Ok(hosts)
    }
}

/// Validates and normalises a public base URL.
pub fn validate_base_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value).map_err(|error| {
        ForgeError::InvalidRequest(format!("invalid base URL {value:?}: {error}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ForgeError::InvalidRequest(format!(
            "base URL {value:?} must use http or https"
        )));
    }
    if parsed.host_str().is_none() {
        return Err(ForgeError::InvalidRequest(format!(
            "base URL {value:?} has no host"
        )));
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(ForgeError::InvalidRequest(format!(
            "base URL {value:?} must not include a path"
        )));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

/// The API root for one forge and web base URL.
pub fn api_base_url(kind: ForgeKind, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match kind {
        ForgeKind::GitHub if base.eq_ignore_ascii_case("https://github.com") => {
            "https://api.github.com".to_owned()
        }
        ForgeKind::GitHub => {
            if base.ends_with("/api/v3") {
                base.to_owned()
            } else {
                format!("{base}/api/v3")
            }
        }
        ForgeKind::GitLab => {
            if base.ends_with("/api/v4") {
                base.to_owned()
            } else {
                format!("{base}/api/v4")
            }
        }
        ForgeKind::Gitea | ForgeKind::Forgejo => {
            if base.ends_with("/api/v1") {
                base.to_owned()
            } else {
                format!("{base}/api/v1")
            }
        }
    }
}

/// Detects the forge from one remote URL.
pub fn detect_remote(remote: &str, url: &str, hosts: &ForgeHosts) -> Option<ForgeRemote> {
    let parsed = parse_remote(url)?;
    let host = hosts.get(&parsed.host)?;
    let segments: Vec<&str> = parsed
        .path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() < 2 || segments.iter().any(|part| *part == "." || *part == "..") {
        return None;
    }
    let repo = segments.last()?.to_string();
    let owner = segments[..segments.len() - 1].join("/");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    // GitHub, Gitea and Forgejo have exactly two path segments. GitLab is the
    // only supported forge with nested namespaces.
    if host.kind != ForgeKind::GitLab && segments.len() != 2 {
        return None;
    }
    Some(ForgeRemote {
        remote: remote.to_owned(),
        repo: ForgeRepoRef {
            forge: host.kind,
            base_url: host.base_url.clone(),
            owner,
            repo,
        },
        url: redact_url(url),
    })
}

/// Reads `git remote -v` in `workspace` and detects every forge remote.
///
/// `origin` is returned first so the UI's default repository is the one the
/// user clones from. Duplicate URLs are omitted.
pub fn detect_repos(workspace: &Path, hosts: &ForgeHosts) -> Result<Vec<ForgeRemote>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["remote", "-v"])
        .output()
        .map_err(|error| ForgeError::Local(format!("run git remote: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ForgeError::Local(format!(
            "{} is not a git repository: {}",
            workspace.display(),
            stderr.trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut remotes: Vec<ForgeRemote> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(remote) = parts.next() else {
            continue;
        };
        let Some(url) = parts.next() else {
            continue;
        };
        // `git remote -v` prints both fetch and push; parse each URL once.
        if remotes
            .iter()
            .any(|existing| existing.url == redact_url(url))
        {
            continue;
        }
        if let Some(detected) = detect_remote(remote, url, hosts) {
            remotes.push(detected);
        }
    }
    remotes.sort_by_key(|item| if item.remote == "origin" { 0 } else { 1 });
    Ok(remotes)
}

/// A parsed remote with the host and repository path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRemote {
    /// Lowercased host or ssh alias.
    pub host: String,
    /// Path after the host, without `.git` or leading slashes.
    pub path: String,
}

/// Parses an https, ssh or scp-like git remote URL.
pub fn parse_remote(value: &str) -> Option<ParsedRemote> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    // A URL with a scheme.
    if let Ok(url) = Url::parse(value) {
        if !matches!(url.scheme(), "http" | "https" | "ssh" | "git") {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let path = normalise_path(url.path())?;
        return Some(ParsedRemote { host, path });
    }

    // scp-like shorthand: [user@]host:path. A slash before the first colon
    // means this is a local path, not a remote.
    let (authority, path) = value.split_once(':')?;
    if authority.contains('/') || authority.is_empty() || path.is_empty() {
        return None;
    }
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let path = normalise_path(path)?;
    Some(ParsedRemote { host, path })
}

fn normalise_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(path.to_owned())
}

/// Removes a username or password from a URL before it is stored or logged.
fn redact_url(value: &str) -> String {
    match Url::parse(value) {
        Ok(mut url) => {
            if !url.username().is_empty() || url.password().is_some() {
                let _ = url.set_username("");
                let _ = url.set_password(None);
            }
            url.to_string()
        }
        Err(_) => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> ForgeHosts {
        let mut hosts = ForgeHosts::default();
        hosts.insert(
            "ghe.example.com",
            ForgeKind::GitHub,
            "https://ghe.example.com",
        );
        hosts.insert(
            "git.example.com",
            ForgeKind::GitLab,
            "https://git.example.com",
        );
        hosts.insert(
            "code.example.com",
            ForgeKind::Forgejo,
            "https://code.example.com",
        );
        hosts.insert("work", ForgeKind::GitHub, "https://github.com");
        hosts
    }

    fn detect(url: &str) -> Option<ForgeRemote> {
        detect_remote("origin", url, &hosts())
    }

    #[test]
    fn remote_url_matrix() {
        let github = detect("https://github.com/chark1es/h4rn.git").unwrap();
        assert_eq!(github.repo.forge, ForgeKind::GitHub);
        assert_eq!(github.repo.owner, "chark1es");
        assert_eq!(github.repo.repo, "h4rn");
        assert_eq!(github.repo.base_url, "https://github.com");

        let scp = detect("git@github.com:chark1es/h4rn.git").unwrap();
        assert_eq!(scp.repo, github.repo);

        let ssh = detect("ssh://git@github.com/chark1es/h4rn").unwrap();
        assert_eq!(ssh.repo, github.repo);

        let with_port = detect("ssh://git@ghe.example.com:2222/team/api.git").unwrap();
        assert_eq!(with_port.repo.forge, ForgeKind::GitHub);
        assert_eq!(with_port.repo.base_url, "https://ghe.example.com");
        assert_eq!(with_port.repo.owner, "team");

        let gitlab = detect("https://gitlab.com/team/platform/api.git").unwrap();
        assert_eq!(gitlab.repo.forge, ForgeKind::GitLab);
        assert_eq!(gitlab.repo.owner, "team/platform");
        assert_eq!(gitlab.repo.repo, "api");

        let forgejo = detect("git@codeberg.org:forgejo/forgejo.git").unwrap();
        assert_eq!(forgejo.repo.forge, ForgeKind::Forgejo);

        let gitea = detect("https://gitea.com/gitea/tea.git").unwrap();
        assert_eq!(gitea.repo.forge, ForgeKind::Gitea);

        let alias = detect("git@work:chark1es/h4rn.git").unwrap();
        assert_eq!(alias.repo.owner, "chark1es");
    }

    #[test]
    fn unknown_hosts_and_local_paths_are_rejected() {
        assert!(detect("https://example.org/owner/repo.git").is_none());
        assert!(detect("/srv/git/repo.git").is_none());
        assert!(detect("file:///srv/git/repo.git").is_none());
        assert!(detect("https://github.com/onlyowner.git").is_none());
        assert!(detect("https://github.com/a/b/c.git").is_none());
    }

    #[test]
    fn credentials_are_redacted() {
        let remote = detect("https://user:secret@github.com/owner/repo.git").unwrap();
        assert!(!remote.url.contains("secret"));
        assert!(remote.url.contains("github.com/owner/repo"));
    }

    #[test]
    fn config_merges_over_defaults_and_requires_a_base_url() {
        let json = br#"{"hosts":[{"host":"ghe.example.com","forge":"github","base_url":"https://ghe.example.com/"}]}"#;
        let hosts = ForgeHosts::from_json(json).unwrap();
        assert_eq!(
            hosts.get("ghe.example.com").unwrap().base_url,
            "https://ghe.example.com"
        );
        assert!(hosts.get("github.com").is_some());

        let missing = br#"{"hosts":[{"host":"ghe.example.com","forge":"github"}]}"#;
        assert!(ForgeHosts::from_json(missing).is_err());
    }

    #[test]
    fn api_bases_follow_each_forge() {
        assert_eq!(
            api_base_url(ForgeKind::GitHub, "https://github.com"),
            "https://api.github.com"
        );
        assert_eq!(
            api_base_url(ForgeKind::GitHub, "https://ghe.example.com"),
            "https://ghe.example.com/api/v3"
        );
        assert_eq!(
            api_base_url(ForgeKind::GitLab, "https://gitlab.com"),
            "https://gitlab.com/api/v4"
        );
        assert_eq!(
            api_base_url(ForgeKind::Forgejo, "https://codeberg.org"),
            "https://codeberg.org/api/v1"
        );
    }
}
