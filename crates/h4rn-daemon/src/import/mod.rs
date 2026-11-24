//! Importing the conversations Claude Code and Codex already saved.
//!
//! Discovery is read-only and bounded: at most 100 conversations and 4 GiB per
//! project per attempt, and one conversation at a time as a stream. The
//! parsers in [`claude`] and [`codex`] own the two on-disk formats; this module
//! groups what they find by git checkout, turns a selected group into real
//! threads, and reports progress.
//!
//! Nothing here invents data: a project appears only when a real transcript
//! names its folder, and a conversation appears only when its file parses.

mod claude;
mod codex;
mod record;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    HostId, ImportEvent, ImportProgress, ImportProject, ImportScan, ImportSource, MessageRole,
    ProtoError, ProtoErrorCode,
};
use tokio::sync::{Mutex, broadcast};

use crate::journal::Journal;
use crate::projects::ProjectService;
use crate::threads::{ImportedThreadMessage, NewImportedThread, ThreadManager};

/// Conversations read per project per attempt.
pub const MAX_CONVERSATIONS_PER_PROJECT: u32 = 100;

/// Bytes of transcripts read per project per attempt.
pub const MAX_PROJECT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// A single transcript larger than this is never imported.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Kept text and metadata may not exceed this per conversation.
pub const MAX_KEPT_BYTES: usize = 32 * 1024 * 1024;

/// Records in one transcript before it is skipped entirely.
pub const MAX_RECORDS: usize = 100_000;

/// Most messages kept from one conversation.
pub const MAX_KEPT_MESSAGES: usize = 200;

/// How recent a project must be to be offered already checked.
const RECOMMENDED_AGE_DAYS: i64 = 30;

/// How many conversations a recommended project needs.
const RECOMMENDED_CONVERSATIONS: u32 = 3;

/// A candidate transcript on disk.
#[derive(Clone, Debug)]
struct Candidate {
    source: ImportSource,
    path: PathBuf,
    size: u64,
    mtime: DateTime<Utc>,
}

/// What a cheap head read of a transcript tells us before parsing it.
#[derive(Clone, Debug)]
pub(crate) struct Peek {
    /// Working directory the conversation ran in.
    pub cwd: PathBuf,
    /// Harness-native session id, when the record names one.
    ///
    /// A harness that names a transcript by its file falls back to the file
    /// stem here, so a scan always has a provisional identity to group by.
    pub session_id: Option<String>,
    /// Whether a record in the head itself named the session id.
    ///
    /// The file-name fallback is not the conversation's own name: a late id
    /// found by the full parse still has to be recognised by path, while an
    /// explicit id at a path is a claim about that conversation.
    pub named_session: bool,
    /// Whether the head already showed a visible message.
    ///
    /// A rollout that only wrote setup metadata is not a conversation and must
    /// not use up a project's import quota.
    pub has_visible: bool,
}

/// Why one conversation was left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The file could not be opened or read.
    Unreadable,
    /// No record named a working directory.
    NoWorkingDirectory,
    /// The conversation has no visible message.
    Empty,
    /// Claude only ran commands and no model reply followed.
    OnlyCommands,
    /// The file holds more records than one import may read.
    TooManyRecords,
    /// The kept text needs more than [`MAX_KEPT_BYTES`].
    TooMuchText,
}

impl SkipReason {
    fn is_notable(self) -> bool {
        !matches!(self, Self::Empty | Self::OnlyCommands)
    }

    fn message(self) -> &'static str {
        match self {
            Self::Unreadable => "a transcript could not be read",
            Self::NoWorkingDirectory => "a transcript named no working folder",
            Self::Empty => "an empty conversation was skipped",
            Self::OnlyCommands => "a conversation with only commands was skipped",
            Self::TooManyRecords => "a transcript had more than 100,000 records",
            Self::TooMuchText => {
                "a conversation needed more text than an import keeps (32 MiB kept, 48 MiB per record)"
            }
        }
    }
}

/// Why one conversation could not become a thread.
enum ImportFailure {
    /// The transcript has nothing importable.
    Skipped(SkipReason),
    /// The transcript parsed but the thread could not be created.
    Failed,
}

/// One message kept from a transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeptMessage {
    pub role: MessageRole,
    pub text: String,
    pub at: DateTime<Utc>,
}

/// A fully parsed conversation ready to become a thread.
#[derive(Clone, Debug)]
pub(crate) struct ParsedConversation {
    pub source: ImportSource,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub title: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub messages: Vec<KeptMessage>,
}

/// The kept window while a transcript is streamed.
///
/// The journal keeps what the transcript said; the handoff applies its own
/// byte ceiling when the person writes again. Only the hard 32 MiB kept-text
/// limit is enforced here, so a long first prompt is stored whole.
pub(crate) struct History {
    first_user: Option<KeptMessage>,
    tail: VecDeque<KeptMessage>,
    /// Bytes of text in `tail`, so the limit is checked as the window grows.
    retained_bytes: usize,
    /// Set when more text was seen than one conversation may keep.
    overflow: bool,
    /// When the first message kept was written.
    first_at: Option<DateTime<Utc>>,
}

impl History {
    pub(crate) fn new() -> Self {
        Self {
            first_user: None,
            tail: VecDeque::new(),
            retained_bytes: 0,
            overflow: false,
            first_at: None,
        }
    }

    /// Adds one visible message, keeping the first request and the newest rest.
    ///
    /// Once the limit is crossed nothing more is retained: the kept window is
    /// released and later messages are ignored, so one oversized conversation
    /// cannot grow memory toward the size of its file.
    pub(crate) fn push(&mut self, message: KeptMessage) {
        if self.overflow {
            return;
        }
        if self.first_at.is_none() {
            self.first_at = Some(message.at);
        }
        if message.role == MessageRole::User && self.first_user.is_none() {
            self.first_user = Some(message.clone());
        }
        self.retained_bytes = self.retained_bytes.saturating_add(message_cost(&message));
        self.tail.push_back(message);
        while self.tail.len() > MAX_KEPT_MESSAGES {
            if let Some(dropped) = self.tail.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(message_cost(&dropped));
            }
        }
        if self.retained_bytes > MAX_KEPT_BYTES {
            self.note_overflow();
        }
    }

    /// Marks the conversation as one that cannot be kept whole.
    ///
    /// Used when the pending window or a single message outgrows the kept-text
    /// limit, so the importer skips it with a warning instead of dropping text.
    /// The text kept so far is released at once: the conversation is skipped,
    /// so holding it would only grow memory.
    pub(crate) fn note_overflow(&mut self) {
        self.overflow = true;
        self.first_user = None;
        self.tail.clear();
        self.retained_bytes = 0;
    }

    /// Bytes this window still holds, for a bounded-memory test.
    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Messages this window still holds, for a bounded-memory test.
    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.tail.len()
    }

    /// When the first message kept was written, whether user or assistant.
    pub(crate) fn first_at(&self) -> Option<DateTime<Utc>> {
        self.first_at
    }

    /// The first user request's text, when one was kept.
    pub(crate) fn first_text(&self) -> Option<String> {
        self.first_user.as_ref().map(|message| message.text.clone())
    }

    /// The final kept messages in order, capped at the import ceiling.
    pub(crate) fn finish(mut self) -> Result<Vec<KeptMessage>, SkipReason> {
        if self.overflow {
            return Err(SkipReason::TooMuchText);
        }
        let mut messages: Vec<KeptMessage> = self.tail.drain(..).collect();
        if let Some(first) = self.first_user.take()
            && !messages.contains(&first)
        {
            messages.insert(0, first);
        }
        // Keep the first request plus the newest 199.
        while messages.len() > MAX_KEPT_MESSAGES {
            messages.remove(1);
        }
        let bytes: usize = messages.iter().map(message_cost).sum();
        if bytes > MAX_KEPT_BYTES {
            return Err(SkipReason::TooMuchText);
        }
        Ok(messages)
    }
}

fn message_cost(message: &KeptMessage) -> usize {
    message.text.len().saturating_add(16)
}

/// Truncates text to at most `bytes`, on a character boundary.
fn truncate_on_char(mut text: String, bytes: usize) -> String {
    if text.len() <= bytes {
        return text;
    }
    let mut cut = bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push('…');
    text
}

/// The first line of a request, used when the source saved no title.
pub(crate) fn derive_title(text: &str) -> String {
    let text = strip_wake_envelope(text);
    let line = text.trim().split('\n').next().unwrap_or_default();
    truncate_on_char(line.trim().to_owned(), 100)
}

fn strip_wake_envelope(text: &str) -> &str {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<wake")
        && let Some(close) = rest.find('>')
    {
        let inner = &rest[close + 1..];
        return inner
            .split_once("</wake>")
            .map(|(body, _)| body)
            .unwrap_or(inner);
    }
    text
}

/// Parses an RFC 3339 timestamp from either format.
pub(crate) fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    let text = value?.trim();
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// Finds the git checkout a folder belongs to, including linked worktrees.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GitGroup {
    key: String,
    path: PathBuf,
    name: String,
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = std::fs::canonicalize(start).ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn git_group(start: &Path) -> Option<GitGroup> {
    let root = find_git_root(start)?;
    let dot_git = root.join(".git");
    let (config, main) = if dot_git.is_dir() {
        (dot_git.join("config"), root.clone())
    } else {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = pointer
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:").map(str::trim))?;
        let git_dir = resolve_path(&root, target);
        let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
        if git_dir
            .components()
            .any(|component| component.as_os_str() == "worktrees")
        {
            let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
            let common_dir = resolve_path(&git_dir, common.trim());
            let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
            let main = if common_dir.file_name().is_some_and(|name| name == ".git") {
                common_dir
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone())
            } else {
                root.clone()
            };
            (common_dir.join("config"), main)
        } else {
            (git_dir.join("config"), root.clone())
        }
    };
    let remote = read_origin_url(&config);
    let (key, name) = match remote.as_deref().and_then(github_repository) {
        Some(repository) => (format!("github:{repository}"), repository),
        None => (format!("path:{}", main.display()), folder_name(&main)),
    };
    Some(GitGroup {
        key,
        path: main,
        name,
    })
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The `origin` URL from a git config file.
fn read_origin_url(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if in_origin
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "url"
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// `owner/name` for a GitHub remote URL, or `None` for any other host.
pub(crate) fn github_repository(url: &str) -> Option<String> {
    let url = url.trim();
    let index = url.find("github.com")?;
    let rest = &url[index + "github.com".len()..];
    let rest = rest.trim_start_matches(['/', ':']);
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner == ".." || name == ".." {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// Resolves a path the same way on every platform.
///
/// Windows may report a path with its extended-length prefix (`\\?\C:\…`,
/// `\\?\UNC\server\share\…`); the prefix describes spelling, not a different
/// place, so it is stripped before comparing. The rest is canonicalised when
/// it exists, following a symlinked `~/Downloads` to its real folder.
fn normalise_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let plain = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    };
    std::fs::canonicalize(&plain).unwrap_or(plain)
}

fn is_under_downloads(path: &Path, home: &Path) -> bool {
    let downloads = normalise_path(&home.join("Downloads"));
    let path = normalise_path(path);
    path.starts_with(&downloads)
}

/// Bytes of a transcript hashed when it names no session id.
const FINGERPRINT_BYTES: usize = 64 * 1024;

/// A stable identity for a transcript that names no session.
///
/// The path alone is not enough: a truncated or rewritten transcript is a new
/// conversation and must be importable. The size and a hash of the head make
/// the identity change when the content does.
fn content_fingerprint(path: &Path, size: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    if let Ok(mut file) = File::open(path) {
        let mut buffer = vec![0u8; FINGERPRINT_BYTES];
        if let Ok(read) = std::io::Read::read(&mut file, &mut buffer) {
            hasher.update(&buffer[..read]);
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// The source identity the journal stores for one transcript.
fn imported_key(source: ImportSource, session_id: Option<&str>, path: &Path, size: u64) -> String {
    let path = path.display().to_string();
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => h4rn_protocol::source_key(source, Some(id), &path),
        None => h4rn_protocol::path_source_key(
            source,
            &path,
            size,
            &content_fingerprint(Path::new(&path), size),
        ),
    }
}

/// One accepted transcript in a project group.
#[derive(Clone, Debug)]
struct ConversationFile {
    source: ImportSource,
    path: PathBuf,
    size: u64,
    already_imported: bool,
    /// The checkout this conversation belongs to, when it differed from the
    /// folder the conversation ran in.
    project_path: Option<String>,
}

/// A project group while scanning.
struct Group {
    key: String,
    path: PathBuf,
    name: String,
    git: bool,
    last_active: DateTime<Utc>,
    files: Vec<ConversationFile>,
    bytes: u64,
    attempts: u32,
    limited_count: bool,
    limited_bytes: bool,
}

/// Whether the per-project limits still have room for one conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Accepted,
    TooMany,
    TooLarge,
}

impl Group {
    /// Counts one not-yet-imported transcript against the per-project limits.
    fn admit(&mut self, size: u64) -> Admission {
        self.attempts = self.attempts.saturating_add(1);
        if self.attempts > MAX_CONVERSATIONS_PER_PROJECT {
            return Admission::TooMany;
        }
        if self.bytes.saturating_add(size) > MAX_PROJECT_BYTES {
            return Admission::TooLarge;
        }
        self.bytes = self.bytes.saturating_add(size);
        Admission::Accepted
    }
}

/// The outcome of one scan with the per-conversation data the importer needs.
struct ScanOutcome {
    scan: ImportScan,
    groups: BTreeMap<String, Group>,
    /// Transcript heads read, so a test can prove the scan stops early.
    #[cfg(test)]
    peeks: u64,
}

/// Imports saved conversations into real threads.
pub struct ImportService {
    journal: Arc<dyn Journal>,
    projects: Arc<ProjectService>,
    threads: Arc<ThreadManager>,
    host: HostId,
    home: PathBuf,
    claude_home: PathBuf,
    codex_home: PathBuf,
    /// Overrides that could not be resolved, shown with the first scan.
    home_warnings: Vec<String>,
    events: broadcast::Sender<ImportEvent>,
    state: Arc<Mutex<ImportState>>,
}

struct ImportState {
    progress: Option<ImportProgress>,
    running: bool,
}

impl ImportService {
    /// Opens the service, resolving each harness's configured home.
    pub fn open(
        journal: Arc<dyn Journal>,
        projects: Arc<ProjectService>,
        threads: Arc<ThreadManager>,
        host: HostId,
        home: PathBuf,
    ) -> Self {
        let mut home_warnings = Vec::new();
        let mut resolved = |kind| {
            let default = home.join(match kind {
                h4rn_protocol::HarnessKind::Codex => ".codex",
                _ => ".claude",
            });
            match h4rn_harness::harness_home(kind, &home) {
                Ok(Some(path)) => path,
                Ok(None) => default,
                Err(error) => {
                    home_warnings.push(format!("{error}. Reading {} instead.", default.display()));
                    default
                }
            }
        };
        let claude_home = resolved(h4rn_protocol::HarnessKind::ClaudeCode);
        let codex_home = resolved(h4rn_protocol::HarnessKind::Codex);
        let mut service = Self::with_homes(
            journal,
            projects,
            threads,
            host,
            home,
            claude_home,
            codex_home,
        );
        service.home_warnings = home_warnings;
        service
    }

    /// Like [`Self::open`] with explicit harness homes (tests).
    pub fn with_homes(
        journal: Arc<dyn Journal>,
        projects: Arc<ProjectService>,
        threads: Arc<ThreadManager>,
        host: HostId,
        home: PathBuf,
        claude_home: PathBuf,
        codex_home: PathBuf,
    ) -> Self {
        Self {
            journal,
            projects,
            threads,
            host,
            home,
            claude_home,
            codex_home,
            home_warnings: Vec::new(),
            events: broadcast::channel(64).0,
            state: Arc::new(Mutex::new(ImportState {
                progress: None,
                running: false,
            })),
        }
    }

    /// Progress frames for connected clients.
    pub fn subscribe(&self) -> broadcast::Receiver<ImportEvent> {
        self.events.subscribe()
    }

    /// The current or most recent run.
    pub async fn status(&self) -> Option<ImportProgress> {
        self.state.lock().await.progress.clone()
    }

    /// Finds importable conversations without changing anything.
    pub async fn scan(&self) -> ImportScan {
        self.scan_outcome().await.scan
    }

    async fn scan_outcome(&self) -> ScanOutcome {
        let imported = match self.journal.list_imported_sources().await {
            Ok(records) => ImportedSources::from_records(records),
            Err(error) => {
                tracing::warn!(%error, "could not read the imported-source list");
                ImportedSources::default()
            }
        };
        let registered: HashSet<PathBuf> = self
            .projects
            .list()
            .await
            .into_iter()
            .map(|project| canonical(&project.path))
            .collect();
        let home = self.home.clone();
        let claude_home = self.claude_home.clone();
        let codex_home = self.codex_home.clone();
        let home_warnings = self.home_warnings.clone();
        let result = tokio::task::spawn_blocking(move || {
            scan_blocking(&home, &claude_home, &codex_home, &imported, &registered)
        })
        .await;
        match result {
            Ok(mut outcome) => {
                outcome.scan.warnings.splice(0..0, home_warnings);
                outcome
            }
            Err(error) => {
                tracing::error!(%error, "the import scan worker panicked");
                ScanOutcome {
                    scan: ImportScan {
                        projects: Vec::new(),
                        warnings: vec!["The import scan could not finish.".to_owned()],
                        scanned_at: Utc::now(),
                    },
                    groups: BTreeMap::new(),
                    #[cfg(test)]
                    peeks: 0,
                }
            }
        }
    }

    /// Starts importing every not-yet-imported conversation in `project_keys`.
    ///
    /// Returns as soon as the run is claimed; progress is broadcast. A second
    /// start while one runs is refused.
    pub async fn start(&self, project_keys: Vec<String>) -> Result<ImportProgress, ProtoError> {
        if project_keys.is_empty() {
            return Err(ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                "Choose at least one project to import.",
                "Select the projects whose conversations you want.",
            ));
        }
        {
            let state = self.state.lock().await;
            if state.running {
                return Err(ProtoError::new(
                    ProtoErrorCode::Conflict,
                    "an import is already running",
                    "Wait for it to finish, then start another.",
                ));
            }
        }
        let outcome = self.scan_outcome().await;
        let wanted: HashSet<&str> = project_keys.iter().map(String::as_str).collect();
        let selected: Vec<Group> = outcome
            .groups
            .into_values()
            .filter(|group| wanted.contains(group.key.as_str()))
            .collect();
        if selected.is_empty() {
            return Err(ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                "No conversations were found for the selected projects.",
                "Scan again; the transcripts may have been removed.",
            ));
        }
        let conversations_total = selected
            .iter()
            .map(|group| group.files.len() as u32)
            .sum::<u32>();
        let import_id = format!("imp_{}", uuid::Uuid::now_v7().simple());
        let progress =
            ImportProgress::starting(import_id, selected.len() as u32, conversations_total);
        {
            let mut state = self.state.lock().await;
            if state.running {
                return Err(ProtoError::new(
                    ProtoErrorCode::Conflict,
                    "an import is already running",
                    "Wait for it to finish, then start another.",
                ));
            }
            state.running = true;
            state.progress = Some(progress.clone());
        }
        let _ = self.events.send(ImportEvent {
            progress: progress.clone(),
        });
        let service = ImportRunner {
            projects: Arc::clone(&self.projects),
            threads: Arc::clone(&self.threads),
            host: self.host.clone(),
            home: self.home.clone(),
            events: self.events.clone(),
            warnings: outcome.scan.warnings,
            state: Arc::clone(&self.state),
        };
        let worker_progress = progress.clone();
        tokio::spawn(async move {
            service.run(selected, worker_progress).await;
        });
        Ok(progress)
    }
}

/// The work of one import run.
struct ImportRunner {
    projects: Arc<ProjectService>,
    threads: Arc<ThreadManager>,
    host: HostId,
    /// The user's home, so a transcript that moved into `~/Downloads` is not
    /// imported even when its head named another folder.
    home: PathBuf,
    events: broadcast::Sender<ImportEvent>,
    warnings: Vec<String>,
    /// The run's state, so every published snapshot is what a reopened dialog
    /// reads.
    state: Arc<Mutex<ImportState>>,
}

impl ImportRunner {
    async fn run(self, groups: Vec<Group>, progress: ImportProgress) {
        let mut progress = progress;
        progress.warnings.extend(self.warnings.iter().cloned());
        let registered = self
            .projects
            .list()
            .await
            .into_iter()
            .map(|project| project.path)
            .collect::<HashSet<_>>();
        for group in groups {
            progress.current_project = Some(group.name.clone());
            self.publish(&progress).await;
            if !registered.contains(&group.path.display().to_string())
                && let Err(error) = self
                    .projects
                    .add(
                        h4rn_protocol::ProjectSource::Existing {
                            path: group.path.display().to_string(),
                        },
                        Some(group.name.clone()),
                    )
                    .await
            {
                progress
                    .warnings
                    .push(format!("Could not add {}: {}", group.name, error.message));
            }
            for file in group.files {
                if file.already_imported {
                    progress.conversations_done += 1;
                    progress.skipped += 1;
                    self.publish(&progress).await;
                    continue;
                }
                let result = self.import_one(&file).await;
                progress.conversations_done += 1;
                match result {
                    Ok(Imported::Created) => progress.imported += 1,
                    Ok(Imported::AlreadyClaimed) => progress.skipped += 1,
                    Ok(Imported::Nothing) => progress.skipped += 1,
                    Err(ImportFailure::Skipped(reason)) => {
                        progress.skipped += 1;
                        if reason.is_notable() {
                            progress.warnings.push(reason.message().to_owned());
                        }
                    }
                    Err(ImportFailure::Failed) => {
                        progress.failed += 1;
                        progress
                            .warnings
                            .push("A conversation could not be turned into a thread.".to_owned());
                    }
                }
                self.publish(&progress).await;
            }
            progress.projects_done += 1;
            self.publish(&progress).await;
        }
        progress.running = false;
        progress.current_project = None;
        progress.finished_at = Some(Utc::now());
        progress.warnings.sort();
        progress.warnings.dedup();
        self.publish(&progress).await;
    }

    /// Parses and creates one thread.
    async fn import_one(&self, file: &ConversationFile) -> Result<Imported, ImportFailure> {
        // Parsing streams a whole transcript from disk. It runs off the async
        // runtime, the same way a discovery scan does, so a long conversation
        // cannot hold a worker thread that other requests need.
        let path = file.path.clone();
        let source = file.source;
        let parsed = match parse_off_runtime(source, path).await {
            Ok(parsed) => parsed,
            Err(failure) => return Err(failure),
        };
        // The head named one folder; the full parser may have found another.
        // Whatever it picked still has to stay out of `~/Downloads`.
        if is_under_downloads(&parsed.cwd, &self.home) {
            return Ok(Imported::Nothing);
        }
        let title = h4rn_forge::redact(parsed.title.trim());
        let messages = parsed
            .messages
            .iter()
            .map(|message| ImportedThreadMessage {
                role: message.role,
                text: h4rn_forge::redact(&message.text),
                at: message.at,
            })
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(Imported::Nothing);
        }
        // The claim uses the identity the full parse found, not the scan's
        // provisional one: a record the scan's head did not reach may name the
        // conversation's session id, and two copies of it must still import
        // once. The unique claim inside the import transaction is the last
        // check, so a concurrent run cannot slip between them.
        let claim_key = imported_key(
            parsed.source,
            parsed.session_id.as_deref(),
            &file.path,
            file.size,
        );
        let request = NewImportedThread {
            host: self.host.clone(),
            harness: file.source.harness(),
            workspace_path: parsed.cwd.display().to_string(),
            project_path: file.project_path.clone(),
            model: parsed.model.unwrap_or_default(),
            title: if title.trim().is_empty() {
                "Imported thread".to_owned()
            } else {
                title
            },
            created_at: parsed.created_at,
            last_active: parsed.last_active,
            resume_id: parsed.session_id,
            source: parsed.source,
            source_path: parsed.path.display().to_string(),
            source_key: claim_key,
            messages,
        };
        self.threads
            .import_thread(request)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "could not create an imported thread");
                ImportFailure::Failed
            })
            .map(|created| {
                if created {
                    Imported::Created
                } else {
                    Imported::AlreadyClaimed
                }
            })
    }

    /// Publishes one progress snapshot and keeps it for a reopening dialog.
    async fn publish(&self, progress: &ImportProgress) {
        {
            let mut state = self.state.lock().await;
            state.progress = Some(progress.clone());
            if !progress.running {
                state.running = false;
            }
        }
        let _ = self.events.send(ImportEvent {
            progress: progress.clone(),
        });
    }
}

/// Parses one transcript on the blocking pool.
///
/// The parsers use ordinary blocking file I/O. Running them inline would stall
/// the async worker for as long as the transcript takes to read, so a busy
/// daemon could not answer anything else meanwhile.
async fn parse_off_runtime(
    source: ImportSource,
    path: PathBuf,
) -> Result<ParsedConversation, ImportFailure> {
    let parsed = tokio::task::spawn_blocking(move || match source {
        ImportSource::ClaudeCode => claude::parse(&path),
        ImportSource::Codex => codex::parse(&path),
    })
    .await;
    match parsed {
        Ok(Ok(conversation)) => Ok(conversation),
        Ok(Err(reason)) => Err(ImportFailure::Skipped(reason)),
        Err(error) => {
            tracing::warn!(%error, "a transcript parser did not finish");
            Err(ImportFailure::Failed)
        }
    }
}

/// What one conversation's import attempt did.
enum Imported {
    /// A new thread was written.
    Created,
    /// Another run had already claimed the source.
    AlreadyClaimed,
    /// The transcript has nothing importable.
    Nothing,
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// The source conversations this daemon already imported.
#[derive(Default)]
pub(crate) struct ImportedSources {
    /// Every claimed source key.
    keys: HashSet<String>,
    /// The claim key for each transcript path.
    ///
    /// A file's path is enough to recognise an already-imported source without
    /// reading its head again, which is what lets a full project stop early.
    by_path: HashMap<String, String>,
}

impl ImportedSources {
    fn from_records(records: Vec<crate::journal::ImportedSourceRecord>) -> Self {
        let mut sources = Self::default();
        for record in records {
            sources.by_path.insert(record.path, record.key.clone());
            sources.keys.insert(record.key);
        }
        sources
    }

    fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    fn key_for_path(&self, path: &Path) -> Option<&str> {
        self.by_path
            .get(path.to_string_lossy().as_ref())
            .map(String::as_str)
    }

    /// Whether the claim stored for this path names its conversation, not its
    /// file version.
    ///
    /// Only a session-id claim is matched by path. A path-keyed claim pins the
    /// file's size and content fingerprint, so a transcript that kept growing
    /// is a new conversation and must be offered again, just as the scan did
    /// before it looked at paths at all.
    fn contains_path(&self, path: &Path) -> bool {
        self.key_for_path(path)
            .filter(|key| !h4rn_protocol::path_keyed_source_key(key))
            .is_some_and(|key| self.contains(key))
    }

    /// Whether this transcript was already imported, however it was named.
    ///
    /// `head_key` is only what the cheap peek could compute. A transcript whose
    /// session id appears later in the file was claimed under its full-parse
    /// key, which is the identity the journal stored against the path; a head
    /// that named no session of its own therefore falls back to the path claim.
    /// A head that did name one has spoken about the conversation itself, and
    /// the path claim only hides it when that claim is the same id: a
    /// different id at the same path is another conversation, such as a
    /// transcript that was replaced in place.
    fn contains_transcript(&self, path: &Path, head_key: &str, named_session: bool) -> bool {
        self.contains(head_key) || (!named_session && self.contains_path(path))
    }

    /// Whether the claim stored for this path already covers this exact file.
    ///
    /// Used when a project's quota is spent and no head is read. A claim that
    /// names the conversation still counts the transcript for its project. A
    /// path-keyed claim pins the file's size, so a transcript that grew after
    /// it was imported is not counted as the same file.
    fn covers_file(&self, path: &Path, size: u64) -> bool {
        let Some(key) = self.key_for_path(path) else {
            return false;
        };
        if !h4rn_protocol::path_keyed_source_key(key) {
            return true;
        }
        h4rn_protocol::path_keyed_source_size(key) == Some(size)
    }
}

fn scan_blocking(
    home: &Path,
    claude_home: &Path,
    codex_home: &Path,
    imported: &ImportedSources,
    registered: &HashSet<PathBuf>,
) -> ScanOutcome {
    let mut warnings = Vec::new();
    let mut candidates = Vec::new();
    candidates.extend(discover(
        claude_home.join("projects"),
        ImportSource::ClaudeCode,
        false,
        &mut warnings,
    ));
    candidates.extend(discover(
        codex_home.join("sessions"),
        ImportSource::Codex,
        true,
        &mut warnings,
    ));
    candidates.sort_by(|left, right| {
        right
            .mtime
            .cmp(&left.mtime)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut identities: HashMap<PathBuf, Option<GitGroup>> = HashMap::new();
    // Claude keeps one folder per working directory, so once a project's quota
    // is full its other transcripts are not read at all. The folder maps to the
    // group whose quota filled, so an already-imported transcript in it is
    // still recorded.
    let mut full_folders: HashMap<PathBuf, String> = HashMap::new();
    #[cfg(test)]
    let mut peeks = 0u64;
    for candidate in candidates {
        if candidate.size == 0 {
            continue;
        }
        if candidate.size > MAX_FILE_BYTES {
            warnings.push("A transcript larger than 4 GiB was skipped.".to_owned());
            continue;
        }
        let folder = candidate
            .path
            .parent()
            .filter(|_| candidate.source == ImportSource::ClaudeCode)
            .map(Path::to_path_buf);
        if let Some(key) = folder.as_ref().and_then(|folder| full_folders.get(folder)) {
            // The quota is spent: no head is read. An already-imported source
            // still counts for its project, so older conversations are not
            // hidden behind it on a later attempt.
            if imported.covers_file(&candidate.path, candidate.size)
                && let Some(group) = groups.get_mut(key)
            {
                group.files.push(ConversationFile {
                    source: candidate.source,
                    path: candidate.path,
                    size: candidate.size,
                    already_imported: true,
                    project_path: None,
                });
            }
            continue;
        }
        #[cfg(test)]
        {
            peeks += 1;
        }
        let peek = match candidate.source {
            ImportSource::ClaudeCode => claude::peek(&candidate.path),
            ImportSource::Codex => codex::peek(&candidate.path),
        };
        let Some(peek) = peek else {
            warnings.push(format!("Could not read {}.", candidate.path.display()));
            continue;
        };
        if !peek.has_visible {
            // A session that only wrote setup metadata has no conversation to
            // offer, and must not use up the project's quota.
            continue;
        }
        if is_under_downloads(&peek.cwd, home) {
            continue;
        }
        if !peek.cwd.is_dir() {
            warnings.push(format!(
                "A conversation ran in {}, which no longer exists.",
                peek.cwd.display()
            ));
            continue;
        }
        let identity = identities
            .entry(peek.cwd.clone())
            .or_insert_with(|| git_group(&peek.cwd));
        let (key, path, name, git, folded) = match identity {
            Some(git) => (
                git.key.clone(),
                git.path.clone(),
                git.name.clone(),
                true,
                (git.path != peek.cwd && normalise_path(&git.path) != normalise_path(&peek.cwd))
                    .then(|| git.path.display().to_string()),
            ),
            None => (
                format!("path:{}", peek.cwd.display()),
                peek.cwd.clone(),
                folder_name(&peek.cwd),
                false,
                None,
            ),
        };
        let head_key = imported_key(
            candidate.source,
            peek.session_id.as_deref(),
            &candidate.path,
            candidate.size,
        );
        let already_imported =
            imported.contains_transcript(&candidate.path, &head_key, peek.named_session);
        let group_key = key.clone();
        let group = groups.entry(key).or_insert_with(|| Group {
            key: group_key.clone(),
            path,
            name,
            git,
            last_active: candidate.mtime,
            files: Vec::new(),
            bytes: 0,
            attempts: 0,
            limited_count: false,
            limited_bytes: false,
        });
        group.last_active = group.last_active.max(candidate.mtime);
        // A source this daemon already imported costs nothing to read, so it
        // neither uses the quota nor hides an older conversation behind it.
        if !already_imported {
            match group.admit(candidate.size) {
                Admission::Accepted => {}
                Admission::TooMany => {
                    if !group.limited_count {
                        group.limited_count = true;
                        warnings.push(format!(
                            "{} has more than {MAX_CONVERSATIONS_PER_PROJECT} conversations; only the newest are offered.",
                            group.name
                        ));
                    }
                    if let Some(folder) = folder {
                        full_folders.insert(folder, group_key.clone());
                    }
                    continue;
                }
                Admission::TooLarge => {
                    if !group.limited_bytes {
                        group.limited_bytes = true;
                        warnings.push(format!(
                            "{} holds more than 4 GiB of transcripts; older conversations were not scanned.",
                            group.name
                        ));
                    }
                    if let Some(folder) = folder {
                        full_folders.insert(folder, group_key.clone());
                    }
                    continue;
                }
            }
        }
        group.files.push(ConversationFile {
            source: candidate.source,
            path: candidate.path,
            size: candidate.size,
            already_imported,
            project_path: folded,
        });
    }
    let now = Utc::now();
    let recent = chrono::TimeDelta::days(RECOMMENDED_AGE_DAYS);
    let mut projects = Vec::with_capacity(groups.len());
    for group in groups.values() {
        let importable = group
            .files
            .iter()
            .filter(|file| !file.already_imported)
            .count() as u32;
        let recommended = group.git
            && !registered.contains(&canonical(&group.path.display().to_string()))
            && group.files.len() as u32 >= RECOMMENDED_CONVERSATIONS
            && now - group.last_active <= recent;
        projects.push(ImportProject {
            key: group.key.clone(),
            path: group.path.display().to_string(),
            name: group.name.clone(),
            git: group.git,
            last_active: group.last_active,
            conversations: group.files.len() as u32,
            importable,
            recommended,
        });
    }
    projects.sort_by(|left, right| {
        right
            .recommended
            .cmp(&left.recommended)
            .then_with(|| right.git.cmp(&left.git))
            .then_with(|| right.last_active.cmp(&left.last_active))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    warnings.sort();
    warnings.dedup();
    ScanOutcome {
        scan: ImportScan {
            projects,
            warnings,
            scanned_at: now,
        },
        groups,
        #[cfg(test)]
        peeks,
    }
}

/// Lists transcripts under one harness root.
///
/// Claude keeps `projects/<project>/*.jsonl`; Codex keeps
/// `sessions/YYYY/MM/DD/rollout-*.jsonl`.
fn discover(
    root: PathBuf,
    source: ImportSource,
    codex: bool,
    warnings: &mut Vec<String>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    if !root.is_dir() {
        return candidates;
    }
    if codex {
        let Ok(years) = std::fs::read_dir(&root) else {
            warnings.push(format!("Could not read {}.", root.display()));
            return candidates;
        };
        for year in years.flatten() {
            if !year.path().is_dir() {
                continue;
            }
            let Ok(months) = std::fs::read_dir(year.path()) else {
                continue;
            };
            for month in months.flatten() {
                if !month.path().is_dir() {
                    continue;
                }
                let Ok(days) = std::fs::read_dir(month.path()) else {
                    continue;
                };
                for day in days.flatten() {
                    collect_files(&day.path(), source, true, &mut candidates);
                }
            }
        }
    } else {
        let Ok(projects) = std::fs::read_dir(&root) else {
            warnings.push(format!("Could not read {}.", root.display()));
            return candidates;
        };
        for project in projects.flatten() {
            if project.path().is_dir() {
                collect_files(&project.path(), source, false, &mut candidates);
            }
        }
    }
    candidates
}

fn collect_files(
    directory: &Path,
    source: ImportSource,
    rollout_only: bool,
    out: &mut Vec<Candidate>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".jsonl") {
            continue;
        }
        if rollout_only && !name.starts_with("rollout-") {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mtime = metadata
            .modified()
            .ok()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(Utc::now);
        out.push(Candidate {
            source,
            path,
            size: metadata.len(),
            mtime,
        });
    }
}

/// Opens a transcript for streaming, or reports it unreadable.
pub(crate) fn open_reader(path: &Path) -> Result<BufReader<File>, SkipReason> {
    let file = File::open(path).map_err(|_| SkipReason::Unreadable)?;
    Ok(BufReader::new(file))
}

/// A record count that skips a transcript past the limit.
pub(crate) fn record_limit(records: usize) -> Result<(), SkipReason> {
    if records > MAX_RECORDS {
        Err(SkipReason::TooManyRecords)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_become_owner_name() {
        for url in [
            "https://github.com/acme/api.git",
            "git@github.com:acme/api.git",
            "ssh://git@github.com/acme/api.git",
            "git://github.com/acme/api",
        ] {
            assert_eq!(github_repository(url).as_deref(), Some("acme/api"), "{url}");
        }
        assert_eq!(github_repository("https://gitlab.com/acme/api.git"), None);
    }

    #[test]
    fn the_kept_history_keeps_the_first_request_and_the_newest_messages() {
        let mut history = History::new();
        history.push(KeptMessage {
            role: MessageRole::User,
            text: "original".into(),
            at: Utc::now(),
        });
        for index in 0..300 {
            history.push(KeptMessage {
                role: MessageRole::Assistant,
                text: format!("message {index}"),
                at: Utc::now(),
            });
        }
        let messages = history.finish().expect("messages");
        assert_eq!(messages.len(), MAX_KEPT_MESSAGES);
        assert_eq!(messages[0].text, "original");
        assert_eq!(messages.last().expect("last").text, "message 299");
    }

    #[test]
    fn a_long_first_prompt_is_stored_whole() {
        let mut history = History::new();
        let text = "x".repeat(70_000);
        history.push(KeptMessage {
            role: MessageRole::User,
            text: text.clone(),
            at: Utc::now(),
        });
        let messages = history.finish().expect("messages");
        assert_eq!(messages[0].text.len(), 70_000);
        assert_eq!(messages[0].text, text);
    }

    #[test]
    fn a_long_conversation_keeps_every_message_within_the_count_ceiling() {
        let mut history = History::new();
        for index in 0..200 {
            history.push(KeptMessage {
                role: if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                text: format!("{index}: {}", "y".repeat(1_500)),
                at: Utc::now(),
            });
        }
        let messages = history.finish().expect("messages");
        assert_eq!(messages.len(), 200);
        assert!(
            messages
                .iter()
                .map(|message| message.text.len())
                .sum::<usize>()
                > 300_000
        );
    }

    #[test]
    fn a_conversation_above_the_kept_text_limit_is_skipped() {
        let mut history = History::new();
        history.push(KeptMessage {
            role: MessageRole::User,
            text: "x".repeat(MAX_KEPT_BYTES + 1),
            at: Utc::now(),
        });
        assert_eq!(history.finish().err(), Some(SkipReason::TooMuchText));
    }

    #[test]
    fn an_overflowing_conversation_stops_holding_text_at_once() {
        let mut history = History::new();
        let chunk = "x".repeat(12 * 1024 * 1024);
        for _ in 0..3 {
            history.push(KeptMessage {
                role: MessageRole::User,
                text: chunk.clone(),
                at: Utc::now(),
            });
        }
        assert_eq!(
            history.retained_bytes(),
            0,
            "the window was released instead of growing toward the file size"
        );
        assert_eq!(history.retained_len(), 0);
        // Later messages are not retained either.
        history.push(KeptMessage {
            role: MessageRole::Assistant,
            text: chunk,
            at: Utc::now(),
        });
        assert_eq!(history.retained_len(), 0);
        assert_eq!(history.finish().err(), Some(SkipReason::TooMuchText));
    }

    fn claude_transcript(cwd: &Path, session: &str, prompt: &str) -> String {
        claude_transcript_with_session(cwd, Some(session), prompt)
    }

    /// A transcript whose records name no session id at all.
    fn claude_transcript_with_session(cwd: &Path, session: Option<&str>, prompt: &str) -> String {
        let mut user = serde_json::json!({
            "type": "user",
            "cwd": cwd.display().to_string(),
            "timestamp": "2026-11-20T10:00:00Z",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        });
        let mut assistant = serde_json::json!({
            "type": "assistant",
            "cwd": cwd.display().to_string(),
            "timestamp": "2026-11-20T10:00:01Z",
            "message": {
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "done"}],
            },
        });
        if let Some(session) = session {
            user["sessionId"] = serde_json::json!(session);
            assistant["sessionId"] = serde_json::json!(session);
        }
        format!("{user}\n{assistant}\n")
    }

    fn imported_claim(path: &Path, key: &str) -> ImportedSources {
        let mut imported = ImportedSources::default();
        imported.keys.insert(key.to_owned());
        imported
            .by_path
            .insert(path.display().to_string(), key.to_owned());
        imported
    }

    #[test]
    fn a_transcript_replaced_by_another_session_is_offered() {
        let path = PathBuf::from("/home/me/.claude/projects/-code-api/conversation.jsonl");
        let imported = imported_claim(&path, "claude_code:session-a");

        assert!(
            imported.contains_transcript(&path, "claude_code:session-a", true),
            "the same session is not offered again"
        );
        assert!(
            !imported.contains_transcript(&path, "claude_code:session-b", true),
            "an explicit, different session id at the same path is a new conversation"
        );
        // The head named no session of its own, so the path claim hides the
        // transcript even though the peek's provisional key is the file name.
        assert!(
            imported.contains_transcript(&path, "claude_code:conversation", false),
            "a late session id is still recognised by the path claim"
        );
    }

    #[test]
    fn a_grown_path_claim_does_not_count_for_a_full_folder() {
        let path = PathBuf::from("/home/me/.codex/sessions/2026/11/21/rollout.jsonl");
        let key = h4rn_protocol::path_source_key(
            ImportSource::Codex,
            &path.display().to_string(),
            100,
            "fingerprint",
        );
        let imported = imported_claim(&path, &key);

        assert!(imported.covers_file(&path, 100), "the same file counts");
        assert!(
            !imported.covers_file(&path, 120),
            "a grown rollout is a new conversation, not the imported file"
        );
        assert!(
            !imported.contains_transcript(&path, "codex:path", false),
            "a path claim is not a session claim"
        );
    }

    #[test]
    fn a_session_claim_counts_for_any_size_of_its_file() {
        let path = PathBuf::from("/home/me/.claude/projects/-code-api/session.jsonl");
        let imported = imported_claim(&path, "claude_code:session");
        assert!(imported.covers_file(&path, 4_096));
    }

    /// Parsing a transcript must not hold the async runtime.
    ///
    /// A FIFO with no writer yet blocks the parser's file open. A single-worker
    /// runtime can deliver its heartbeat only while that read runs on the
    /// blocking pool: without `spawn_blocking` the worker is stuck until the
    /// writer arrives, so the heartbeat lands after the writer.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn parsing_a_transcript_does_not_stall_the_runtime() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("transcript.jsonl");
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
        let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let body = claude_transcript(&workspace, "fifo-session", "read from the pipe");

        // The writer waits, so the parse is blocked on the FIFO while the
        // heartbeat is due.
        let writer_at = Arc::new(Mutex::new(None::<Instant>));
        let writer_mark = Arc::clone(&writer_at);
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2_000));
            *writer_mark.lock().expect("mark") = Some(Instant::now());
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .expect("open fifo");
            file.write_all(body.as_bytes()).expect("write");
        });

        let heartbeat_at = Arc::new(Mutex::new(None::<Instant>));
        let heartbeat_mark = Arc::clone(&heartbeat_at);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            *heartbeat_mark.lock().expect("mark") = Some(Instant::now());
        });
        let parse_path = path.clone();
        let parse =
            tokio::spawn(
                async move { parse_off_runtime(ImportSource::ClaudeCode, parse_path).await },
            );

        let parsed = tokio::time::timeout(Duration::from_secs(20), parse)
            .await
            .expect("the parser finished")
            .expect("parser task");
        writer.join().expect("writer");
        let heartbeat = heartbeat_at.lock().expect("mark").expect("heartbeat ran");
        let written = writer_at.lock().expect("mark").expect("writer ran");
        assert!(
            heartbeat < written,
            "the runtime heartbeat waited for the transcript read: parsing ran inline"
        );
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(_) => panic!("the transcript from the FIFO parsed"),
        };
        assert_eq!(
            parsed.messages.first().expect("first message").text,
            "read from the pipe"
        );
    }

    #[test]
    fn a_full_project_stops_reading_heads_and_keeps_already_imported_sources() {
        let home = tempfile::tempdir().expect("home");
        let home_path = home.path();
        let workspace = home_path.join("code").join("api");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let project = home_path.join(".claude/projects/-code-api");
        std::fs::create_dir_all(&project).expect("project dir");
        for index in 0..(MAX_CONVERSATIONS_PER_PROJECT + 2) {
            let session = format!("session-{index:03}");
            std::fs::write(
                project.join(format!("{session}.jsonl")),
                claude_transcript(&workspace, &session, &format!("conversation {index}")),
            )
            .expect("transcript");
        }

        let outcome = scan_blocking(
            home_path,
            &home_path.join(".claude"),
            &home_path.join(".codex"),
            &ImportedSources::default(),
            &HashSet::new(),
        );
        assert_eq!(outcome.scan.projects.len(), 1);
        assert_eq!(
            outcome.scan.projects[0].conversations, MAX_CONVERSATIONS_PER_PROJECT,
            "the project offers its newest conversations only"
        );
        assert_eq!(
            outcome.peeks,
            u64::from(MAX_CONVERSATIONS_PER_PROJECT) + 1,
            "once the quota is full, later heads are not read"
        );

        // An already-imported source in the full folder is still recognised,
        // so it keeps its place rather than vanishing from the list.
        let old = project.join("session-000.jsonl");
        let mut imported = ImportedSources::default();
        imported.keys.insert(h4rn_protocol::source_key(
            ImportSource::ClaudeCode,
            Some("session-000"),
            &old.display().to_string(),
        ));
        imported
            .by_path
            .insert(old.display().to_string(), "claude_code:session-000".into());
        let outcome = scan_blocking(
            home_path,
            &home_path.join(".claude"),
            &home_path.join(".codex"),
            &imported,
            &HashSet::new(),
        );
        assert_eq!(outcome.scan.projects[0].conversations, 101);
        assert_eq!(outcome.scan.projects[0].importable, 100);
    }

    #[test]
    fn a_full_folder_counts_an_imported_transcript_that_names_no_session() {
        let home = tempfile::tempdir().expect("home");
        let home_path = home.path();
        let workspace = home_path.join("code").join("api");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let project = home_path.join(".claude/projects/-code-api");
        std::fs::create_dir_all(&project).expect("project dir");
        for index in 0..(MAX_CONVERSATIONS_PER_PROJECT + 2) {
            let session = format!("session-{index:03}");
            let body = if index == 0 {
                // The oldest transcript names no session id, so its claim is
                // its file name rather than a session.
                claude_transcript_with_session(&workspace, None, "oldest")
            } else {
                claude_transcript(&workspace, &session, &format!("conversation {index}"))
            };
            std::fs::write(project.join(format!("{session}.jsonl")), body).expect("transcript");
        }
        let old = project.join("session-000.jsonl");
        // Make it the oldest transcript, so the quota fills before the scan
        // reaches it and it is only recognised by its stored claim.
        std::fs::File::options()
            .write(true)
            .open(&old)
            .expect("old transcript")
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
            )
            .expect("old mtime");
        let imported = imported_claim(&old, "claude_code:session-000");

        let outcome = scan_blocking(
            home_path,
            &home_path.join(".claude"),
            &home_path.join(".codex"),
            &imported,
            &HashSet::new(),
        );
        assert_eq!(
            outcome.peeks,
            u64::from(MAX_CONVERSATIONS_PER_PROJECT) + 1,
            "the quota filled before the oldest transcript was reached"
        );
        assert_eq!(
            outcome.scan.projects[0].conversations, 101,
            "the imported, id-less transcript still counts for its project"
        );
        assert_eq!(
            outcome.scan.projects[0].importable, 100,
            "it is not offered again"
        );
    }

    fn group() -> Group {
        Group {
            key: "path:/work/api".into(),
            path: PathBuf::from("/work/api"),
            name: "api".into(),
            git: true,
            last_active: Utc::now(),
            files: Vec::new(),
            bytes: 0,
            attempts: 0,
            limited_count: false,
            limited_bytes: false,
        }
    }

    #[test]
    fn a_project_offers_at_most_a_hundred_conversations_per_scan() {
        let mut group = group();
        for _ in 0..MAX_CONVERSATIONS_PER_PROJECT {
            assert_eq!(group.admit(1_024), Admission::Accepted);
        }
        assert_eq!(group.admit(1_024), Admission::TooMany);
    }

    #[test]
    fn a_project_reads_at_most_four_gigabytes_per_scan() {
        let mut group = group();
        let one_gib = 1024 * 1024 * 1024;
        assert_eq!(group.admit(3 * one_gib), Admission::Accepted);
        assert_eq!(group.admit(one_gib), Admission::Accepted);
        assert_eq!(group.admit(1), Admission::TooLarge);
    }

    #[test]
    fn downloads_is_recognised_through_a_symlink() {
        let home = tempfile::tempdir().expect("home");
        let real = home.path().join("real-downloads");
        std::fs::create_dir_all(real.join("thing")).expect("downloads");
        let link = home.path().join("Downloads");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(not(unix))]
        std::fs::create_dir_all(&link).expect("downloads dir");

        assert!(is_under_downloads(&real.join("thing"), home.path()));
        assert!(is_under_downloads(&link.join("thing"), home.path()));
        assert!(!is_under_downloads(
            &home.path().join("code/api"),
            home.path()
        ));
    }
}
