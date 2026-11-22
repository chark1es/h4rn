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
//! the target, after hard-linking the old binary to `h4rnd.previous`, so an
//! interrupted update always leaves a runnable binary at the install path. On
//! Windows a running executable cannot be replaced in place: the old binary is
//! moved to `h4rnd.old.exe` and the new one moved in. Startup recovery (in
//! `main`, before argument parsing) takes the same lock and restores the old
//! binary if the install path is missing, deriving the install path from the
//! moved-aside name so it works even when `h4rnd.exe` does not exist. The
//! scheduled task installed by `h4rnd service install` runs a small inline
//! recovery step before the daemon, so a machine whose update was cut off still
//! comes back up without a reinstall.

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
                "another `h4rnd update` is already running for {}; wait for it to finish \
                 (lock: {})",
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

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let _ = std::fs::remove_file(to);
    std::fs::copy(from, to)
        .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
    if let Ok(file) = std::fs::File::open(to) {
        let _ = file.sync_all();
    }
    Ok(())
}

/// Replaces `current` with `bytes`, keeping the old binary as `h4rnd.previous`.
pub fn replace_binary(current: &Path, bytes: &[u8]) -> Result<()> {
    let parent = current
        .parent()
        .context("the current executable has no parent directory")?;
    let staged = stage_binary(current, bytes)?;
    let result = if cfg!(windows) {
        replace_binary_windows(current, &staged)
    } else {
        replace_binary_unix(current, &staged)
    };
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    fsync_dir(parent);
    Ok(())
}

#[cfg(not(windows))]
fn replace_binary_unix(current: &Path, staged: &Path) -> Result<()> {
    let previous = previous_binary_path(current);
    // A hard link keeps the old binary alive under `.previous` while the
    // staged file replaces the executable's path with one rename. If linking
    // is not supported on this filesystem, a copy has the same guarantee.
    let _ = std::fs::remove_file(&previous);
    if std::fs::hard_link(current, &previous).is_err() {
        copy_file(current, &previous)?;
    }
    std::fs::rename(staged, current)
        .with_context(|| format!("replace {} with the staged binary", current.display()))
}

#[cfg(windows)]
fn replace_binary_windows(current: &Path, staged: &Path) -> Result<()> {
    let old = old_binary_path(current);
    let previous = previous_binary_path(current);
    // Back up the running binary before touching it. Copying a running image is
    // allowed on Windows; if the backup fails, the installation is untouched.
    copy_file(current, &previous)?;
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

#[cfg(not(windows))]
fn replace_binary_windows(_current: &Path, _staged: &Path) -> Result<()> {
    unreachable!("the Windows replace path is only compiled on Windows")
}

/// Swaps `h4rnd.previous` back in.
pub fn rollback_binary(current: &Path) -> Result<()> {
    let previous = previous_binary_path(current);
    if !previous.exists() {
        bail!(
            "no previous binary at {}; only one update can be rolled back",
            previous.display()
        );
    }
    let parent = current.parent();
    if cfg!(windows) {
        rollback_binary_windows(current, &previous)?;
    } else {
        // One rename over the target: an interruption cannot leave the install
        // path empty.
        std::fs::rename(&previous, current)
            .with_context(|| format!("restore {}", current.display()))?;
    }
    if let Some(parent) = parent {
        fsync_dir(parent);
    }
    Ok(())
}

#[cfg(not(windows))]
fn rollback_binary_windows(_current: &Path, _previous: &Path) -> Result<()> {
    unreachable!("the Windows rollback path is only compiled on Windows")
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
        // Nothing can be restored (for example, the staged binary and backup
        // were both lost). Leave every file for a person to inspect.
        return;
    }
    let _ = std::fs::remove_file(old_binary_path(current));
    let _ = std::fs::remove_file(failed_binary_path(current));
    report.removed_staging = remove_staging_files(current);
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

/// Runs one `h4rnd update`.
pub async fn run(config: &Config, options: UpdateOptions) -> Result<()> {
    if options.rollback && (options.check || options.version.is_some()) {
        bail!("--rollback cannot be combined with --check or --version");
    }
    let current = crate::service::current_binary_path()?;
    let addr = health_addr(config.bind);

    if options.rollback {
        // One update at a time, and the same fail-closed guard as an update:
        // rolling back while a turn runs would interrupt it just as badly.
        let _lock = UpdateLock::acquire(&current)?;
        check_daemon_idle(addr, crate::service::is_active(), options.force).await?;
        rollback_binary(&current)?;
        println!("Restored the previous h4rnd binary.");
        restart_or_explain(&current);
        return Ok(());
    }

    // One `h4rnd update` at a time for this installation. `--check` only reads
    // release metadata, so it does not need the lock (or a writable install
    // directory).
    let _lock = if options.check {
        None
    } else {
        Some(UpdateLock::acquire(&current)?)
    };

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
    // The checksums are only trusted through their signature, which binds the
    // release version and target through the asset name inside the file.
    let sums_url = release.assets.get("SHA256SUMS.sig").with_context(|| {
        format!(
            "release {} has no SHA256SUMS.sig; refusing to install a release whose \
             checksums are not signed",
            release.version
        )
    })?;

    // Fail before downloading anything when a turn is in flight.
    check_daemon_idle(addr, crate::service::is_active(), options.force).await?;

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
    check_daemon_idle(addr, crate::service::is_active(), options.force).await?;

    replace_binary(&current, &binary)?;
    println!(
        "Installed h4rnd {} at {}.",
        release.version,
        current.display()
    );
    restart_or_explain(&current);
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

/// Restarts the service only when it runs the binary that was just replaced.
///
/// A restart failure is a warning: the binary is already installed and the
/// user can restart the service by hand.
fn restart_or_explain(current: &Path) {
    use crate::service::RestartOutcome;

    match crate::service::restart_after_update(current) {
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
        assert_eq!(
            std::fs::read(previous_binary_path(&current)).unwrap(),
            b"old"
        );
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
        }

        rollback_binary(&current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
        assert!(!previous_binary_path(&current).exists());
        assert!(
            !failed_binary_path(&current).exists(),
            "rollback needs no intermediate moved-aside file on Unix"
        );
        assert!(rollback_binary(&current).is_err());
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
                .contains("another `h4rnd update` is already running"),
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
}
