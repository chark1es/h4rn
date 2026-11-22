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
        hosts.insert(
            "bitbucket.org",
            ForgeKind::Bitbucket,
            "https://bitbucket.org",
        );
        // The SSH host serves the same web/API account as bitbucket.org.
        hosts.insert(
            "ssh.bitbucket.org",
            ForgeKind::Bitbucket,
            "https://bitbucket.org",
        );
        hosts.insert(
            "dev.azure.com",
            ForgeKind::AzureDevOps,
            "https://dev.azure.com",
        );
        hosts.insert(
            "ssh.dev.azure.com",
            ForgeKind::AzureDevOps,
            "https://dev.azure.com",
        );
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
            let base_url = validate_cloud_base_url(entry.forge, &base_url)
                .and_then(|()| validate_base_url_for(entry.forge, &base_url))?;
            hosts.insert(&entry.host, entry.forge, &base_url);
        }
        Ok(hosts)
    }
}

/// Rejects a Bitbucket or Azure DevOps base URL that is not the cloud service.
///
/// Both are cloud-only in this crate: their on-premises products use different
/// APIs and hosts. The URL must also be a plain HTTPS origin (plus, for Azure,
/// at most one organization segment), without credentials, a query or a
/// fragment. Errors never echo the input, because it may carry a password.
pub fn validate_cloud_base_url(kind: ForgeKind, base_url: &str) -> Result<()> {
    if !matches!(kind, ForgeKind::Bitbucket | ForgeKind::AzureDevOps) {
        return Ok(());
    }
    let display = kind.display_name();
    let parsed = Url::parse(base_url).map_err(|_| {
        ForgeError::InvalidRequest(format!("{display} base URLs must be a full HTTPS URL"))
    })?;
    if parsed.scheme() != "https" {
        return Err(ForgeError::InvalidRequest(format!(
            "{display} base URLs must use https://"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ForgeError::InvalidRequest(format!(
            "{display} base URLs must not include a username or password"
        )));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ForgeError::InvalidRequest(format!(
            "{display} base URLs must not include a query or fragment"
        )));
    }
    if kind == ForgeKind::Bitbucket && parsed.path() != "/" && !parsed.path().is_empty() {
        // A path can carry a secret; say the rule without repeating the input,
        // the way Azure DevOps does.
        return Err(ForgeError::InvalidRequest(
            "the Bitbucket Cloud base URL must be the host with no path".into(),
        ));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    match kind {
        ForgeKind::Bitbucket if host != "bitbucket.org" => Err(ForgeError::InvalidRequest(
            "Bitbucket Server and Data Center are not supported; use Bitbucket Cloud (bitbucket.org)"
                .into(),
        )),
        ForgeKind::AzureDevOps
            if host != "dev.azure.com" && !host.ends_with(".visualstudio.com") =>
        {
            Err(ForgeError::InvalidRequest(
                "Azure DevOps Server is not supported; use Azure DevOps Services (dev.azure.com)"
                    .into(),
            ))
        }
        _ => Ok(()),
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

/// Validates a public base URL for one forge.
///
/// Only Azure DevOps keeps a path in its base URL: the organization for
/// `https://dev.azure.com/acme`, or `DefaultCollection` on the legacy
/// `https://acme.visualstudio.com/DefaultCollection` host. Every other forge
/// must be hosted at the origin.
pub fn validate_base_url_for(kind: ForgeKind, value: &str) -> Result<String> {
    if kind == ForgeKind::Bitbucket {
        // Bitbucket Cloud is an origin, like Azure DevOps: errors describe the
        // rule and never echo the input, whose path may hold a secret.
        let trimmed = value.trim().trim_end_matches('/');
        let parsed = Url::parse(trimmed).map_err(|_| {
            ForgeError::InvalidRequest("the Bitbucket Cloud base URL is not a valid URL".into())
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ForgeError::InvalidRequest(
                "the Bitbucket Cloud base URL must use http or https".into(),
            ));
        }
        if parsed.host_str().is_none() {
            return Err(ForgeError::InvalidRequest(
                "the Bitbucket Cloud base URL has no host".into(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(ForgeError::InvalidRequest(
                "the Bitbucket Cloud base URL must not include a query or fragment".into(),
            ));
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(ForgeError::InvalidRequest(
                "the Bitbucket Cloud base URL must be the host with no path".into(),
            ));
        }
        return Ok(trimmed.to_owned());
    }
    if kind != ForgeKind::AzureDevOps {
        return validate_base_url(value);
    }
    let trimmed = value.trim().trim_end_matches('/');
    // The input may carry a secret in its path; Azure errors never echo it.
    let parsed = Url::parse(trimmed).map_err(|_| {
        ForgeError::InvalidRequest("the Azure DevOps base URL is not a valid URL".into())
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ForgeError::InvalidRequest(
            "the Azure DevOps base URL must use http or https".into(),
        ));
    }
    if parsed.host_str().is_none() {
        return Err(ForgeError::InvalidRequest(
            "the Azure DevOps base URL has no host".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ForgeError::InvalidRequest(
            "the Azure DevOps base URL must not include a query or fragment".into(),
        ));
    }
    let segments: Vec<&str> = parsed
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() > 1 || segments.iter().any(|part| *part == "." || *part == "..") {
        return Err(ForgeError::InvalidRequest(
            "the Azure DevOps base URL must be the host and at most one organization or collection segment"
                .into(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Whether two account base URLs name the same forge account.
///
/// Only Azure DevOps has an alias: a legacy organization is reachable both as
/// `https://<org>.visualstudio.com` and, with the historical collection
/// segment, `https://<org>.visualstudio.com/DefaultCollection`. A remote can
/// use either form while the account was added with the other, and both name
/// the same organization.
pub fn base_urls_match(kind: ForgeKind, a: &str, b: &str) -> bool {
    fn normalize(kind: ForgeKind, value: &str) -> String {
        let value = value.trim().trim_end_matches('/');
        if kind == ForgeKind::AzureDevOps
            && let Some(base) = value.strip_suffix("/DefaultCollection")
            && Url::parse(base)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| host.to_ascii_lowercase().ends_with(".visualstudio.com"))
        {
            return base.to_owned();
        }
        value.to_owned()
    }

    normalize(kind, a).eq_ignore_ascii_case(&normalize(kind, b))
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
        ForgeKind::Bitbucket => {
            if base.eq_ignore_ascii_case("https://bitbucket.org") {
                "https://api.bitbucket.org/2.0".to_owned()
            } else {
                // An explicit Cloud-compatible endpoint (tests and proxies).
                // Bitbucket Server and Data Center use a different API and are
                // not supported.
                format!("{base}/2.0")
            }
        }
        // Azure DevOps paths are built from the organization base, not a
        // separate API host.
        ForgeKind::AzureDevOps => base.to_owned(),
    }
}

/// Detects the forge from one remote URL.
pub fn detect_remote(remote: &str, url: &str, hosts: &ForgeHosts) -> Option<ForgeRemote> {
    let parsed = parse_remote(url)?;
    let host = match hosts.get(&parsed.host) {
        Some(host) => host.clone(),
        // Legacy Azure DevOps organizations use a per-organization cloud host.
        // It is still a well-known cloud service, never a guessed self-hosted
        // one, so it is recognised without a forge-hosts.json entry.
        None if is_visualstudio_cloud_host(&parsed.host) => ForgeHost {
            host: parsed.host.clone(),
            kind: ForgeKind::AzureDevOps,
            base_url: format!("https://{}", parsed.host),
        },
        None => return None,
    };
    if host.kind == ForgeKind::AzureDevOps {
        return detect_azure_remote(remote, url, &parsed, &host);
    }
    // Other forges keep the lenient path handling they have always had: empty
    // segments collapse (GitLab's `team//repo` resolves like `team/repo`) and
    // no percent-decoding or traversal check happens here. The strict rules
    // belong to Azure DevOps, whose project and repository names are decoded
    // and encoded again.
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
    // GitHub, Gitea, Forgejo and Bitbucket Cloud have exactly two path
    // segments. GitLab is the only supported forge with nested namespaces.
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

fn is_visualstudio_cloud_host(host: &str) -> bool {
    host.strip_suffix(".visualstudio.com")
        .is_some_and(|organization| !organization.is_empty() && !organization.contains('.'))
}

/// Parses the Azure DevOps remote forms into an organization base URL, project
/// and repository.
fn detect_azure_remote(
    remote: &str,
    url: &str,
    parsed: &ParsedRemote,
    host: &ForgeHost,
) -> Option<ForgeRemote> {
    // Azure project and repository names may be percent-encoded (`My%20Project`).
    // Decode every segment, reject a decoded separator or a traversal segment,
    // and reject an empty segment: an Azure path names one project and one
    // repository, so a double slash is malformed rather than an empty name.
    let decoded = azure_path_segments(&parsed.path)?;
    let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();
    if segments.is_empty() {
        return None;
    }
    // The legacy SSH host has no web root of its own: only its `v3` path
    // names an organization to derive `https://<org>.visualstudio.com` from.
    if parsed.host == "vs-ssh.visualstudio.com"
        && (segments.first() != Some(&"v3") || segments.len() != 4)
    {
        return None;
    }

    // SSH form: `v3/<org>/<project>/<repo>`.
    let (base_url, project, repo) = if segments.first() == Some(&"v3") && segments.len() == 4 {
        let organization = segments[1];
        let base = if parsed.host == "vs-ssh.visualstudio.com" {
            // The legacy SSH host is not a web host; the organization is the
            // first path segment, so its browser/API base is the per-org host.
            format!("https://{organization}.visualstudio.com")
        } else if parsed.host == "dev.azure.com"
            || parsed.host == "ssh.dev.azure.com"
            || host.base_url == "https://dev.azure.com"
        {
            format!("https://dev.azure.com/{organization}")
        } else if host.base_url.ends_with(&format!("/{organization}")) {
            host.base_url.clone()
        } else if host
            .base_url
            .trim_end_matches('/')
            .ends_with("/dev.azure.com")
        {
            format!("{}/{}", host.base_url.trim_end_matches('/'), organization)
        } else {
            host.base_url.clone()
        };
        (base, segments[2].to_string(), segments[3].to_string())
    } else {
        // HTTPS form: `[<org>|DefaultCollection/]<project>/_git/<repo>`.
        if segments.len() < 3 || segments[segments.len() - 2] != "_git" {
            return None;
        }
        let repo = segments.last()?.to_string();
        let project = segments[segments.len() - 3].to_string();
        let prefix = &segments[..segments.len() - 3];
        let base_url = if parsed.host == "dev.azure.com" {
            if prefix.len() != 1 {
                return None;
            }
            let organization = prefix[0];
            format!("https://dev.azure.com/{organization}")
        } else if prefix.is_empty() {
            host.base_url.clone()
        } else if host
            .base_url
            .trim_end_matches('/')
            .ends_with(parsed.host.as_str())
        {
            format!(
                "{}/{}",
                host.base_url.trim_end_matches('/'),
                prefix.join("/")
            )
        } else {
            host.base_url.clone()
        };
        (base_url, project, repo)
    };

    if project.is_empty()
        || repo.is_empty()
        || project == "."
        || project == ".."
        || repo == "."
        || repo == ".."
    {
        return None;
    }
    Some(ForgeRemote {
        remote: remote.to_owned(),
        repo: ForgeRepoRef {
            forge: ForgeKind::AzureDevOps,
            base_url,
            owner: project,
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
        let path = trim_remote_path(url.path())?;
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
    let path = trim_remote_path(path)?;
    Some(ParsedRemote { host, path })
}

/// Trims a remote path and drops the `.git` suffix, keeping any empty
/// segments exactly as they were written.
fn trim_remote_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_matches('/');
    (!path.is_empty()).then(|| path.to_owned())
}

/// Decodes an Azure DevOps path into project and repository segments.
///
/// Decoded separators and traversal segments are rejected so a crafted remote
/// cannot address a different path once the client encodes it again. Empty
/// segments are malformed in an Azure path and rejected here, which is why
/// this stricter form is only used for Azure; other forges keep their
/// long-standing lenient handling.
fn azure_path_segments(path: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    for raw in path.split('/') {
        if raw.is_empty() {
            return None;
        }
        let segment = percent_decode(raw)?;
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains('/')
            || segment.contains('\\')
            || segment.contains('?')
            || segment.contains('#')
        {
            return None;
        }
        segments.push(segment);
    }
    Some(segments)
}

/// Decodes `%XX` escapes in one path segment.
fn percent_decode(segment: &str) -> Option<String> {
    if !segment.contains('%') {
        return Some(segment.to_owned());
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
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
    fn bitbucket_remote_forms() {
        for url in [
            "git@bitbucket.org:acme/widget.git",
            "https://acme@bitbucket.org/acme/widget.git",
            "ssh://git@bitbucket.org/acme/widget.git",
            // The SSH host is part of Bitbucket Cloud and shares its account.
            "git@ssh.bitbucket.org:acme/widget.git",
            "ssh://git@ssh.bitbucket.org/acme/widget.git",
        ] {
            let detected = detect(url).unwrap();
            assert_eq!(detected.repo.forge, ForgeKind::Bitbucket, "{url}");
            assert_eq!(detected.repo.owner, "acme");
            assert_eq!(detected.repo.repo, "widget");
            assert_eq!(detected.repo.base_url, "https://bitbucket.org");
        }
        let with_credentials = detect("https://user:secret@bitbucket.org/acme/widget.git").unwrap();
        assert!(!with_credentials.url.contains("secret"));
    }

    #[test]
    fn other_forges_keep_their_lenient_path_handling() {
        // A double slash collapses the way it always has; the strict segment
        // rules apply to Azure DevOps only.
        let gitlab = detect("https://gitlab.com/team//repo.git").unwrap();
        assert_eq!(gitlab.repo.forge, ForgeKind::GitLab);
        assert_eq!(gitlab.repo.owner, "team");
        assert_eq!(gitlab.repo.repo, "repo");

        let github = detect("https://github.com/owner//repo.git").unwrap();
        assert_eq!(github.repo.owner, "owner");
        assert_eq!(github.repo.repo, "repo");
    }

    #[test]
    fn remote_path_segments_are_percent_decoded_and_checked() {
        let decoded = detect("https://dev.azure.com/acme/My%20Project/_git/my%20repo").unwrap();
        assert_eq!(decoded.repo.owner, "My Project");
        assert_eq!(decoded.repo.repo, "my repo");

        // Decoded separators and traversal segments are rejected rather than
        // becoming a different path once the client encodes them again.
        assert!(detect("https://dev.azure.com/acme/My%2FProject/_git/repo").is_none());
        assert!(detect("https://dev.azure.com/acme/%2E%2E/_git/repo").is_none());
        assert!(detect("https://dev.azure.com/acme/My%5CProject/_git/repo").is_none());
    }

    #[test]
    fn azure_devops_remote_forms() {
        let expected = |detected: Option<ForgeRemote>, base: &str| {
            let detected = detected.expect("azure remote");
            assert_eq!(detected.repo.forge, ForgeKind::AzureDevOps);
            assert_eq!(detected.repo.base_url, base);
            assert_eq!(detected.repo.owner, "platform");
            assert_eq!(detected.repo.repo, "widget");
        };

        expected(
            detect("git@ssh.dev.azure.com:v3/acme/platform/widget"),
            "https://dev.azure.com/acme",
        );
        // The legacy SSH host derives the per-organization web host.
        expected(
            detect("git@vs-ssh.visualstudio.com:v3/acme/platform/widget"),
            "https://acme.visualstudio.com",
        );
        expected(
            detect("https://acme@dev.azure.com/acme/platform/_git/widget"),
            "https://dev.azure.com/acme",
        );
        expected(
            detect("https://acme.visualstudio.com/platform/_git/widget"),
            "https://acme.visualstudio.com",
        );
        expected(
            detect("https://acme.visualstudio.com/DefaultCollection/platform/_git/widget"),
            "https://acme.visualstudio.com/DefaultCollection",
        );
        // An ssh alias is still explicit: it must be declared as an Azure host.
        let mut hosts = hosts();
        hosts.insert("ado", ForgeKind::AzureDevOps, "https://dev.azure.com");
        let aliased = detect_remote("origin", "git@ado:v3/acme/platform/widget", &hosts).unwrap();
        assert_eq!(aliased.repo.base_url, "https://dev.azure.com/acme");
        assert_eq!(aliased.repo.owner, "platform");
    }

    #[test]
    fn azure_requires_the_organization_and_project() {
        assert!(detect("https://dev.azure.com/acme/platform").is_none());
        assert!(detect("https://dev.azure.com/acme/_git/widget").is_none());
        assert!(detect("https://dev.azure.com/acme/platform/extra/_git/widget").is_none());
        assert!(detect("https://acme.visualstudio.com/widget").is_none());
        // The legacy SSH host is only meaningful for its v3 path.
        assert!(detect("https://vs-ssh.visualstudio.com/platform/_git/widget").is_none());
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
    fn only_cloud_bitbucket_and_azure_hosts_may_be_declared() {
        let cloud = br#"{"hosts":[
            {"host":"bitbucket.org","forge":"bitbucket","base_url":"https://bitbucket.org"},
            {"host":"ado","forge":"azure_devops","base_url":"https://dev.azure.com"},
            {"host":"legacy","forge":"azure_devops","base_url":"https://acme.visualstudio.com"}
        ]}"#;
        assert!(ForgeHosts::from_json(cloud).is_ok());

        let server = br#"{"hosts":[{"host":"bb.example.com","forge":"bitbucket","base_url":"https://bb.example.com"}]}"#;
        assert!(ForgeHosts::from_json(server).is_err());
        let on_prem = br#"{"hosts":[{"host":"ado.example.com","forge":"azure_devops","base_url":"https://ado.example.com"}]}"#;
        assert!(ForgeHosts::from_json(on_prem).is_err());
        assert!(validate_cloud_base_url(ForgeKind::Bitbucket, "https://bb.example.com").is_err());
        assert!(
            validate_cloud_base_url(ForgeKind::AzureDevOps, "https://ado.example.com").is_err()
        );
        assert!(
            validate_cloud_base_url(ForgeKind::AzureDevOps, "https://acme.visualstudio.com")
                .is_ok()
        );
    }

    #[test]
    fn cloud_base_urls_must_be_plain_https() {
        for forge in [ForgeKind::Bitbucket, ForgeKind::AzureDevOps] {
            assert!(validate_cloud_base_url(forge, "http://bitbucket.org").is_err());
            assert!(validate_cloud_base_url(forge, "https://user:secret@bitbucket.org").is_err());
            assert!(validate_cloud_base_url(forge, "https://bitbucket.org?x=1").is_err());
            assert!(validate_cloud_base_url(forge, "https://bitbucket.org#top").is_err());
        }
        let error =
            validate_cloud_base_url(ForgeKind::Bitbucket, "https://alice:s3cret@bitbucket.org")
                .unwrap_err()
                .to_string();
        assert!(!error.contains("s3cret"), "{error}");

        assert!(validate_cloud_base_url(ForgeKind::Bitbucket, "https://bitbucket.org").is_ok());
        assert!(validate_cloud_base_url(ForgeKind::AzureDevOps, "https://dev.azure.com").is_ok());
        assert!(
            validate_cloud_base_url(ForgeKind::AzureDevOps, "https://dev.azure.com/acme").is_ok()
        );
        assert!(
            validate_cloud_base_url(ForgeKind::AzureDevOps, "https://acme.visualstudio.com")
                .is_ok()
        );
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
        assert_eq!(
            api_base_url(ForgeKind::Bitbucket, "https://bitbucket.org"),
            "https://api.bitbucket.org/2.0"
        );
        assert_eq!(
            api_base_url(ForgeKind::AzureDevOps, "https://dev.azure.com/acme"),
            "https://dev.azure.com/acme"
        );
    }

    #[test]
    fn default_collection_is_the_same_azure_account() {
        assert!(base_urls_match(
            ForgeKind::AzureDevOps,
            "https://acme.visualstudio.com",
            "https://acme.visualstudio.com/DefaultCollection"
        ));
        assert!(base_urls_match(
            ForgeKind::AzureDevOps,
            "https://acme.visualstudio.com/DefaultCollection/",
            "https://acme.visualstudio.com"
        ));
        assert!(base_urls_match(
            ForgeKind::AzureDevOps,
            "https://dev.azure.com/acme",
            "https://dev.azure.com/acme/"
        ));
        assert!(!base_urls_match(
            ForgeKind::AzureDevOps,
            "https://dev.azure.com/acme/DefaultCollection",
            "https://dev.azure.com/acme"
        ));
        assert!(!base_urls_match(
            ForgeKind::AzureDevOps,
            "https://acme.visualstudio.com",
            "https://other.visualstudio.com/DefaultCollection"
        ));
        // The alias belongs to Azure DevOps only.
        assert!(!base_urls_match(
            ForgeKind::Bitbucket,
            "https://bitbucket.org",
            "https://bitbucket.org/DefaultCollection"
        ));
    }

    #[test]
    fn azure_base_urls_keep_the_organization() {
        assert_eq!(
            validate_base_url_for(ForgeKind::AzureDevOps, "https://dev.azure.com/acme/").unwrap(),
            "https://dev.azure.com/acme"
        );
        assert!(validate_base_url_for(ForgeKind::AzureDevOps, "https://dev.azure.com").is_ok());
        assert!(
            validate_base_url_for(
                ForgeKind::AzureDevOps,
                "https://dev.azure.com/acme/platform"
            )
            .is_err()
        );
        assert!(validate_base_url_for(ForgeKind::GitHub, "https://ghe.example.com/acme").is_err());
    }

    #[test]
    fn cloud_validation_errors_never_echo_the_input() {
        // A path, query or fragment can carry a secret; the Azure error must
        // describe the rule without repeating it.
        for value in [
            "https://dev.azure.com/acme/placeholder-secret",
            "https://dev.azure.com/acme?placeholder-secret=1",
            "https://dev.azure.com/acme#placeholder-secret",
            "not a url placeholder-secret",
        ] {
            let error = validate_base_url_for(ForgeKind::AzureDevOps, value)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("placeholder-secret"), "{error}");
            assert!(!error.contains(value), "{error}");
        }
        let error = validate_cloud_base_url(
            ForgeKind::AzureDevOps,
            "https://placeholder-secret.example.com",
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("placeholder-secret"), "{error}");

        // Bitbucket Cloud follows the same rule: its path, query or fragment
        // may carry a secret and the error never repeats it.
        for value in [
            "https://bitbucket.org/placeholder-secret",
            "https://bitbucket.org?placeholder-secret=1",
            "https://bitbucket.org#placeholder-secret",
            "not a url placeholder-secret",
        ] {
            let error = validate_base_url_for(ForgeKind::Bitbucket, value)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("placeholder-secret"), "{error}");
            assert!(!error.contains(value), "{error}");
        }
        let error = validate_cloud_base_url(
            ForgeKind::Bitbucket,
            "https://bitbucket.org/placeholder-secret",
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("placeholder-secret"), "{error}");
    }
}
