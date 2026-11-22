//! `h4rnd update`: install a signed daemon release.
//!
//! The release repository is configured at build time
//! (`H4RN_REPOSITORY`, falling back to the workspace `repository`), and the
//! minisign public key is compiled in from `H4RN_UPDATER_PUBKEY` — the same key
//! the desktop updater uses. An unsigned build refuses to self-update: a
//! downloaded binary without a verified signature is never installed.
//!
//! The archive is selected for this build's target triple, checked against
//! `SHA256SUMS`, and the checksum file (or the archive) must carry a minisign
//! signature. The new binary is written beside the current one, fsynced, and
//! renamed into place; the old binary is kept as `h4rnd.previous` for
//! `h4rnd update --rollback`.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// The repository a build came from, used when no build-time override exists.
pub const DEFAULT_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// The target this binary was built for, reported by `build.rs`.
pub const TARGET_TRIPLE: &str = env!("H4RN_TARGET");

const REPOSITORY_OVERRIDE: Option<&str> = option_env!("H4RN_REPOSITORY");
const UPDATER_PUBKEY: Option<&str> = option_env!("H4RN_UPDATER_PUBKEY");
const PLACEHOLDER_PUBKEY: &str = "__H4RN_UPDATER_PUBKEY__";
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Options for one `h4rnd update` invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    /// Print current and latest versions and stop.
    pub check: bool,
    /// Install this exact version instead of the latest release.
    pub version: Option<String>,
    /// Swap the current binary back with `h4rnd.previous`.
    pub rollback: bool,
    /// Replace the binary even when a turn is active or the daemon cannot say.
    pub force: bool,
}

/// A GitHub repository that publishes h4rn releases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRepository {
    /// GitHub account or organisation.
    pub owner: String,
    /// Repository name without `.git`.
    pub name: String,
}

impl ReleaseRepository {
    /// Parses the usual GitHub HTTPS and SSH remote forms.
    pub fn parse(url: &str) -> Option<Self> {
        let url = url.trim().trim_end_matches('/');
        let path = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("http://github.com/"))
            .or_else(|| url.strip_prefix("git@github.com:"))
            .or_else(|| url.strip_prefix("ssh://git@github.com/"))
            .or_else(|| url.strip_prefix("git://github.com/"))?;
        let path = path.trim_end_matches(".git");
        let (owner, name) = path.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The repository this build was configured to update from.
    pub fn configured() -> Result<Self> {
        let url = REPOSITORY_OVERRIDE
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_REPOSITORY);
        Self::parse(url).with_context(|| {
            format!(
                "release repository {url:?} is not a GitHub repository; \
                 set H4RN_REPOSITORY at build time or update manually"
            )
        })
    }

    /// The human-facing releases page.
    pub fn releases_url(&self) -> String {
        format!("https://github.com/{}/{}/releases", self.owner, self.name)
    }
}

/// One release and its downloadable assets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Version without the leading `v`.
    pub version: String,
    /// Asset name to download URL.
    pub assets: HashMap<String, String>,
}

impl ReleaseInfo {
    /// The URL for one asset.
    pub fn asset(&self, name: &str) -> Result<&str> {
        self.assets.get(name).map(String::as_str).ok_or_else(|| {
            let mut names: Vec<&str> = self.assets.keys().map(String::as_str).collect();
            names.sort_unstable();
            anyhow::anyhow!(
                "release {} has no asset {name}; available assets: {}",
                self.version,
                names.join(", ")
            )
        })
    }
}

/// The GitHub Releases client.
pub struct ReleaseClient {
    http: reqwest::Client,
    api_base: String,
    repository: ReleaseRepository,
}

impl ReleaseClient {
    /// A client for the configured repository.
    pub fn new(repository: ReleaseRepository) -> Result<Self> {
        Self::with_api_base(repository, "https://api.github.com")
    }

    /// A client against another API base, for tests and self-hosted mirrors.
    pub fn with_api_base(
        repository: ReleaseRepository,
        api_base: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("h4rnd/{}", crate::version()))
            .timeout(Duration::from_secs(60))
            .build()
            .context("build the release HTTP client")?;
        Ok(Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            repository,
        })
    }

    /// The newest published release.
    pub async fn latest(&self) -> Result<ReleaseInfo> {
        let url = format!(
            "{}/repos/{}/{}/releases/latest",
            self.api_base, self.repository.owner, self.repository.name
        );
        self.get_release(&url).await
    }

    /// An exact release, which may be a pre-release.
    pub async fn release(&self, version: &str) -> Result<ReleaseInfo> {
        let tag = format!("v{}", release_version(version));
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{tag}",
            self.api_base, self.repository.owner, self.repository.name
        );
        self.get_release(&url).await
    }

    async fn get_release(&self, url: &str) -> Result<ReleaseInfo> {
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .with_context(|| format!("request {url}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("read the release response")?;
        if !status.is_success() {
            bail!(
                "GitHub release request failed ({status}): {}",
                String::from_utf8_lossy(&body).trim()
            );
        }
        parse_release_response(&body)
    }

    /// Downloads one asset, refusing an unreasonably large response.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("download {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("download {url} failed ({status})");
        }
        if let Some(length) = response.content_length()
            && length > MAX_DOWNLOAD_BYTES
        {
            bail!(
                "{url} is {length} bytes, larger than the {} MiB update limit",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            );
        }
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("read {url}"))?;
        if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
            bail!(
                "{url} is larger than the {} MiB update limit",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            );
        }
        Ok(bytes.to_vec())
    }
}

fn parse_release_response(body: &[u8]) -> Result<ReleaseInfo> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }

    let release: Release =
        serde_json::from_slice(body).context("parse the GitHub release response")?;
    let assets = release
        .assets
        .into_iter()
        .map(|asset| (asset.name, asset.browser_download_url))
        .collect();
    Ok(ReleaseInfo {
        version: release_version(&release.tag_name).to_owned(),
        assets,
    })
}

/// Removes a leading `v` from a release tag.
pub fn release_version(tag: &str) -> &str {
    tag.trim().strip_prefix('v').unwrap_or(tag.trim())
}

/// The release archive name for a version and target triple.
pub fn archive_name(version: &str, target: &str) -> String {
    let target = asset_target(target);
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("h4rnd-v{}-{target}.{extension}", release_version(version))
}

/// The target name used in release asset names.
///
/// Apple builds are published as one universal archive; the other targets
/// keep their Rust triple.
pub fn asset_target(target: &str) -> &str {
    if target.contains("apple-darwin") {
        "universal-apple-darwin"
    } else {
        target
    }
}

/// Whether `latest` is a newer semantic version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |value: &str| Version::parse(release_version(value)).ok();
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The public key compiled into this build, if any.
pub fn configured_public_key() -> Option<&'static str> {
    UPDATER_PUBKEY
        .map(str::trim)
        .filter(|key| !key.is_empty() && *key != PLACEHOLDER_PUBKEY)
}

/// Parses a minisign public key in `.key.pub`, base64-key or Tauri-config form.
pub fn parse_public_key(raw: &str) -> Result<PublicKey> {
    let raw = raw.trim();
    if raw.is_empty() || raw == PLACEHOLDER_PUBKEY {
        bail!("no minisign public key is compiled into this build");
    }
    // A key file copied from Windows may carry CRLF line endings.
    let normalized = raw.replace("\r\n", "\n");
    let raw = normalized.trim();
    if raw.starts_with("untrusted comment:") {
        return PublicKey::decode(raw).context("parse the minisign public key");
    }
    // Tauri stores the base64 of the entire public-key file in its config.
    let compact: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
    if let Ok(decoded) = BASE64.decode(&compact)
        && let Ok(text) = String::from_utf8(decoded)
        && text.contains("untrusted comment:")
    {
        return PublicKey::decode(text.trim()).context("parse the minisign public key");
    }
    PublicKey::from_base64(&compact).context("parse the minisign public key")
}

/// Parses a minisign signature in `.minisig` or Tauri `.sig` form.
pub fn parse_signature(raw: &str) -> Result<Signature> {
    let raw = raw.trim().replace("\r\n", "\n");
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("the release signature is empty");
    }
    if raw.starts_with("untrusted comment:") {
        return Signature::decode(raw).context("parse the minisign signature");
    }
    let decoded = BASE64
        .decode(
            raw.chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>(),
        )
        .context("the release signature is not minisign text or base64")?;
    let text = String::from_utf8(decoded).context("the release signature is not UTF-8 text")?;
    Signature::decode(text.trim()).context("parse the minisign signature")
}

/// Verifies a minisign signature over `data`.
pub fn verify_signature(data: &[u8], signature: &str, public_key: &str) -> Result<()> {
    let key = parse_public_key(public_key)?;
    let signature = parse_signature(signature)?;
    // Accept older non-prehashed minisign signatures, as the desktop updater
    // does; the key ID and global signature are still checked either way.
    key.verify(data, &signature, true)
        .context("the release signature does not match the compiled public key")
}

/// Finds an archive's SHA-256 in `SHA256SUMS` output.
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let file = fields.next()?;
        if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return None;
        }
        let file = file.strip_prefix('*').unwrap_or(file);
        let file = file.strip_prefix("./").unwrap_or(file);
        (file == name || file.ends_with(&format!("/{name}"))).then(|| hash.to_ascii_lowercase())
    })
}

/// The SHA-256 of a downloaded archive.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Checks the archive hash against `SHA256SUMS` and the signature over the
/// checksum file (preferred) or the archive.
pub fn verify_download(
    asset_name: &str,
    archive: &[u8],
    sums: &str,
    sums_signature: Option<&str>,
    archive_signature: Option<&str>,
    public_key: &str,
) -> Result<()> {
    let expected = checksum_for(sums, asset_name)
        .with_context(|| format!("SHA256SUMS has no entry for {asset_name}"))?;
    let actual = sha256_hex(archive);
    if actual != expected {
        bail!("{asset_name} does not match SHA256SUMS (expected {expected}, got {actual})");
    }
    match sums_signature.filter(|value| !value.trim().is_empty()) {
        Some(signature) => {
            verify_signature(sums.as_bytes(), signature, public_key).context("verify SHA256SUMS")
        }
        None => match archive_signature.filter(|value| !value.trim().is_empty()) {
            Some(signature) => verify_signature(archive, signature, public_key)
                .with_context(|| format!("verify {asset_name}")),
            None => bail!(
                "release has no minisign signature for SHA256SUMS or {asset_name}; \
                 refusing to install an unsigned archive"
            ),
        },
    }
}

/// Extracts the `h4rnd` binary from a release archive.
pub fn extract_binary(archive: &[u8], target: &str) -> Result<Vec<u8>> {
    let binary_name = if target.contains("windows") {
        "h4rnd.exe"
    } else {
        "h4rnd"
    };
    if target.contains("windows") {
        extract_zip(archive, binary_name)
    } else {
        extract_tar_gz(archive, binary_name)
    }
}

fn extract_tar_gz(archive: &[u8], binary_name: &str) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(archive));
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("read the daemon archive")? {
        let entry = entry.context("read a daemon archive entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .context("read an archive entry path")?
            .into_owned();
        if path.file_name().and_then(|name| name.to_str()) != Some(binary_name) {
            continue;
        }
        let size = entry.size();
        if size > MAX_DOWNLOAD_BYTES {
            bail!("{binary_name} in the archive is unreasonably large");
        }
        let mut bytes = Vec::with_capacity(size.min(16 * 1024 * 1024) as usize);
        entry
            .take(MAX_DOWNLOAD_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("extract h4rnd from the archive")?;
        if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
            bail!("{binary_name} in the archive is unreasonably large");
        }
        if bytes.is_empty() {
            bail!("{binary_name} in the archive is empty");
        }
        return Ok(bytes);
    }
    bail!("the daemon archive does not contain {binary_name}")
}

fn extract_zip(archive: &[u8], binary_name: &str) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).context("read the daemon zip")?;
    for index in 0..zip.len() {
        let file = zip.by_index(index).context("read a daemon zip entry")?;
        if !file.is_file() {
            continue;
        }
        let name = Path::new(file.name())
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if name != binary_name {
            continue;
        }
        if file.size() > MAX_DOWNLOAD_BYTES {
            bail!("{binary_name} in the archive is unreasonably large");
        }
        let mut bytes = Vec::with_capacity(file.size().min(16 * 1024 * 1024) as usize);
        file.take(MAX_DOWNLOAD_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("extract h4rnd from the archive")?;
        if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
            bail!("{binary_name} in the archive is unreasonably large");
        }
        if bytes.is_empty() {
            bail!("{binary_name} in the archive is empty");
        }
        return Ok(bytes);
    }
    bail!("the daemon archive does not contain {binary_name}")
}

/// The sidecar path that keeps the previous binary.
pub fn previous_binary_path(current: &Path) -> PathBuf {
    current.with_extension("previous")
}

fn temporary_binary_path(current: &Path) -> PathBuf {
    current.with_extension("next")
}

fn old_binary_path(current: &Path) -> PathBuf {
    current.with_extension("old")
}

fn failed_binary_path(current: &Path) -> PathBuf {
    current.with_extension("failed")
}

/// Replaces `current` with `bytes`, keeping the old binary as `h4rnd.previous`.
pub fn replace_binary(current: &Path, bytes: &[u8]) -> Result<()> {
    let parent = current
        .parent()
        .context("the current executable has no parent directory")?;
    let temporary = temporary_binary_path(current);
    let previous = previous_binary_path(current);
    write_staged_binary(&temporary, bytes, current)?;

    if cfg!(windows) {
        replace_binary_windows(current, &temporary, &previous)?;
    } else {
        // A hard link keeps the old binary alive under `.previous` while the
        // staged file replaces the executable's path atomically.
        let _ = std::fs::remove_file(&previous);
        if std::fs::hard_link(current, &previous).is_err() {
            std::fs::copy(current, &previous)
                .with_context(|| format!("copy {}", previous.display()))?;
        }
        if let Err(error) = std::fs::rename(&temporary, current) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("replace {}", current.display()));
        }
    }
    fsync_dir(parent);
    Ok(())
}

#[cfg(windows)]
fn replace_binary_windows(current: &Path, temporary: &Path, previous: &Path) -> Result<()> {
    let old = old_binary_path(current);
    let _ = std::fs::remove_file(previous);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(current, &old)
        .with_context(|| format!("move the running binary to {}", old.display()))?;
    if let Err(error) = std::fs::rename(temporary, current) {
        let _ = std::fs::rename(&old, current);
        let _ = std::fs::remove_file(temporary);
        return Err(error).with_context(|| format!("replace {}", current.display()));
    }
    if let Err(error) = std::fs::rename(&old, previous) {
        // The new binary is in place; a leftover `.old` is cleaned next start.
        tracing::warn!(%error, "could not keep the previous binary for rollback");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_binary_windows(_current: &Path, _temporary: &Path, _previous: &Path) -> Result<()> {
    unreachable!("the Windows replace path is only compiled on Windows")
}

fn write_staged_binary(temporary: &Path, bytes: &[u8], current: &Path) -> Result<()> {
    let _ = std::fs::remove_file(temporary);
    let mut file = std::fs::File::create(temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(current)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0o755);
        std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(mode | 0o111))
            .with_context(|| format!("make {} executable", temporary.display()))?;
    }
    drop(file);
    Ok(())
}

/// Swaps `h4rnd.previous` back in, keeping the failed binary beside it.
pub fn rollback_binary(current: &Path) -> Result<()> {
    let previous = previous_binary_path(current);
    if !previous.exists() {
        bail!(
            "no previous binary at {}; only one update can be rolled back",
            previous.display()
        );
    }
    let failed = failed_binary_path(current);
    let _ = std::fs::remove_file(&failed);
    std::fs::rename(current, &failed)
        .with_context(|| format!("move the current binary to {}", failed.display()))?;
    if let Err(error) = std::fs::rename(&previous, current) {
        let _ = std::fs::rename(&failed, current);
        return Err(error).with_context(|| format!("restore {}", current.display()));
    }
    let _ = std::fs::remove_file(&failed);
    if let Some(parent) = current.parent() {
        fsync_dir(parent);
    }
    Ok(())
}

/// Removes staged files left by an interrupted update. On Windows, restores a
/// `.old` binary if the current path disappeared between the two renames; the
/// same recovery puts back `.failed` when a rollback was interrupted after it
/// moved the current binary aside.
pub fn cleanup_stale_update_files(current: &Path) {
    let old = old_binary_path(current);
    if old.exists() {
        if current.exists() {
            let _ = std::fs::remove_file(&old);
        } else {
            let _ = std::fs::rename(&old, current);
        }
    }
    let failed = failed_binary_path(current);
    if !current.exists() && failed.exists() {
        let _ = std::fs::rename(&failed, current);
    }
    if current.exists() {
        let _ = std::fs::remove_file(&failed);
        let _ = std::fs::remove_file(temporary_binary_path(current));
    }
}

/// Cleans staged files beside this executable.
pub fn cleanup_stale_files() {
    if let Ok(executable) = std::env::current_exe() {
        cleanup_stale_update_files(&executable);
    }
}

fn fsync_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Ok(directory) = std::fs::File::open(path) {
            let _ = directory.sync_all();
        }
    }
}

/// The loopback health address for a bind address shown in `/healthz`.
pub fn health_addr(bind: SocketAddr) -> SocketAddr {
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bind.port())
}

/// A running daemon's health report.
#[derive(Clone, Debug, serde::Deserialize)]
struct Health {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    active_turns: Option<usize>,
}

async fn running_daemon(config: &Config) -> Option<Health> {
    let url = format!("http://{}/healthz", health_addr(config.bind));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .ok()?;
    let response = http.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Runs one `h4rnd update`.
pub async fn run(config: &Config, options: UpdateOptions) -> Result<()> {
    if options.rollback && (options.check || options.version.is_some()) {
        bail!("--rollback cannot be combined with --check or --version");
    }
    if options.rollback {
        let current = crate::service::current_binary_path()?;
        rollback_binary(&current)?;
        println!("Restored the previous h4rnd binary.");
        restart_or_explain(config)?;
        return Ok(());
    }

    let repository = ReleaseRepository::configured()?;
    let client = ReleaseClient::new(repository.clone())?;
    let release = match options.version.as_deref() {
        Some(version) => client.release(version).await?,
        None => client.latest().await?,
    };

    if options.check {
        report_versions(&release);
        return Ok(());
    }

    if options.version.is_none() && !is_newer(&release.version, crate::version()) {
        println!("h4rnd {} is already the latest release.", crate::version());
        return Ok(());
    }
    if release.version == crate::version() {
        println!("h4rnd {} is already installed.", crate::version());
        return Ok(());
    }

    let Some(public_key) = configured_public_key() else {
        print_manual_steps(&repository, &release);
        bail!("self-update is disabled in this build (no H4RN_UPDATER_PUBKEY)");
    };

    if !options.force
        && let Some(health) = running_daemon(config).await
    {
        let version = health.version.as_deref().unwrap_or("unknown");
        match update_refusal(version, health.active_turns) {
            Some(reason) => bail!("{reason}"),
            None => println!("h4rnd {version} is running and idle; it will be restarted."),
        }
    }

    let asset_name = archive_name(&release.version, TARGET_TRIPLE);
    let archive = client.download(release.asset(&asset_name)?).await?;
    let sums = client.download(release.asset("SHA256SUMS")?).await?;
    let sums_signature = match release.assets.get("SHA256SUMS.sig") {
        Some(url) => Some(
            String::from_utf8(client.download(url).await?)
                .context("SHA256SUMS.sig is not UTF-8")?,
        ),
        None => None,
    };
    let archive_signature = match release.assets.get(&format!("{asset_name}.sig")) {
        Some(url) => Some(
            String::from_utf8(client.download(url).await?)
                .context("the archive signature is not UTF-8")?,
        ),
        None => None,
    };
    let sums = String::from_utf8(sums).context("SHA256SUMS is not UTF-8")?;
    verify_download(
        &asset_name,
        &archive,
        &sums,
        sums_signature.as_deref(),
        archive_signature.as_deref(),
        public_key,
    )?;
    let binary = extract_binary(&archive, TARGET_TRIPLE)?;

    let current = crate::service::current_binary_path()?;
    replace_binary(&current, &binary)?;
    println!(
        "Installed h4rnd {} at {}.",
        release.version,
        current.display()
    );
    restart_or_explain(config)?;
    Ok(())
}

/// Why a running daemon blocks an update, or `None` when it is idle.
fn update_refusal(version: &str, active_turns: Option<usize>) -> Option<String> {
    match active_turns {
        Some(0) => None,
        Some(turns) => Some(format!(
            "h4rnd {version} is running with {turns} active turn(s); \
             stop the turn or pass --force to update anyway"
        )),
        None => Some(format!(
            "h4rnd {version} is running and does not report active turns; \
             stop the daemon or pass --force to update anyway"
        )),
    }
}

fn report_versions(release: &ReleaseInfo) {
    println!("current: {}", crate::version());
    println!("latest:  {}", release.version);
    if is_newer(&release.version, crate::version()) {
        println!("An update is available.");
    } else if release.version == crate::version() {
        println!("Up to date.");
    } else {
        println!("The installed version is newer than the latest release.");
    }
}

fn restart_or_explain(config: &Config) -> Result<()> {
    match crate::service::is_installed() {
        Ok(true) => {
            crate::service::restart(config)?;
            Ok(())
        }
        Ok(false) => {
            println!("Restart h4rnd to use the new binary.");
            Ok(())
        }
        Err(error) => {
            eprintln!("warning: could not check for an installed service: {error}");
            println!("Restart h4rnd to use the new binary.");
            Ok(())
        }
    }
}

fn print_manual_steps(repository: &ReleaseRepository, release: &ReleaseInfo) {
    let asset = archive_name(&release.version, TARGET_TRIPLE);
    println!();
    println!("This build has no updater public key, so it will not install an unsigned binary.");
    println!("Update by hand:");
    println!("  1. Download {asset} from {}", repository.releases_url());
    println!("  2. Verify it against SHA256SUMS and its signature.");
    println!("  3. Replace this h4rnd with the binary inside the archive.");
    println!("  4. Restart h4rnd (or `h4rnd service restart`).");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::engine::general_purpose::STANDARD as B64;
    use blake2::Blake2b512;
    use ed25519_dalek::{Signer, SigningKey};

    fn test_key() -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let key_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut blob = Vec::with_capacity(42);
        blob.extend_from_slice(b"Ed");
        blob.extend_from_slice(&key_id);
        blob.extend_from_slice(key.verifying_key().as_bytes());
        (key, B64.encode(blob))
    }

    fn sign(data: &[u8], key: &SigningKey) -> String {
        let key_id = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let trusted = "timestamp:0\tfile:test\tprehashed";
        let prehash = Blake2b512::digest(data);
        let signature = key.sign(prehash.as_slice());
        let mut blob = Vec::with_capacity(74);
        blob.extend_from_slice(b"ED");
        blob.extend_from_slice(&key_id);
        blob.extend_from_slice(&signature.to_bytes());
        let mut global_input = signature.to_bytes().to_vec();
        global_input.extend_from_slice(trusted.as_bytes());
        let global = key.sign(&global_input);
        format!(
            "untrusted comment: test signature\n{}\ntrusted comment: {}\n{}",
            B64.encode(blob),
            trusted,
            B64.encode(global.to_bytes())
        )
    }

    fn signed_sums(archive: &[u8], name: &str, key: &SigningKey) -> (String, String) {
        let sums = format!("{}  ./{name}\n", sha256_hex(archive));
        let signature = sign(sums.as_bytes(), key);
        (sums, signature)
    }

    #[test]
    fn repository_forms_are_understood() {
        for url in [
            "https://github.com/chark1es/h4rn",
            "https://github.com/chark1es/h4rn.git",
            "git@github.com:chark1es/h4rn.git",
            "ssh://git@github.com/chark1es/h4rn",
        ] {
            assert_eq!(
                ReleaseRepository::parse(url),
                Some(ReleaseRepository {
                    owner: "chark1es".to_owned(),
                    name: "h4rn".to_owned(),
                })
            );
        }
        assert!(ReleaseRepository::parse("https://example.com/h4rn").is_none());
        assert!(ReleaseRepository::parse("https://github.com/onlyowner").is_none());
    }

    #[test]
    fn version_comparison_is_semver() {
        assert!(is_newer("1.2.0", "1.1.9"));
        assert!(is_newer("v2.0.0", "1.99.99"));
        assert!(!is_newer("1.0.1", "1.0.1"));
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        assert!(!is_newer("not-a-version", "1.0.0"));
    }

    #[test]
    fn asset_names_follow_the_target() {
        assert_eq!(
            archive_name("1.2.3", "x86_64-unknown-linux-gnu"),
            "h4rnd-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            archive_name("v1.2.3", "x86_64-unknown-linux-musl"),
            "h4rnd-v1.2.3-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            archive_name("1.2.3", "aarch64-apple-darwin"),
            "h4rnd-v1.2.3-universal-apple-darwin.tar.gz"
        );
        assert_eq!(
            archive_name("1.2.3", "x86_64-pc-windows-msvc"),
            "h4rnd-v1.2.3-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn checksum_file_matches_plain_and_prefixed_names() {
        let sums = "aaaa  ./h4rnd-v1-x86_64-unknown-linux-gnu.tar.gz\n\
                    bbbb *h4rnd-v1-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(
            checksum_for(sums, "h4rnd-v1-x86_64-unknown-linux-gnu.tar.gz"),
            None,
            "invalid hex is ignored"
        );
        let sums = format!(
            "{}  ./h4rnd-v1-x86_64-unknown-linux-gnu.tar.gz\n\
             {} *h4rnd-v1-x86_64-pc-windows-msvc.zip\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(
            checksum_for(&sums, "h4rnd-v1-x86_64-unknown-linux-gnu.tar.gz"),
            Some("a".repeat(64))
        );
        assert_eq!(
            checksum_for(&sums, "h4rnd-v1-x86_64-pc-windows-msvc.zip"),
            Some("b".repeat(64))
        );
        assert_eq!(checksum_for(&sums, "missing"), None);
    }

    #[test]
    fn a_good_signature_over_the_checksum_file_verifies() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, name, &key);
        verify_download(name, archive, &sums, Some(&signature), None, &public_key)
            .expect("a correctly signed release verifies");
    }

    #[test]
    fn tauri_base64_signature_and_public_key_verify() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, name, &key);
        // Tauri stores base64 of the whole minisign text for both values.
        let signature = B64.encode(signature.as_bytes());
        let public_key = B64.encode(format!("untrusted comment: public key\n{public_key}\n"));
        verify_download(name, archive, &sums, Some(&signature), None, &public_key)
            .expect("the desktop updater's encoding verifies");
    }

    #[test]
    fn a_tampered_archive_is_rejected() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, name, &key);
        let error = verify_download(
            name,
            b"tampered archive",
            &sums,
            Some(&signature),
            None,
            &public_key,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match SHA256SUMS"));
    }

    #[test]
    fn tampered_checksums_and_missing_signatures_are_rejected() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, name, &key);
        let tampered = sums.replace(&sha256_hex(archive), &"0".repeat(64));
        assert!(
            verify_download(
                name,
                archive,
                &tampered,
                Some(&signature),
                None,
                &public_key
            )
            .is_err()
        );
        assert!(
            verify_download(name, archive, &sums, None, None, &public_key)
                .unwrap_err()
                .to_string()
                .contains("no minisign signature")
        );
        // A line appended to SHA256SUMS keeps the archive hash valid but breaks
        // the signature over the file.
        let appended = format!("{sums}{}  other-asset\n", "c".repeat(64));
        assert!(
            verify_download(
                name,
                archive,
                &appended,
                Some(&signature),
                None,
                &public_key
            )
            .is_err()
        );
        assert!(verify_download(name, archive, &sums, Some(&signature), None, "").is_err());
    }

    #[test]
    fn an_archive_signature_is_accepted() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, _) = signed_sums(archive, name, &key);
        let signature = sign(archive, &key);
        verify_download(name, archive, &sums, None, Some(&signature), &public_key)
            .expect("a signed archive verifies");
    }

    #[test]
    fn an_unsigned_build_refuses_to_parse_a_key() {
        assert!(parse_public_key("").is_err());
        assert!(parse_public_key(PLACEHOLDER_PUBKEY).is_err());
        let (_, public_key) = test_key();
        assert!(parse_public_key(&public_key).is_ok());
    }

    fn tar_gz_with_binary(binary: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(binary.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "h4rnd-v1.0.0-x86_64-unknown-linux-gnu/h4rnd",
                    binary,
                )
                .unwrap();
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[test]
    fn the_binary_is_extracted_from_each_archive_kind() {
        let tar = tar_gz_with_binary(b"tar binary");
        assert_eq!(
            extract_binary(&tar, "x86_64-unknown-linux-gnu").unwrap(),
            b"tar binary"
        );

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "h4rnd-v1.0.0-x86_64-pc-windows-msvc/h4rnd.exe",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(b"zip binary").unwrap();
        let zip = writer.finish().unwrap().into_inner();
        assert_eq!(
            extract_binary(&zip, "x86_64-pc-windows-msvc").unwrap(),
            b"zip binary"
        );
        assert!(extract_binary(&tar, "x86_64-unknown-linux-gnu").is_ok());
        assert!(extract_binary(b"not an archive", "x86_64-unknown-linux-gnu").is_err());
    }

    #[test]
    fn replacing_and_rolling_back_keep_one_previous_binary() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("h4rnd");
        std::fs::write(&current, b"old").unwrap();
        replace_binary(&current, b"new").unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        assert_eq!(
            std::fs::read(previous_binary_path(&current)).unwrap(),
            b"old"
        );
        assert!(!temporary_binary_path(&current).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(&current).unwrap().permissions().mode() & 0o111,
                0
            );
        }

        rollback_binary(&current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!previous_binary_path(&current).exists());
        assert!(rollback_binary(&current).is_err());
    }

    #[test]
    fn stale_files_are_cleaned_or_restored() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("h4rnd");
        let old = old_binary_path(&current);
        std::fs::write(&old, b"old").unwrap();
        cleanup_stale_update_files(&current);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!old.exists());

        std::fs::write(old_binary_path(&current), b"stale").unwrap();
        std::fs::write(temporary_binary_path(&current), b"staged").unwrap();
        cleanup_stale_update_files(&current);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!old_binary_path(&current).exists());
        assert!(!temporary_binary_path(&current).exists());

        // A rollback interrupted between the two renames leaves only
        // `.failed`; the next start puts it back instead of deleting it.
        std::fs::remove_file(&current).unwrap();
        std::fs::write(failed_binary_path(&current), b"new").unwrap();
        cleanup_stale_update_files(&current);
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        assert!(!failed_binary_path(&current).exists());
    }

    #[test]
    fn an_active_turn_blocks_an_update_unless_forced() {
        assert!(update_refusal("1.0.0", Some(0)).is_none());
        let busy = update_refusal("1.0.0", Some(2)).unwrap();
        assert!(busy.contains("2 active turn(s)"));
        assert!(busy.contains("--force"));
        let unknown = update_refusal("1.0.0", None).unwrap();
        assert!(unknown.contains("does not report active turns"));
    }

    #[test]
    fn health_uses_loopback_for_wildcard_binds() {
        assert_eq!(
            health_addr("0.0.0.0:47800".parse().unwrap()),
            "127.0.0.1:47800".parse().unwrap()
        );
        assert_eq!(
            health_addr("[::]:47800".parse().unwrap()),
            "[::1]:47800".parse().unwrap()
        );
        assert_eq!(
            health_addr("192.168.1.5:47800".parse().unwrap()),
            "192.168.1.5:47800".parse().unwrap()
        );
    }

    #[test]
    fn release_json_yields_version_and_assets() {
        let body = br#"{
            "tag_name": "v1.2.3",
            "assets": [
                {"name": "SHA256SUMS", "browser_download_url": "https://example.test/sums"},
                {"name": "h4rnd-v1.2.3-x86_64-unknown-linux-gnu.tar.gz",
                 "browser_download_url": "https://example.test/archive"}
            ]
        }"#;
        let release = parse_release_response(body).unwrap();
        assert_eq!(release.version, "1.2.3");
        assert_eq!(
            release
                .asset("h4rnd-v1.2.3-x86_64-unknown-linux-gnu.tar.gz")
                .unwrap(),
            "https://example.test/archive"
        );
        assert!(release.asset("missing").is_err());
    }

    #[tokio::test]
    async fn the_release_client_asks_the_configured_repository() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/chark1es/h4rn/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{
                    "tag_name": "v9.8.7",
                    "assets": [
                        {"name": "SHA256SUMS",
                         "browser_download_url": "PLACEHOLDER/sums"}
                    ]
                }"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sums"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"checksums"))
            .mount(&server)
            .await;

        let client = ReleaseClient::with_api_base(
            ReleaseRepository {
                owner: "chark1es".to_owned(),
                name: "h4rn".to_owned(),
            },
            server.uri(),
        )
        .unwrap();
        let release = client.latest().await.unwrap();
        assert_eq!(release.version, "9.8.7");
        let url = release
            .asset("SHA256SUMS")
            .unwrap()
            .replace("PLACEHOLDER", &server.uri());
        assert_eq!(client.download(&url).await.unwrap(), b"checksums");
    }
}
