//! `h4rnd update`: install a signed daemon release.
//!
//! The release repository is configured at build time
//! (`H4RN_REPOSITORY`, falling back to the workspace `repository`), and the
//! minisign public key is compiled in from `H4RN_UPDATER_PUBKEY` — the same key
//! the desktop updater uses. An unsigned build refuses to self-update: a
//! downloaded binary without a verified signature is never installed.
//!
//! The archive is selected for this build's target triple and checked against
//! `SHA256SUMS`; that checksum file must itself carry a minisign signature, so
//! one signature binds the version, the target and the binary. An archive
//! signature alone is not accepted.
//!
//! One invocation updates one installation at a time. The whole operation —
//! download, verify, replace, rollback, restart — holds an advisory lock in a
//! file beside the binary, and staging files are created exclusively with a
//! unique name. A second invocation refuses to start instead of racing.
//!
//! On Unix the binary is replaced and rolled back with a single `rename` over
//! the target, after the old binary is copied into a unique temp file, fsynced
//! and renamed over `h4rnd.previous`, so an interrupted update always leaves a
//! runnable binary at the install path. On Windows a running executable cannot
//! be replaced in place: the old binary is backed up the same way, then moved to
//! `h4rnd.old.exe` and the new one moved in. Startup recovery (in `main`, before
//! argument parsing) takes the same lock and restores the old binary if the
//! install path is missing, deriving the install path from the moved-aside name
//! so it works even when `h4rnd.exe` does not exist. The scheduled task installed
//! by `h4rnd service install` runs `h4rnd --recover-only` before the daemon, so a
//! machine whose update was cut off comes back up through the same lock-protected
//! recovery instead of a separate PowerShell copy.

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
/// How long a release metadata request may take end to end. Release JSON is
/// small, so a slow response means the network or GitHub is unhealthy.
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an asset download may go without receiving any bytes. A total
/// timeout would abort a large archive on a slow connection, so the download
/// client only gives up when the transfer stalls.
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an asset download may take to establish its connection.
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the health probe may take before the daemon counts as unreachable.
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long a TCP connect may take before deciding nothing is listening.
const PORT_TIMEOUT: Duration = Duration::from_millis(400);
/// Tags appended to the install path for the rollback backup and interrupted
/// Windows/rollback steps.
const PREVIOUS_TAG: &str = "previous";
const OLD_TAG: &str = "old";
const FAILED_TAG: &str = "failed";

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
    /// Metadata requests, bounded by a short total timeout.
    http: reqwest::Client,
    /// Asset downloads, bounded by an idle/read timeout instead of a total one.
    downloads: reqwest::Client,
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
        Self::with_timeouts(
            repository,
            api_base,
            METADATA_TIMEOUT,
            DOWNLOAD_IDLE_TIMEOUT,
        )
    }

    /// A client with explicit timeouts, so a test does not have to wait 20 s.
    fn with_timeouts(
        repository: ReleaseRepository,
        api_base: impl Into<String>,
        metadata_timeout: Duration,
        download_idle_timeout: Duration,
    ) -> Result<Self> {
        let user_agent = format!("h4rnd/{}", crate::version());
        let http = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .timeout(metadata_timeout)
            .build()
            .context("build the release metadata HTTP client")?;
        // No total timeout: a large archive may legitimately take minutes.
        // `read_timeout` fires only when the connection stops making progress.
        let downloads = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
            .read_timeout(download_idle_timeout)
            .build()
            .context("build the release download HTTP client")?;
        Ok(Self {
            http,
            downloads,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            repository,
        })
    }

    /// The repository this client reads releases from.
    pub fn repository(&self) -> &ReleaseRepository {
        &self.repository
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
    ///
    /// The transfer is bounded by an idle timeout, not a total one, so a slow
    /// but healthy link can finish while a stalled connection is abandoned.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .downloads
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

/// Checks the archive hash against a signed `SHA256SUMS`.
///
/// The signature over the checksum file is required. A signature over the
/// archive alone does not bind the release version or the target triple, so it
/// is never enough to install an update.
pub fn verify_download(
    asset_name: &str,
    archive: &[u8],
    sums: &str,
    sums_signature: &str,
    public_key: &str,
) -> Result<()> {
    if sums_signature.trim().is_empty() {
        bail!(
            "release has no SHA256SUMS.sig; refusing to install a release whose checksums \
             are not signed"
        );
    }
    verify_signature(sums.as_bytes(), sums_signature, public_key)
        .with_context(|| format!("verify SHA256SUMS for {asset_name}"))?;
    let expected = checksum_for(sums, asset_name)
        .with_context(|| format!("SHA256SUMS has no entry for {asset_name}"))?;
    let actual = sha256_hex(archive);
    if actual != expected {
        bail!("{asset_name} does not match SHA256SUMS (expected {expected}, got {actual})");
    }
    Ok(())
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

/// Appends a sidecar tag to the install path, keeping a `.exe` extension on
/// Windows: `h4rnd` → `h4rnd.previous`, `h4rnd.exe` → `h4rnd.previous.exe`.
fn sidecar_path(current: &Path, tag: &str) -> PathBuf {
    let Some(name) = current.file_name().and_then(|name| name.to_str()) else {
        return current.with_extension(tag);
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    let file = match extension {
        Some(extension) => format!("{stem}.{tag}.{extension}"),
        None => format!("{stem}.{tag}"),
    };
    current.with_file_name(file)
}

/// Removes one sidecar tag, returning the install file name it belongs to.
fn strip_sidecar_tag(name: &str, tag: &str) -> Option<String> {
    let marker = format!(".{tag}");
    if let Some(stem) = name.strip_suffix(&marker) {
        return Some(stem.to_owned());
    }
    let (stem, extension) = name.rsplit_once('.')?;
    let stem = stem.strip_suffix(&marker)?;
    Some(format!("{stem}.{extension}"))
}

/// The sidecar path that keeps the previous binary.
pub fn previous_binary_path(current: &Path) -> PathBuf {
    sidecar_path(current, PREVIOUS_TAG)
}

fn old_binary_path(current: &Path) -> PathBuf {
    sidecar_path(current, OLD_TAG)
}

fn failed_binary_path(current: &Path) -> PathBuf {
    sidecar_path(current, FAILED_TAG)
}

/// The lock file that serializes updates for one installation.
pub fn update_lock_path(current: &Path) -> PathBuf {
    let Some(name) = current.file_name().and_then(|name| name.to_str()) else {
        return current.with_extension("update.lock");
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    let file = match extension {
        Some(extension) => format!("{stem}.update.{extension}.lock"),
        None => format!("{stem}.update.lock"),
    };
    current.with_file_name(file)
}

/// The exclusive per-installation update lock.
///
/// The lock is advisory and lives in a file beside the binary. Unix uses
/// `flock`; Windows opens the lock file with an empty share mode, so a second
/// process cannot open it at all. Either way the lock is released when the
/// guard is dropped or the process dies, so a crash cannot leave a stale lock.
#[derive(Debug)]
pub struct UpdateLock {
    // Holding the open file is what holds the lock.
    _file: std::fs::File,
}

impl UpdateLock {
    /// Takes the lock, or returns `None` when another run holds it.
    pub fn try_acquire(current: &Path) -> Result<Option<Self>> {
        let path = update_lock_path(current);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(windows)]
        {
            // No sharing: the second open fails with a sharing violation.
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(32) => {
                // ERROR_SHARING_VIOLATION: another run holds the lock.
                return Ok(None);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", path.display()));
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(error).with_context(|| format!("lock {}", path.display()));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Some(Self { _file: file }))
    }

    /// Takes the lock or explains which installation is already being updated.
    pub fn acquire(current: &Path) -> Result<Self> {
        match Self::try_acquire(current)? {
            Some(lock) => Ok(lock),
            None => bail!(
                "another h4rnd update, rollback or recovery is already running for {}; wait for \
                 it to finish (lock: {})",
                current.display(),
                update_lock_path(current).display()
            ),
        }
    }
}

/// A staged file name prefix used to recognise this installation's leftovers.
fn staging_prefix(current: &Path) -> String {
    let name = current
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("h4rnd");
    format!("{name}.update-")
}

fn staging_name(current: &Path, attempt: u32) -> String {
    format!(
        "{}{}-{attempt}-{:08x}",
        staging_prefix(current),
        std::process::id(),
        rand::random::<u32>()
    )
}

/// Writes `bytes` to a new, uniquely named staging file beside the binary.
///
/// `create_new` means a name collision (including another run's staging file)
/// can never truncate it; a fresh name is chosen instead.
fn stage_binary(current: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let parent = current
        .parent()
        .context("the current executable has no parent directory")?;
    let mut last_error = None;
    for attempt in 0..16u32 {
        let path = parent.join(staging_name(current, attempt));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create staging file {}", path.display()));
            }
        };
        let written = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write staging file {}", path.display()));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(current)
                .map(|metadata| metadata.permissions().mode())
                .unwrap_or(0o755);
            if let Err(error) =
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode | 0o111))
            {
                let _ = std::fs::remove_file(&path);
                return Err(error).with_context(|| format!("make {} executable", path.display()));
            }
        }
        drop(file);
        return Ok(path);
    }
    Err(last_error.map(anyhow::Error::from).unwrap_or_else(|| {
        anyhow::anyhow!(
            "could not create a staging file next to {}",
            current.display()
        )
    }))
}

/// The checksum sidecar that describes the rollback backup.
fn previous_checksum_path(previous: &Path) -> PathBuf {
    let Some(name) = previous.file_name().and_then(|name| name.to_str()) else {
        return previous.with_extension("sha256");
    };
    previous.with_file_name(format!("{name}.sha256"))
}

/// Publishes the backup's checksum sidecar.
///
/// Production always uses [`AtomicChecksum`]; the trait exists so a test can
/// inject a publication failure inside the real backup copy instead of
/// fabricating a file state.
trait ChecksumPublisher {
    /// Stages `contents` under a unique temp name and returns its path.
    ///
    /// Nothing visible changes until [`Self::commit`] runs, so a failure here
    /// leaves the previous backup and its sidecar untouched.
    fn stage(&self, path: &Path, staging_base: &Path, contents: &str) -> Result<PathBuf>;
    /// Renames a staged sidecar over `path`.
    fn commit(&self, staged: &Path, path: &Path) -> Result<()>;
}

struct AtomicChecksum;

impl ChecksumPublisher for AtomicChecksum {
    fn stage(&self, path: &Path, staging_base: &Path, contents: &str) -> Result<PathBuf> {
        stage_checksum(path, staging_base, contents)
    }

    fn commit(&self, staged: &Path, path: &Path) -> Result<()> {
        if let Err(error) = std::fs::rename(staged, path) {
            let _ = std::fs::remove_file(staged);
            return Err(error)
                .with_context(|| format!("move the backup checksum into {}", path.display()));
        }
        if let Some(parent) = path.parent() {
            fsync_dir(parent);
        }
        Ok(())
    }
}

/// Writes the sidecar to a new, unique temp file beside its final name.
///
/// The temp file is fsynced but not renamed, so the old sidecar stays valid
/// until the caller has also staged the backup it describes.
fn stage_checksum(path: &Path, staging_base: &Path, contents: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("the backup checksum has no parent directory")?;
    let mut last_error = None;
    for attempt in 0..16u32 {
        let temp = parent.join(staging_name(staging_base, attempt));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create the backup checksum file {}", temp.display())
                });
            }
        };
        let written = file
            .write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write the backup checksum file {}", temp.display()));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        drop(file);
        return Ok(temp);
    }
    Err(last_error.map(anyhow::Error::from).unwrap_or_else(|| {
        anyhow::anyhow!(
            "could not create a backup checksum file next to {}",
            path.display()
        )
    }))
}

/// Copies `current` to a uniquely named temp file, fsyncs it, and renames it
/// over `previous`.
///
/// The backup is never written straight to `h4rnd.previous`: an interrupted
/// copy leaves the previous backup (or no backup) in place, never a truncated
/// file that `--rollback` could install. The new backup and its checksum sidecar
/// are both staged first, so a failure while writing either one leaves the
/// previous backup and its sidecar exactly as they were; only then are the two
/// renamed into place. A sidecar failure fails the update before the installed
/// binary is touched or the old backup is replaced.
fn write_backup<P: ChecksumPublisher>(from: &Path, to: &Path, publisher: &P) -> Result<()> {
    let parent = from
        .parent()
        .context("the current executable has no parent directory")?;
    let metadata = std::fs::metadata(from).with_context(|| format!("read {}", from.display()))?;
    let mut source =
        std::fs::File::open(from).with_context(|| format!("open {}", from.display()))?;
    let mut hasher = Sha256::new();
    let mut last_error = None;
    for attempt in 0..16u32 {
        let temp = parent.join(staging_name(from, attempt));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create backup file {}", temp.display()));
            }
        };
        let copied = (|| -> std::io::Result<()> {
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = source.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                file.write_all(&buffer[..read])?;
            }
            file.sync_all()
        })();
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&temp);
            return Err(error).with_context(|| format!("write backup file {}", temp.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode();
            if let Err(error) =
                std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode | 0o111))
            {
                let _ = std::fs::remove_file(&temp);
                return Err(error).with_context(|| format!("make {} executable", temp.display()));
            }
        }
        drop(file);
        let checksum_path = previous_checksum_path(to);
        let checksum = format!("{} {}\n", hex::encode(hasher.finalize()), metadata.len());
        // Stage the sidecar first: if this fails, the old backup and its
        // sidecar are still the valid pair. Only then replace the backup, and
        // commit the staged sidecar last.
        let staged_checksum = match publisher.stage(&checksum_path, from, &checksum) {
            Ok(path) => path,
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                return Err(error);
            }
        };
        if let Err(error) = std::fs::rename(&temp, to) {
            let _ = std::fs::remove_file(&temp);
            let _ = std::fs::remove_file(&staged_checksum);
            return Err(error).with_context(|| format!("move the backup into {}", to.display()));
        }
        fsync_dir(parent);
        return publisher.commit(&staged_checksum, &checksum_path);
    }
    Err(last_error.map(anyhow::Error::from).unwrap_or_else(|| {
        anyhow::anyhow!("could not create a backup file next to {}", from.display())
    }))
}

/// Reads the hint written after a completed backup.
fn read_backup_checksum(previous: &Path) -> Option<(String, u64)> {
    let text = std::fs::read_to_string(previous_checksum_path(previous)).ok()?;
    let mut fields = text.split_whitespace();
    let hash = fields.next()?;
    let size = fields.next()?.parse::<u64>().ok()?;
    (hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit()))
        .then(|| (hash.to_ascii_lowercase(), size))
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Refuses a backup that is missing, empty, not executable, or does not match
/// the checksum recorded when it was written.
fn check_previous_binary(previous: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(previous) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "no previous binary at {}; only one update can be rolled back",
                previous.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", previous.display()));
        }
    };
    if metadata.len() == 0 {
        bail!(
            "the previous binary at {} is empty, so it cannot be installed; run `h4rnd update` \
             again or install h4rnd",
            previous.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!(
                "the previous binary at {} is not executable, so it cannot be installed; run \
                 `h4rnd update` again or install h4rnd",
                previous.display()
            );
        }
    }
    if let Some((hash, size)) = read_backup_checksum(previous)
        && (size != metadata.len() || file_sha256(previous).ok().as_deref() != Some(hash.as_str()))
    {
        bail!(
            "the previous binary at {} does not match its recorded checksum, so the backup copy \
             was probably interrupted; run `h4rnd update` again or install h4rnd",
            previous.display()
        );
    }
    Ok(())
}

/// Removes the checksum sidecar after the backup has been consumed.
fn remove_backup_checksum(previous: &Path) {
    let _ = std::fs::remove_file(previous_checksum_path(previous));
}

/// Replaces `current` with `bytes`, keeping the old binary as `h4rnd.previous`.
pub fn replace_binary(current: &Path, bytes: &[u8]) -> Result<()> {
    replace_binary_with(current, bytes, &AtomicChecksum)
}

fn replace_binary_with<P: ChecksumPublisher>(
    current: &Path,
    bytes: &[u8],
    publisher: &P,
) -> Result<()> {
    let parent = current
        .parent()
        .context("the current executable has no parent directory")?;
    let staged = stage_binary(current, bytes)?;
    // cfg! would compile both calls, and each replace function only exists on
    // its own platform.
    #[cfg(windows)]
    let result = replace_binary_windows(current, &staged, publisher);
    #[cfg(not(windows))]
    let result = replace_binary_unix(current, &staged, publisher);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    fsync_dir(parent);
    Ok(())
}

#[cfg(not(windows))]
fn replace_binary_unix<P: ChecksumPublisher>(
    current: &Path,
    staged: &Path,
    publisher: &P,
) -> Result<()> {
    // The backup is copied to a temp file and renamed over `.previous`, so an
    // interrupted copy cannot leave a truncated rollback target.
    write_backup(current, &previous_binary_path(current), publisher)?;
    std::fs::rename(staged, current)
        .with_context(|| format!("replace {} with the staged binary", current.display()))
}

#[cfg(windows)]
fn replace_binary_windows<P: ChecksumPublisher>(
    current: &Path,
    staged: &Path,
    publisher: &P,
) -> Result<()> {
    let old = old_binary_path(current);
    let previous = previous_binary_path(current);
    // Back up the running binary before touching it. Copying a running image is
    // allowed on Windows; the copy is staged and renamed so an interruption
    // cannot leave a truncated `h4rnd.previous.exe`.
    write_backup(current, &previous, publisher)?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(current, &old)
        .with_context(|| format!("move the running binary to {}", old.display()))?;
    if let Err(error) = std::fs::rename(staged, current) {
        // Put the old binary back; the install path must never stay empty.
        let _ = std::fs::rename(&old, current);
        return Err(error).with_context(|| format!("replace {}", current.display()));
    }
    // `.previous` is already the backup, so the moved-aside copy is redundant.
    // If removing it fails it is cleaned on the next start.
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// Swaps `h4rnd.previous` back in.
pub fn rollback_binary(current: &Path) -> Result<()> {
    let previous = previous_binary_path(current);
    // A missing, empty, non-executable or truncated backup must never replace
    // a working installation.
    check_previous_binary(&previous)?;
    let parent = current.parent();
    #[cfg(windows)]
    rollback_binary_windows(current, &previous)?;
    #[cfg(not(windows))]
    // One rename over the target: an interruption cannot leave the install
    // path empty.
    std::fs::rename(&previous, current)
        .with_context(|| format!("restore {}", current.display()))?;
    remove_backup_checksum(&previous);
    if let Some(parent) = parent {
        fsync_dir(parent);
    }
    Ok(())
}

#[cfg(windows)]
fn rollback_binary_windows(current: &Path, previous: &Path) -> Result<()> {
    let failed = failed_binary_path(current);
    let _ = std::fs::remove_file(&failed);
    std::fs::rename(current, &failed)
        .with_context(|| format!("move the current binary to {}", failed.display()))?;
    if let Err(error) = std::fs::rename(previous, current) {
        // A failed rollback must leave the binary that was running in place.
        let _ = std::fs::rename(&failed, current);
        return Err(error).with_context(|| format!("restore {}", current.display()));
    }
    let _ = std::fs::remove_file(&failed);
    Ok(())
}

/// What startup recovery did to one installation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Whether a moved-aside binary was put back at the install path.
    pub restored: bool,
    /// How many staging files left by dead runs were removed.
    pub removed_staging: usize,
    /// Whether another run held the lock, so recovery was skipped.
    pub skipped_locked: bool,
    /// Whether the rollback copy was refused because it failed validation.
    pub refused_previous: bool,
}

/// The install path a running image belongs to.
///
/// A binary started from a moved-aside name (`h4rnd.old.exe`,
/// `h4rnd.previous`, `h4rnd.failed.exe`) recovers the install path it belongs
/// to, so recovery works even when the install path itself does not exist.
fn install_path_for(executable: &Path) -> PathBuf {
    let Some(name) = executable.file_name().and_then(|name| name.to_str()) else {
        return executable.to_path_buf();
    };
    for tag in [OLD_TAG, FAILED_TAG, PREVIOUS_TAG] {
        if let Some(target) = strip_sidecar_tag(name, tag)
            && !target.is_empty()
        {
            return executable.with_file_name(target);
        }
    }
    executable.to_path_buf()
}

/// Startup recovery for this process's installation.
///
/// `main` calls this before parsing arguments: it finishes or undoes an
/// interrupted replace/rollback, and removes this installation's orphaned
/// staging files when no update is running.
pub fn startup_recovery() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let executable = crate::service::strip_verbatim_prefix(executable);
    let report = startup_recovery_for(&executable);
    if report.restored {
        eprintln!(
            "Recovered {} from an interrupted update.",
            install_path_for(&executable).display()
        );
    }
}

/// Startup recovery for one executable's installation.
///
/// This is the body of [`startup_recovery`], exposed so a test can simulate a
/// process started from a moved-aside image. It takes the installation lock,
/// restores a missing binary from `h4rnd.old`/`h4rnd.failed`, and removes
/// orphaned staging files. When another run holds the lock it touches nothing.
pub fn startup_recovery_for(executable: &Path) -> RecoveryReport {
    let current = install_path_for(executable);
    let mut report = RecoveryReport::default();
    let lock = match UpdateLock::try_acquire(&current) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            // An update is in progress. Its staging file and moved-aside
            // binary belong to that run: never touch them.
            report.skipped_locked = true;
            return report;
        }
        Err(_) => return report,
    };
    recover(&current, &mut report);
    drop(lock);
    report
}

fn recover(current: &Path, report: &mut RecoveryReport) {
    if !current.exists() {
        let old = old_binary_path(current);
        if old.exists() && std::fs::rename(&old, current).is_ok() {
            report.restored = true;
        } else {
            let failed = failed_binary_path(current);
            if failed.exists() && std::fs::rename(&failed, current).is_ok() {
                report.restored = true;
            }
        }
    }
    if !current.exists() {
        // A replace that lost both moved-aside names still has the rollback
        // copy. Validate it the way `--rollback` does before publishing it: an
        // empty, non-executable or truncated copy must never become the
        // installed binary. Publish it atomically (temp beside the target,
        // fsync, rename), never with a plain copy a crash could truncate.
        let previous = previous_binary_path(current);
        if previous.exists() {
            match check_previous_binary(&previous) {
                Ok(()) => match copy_file_atomic(&previous, current) {
                    Ok(()) => report.restored = true,
                    Err(error) => eprintln!(
                        "Could not recover {} from {}: {error}",
                        current.display(),
                        previous.display()
                    ),
                },
                Err(error) => {
                    // Leave every file for a person to inspect: the backup may
                    // be the only copy of a previous installation.
                    report.refused_previous = true;
                    eprintln!(
                        "Cannot recover {} from {}: {error}",
                        current.display(),
                        previous.display()
                    );
                    eprintln!(
                        "The files were left in place. Run `h4rnd update` again or reinstall h4rn."
                    );
                    return;
                }
            }
        }
    }
    if !current.exists() {
        // Nothing can be restored (for example, the staged binary and backup
        // were both lost). Leave every file for a person to inspect.
        return;
    }
    let _ = std::fs::remove_file(old_binary_path(current));
    let _ = std::fs::remove_file(failed_binary_path(current));
    report.removed_staging = remove_staging_files(current);
}

/// Copies `from` to a unique temp file beside `to`, fsyncs it, and renames it
/// over `to`. `to` is never partially written.
fn copy_file_atomic(from: &Path, to: &Path) -> Result<()> {
    let parent = to
        .parent()
        .context("the install path has no parent directory")?;
    // Only the Unix publication copies the source's executable bits. `open`
    // below still reports a missing source on every platform.
    #[cfg(unix)]
    let metadata = std::fs::metadata(from).with_context(|| format!("read {}", from.display()))?;
    let mut source =
        std::fs::File::open(from).with_context(|| format!("open {}", from.display()))?;
    let mut last_error = None;
    for attempt in 0..16u32 {
        let temp = parent.join(staging_name(to, attempt));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create recovery file {}", temp.display()));
            }
        };
        let copied = std::io::copy(&mut source, &mut file).and_then(|_| file.sync_all());
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&temp);
            return Err(error).with_context(|| format!("write recovery file {}", temp.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode();
            if let Err(error) =
                std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode | 0o111))
            {
                let _ = std::fs::remove_file(&temp);
                return Err(error).with_context(|| format!("make {} executable", temp.display()));
            }
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temp, to) {
            let _ = std::fs::remove_file(&temp);
            return Err(error).with_context(|| format!("publish {}", to.display()));
        }
        fsync_dir(parent);
        return Ok(());
    }
    Err(last_error.map(anyhow::Error::from).unwrap_or_else(|| {
        anyhow::anyhow!("could not create a recovery file next to {}", to.display())
    }))
}

/// Runs the startup recovery for the Windows logon task and fails when another
/// update holds the installation lock.
///
/// The scheduled task calls `h4rnd --recover-only`, so it uses the same lock,
/// restore rules and atomic publication as `h4rnd update` instead of a separate
/// PowerShell copy.
pub fn recover_only() -> Result<RecoveryReport> {
    let executable = std::env::current_exe().context("locate the running h4rnd executable")?;
    let executable = crate::service::strip_verbatim_prefix(executable);
    recover_only_for(&executable)
}

/// [`recover_only`] for an explicit executable, testable without `current_exe`.
pub fn recover_only_for(executable: &Path) -> Result<RecoveryReport> {
    let report = startup_recovery_for(executable);
    if report.skipped_locked {
        bail!(
            "another h4rnd update, rollback or recovery is running for {}; not starting the \
             daemon yet",
            install_path_for(executable).display()
        );
    }
    let current = install_path_for(executable);
    if !current.exists() {
        bail!(
            "{} is missing and could not be recovered; run `h4rnd update` again or reinstall \
             h4rn",
            current.display()
        );
    }
    Ok(report)
}

/// Whether a file name is one of this installation's staging files.
fn is_staging_name(current: &Path, name: &str) -> bool {
    let Some(rest) = name.strip_prefix(&staging_prefix(current)) else {
        return false;
    };
    // `<pid>-<attempt>-<8 hex digits>`, exactly.
    let mut parts = rest.split('-');
    let (Some(pid), Some(attempt), Some(nonce), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    pid.parse::<u32>().is_ok()
        && attempt.parse::<u32>().is_ok()
        && nonce.len() == 8
        && nonce.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// Removes staging files this installation's dead runs left behind.
///
/// The caller holds the installation lock, so no other update can be writing
/// them. A file that merely shares the prefix is left alone.
fn remove_staging_files(current: &Path) -> usize {
    let Some(parent) = current.parent() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_staging_name(current, name) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn fsync_dir(_path: &Path) {
    // Windows has no directory handle to fsync; the file itself is synced
    // before it is renamed into place.
    #[cfg(unix)]
    {
        if let Ok(directory) = std::fs::File::open(_path) {
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

/// What can be said about the daemon that owns an installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    /// No service is active and nothing is listening: safe to replace.
    Stopped,
    /// The daemon answered `/healthz`.
    Running {
        /// Version it reported, when it reports one.
        version: Option<String>,
        /// Turns in flight; `None` when the health response omits the field.
        active_turns: Option<usize>,
    },
    /// Something is there (an active service or an open port) but it did not
    /// answer the health check. The guard fails closed.
    Unreachable {
        /// The health address that was probed.
        addr: SocketAddr,
    },
}

async fn health_at(addr: SocketAddr) -> Option<Health> {
    let url = format!("http://{addr}/healthz");
    let http = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .ok()?;
    let response = http.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

async fn port_open(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(PORT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Probes the daemon that would have to restart for this update.
///
/// `service_active` is the service manager's view; an active unit whose
/// daemon cannot answer still blocks the update rather than being treated as
/// stopped.
pub async fn daemon_state(addr: SocketAddr, service_active: bool) -> DaemonState {
    if addr.port() == 0 {
        // An ephemeral port cannot be probed from another process.
        return DaemonState::Unreachable { addr };
    }
    if let Some(health) = health_at(addr).await {
        return DaemonState::Running {
            version: health.version,
            active_turns: health.active_turns,
        };
    }
    if service_active || port_open(addr).await {
        return DaemonState::Unreachable { addr };
    }
    DaemonState::Stopped
}

/// Why an update must not continue, or `None` when it is safe.
///
/// Reads "fails closed": an unanswerable daemon and a health response without
/// `active_turns` both block the update.
pub fn state_refusal(state: &DaemonState) -> Option<String> {
    match state {
        DaemonState::Stopped => None,
        DaemonState::Running {
            active_turns: Some(0),
            ..
        } => None,
        DaemonState::Running {
            version,
            active_turns: Some(turns),
        } => Some(format!(
            "h4rnd {} is running with {turns} active turn(s)",
            version.as_deref().unwrap_or("(unknown version)")
        )),
        DaemonState::Running {
            version,
            active_turns: None,
        } => Some(format!(
            "h4rnd {} is running but its health check does not report active turns",
            version.as_deref().unwrap_or("(unknown version)")
        )),
        DaemonState::Unreachable { addr } if addr.port() == 0 => Some(format!(
            "h4rnd is configured to listen on an ephemeral port ({addr}), so it cannot be \
             probed for active turns"
        )),
        DaemonState::Unreachable { addr } => Some(format!(
            "h4rnd appears to be running at {addr} but did not answer its health check"
        )),
    }
}

/// Refuses to replace the binary while a turn is running or the daemon cannot
/// say whether one is.
///
/// Called before the download and again immediately before the binary is
/// replaced, so a turn that started during the download still blocks the
/// restart.
pub async fn check_daemon_idle(addr: SocketAddr, service_active: bool, force: bool) -> Result<()> {
    let state = daemon_state(addr, service_active).await;
    if let Some(problem) = state_refusal(&state) {
        if !force {
            bail!("{problem}; finish the turn, stop the service, or pass --force to update anyway");
        }
        println!("warning: {problem}; continuing because --force was passed");
        return Ok(());
    }
    if let DaemonState::Running { version, .. } = &state {
        match version {
            Some(version) => println!("h4rnd {version} is running and idle."),
            None => println!("h4rnd is running and idle."),
        }
    }
    Ok(())
}

/// Why the service must not be restarted after the binary was already
/// replaced, or `None` when the restart can go ahead.
///
/// The binary is already installed at this point, so a turn that started since
/// the last check is left running: the user restarts h4rnd when it finishes.
pub fn restart_skip_reason(state: &DaemonState, force: bool) -> Option<String> {
    if force { None } else { state_refusal(state) }
}

/// The daemon an update or rollback must not interrupt.
///
/// The registered service is resolved first: its own unit/plist/task names the
/// executable and the health endpoint, so the guard never probes the updater's
/// own `--bind` (which may belong to a different daemon or to nothing at all).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateTarget {
    /// What the platform service manager has registered.
    pub registered: crate::service::RegisteredTarget,
    /// The updater's own bind, probed only when no service is installed.
    pub standalone_health: SocketAddr,
}

impl UpdateTarget {
    /// Resolves the target for this host.
    pub(crate) fn resolve(config: &Config) -> Self {
        Self {
            registered: crate::service::registered_target(),
            standalone_health: health_addr(config.bind),
        }
    }
}

/// Checks the daemon that this update would have to restart.
async fn check_target_idle(
    target: &UpdateTarget,
    force: bool,
    service_active: &dyn Fn() -> bool,
) -> Result<()> {
    match &target.registered {
        crate::service::RegisteredTarget::Unverified { reason } => {
            let problem = unverified_refusal(reason);
            if !force {
                bail!("{problem}; pass --force to update without checking the service");
            }
            println!("warning: {problem}; continuing because --force was passed");
            Ok(())
        }
        crate::service::RegisteredTarget::NotInstalled => {
            check_daemon_idle(target.standalone_health, false, force).await
        }
        crate::service::RegisteredTarget::Runs { health, .. } => {
            check_daemon_idle(*health, service_active(), force).await
        }
    }
}

/// Why the restart must be skipped after the binary was already replaced.
async fn target_restart_skip_reason(
    target: &UpdateTarget,
    force: bool,
    service_active: &dyn Fn() -> bool,
) -> Option<String> {
    if force {
        return None;
    }
    match &target.registered {
        crate::service::RegisteredTarget::Unverified { reason } => Some(unverified_refusal(reason)),
        crate::service::RegisteredTarget::NotInstalled => {
            let state = daemon_state(target.standalone_health, false).await;
            restart_skip_reason(&state, false)
        }
        crate::service::RegisteredTarget::Runs { health, .. } => {
            let state = daemon_state(*health, service_active()).await;
            restart_skip_reason(&state, false)
        }
    }
}

fn unverified_refusal(reason: &str) -> String {
    format!(
        "the registered h4rnd service could not be verified ({reason}), so the daemon that \
         would restart could not be checked"
    )
}

/// What a rollback tells the user when a turn keeps the restart from running.
pub(crate) const ROLLBACK_RESTART_SKIP: &str =
    "binary restored; restart h4rnd when the turn finishes (`h4rnd service restart`)";

/// Everything one update run needs, so the whole flow can be tested without
/// reaching GitHub or touching the real service manager.
pub(crate) struct UpdateDeps {
    /// The binary being replaced or rolled back.
    pub current: PathBuf,
    /// The daemon that must be idle before the restart.
    pub target: UpdateTarget,
    /// Release client; `None` for `--rollback`.
    pub client: Option<ReleaseClient>,
    /// The compiled minisign key, passed in so tests do not need a signed build.
    pub public_key: Option<String>,
    /// Restarts the service; a test records whether it ran.
    pub restart: Box<dyn Fn(&Path) -> crate::service::RestartOutcome>,
    /// Whether the service manager reports the service as active.
    pub service_active: Box<dyn Fn() -> bool>,
    /// Prepares the installation before the replace (Windows migrates the
    /// scheduled task's launcher so it can recover a missing binary).
    pub prepare_replace: Box<dyn Fn() -> Result<()>>,
}

/// Runs one `h4rnd update`.
pub async fn run(config: &Config, options: UpdateOptions) -> Result<()> {
    if options.rollback && (options.check || options.version.is_some()) {
        bail!("--rollback cannot be combined with --check or --version");
    }
    let current = crate::service::current_binary_path()?;
    let client = if options.rollback {
        None
    } else {
        Some(ReleaseClient::new(ReleaseRepository::configured()?)?)
    };
    let target = if options.check {
        // `--check` reads release metadata only and must not depend on the
        // service manager being available.
        UpdateTarget {
            registered: crate::service::RegisteredTarget::NotInstalled,
            standalone_health: health_addr(config.bind),
        }
    } else {
        UpdateTarget::resolve(config)
    };
    let prepare_current = current.clone();
    run_with(
        options,
        UpdateDeps {
            current,
            target,
            client,
            public_key: configured_public_key().map(str::to_owned),
            restart: Box::new(crate::service::restart_after_update),
            service_active: Box::new(crate::service::is_active),
            prepare_replace: Box::new(move || crate::service::prepare_replace(&prepare_current)),
        },
    )
    .await
}

/// Takes the installation lock, unless `--check` only reads metadata.
fn acquire_lock(current: &Path, check: bool) -> Result<Option<UpdateLock>> {
    if check {
        Ok(None)
    } else {
        Ok(Some(UpdateLock::acquire(current)?))
    }
}

/// The update flow, with its external effects injected.
pub(crate) async fn run_with(options: UpdateOptions, deps: UpdateDeps) -> Result<()> {
    let UpdateDeps {
        current,
        target,
        client,
        public_key,
        restart,
        service_active,
        prepare_replace,
    } = deps;

    if options.rollback {
        // One update at a time, and the same fail-closed guard as an update:
        // rolling back while a turn runs would interrupt it just as badly.
        let lock = acquire_lock(&current, false)?;
        check_target_idle(&target, options.force, service_active.as_ref()).await?;
        // A Windows rollback also moves the install path aside, so the task must
        // be able to recover it first.
        prepare_replace()?;
        rollback_binary(&current)?;
        println!("Restored the previous h4rn binary.");
        // A turn can start between the guard and the restart; the restored
        // binary is already in place, so leave the daemon alone.
        if let Some(problem) =
            target_restart_skip_reason(&target, options.force, service_active.as_ref()).await
        {
            for line in rollback_skip_lines(&problem) {
                println!("{line}");
            }
            return Ok(());
        }
        // Release the lock before the service starts again: on Windows the
        // task's launcher waits for it, and reporting success while the daemon
        // cannot take it would be a lie.
        drop(lock);
        restart_or_explain(&current, &restart);
        return Ok(());
    }

    // One `h4rnd update` at a time for this installation. `--check` only reads
    // release metadata, so it does not need the lock (or a writable install
    // directory).
    let lock = acquire_lock(&current, options.check)?;

    let client = client.context("an update needs a release client")?;
    let repository = client.repository().clone();
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

    let Some(public_key) = public_key.as_deref() else {
        print_manual_steps(&repository, &release);
        bail!("self-update is disabled in this build (no H4RN_UPDATER_PUBKEY)");
    };
    // The checksums are only trusted through their signature, which binds the
    // release version and target through the asset name inside the file.
    let sums_url = release.assets.get("SHA256SUMS.sig").with_context(|| {
        format!(
            "release {} has no SHA256SUMS.sig; refusing to install a release whose \
             checksums are not signed",
            release.version
        )
    })?;

    // Fail before downloading anything when the daemon that would restart is
    // busy, using the service's endpoint rather than this process's bind.
    check_target_idle(&target, options.force, service_active.as_ref()).await?;

    let asset_name = archive_name(&release.version, TARGET_TRIPLE);
    let archive = client.download(release.asset(&asset_name)?).await?;
    let sums = client.download(release.asset("SHA256SUMS")?).await?;
    let sums_signature = String::from_utf8(client.download(sums_url).await?)
        .context("SHA256SUMS.sig is not UTF-8")?;
    let sums = String::from_utf8(sums).context("SHA256SUMS is not UTF-8")?;
    verify_download(&asset_name, &archive, &sums, &sums_signature, public_key)?;
    let binary = extract_binary(&archive, TARGET_TRIPLE)?;

    // A turn that started while the download ran must not be cut off by the
    // restart below.
    check_target_idle(&target, options.force, service_active.as_ref()).await?;

    // On Windows the registered task must be able to recover a missing install
    // path before the running binary is moved aside.
    prepare_replace()?;
    replace_binary(&current, &binary)?;
    println!(
        "Installed h4rnd {} at {}.",
        release.version,
        current.display()
    );
    // A turn can start between the guard above and the restart. The new binary
    // is already installed, so keep it and let the user restart when the turn
    // finishes instead of interrupting the work.
    if let Some(problem) =
        target_restart_skip_reason(&target, options.force, service_active.as_ref()).await
    {
        println!("The binary was updated, but {problem}.");
        println!("binary updated; restart h4rnd when the turn finishes (`h4rnd service restart`)");
        return Ok(());
    }
    // The replace is complete, so release the installation lock before the
    // service starts: the Windows launcher waits for this lock, and a restart
    // while it is held would make the task fail or stall.
    drop(lock);
    restart_or_explain(&current, &restart);
    Ok(())
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

/// The two lines printed when a rollback skipped the restart.
fn rollback_skip_lines(problem: &str) -> [String; 2] {
    [
        format!("The binary was restored, but {problem}."),
        ROLLBACK_RESTART_SKIP.to_owned(),
    ]
}

/// Restarts the service only when it runs the binary that was just replaced.
///
/// A restart failure is a warning: the binary is already installed and the
/// user can restart the service by hand.
fn restart_or_explain(current: &Path, restart: &dyn Fn(&Path) -> crate::service::RestartOutcome) {
    use crate::service::RestartOutcome;

    match restart(current) {
        RestartOutcome::Restarted => {
            println!("Restarted the h4rnd service.");
        }
        RestartOutcome::NotInstalled => {
            println!("Restart h4rnd to use the new binary.");
        }
        RestartOutcome::OtherInstall(installed) => {
            println!(
                "The h4rnd service runs {}, not the binary that was just replaced.",
                installed.display()
            );
            println!("It was not restarted. Run `h4rnd service restart` there, or");
            println!("`h4rnd service install` from this binary to switch the service.");
        }
        RestartOutcome::UnknownInstall => {
            println!("The h4rnd service is installed, but the binary it runs could not be read.");
            println!("It was not restarted. Run `h4rnd service restart` to pick up the new build.");
        }
        RestartOutcome::Failed(error) => {
            eprintln!("warning: the binary was updated but the service did not restart: {error}");
            eprintln!("Run `h4rnd service restart` to pick up the new binary.");
        }
    }
}

fn print_manual_steps(repository: &ReleaseRepository, release: &ReleaseInfo) {
    let asset = archive_name(&release.version, TARGET_TRIPLE);
    println!();
    println!("This build has no updater public key, so it will not install an unsigned binary.");
    println!("Update by hand:");
    println!("  1. Download {asset} from {}", repository.releases_url());
    println!("  2. Verify it against SHA256SUMS and SHA256SUMS.sig.");
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

    fn install_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let current = dir
            .path()
            .join(if cfg!(windows) { "h4rnd.exe" } else { "h4rnd" });
        std::fs::write(&current, b"old").unwrap();
        (dir, current)
    }

    fn staged_path(current: &Path) -> PathBuf {
        current.parent().unwrap().join(staging_name(current, 0))
    }

    /// Serves `/healthz` answers in order, one connection per probe.
    struct HealthStub {
        addr: SocketAddr,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for HealthStub {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn health_stub(turns: Vec<usize>) -> HealthStub {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let turns = turns
                    .get(index)
                    .or_else(|| turns.last())
                    .copied()
                    .unwrap_or(0);
                let body =
                    format!("{{\"status\":\"ok\",\"version\":\"1.0.0\",\"active_turns\":{turns}}}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        HealthStub { addr, hits, task }
    }

    fn counting_restart(
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Box<dyn Fn(&Path) -> crate::service::RestartOutcome> {
        Box::new(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::service::RestartOutcome::Restarted
        })
    }

    fn release_archive(binary: &[u8]) -> Vec<u8> {
        if TARGET_TRIPLE.contains("windows") {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            writer
                .start_file(
                    "h4rnd-v9.9.9/h4rnd.exe",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(binary).unwrap();
            writer.finish().unwrap().into_inner()
        } else {
            tar_gz_with_binary(binary)
        }
    }

    /// A mocked release that this binary would install.
    async fn mock_release(
        binary: &[u8],
        version: &str,
        key: &SigningKey,
    ) -> (wiremock::MockServer, ReleaseClient, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let archive = release_archive(binary);
        let name = archive_name(version, TARGET_TRIPLE);
        let (sums, signature) = signed_sums(&archive, &name, key);
        let assets = format!(
            r#"{{"tag_name":"v{version}","assets":[
                {{"name":"SHA256SUMS","browser_download_url":"{base}/sums"}},
                {{"name":"SHA256SUMS.sig","browser_download_url":"{base}/sums.sig"}},
                {{"name":{name},"browser_download_url":"{base}/archive"}}]}}"#,
            base = server.uri(),
            name = serde_json::to_string(&name).unwrap(),
        );
        Mock::given(method("GET"))
            .and(path("/repos/chark1es/h4rn/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(assets, "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sums"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sums.sig"))
            .respond_with(ResponseTemplate::new(200).set_body_string(signature))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/archive"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
            .mount(&server)
            .await;
        let client = ReleaseClient::with_api_base(test_repository(), server.uri()).unwrap();
        let (_, public_key) = test_key();
        (server, client, public_key)
    }

    fn target_for(
        registered: crate::service::RegisteredTarget,
        standalone_health: SocketAddr,
    ) -> UpdateTarget {
        UpdateTarget {
            registered,
            standalone_health,
        }
    }

    /// Publishes the real backup but fails the checksum step.
    struct FailingChecksum;

    impl ChecksumPublisher for FailingChecksum {
        fn stage(&self, _path: &Path, _base: &Path, _contents: &str) -> Result<PathBuf> {
            Err(anyhow::anyhow!("injected checksum publication failure"))
        }

        fn commit(&self, _staged: &Path, _path: &Path) -> Result<()> {
            unreachable!("the checksum is never staged when staging fails")
        }
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
        verify_download(name, archive, &sums, &signature, &public_key)
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
        verify_download(name, archive, &sums, &signature, &public_key)
            .expect("the desktop updater's encoding verifies");
    }

    #[test]
    fn a_tampered_archive_is_rejected() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, name, &key);
        let error =
            verify_download(name, b"tampered archive", &sums, &signature, &public_key).unwrap_err();
        assert!(error.to_string().contains("does not match SHA256SUMS"));
    }

    #[test]
    fn tampered_checksums_and_missing_signatures_are_rejected() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, name, &key);
        let tampered = sums.replace(&sha256_hex(archive), &"0".repeat(64));
        assert!(verify_download(name, archive, &tampered, &signature, &public_key).is_err());
        // No signature over the checksum file: refuse, even though the hash
        // matches. An archive signature alone would not bind version/target.
        for missing in ["", "   \n"] {
            let error = verify_download(name, archive, &sums, missing, &public_key).unwrap_err();
            assert!(
                error.to_string().contains("no SHA256SUMS.sig"),
                "unexpected error: {error}"
            );
        }
        // A line appended to SHA256SUMS keeps the archive hash valid but breaks
        // the signature over the file.
        let appended = format!("{sums}{}  other-asset\n", "c".repeat(64));
        assert!(verify_download(name, archive, &appended, &signature, &public_key).is_err());
        assert!(verify_download(name, archive, &sums, &signature, "").is_err());
    }

    #[test]
    fn a_signature_for_another_asset_does_not_verify_this_one() {
        let (key, public_key) = test_key();
        let archive = b"daemon archive";
        let name = "h4rnd-v1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        // The signature is valid, but over checksums for a different version.
        let other = "h4rnd-v0.9.0-x86_64-unknown-linux-gnu.tar.gz";
        let (sums, signature) = signed_sums(archive, other, &key);
        let error = verify_download(name, archive, &sums, &signature, &public_key).unwrap_err();
        assert!(error.to_string().contains("no entry"));
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
        let (_dir, current) = install_dir();
        replace_binary(&current, b"new").unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        let previous = previous_binary_path(&current);
        assert_eq!(std::fs::read(&previous).unwrap(), b"old");
        // The backup carries a size and SHA-256 recorded after the copy.
        let checksum = read_backup_checksum(&previous).expect("the backup records its checksum");
        assert_eq!(checksum.0, sha256_hex(b"old"));
        assert_eq!(checksum.1, 3);
        // The staged file was renamed into place, not left behind.
        assert_eq!(remove_staging_files(&current), 0);
        assert!(!staged_path(&current).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(&current).unwrap().permissions().mode() & 0o111,
                0
            );
            assert_ne!(
                std::fs::metadata(&previous).unwrap().permissions().mode() & 0o111,
                0,
                "the backup must be installable"
            );
        }

        rollback_binary(&current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!previous.exists());
        assert!(
            !previous_checksum_path(&previous).exists(),
            "the consumed backup's checksum sidecar is removed"
        );
        assert!(
            !failed_binary_path(&current).exists(),
            "rollback needs no intermediate moved-aside file on Unix"
        );
        assert!(rollback_binary(&current).is_err());
    }

    #[test]
    fn an_interrupted_backup_cannot_be_rolled_back() {
        let (_dir, current) = install_dir();
        let previous = previous_binary_path(&current);
        // The copy was cut off before it wrote anything: an empty file at the
        // final name (an older layout) is not installable.
        std::fs::write(&previous, b"").unwrap();
        let error = rollback_binary(&current).unwrap_err();
        assert!(error.to_string().contains("is empty"), "{error}");
        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"old",
            "a refused rollback must not touch the installed binary"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&previous, b"old").unwrap();
            std::fs::set_permissions(&previous, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = rollback_binary(&current).unwrap_err();
            assert!(error.to_string().contains("not executable"), "{error}");
            assert_eq!(std::fs::read(&current).unwrap(), b"old");
        }

        // No backup at all is refused as before.
        let _ = std::fs::remove_file(&previous);
        assert!(rollback_binary(&current).is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"old");

        // A copy cut off before its rename leaves only a staging file: there is
        // still no backup to install, and the installed binary is untouched.
        let staging = staged_path(&current);
        std::fs::write(&staging, b"old").unwrap();
        assert!(rollback_binary(&current).is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!previous.exists());
        let _ = std::fs::remove_file(&staging);
    }

    #[test]
    fn a_damaged_backup_is_refused_by_its_checksum() {
        let (_dir, current) = install_dir();
        replace_binary(&current, b"new").unwrap();
        let previous = previous_binary_path(&current);
        std::fs::write(&previous, b"truncated").unwrap();
        let error = rollback_binary(&current).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its recorded checksum"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"new",
            "a damaged backup must not replace the installed binary"
        );
    }

    #[test]
    fn sidecar_names_keep_a_windows_extension() {
        assert_eq!(
            previous_binary_path(Path::new("/opt/h4rn/h4rnd")),
            PathBuf::from("/opt/h4rn/h4rnd.previous")
        );
        assert_eq!(
            previous_binary_path(Path::new(r"C:\h4rn\h4rnd.exe")),
            PathBuf::from(r"C:\h4rn\h4rnd.previous.exe")
        );
        assert_eq!(
            install_path_for(Path::new(r"C:\h4rn\h4rnd.old.exe")),
            PathBuf::from(r"C:\h4rn\h4rnd.exe")
        );
        assert_eq!(
            install_path_for(Path::new("/opt/h4rn/h4rnd.failed")),
            PathBuf::from("/opt/h4rn/h4rnd")
        );
        // A normal binary name is not mistaken for a sidecar.
        assert_eq!(
            install_path_for(Path::new("/opt/h4rn/olden")),
            PathBuf::from("/opt/h4rn/olden")
        );
    }

    #[test]
    fn only_one_update_can_hold_the_installation_lock() {
        let (_dir, current) = install_dir();
        let held = UpdateLock::acquire(&current).unwrap();
        let error = UpdateLock::acquire(&current).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another h4rnd update, rollback or recovery is already running"),
            "unexpected error: {error}"
        );
        assert!(UpdateLock::try_acquire(&current).unwrap().is_none());
        drop(held);
        assert!(UpdateLock::try_acquire(&current).unwrap().is_some());
    }

    #[test]
    fn recovery_restores_a_binary_interrupted_between_renames() {
        let (_dir, current) = install_dir();
        let old = old_binary_path(&current);
        let staging = staged_path(&current);
        // A Windows replace moved the running binary aside and died before the
        // new one landed; a staged file is left behind too.
        std::fs::rename(&current, &old).unwrap();
        std::fs::write(&staging, b"new").unwrap();

        // The real startup entry point, run from the moved-aside image (which
        // is all that exists at this point).
        let report = startup_recovery_for(&old);
        assert!(report.restored);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!old.exists());
        assert!(!staging.exists(), "an orphaned staging file is removed");
    }

    #[test]
    fn recovery_leaves_another_runs_staging_file_alone() {
        let (_dir, current) = install_dir();
        let staging = staged_path(&current);
        std::fs::write(&staging, b"partial").unwrap();
        // A neighbouring file that only shares the prefix is not a staging
        // file and must survive too.
        let lookalike = current.parent().unwrap().join(format!(
            "{}.update-notes.txt",
            current.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&lookalike, b"notes").unwrap();

        // A live update holds the lock: its staging file is not ours to remove.
        let held = UpdateLock::acquire(&current).unwrap();
        let report = startup_recovery_for(&current);
        assert!(report.skipped_locked);
        assert!(staging.exists());
        drop(held);

        // With the lock free the leftovers belong to a dead run.
        let report = startup_recovery_for(&current);
        assert!(!report.skipped_locked);
        assert_eq!(report.removed_staging, 1);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!staging.exists());
        assert!(lookalike.exists());
    }

    #[test]
    fn recovery_restores_a_rollback_interrupted_after_the_first_rename() {
        let (_dir, current) = install_dir();
        let failed = failed_binary_path(&current);
        std::fs::rename(&current, &failed).unwrap();
        let report = startup_recovery_for(&current);
        assert!(report.restored);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!failed.exists());
    }

    #[test]
    fn a_health_report_without_turns_fails_closed() {
        let running = DaemonState::Running {
            version: Some("1.0.0".into()),
            active_turns: Some(2),
        };
        assert!(
            state_refusal(&running)
                .unwrap()
                .contains("2 active turn(s)")
        );
        let unknown = DaemonState::Running {
            version: Some("1.0.0".into()),
            active_turns: None,
        };
        assert!(state_refusal(&unknown).unwrap().contains("does not report"));
        assert!(
            state_refusal(&DaemonState::Running {
                version: Some("1.0.0".into()),
                active_turns: Some(0),
            })
            .is_none()
        );
        assert!(state_refusal(&DaemonState::Stopped).is_none());
        let unreachable = DaemonState::Unreachable {
            addr: "127.0.0.1:47800".parse().unwrap(),
        };
        assert!(
            state_refusal(&unreachable)
                .unwrap()
                .contains("did not answer")
        );
    }

    #[tokio::test]
    async fn an_unreachable_but_active_daemon_blocks_the_update() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept the connection but never answer the health request.
        let accept = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            socket.readable().await.unwrap();
            std::future::pending::<()>().await;
        });
        assert!(matches!(
            daemon_state(addr, true).await,
            DaemonState::Unreachable { .. }
        ));
        let error = check_daemon_idle(addr, true, false).await.unwrap_err();
        assert!(error.to_string().contains("did not answer"), "{error}");
        assert!(error.to_string().contains("--force"), "{error}");
        check_daemon_idle(addr, true, true)
            .await
            .expect("--force continues");
        accept.abort();
    }

    #[tokio::test]
    async fn a_stopped_or_idle_daemon_does_not_block_the_update() {
        // Nothing is listening on a port that was just released.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert_eq!(daemon_state(addr, false).await, DaemonState::Stopped);
        check_daemon_idle(addr, false, false).await.unwrap();

        // An active service whose port is closed still fails closed.
        assert!(matches!(
            daemon_state(addr, true).await,
            DaemonState::Unreachable { .. }
        ));
        assert!(check_daemon_idle(addr, true, false).await.is_err());
    }

    #[test]
    fn a_turn_that_starts_after_the_install_skips_the_restart() {
        let idle = DaemonState::Running {
            version: Some("1.0.0".into()),
            active_turns: Some(0),
        };
        assert!(restart_skip_reason(&idle, false).is_none());
        assert!(restart_skip_reason(&DaemonState::Stopped, false).is_none());

        let busy = DaemonState::Running {
            version: Some("1.0.0".into()),
            active_turns: Some(1),
        };
        let reason = restart_skip_reason(&busy, false).expect("a started turn skips the restart");
        assert!(reason.contains("1 active turn"), "{reason}");
        let unknown = DaemonState::Running {
            version: Some("1.0.0".into()),
            active_turns: None,
        };
        assert!(restart_skip_reason(&unknown, false).is_some());
        let unreachable = DaemonState::Unreachable {
            addr: "127.0.0.1:47800".parse().unwrap(),
        };
        assert!(restart_skip_reason(&unreachable, false).is_some());

        // --force restarts anyway, as it allowed the update in the first place.
        assert!(restart_skip_reason(&busy, true).is_none());
    }

    #[tokio::test]
    async fn an_ephemeral_port_cannot_be_probed_and_fails_closed() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert!(matches!(
            daemon_state(addr, false).await,
            DaemonState::Unreachable { .. }
        ));
        let error = check_daemon_idle(addr, false, false).await.unwrap_err();
        assert!(error.to_string().contains("ephemeral port"), "{error}");
        check_daemon_idle(addr, false, true).await.unwrap();
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

    fn test_repository() -> ReleaseRepository {
        ReleaseRepository {
            owner: "chark1es".to_owned(),
            name: "h4rn".to_owned(),
        }
    }

    #[tokio::test]
    async fn release_metadata_requests_time_out_quickly() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/chark1es/h4rn/releases/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(3))
                    .set_body_raw(r#"{"tag_name":"v9.9.9"}"#, "application/json"),
            )
            .mount(&server)
            .await;

        // Metadata is small: a slow answer is abandoned instead of waiting.
        let client = ReleaseClient::with_timeouts(
            test_repository(),
            server.uri(),
            Duration::from_millis(150),
            Duration::from_secs(30),
        )
        .unwrap();
        let error = client.latest().await.unwrap_err();
        let message = format!("{error:#}").to_ascii_lowercase();
        assert!(
            message.contains("timed out") || message.contains("timeout"),
            "expected a timeout, got: {message}"
        );
    }

    #[test]
    fn publishing_a_backup_replaces_the_old_checksum_sidecar() {
        let (_dir, current) = install_dir();
        replace_binary(&current, b"first").unwrap();
        let previous = previous_binary_path(&current);
        let checksum_path = previous_checksum_path(&previous);
        // A sidecar left over from an older backup must not survive.
        std::fs::write(&checksum_path, format!("{} 999\n", "0".repeat(64))).unwrap();

        replace_binary(&current, b"second").unwrap();
        let (hash, size) = read_backup_checksum(&previous).expect("the new backup has a checksum");
        assert_eq!(hash, sha256_hex(b"first"));
        assert_eq!(size, 5);
        assert!(
            !std::fs::read_to_string(&checksum_path)
                .unwrap_or_default()
                .contains(&"0".repeat(64)),
            "the stale checksum is gone"
        );
    }

    #[test]
    fn a_failed_checksum_publication_keeps_the_previous_backup_rollbackable() {
        let (_dir, current) = install_dir();
        // The installation has been upgraded once: `.previous` is "old" and
        // `.previous.sha256` describes it.
        replace_binary(&current, b"first").unwrap();
        let previous = previous_binary_path(&current);
        assert!(previous_checksum_path(&previous).exists());

        // A B→C upgrade stages the new backup and sidecar but fails to publish
        // the sidecar. Neither A nor its sidecar may be touched.
        let error = replace_binary_with(&current, b"second", &FailingChecksum).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected checksum publication failure"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"first",
            "a failed update must leave the installed binary in place"
        );
        assert_eq!(
            std::fs::read(&previous).unwrap(),
            b"old",
            "the previous backup must survive a failed publication"
        );
        assert!(
            check_previous_binary(&previous).is_ok(),
            "the surviving backup still matches its sidecar"
        );
        // The old backup is still what `--rollback` installs.
        rollback_binary(&current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        // No staging or checksum temp file is left behind.
        assert_eq!(remove_staging_files(&current), 0);
    }

    #[tokio::test]
    async fn a_download_can_outlast_the_idle_timeout_when_bytes_keep_arriving() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 23\r\n\r\n")
                .await
                .unwrap();
            // Six chunks, 100 ms apart: 600 ms in total, longer than the 250 ms
            // idle timeout, but no single gap reaches it.
            for chunk in [
                b"hello".as_slice(),
                b" world",
                b"!",
                b"..",
                b"....",
                b".....",
            ] {
                tokio::time::sleep(Duration::from_millis(100)).await;
                socket.write_all(chunk).await.unwrap();
                socket.flush().await.unwrap();
            }
            let _ = socket.shutdown().await;
        });

        let client = ReleaseClient::with_timeouts(
            test_repository(),
            "http://127.0.0.1:9",
            // The total (metadata) deadline is below the transfer's 600 ms. A
            // download that inherited it would fail, which is exactly what this
            // regression test must catch.
            Duration::from_millis(200),
            Duration::from_millis(250),
        )
        .unwrap();
        let url = format!("http://{addr}/archive");
        let bytes = client
            .download(&url)
            .await
            .expect("a transfer longer than the injectable total deadline still succeeds");
        assert_eq!(bytes, b"hello world!...........");
        server.abort();
    }

    #[tokio::test]
    async fn a_busy_service_blocks_the_update_even_when_the_cli_port_is_idle() {
        let idle = health_stub(vec![0]).await;
        let busy = health_stub(vec![1]).await;
        let (_dir, current) = install_dir();
        let (_server, client, public_key) =
            mock_release(b"new binary", "9.9.9", &test_key().0).await;
        let restarts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let error = run_with(
            UpdateOptions::default(),
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Runs {
                        binary: current.clone(),
                        health: busy.addr,
                    },
                    idle.addr,
                ),
                client: Some(client),
                public_key: Some(public_key),
                restart: counting_restart(restarts.clone()),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(|| Ok(())),
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("active turn"), "{error}");
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert_eq!(
            idle.hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the updater's own bind is never mistaken for the service"
        );
    }

    #[tokio::test]
    async fn a_busy_service_blocks_the_rollback_even_when_the_cli_port_is_idle() {
        let idle = health_stub(vec![0]).await;
        let busy = health_stub(vec![1]).await;
        let (_dir, current) = install_dir();
        replace_binary(&current, b"new binary").unwrap();
        let restarts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let error = run_with(
            UpdateOptions {
                rollback: true,
                ..UpdateOptions::default()
            },
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Runs {
                        binary: current.clone(),
                        health: busy.addr,
                    },
                    idle.addr,
                ),
                client: None,
                public_key: None,
                restart: counting_restart(restarts.clone()),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(|| Ok(())),
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("active turn"), "{error}");
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&current).unwrap(), b"new binary");
        assert_eq!(idle.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_idle_service_that_turns_busy_before_the_restart_skips_it() {
        // Idle before the download and before the replace, busy for the probe
        // immediately before the restart.
        let service = health_stub(vec![0, 0, 1]).await;
        let (_dir, current) = install_dir();
        let (_server, client, public_key) =
            mock_release(b"new binary", "9.9.9", &test_key().0).await;
        let restarts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        run_with(
            UpdateOptions::default(),
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Runs {
                        binary: current.clone(),
                        health: service.addr,
                    },
                    service.addr,
                ),
                client: Some(client),
                public_key: Some(public_key),
                restart: counting_restart(restarts.clone()),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(|| Ok(())),
            },
        )
        .await
        .expect("the binary is installed even when the turn starts late");

        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"new binary",
            "the binary is already installed"
        );
        assert_eq!(
            restarts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a turn that starts before the restart must not be cut off"
        );
        assert!(
            service.hits.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the service is probed before the replace and before the restart"
        );
    }

    #[tokio::test]
    async fn an_idle_service_that_turns_busy_before_the_rollback_restart_skips_it() {
        let service = health_stub(vec![0, 1]).await;
        let (_dir, current) = install_dir();
        replace_binary(&current, b"new binary").unwrap();
        let restarts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        run_with(
            UpdateOptions {
                rollback: true,
                ..UpdateOptions::default()
            },
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Runs {
                        binary: current.clone(),
                        health: service.addr,
                    },
                    service.addr,
                ),
                client: None,
                public_key: None,
                restart: counting_restart(restarts.clone()),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(|| Ok(())),
            },
        )
        .await
        .expect("the rollback completes even when the turn starts late");

        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unverifiable_service_refuses_without_force() {
        let (_dir, current) = install_dir();
        let restarts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let error = run_with(
            UpdateOptions {
                rollback: true,
                ..UpdateOptions::default()
            },
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Unverified {
                        reason: "the unit has no readable ExecStart".to_owned(),
                    },
                    // An idle daemon here must not make an unverified service
                    // look safe.
                    "127.0.0.1:9".parse().unwrap(),
                ),
                client: None,
                public_key: None,
                restart: counting_restart(restarts.clone()),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(|| Ok(())),
            },
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("could not be verified"),
            "{error}"
        );
        assert!(error.to_string().contains("--force"), "{error}");
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
    }

    #[test]
    fn recovery_publishes_the_rollback_copy_atomically() {
        let (_dir, current) = install_dir();
        let previous = previous_binary_path(&current);
        // Only the rollback backup exists: a replace that lost both the
        // install path and the moved-aside name.
        std::fs::write(&previous, b"previous binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&previous, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_file(&current).unwrap();

        let report = recover_only_for(&previous).unwrap();
        assert!(report.restored);
        assert_eq!(std::fs::read(&current).unwrap(), b"previous binary");
        assert!(
            previous.exists(),
            "the rollback copy stays available after recovery"
        );
        assert_eq!(remove_staging_files(&current), 0);
    }

    #[test]
    fn recover_only_skips_while_an_update_holds_the_lock() {
        let (_dir, current) = install_dir();
        let old = old_binary_path(&current);
        std::fs::rename(&current, &old).unwrap();
        let staging = staged_path(&current);
        std::fs::write(&staging, b"staged").unwrap();

        let held = UpdateLock::acquire(&current).unwrap();
        let error = recover_only_for(&old).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another h4rnd update, rollback or recovery is running"),
            "{error}"
        );
        assert!(!current.exists(), "the update's files are untouched");
        assert!(old.exists());
        assert!(staging.exists());
        drop(held);

        let report = recover_only_for(&old).unwrap();
        assert!(report.restored);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!old.exists());
        assert!(!staging.exists());
    }

    #[test]
    fn recovery_restores_the_failed_image_after_an_interrupted_rollback() {
        let (_dir, current) = install_dir();
        // A rollback moved the installed binary aside and died before the
        // backup was renamed in.
        let failed = failed_binary_path(&current);
        std::fs::rename(&current, &failed).unwrap();
        let report = recover_only_for(&failed).unwrap();
        assert!(report.restored);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!failed.exists());
    }

    #[tokio::test]
    async fn a_stalled_asset_download_times_out_instead_of_waiting_for_a_total_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            // Headers arrive, then the transfer stalls with no body bytes.
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n")
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = socket.write_all(b"hello").await;
        });

        let client = ReleaseClient::with_timeouts(
            test_repository(),
            "http://127.0.0.1:9",
            Duration::from_secs(5),
            Duration::from_millis(150),
        )
        .unwrap();
        let url = format!("http://{addr}/archive");
        let error = client.download(&url).await.unwrap_err();
        let message = format!("{error:#}").to_ascii_lowercase();
        assert!(
            message.contains("timed out") || message.contains("timeout"),
            "expected an idle timeout, got: {message}"
        );
        server.abort();
    }

    /// Whether the installation lock is free right now.
    ///
    /// `flock()` can briefly report a just-released lock as held on some
    /// kernels (WSL2 in particular), so this retries briefly. Before the fix the
    /// update holds the lock for the whole synchronous dispatch, so the retry
    /// still fails deterministically.
    fn lock_is_free(current: &Path) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match UpdateLock::try_acquire(current) {
                Ok(Some(lock)) => {
                    drop(lock);
                    return true;
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => return false,
            }
        }
    }

    #[tokio::test]
    async fn the_update_releases_the_lock_before_dispatching_the_restart() {
        let service = health_stub(vec![0]).await;
        let (_dir, current) = install_dir();
        let (_server, client, public_key) =
            mock_release(b"new binary", "9.9.9", &test_key().0).await;
        let dispatched_free = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = dispatched_free.clone();
        let prepared_while_old = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prepared = prepared_while_old.clone();
        let prepare_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = prepare_calls.clone();
        let prepare_path = current.clone();

        run_with(
            UpdateOptions::default(),
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Runs {
                        binary: current.clone(),
                        health: service.addr,
                    },
                    service.addr,
                ),
                client: Some(client),
                public_key: Some(public_key),
                restart: Box::new(move |path| {
                    flag.store(lock_is_free(path), std::sync::atomic::Ordering::SeqCst);
                    crate::service::RestartOutcome::Restarted
                }),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(move || {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Preparation must happen before the binary is replaced.
                    prepared.store(
                        std::fs::read(&prepare_path)
                            .map(|bytes| bytes == b"old")
                            .unwrap_or(false),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    Ok(())
                }),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            prepare_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the Windows launcher is prepared once, before the replace"
        );
        assert!(
            dispatched_free.load(std::sync::atomic::Ordering::SeqCst),
            "an immediate dispatch must find the installation lock released"
        );
        assert!(
            prepared_while_old.load(std::sync::atomic::Ordering::SeqCst),
            "the launcher is prepared while the old binary is still installed"
        );
        assert_eq!(std::fs::read(&current).unwrap(), b"new binary");
    }

    #[tokio::test]
    async fn the_rollback_releases_the_lock_before_dispatching_the_restart() {
        let service = health_stub(vec![0]).await;
        let (_dir, current) = install_dir();
        replace_binary(&current, b"new binary").unwrap();
        let dispatched_free = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = dispatched_free.clone();
        let prepared_while_new = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prepared = prepared_while_new.clone();
        let prepare_path = current.clone();

        run_with(
            UpdateOptions {
                rollback: true,
                ..UpdateOptions::default()
            },
            UpdateDeps {
                current: current.clone(),
                target: target_for(
                    crate::service::RegisteredTarget::Runs {
                        binary: current.clone(),
                        health: service.addr,
                    },
                    service.addr,
                ),
                client: None,
                public_key: None,
                restart: Box::new(move |path| {
                    flag.store(lock_is_free(path), std::sync::atomic::Ordering::SeqCst);
                    crate::service::RestartOutcome::Restarted
                }),
                service_active: Box::new(|| false),
                prepare_replace: Box::new(move || {
                    prepared.store(
                        std::fs::read(&prepare_path)
                            .map(|bytes| bytes == b"new binary")
                            .unwrap_or(false),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    Ok(())
                }),
            },
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(
            prepared_while_new.load(std::sync::atomic::Ordering::SeqCst),
            "the Windows launcher is prepared before the rollback moves the binary"
        );
        assert!(
            dispatched_free.load(std::sync::atomic::Ordering::SeqCst),
            "an immediate dispatch after a rollback must find the lock released"
        );
    }

    #[test]
    fn recovery_refuses_an_invalid_previous_copy() {
        let (_dir, current) = install_dir();
        let previous = previous_binary_path(&current);
        // The install path is gone and the only rollback copy is empty.
        std::fs::remove_file(&current).unwrap();
        std::fs::write(&previous, b"").unwrap();

        let report = startup_recovery_for(&current);
        assert!(report.refused_previous);
        assert!(!report.restored);
        assert!(!current.exists(), "an empty backup is never published");
        assert!(previous.exists(), "the refused backup is left in place");

        let error = recover_only_for(&current).unwrap_err();
        assert!(
            error.to_string().contains("could not be recovered"),
            "{error}"
        );
        assert!(!current.exists());
        assert!(previous.exists());
    }

    #[test]
    fn recovery_refuses_a_previous_copy_that_fails_its_checksum() {
        let (_dir, current) = install_dir();
        let previous = previous_binary_path(&current);
        std::fs::remove_file(&current).unwrap();
        std::fs::write(&previous, b"damaged").unwrap();
        std::fs::write(
            previous_checksum_path(&previous),
            format!("{} 7\n", "0".repeat(64)),
        )
        .unwrap();

        let report = startup_recovery_for(&current);
        assert!(report.refused_previous);
        assert!(!current.exists());
        assert!(previous.exists());
        assert!(previous_checksum_path(&previous).exists());
    }

    #[test]
    fn the_rollback_skip_message_says_the_binary_was_restored() {
        let lines = rollback_skip_lines("a turn is running");
        assert_eq!(
            lines[1],
            "binary restored; restart h4rnd when the turn finishes (`h4rnd service restart`)"
        );
        assert!(lines[0].contains("a turn is running"));
    }
}
