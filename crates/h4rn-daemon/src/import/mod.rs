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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    HostId, ImportEvent, ImportProgress, ImportProject, ImportScan, ImportSource, MessageRole,
    ProtoError, ProtoErrorCode, ThreadId,
};
use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

use crate::journal::{ImportedSourceRecord, Journal};
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

/// A line longer than this is treated as malformed instead of parsed.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

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
    pub session_id: Option<String>,
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
            Self::TooMuchText => "a conversation needed more than 32 MiB of kept text",
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
pub(crate) struct History {
    first_user: Option<KeptMessage>,
    tail: VecDeque<KeptMessage>,
}

impl History {
    pub(crate) fn new() -> Self {
        Self {
            first_user: None,
            tail: VecDeque::new(),
        }
    }

    /// Adds one visible message, keeping the first request and the newest rest.
    pub(crate) fn push(&mut self, message: KeptMessage) {
        if message.role == MessageRole::User && self.first_user.is_none() {
            self.first_user = Some(message.clone());
        }
        self.tail.push_back(message);
        if self.tail.len() > MAX_KEPT_MESSAGES {
            self.tail.pop_front();
        }
    }

    /// The first user request's text, when one was kept.
    pub(crate) fn first_text(&self) -> Option<String> {
        self.first_user.as_ref().map(|message| message.text.clone())
    }

    /// The final kept messages in order, capped at the import ceiling.
    pub(crate) fn finish(mut self) -> Result<Vec<KeptMessage>, SkipReason> {
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
        let mut bytes: usize = messages.iter().map(message_cost).sum();
        if bytes > MAX_KEPT_BYTES {
            return Err(SkipReason::TooMuchText);
        }
        while bytes > crate::handoff::IMPORTED_HISTORY_BYTES && messages.len() > 1 {
            let dropped = messages.remove(1);
            bytes = bytes.saturating_sub(message_cost(&dropped));
        }
        if bytes > crate::handoff::IMPORTED_HISTORY_BYTES
            && let Some(first) = messages.first_mut()
        {
            let room = crate::handoff::IMPORTED_HISTORY_BYTES.saturating_sub(24);
            first.text = truncate_on_char(first.text.clone(), room);
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

/// The visible text of a `message.content` value, in either harness's shape.
pub(crate) fn extract_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.trim().to_owned(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !matches!(kind, "text" | "input_text" | "output_text") {
                    continue;
                }
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    let text = text.trim();
                    if !text.is_empty() {
                        parts.push(text.to_owned());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
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
pub(crate) fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let text = value?.as_str()?.trim();
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// Reads one bounded line; `None` at end of file, `Some(false)` when the line
/// was too long and its tail was discarded.
pub(crate) fn read_line_bounded(
    reader: &mut impl BufRead,
    out: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    out.clear();
    let mut truncated = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if out.is_empty() {
                None
            } else {
                Some(!truncated)
            });
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if out.len() < MAX_LINE_BYTES {
                let take = (MAX_LINE_BYTES - out.len()).min(position);
                out.extend_from_slice(&available[..take]);
                if take < position {
                    truncated = true;
                }
            } else {
                truncated = true;
            }
            reader.consume(position + 1);
            return Ok(Some(!truncated));
        }
        let length = available.len();
        if out.len() < MAX_LINE_BYTES {
            let take = (MAX_LINE_BYTES - out.len()).min(length);
            out.extend_from_slice(&available[..take]);
            if take < length {
                truncated = true;
            }
        } else {
            truncated = true;
        }
        reader.consume(length);
    }
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

fn is_under_downloads(path: &Path, home: &Path) -> bool {
    let downloads = home.join("Downloads");
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.starts_with(&downloads)
}

/// One accepted transcript in a project group.
#[derive(Clone, Debug)]
struct ConversationFile {
    source: ImportSource,
    path: PathBuf,
    imported_key: String,
    already_imported: bool,
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

/// The outcome of one scan with the per-conversation data the importer needs.
struct ScanOutcome {
    scan: ImportScan,
    groups: BTreeMap<String, Group>,
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
        let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".claude"));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".codex"));
        Self::with_homes(
            journal,
            projects,
            threads,
            host,
            home,
            claude_home,
            codex_home,
        )
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
            Ok(records) => records
                .into_iter()
                .map(|record| record.key)
                .collect::<HashSet<_>>(),
            Err(error) => {
                tracing::warn!(%error, "could not read the imported-source list");
                HashSet::new()
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
        let result = tokio::task::spawn_blocking(move || {
            scan_blocking(&home, &claude_home, &codex_home, &imported, &registered)
        })
        .await;
        match result {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(%error, "the import scan worker panicked");
                ScanOutcome {
                    scan: ImportScan {
                        projects: Vec::new(),
                        warnings: vec!["The import scan could not finish.".to_owned()],
                        scanned_at: Utc::now(),
                    },
                    groups: BTreeMap::new(),
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
            .map(|group| {
                group
                    .files
                    .iter()
                    .filter(|file| !file.already_imported)
                    .count() as u32
            })
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
            journal: Arc::clone(&self.journal),
            projects: Arc::clone(&self.projects),
            threads: Arc::clone(&self.threads),
            host: self.host.clone(),
            events: self.events.clone(),
            warnings: outcome.scan.warnings,
        };
        let state = Arc::clone(&self.state);
        let worker_progress = progress.clone();
        tokio::spawn(async move {
            service.run(selected, worker_progress, state).await;
        });
        Ok(progress)
    }
}

/// The work of one import run.
struct ImportRunner {
    journal: Arc<dyn Journal>,
    projects: Arc<ProjectService>,
    threads: Arc<ThreadManager>,
    host: HostId,
    events: broadcast::Sender<ImportEvent>,
    warnings: Vec<String>,
}

impl ImportRunner {
    async fn run(
        self,
        groups: Vec<Group>,
        progress: ImportProgress,
        state: Arc<Mutex<ImportState>>,
    ) {
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
            self.publish(&progress);
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
                progress.conversations_done += 1;
                if file.already_imported {
                    progress.skipped += 1;
                    self.publish(&progress);
                    continue;
                }
                match self.import_one(&file).await {
                    Ok(Some(thread_id)) => {
                        progress.imported += 1;
                        let record = ImportedSourceRecord {
                            key: file.imported_key.clone(),
                            thread_id,
                            path: file.path.display().to_string(),
                            imported_at: Utc::now(),
                        };
                        if let Err(error) = self.journal.record_imported_source(&record).await {
                            tracing::warn!(%error, "could not record an imported conversation");
                            progress.warnings.push("A thread was created but its source could not be recorded, so a later import may repeat it.".to_owned());
                        }
                    }
                    Ok(None) => progress.skipped += 1,
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
                self.publish(&progress);
            }
            progress.projects_done += 1;
            self.publish(&progress);
        }
        progress.running = false;
        progress.current_project = None;
        progress.finished_at = Some(Utc::now());
        progress.warnings.sort();
        progress.warnings.dedup();
        {
            // Record the finished run before announcing it: a client that sees
            // the final frame can start the next import immediately.
            let mut state = state.lock().await;
            state.running = false;
            state.progress = Some(progress.clone());
        }
        self.publish(&progress);
    }

    /// Parses and creates one thread; `Ok(None)` when the source was skipped.
    async fn import_one(&self, file: &ConversationFile) -> Result<Option<ThreadId>, ImportFailure> {
        let parsed = match file.source {
            ImportSource::ClaudeCode => claude::parse(&file.path),
            ImportSource::Codex => codex::parse(&file.path),
        }
        .map_err(ImportFailure::Skipped)?;
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
            return Ok(None);
        }
        let request = NewImportedThread {
            host: self.host.clone(),
            harness: file.source.harness(),
            workspace_path: parsed.cwd.display().to_string(),
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
            messages,
        };
        let thread_id = self.threads.import_thread(request).await.map_err(|error| {
            tracing::warn!(%error, "could not create an imported thread");
            ImportFailure::Failed
        })?;
        Ok(Some(thread_id))
    }

    fn publish(&self, progress: &ImportProgress) {
        let _ = self.events.send(ImportEvent {
            progress: progress.clone(),
        });
    }
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn scan_blocking(
    home: &Path,
    claude_home: &Path,
    codex_home: &Path,
    imported: &HashSet<String>,
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
    for candidate in candidates {
        if candidate.size == 0 {
            continue;
        }
        if candidate.size > MAX_FILE_BYTES {
            warnings.push("A transcript larger than 4 GiB was skipped.".to_owned());
            continue;
        }
        let peek = match candidate.source {
            ImportSource::ClaudeCode => claude::peek(&candidate.path),
            ImportSource::Codex => codex::peek(&candidate.path),
        };
        let Some(peek) = peek else {
            warnings.push(format!("Could not read {}.", candidate.path.display()));
            continue;
        };
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
        let (key, path, name, git) = match identity {
            Some(git) => (git.key.clone(), git.path.clone(), git.name.clone(), true),
            None => (
                format!("path:{}", peek.cwd.display()),
                peek.cwd.clone(),
                folder_name(&peek.cwd),
                false,
            ),
        };
        let group = groups.entry(key.clone()).or_insert_with(|| Group {
            key,
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
        group.attempts = group.attempts.saturating_add(1);
        if group.attempts > MAX_CONVERSATIONS_PER_PROJECT {
            if !group.limited_count {
                group.limited_count = true;
                warnings.push(format!(
                    "{} has more than {MAX_CONVERSATIONS_PER_PROJECT} conversations; only the newest are offered.",
                    group.name
                ));
            }
            continue;
        }
        if group.bytes.saturating_add(candidate.size) > MAX_PROJECT_BYTES {
            if !group.limited_bytes {
                group.limited_bytes = true;
                warnings.push(format!(
                    "{} holds more than 4 GiB of transcripts; older conversations were not scanned.",
                    group.name
                ));
            }
            continue;
        }
        group.bytes = group.bytes.saturating_add(candidate.size);
        let imported_key = h4rn_protocol::source_key(
            candidate.source,
            peek.session_id.as_deref(),
            &candidate.path.display().to_string(),
        );
        group.files.push(ConversationFile {
            source: candidate.source,
            path: candidate.path,
            already_imported: imported.contains(&imported_key),
            imported_key,
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
    fn a_single_huge_request_is_truncated_to_the_import_ceiling() {
        let mut history = History::new();
        history.push(KeptMessage {
            role: MessageRole::User,
            text: "x".repeat(crate::handoff::IMPORTED_HISTORY_BYTES + 200),
            at: Utc::now(),
        });
        let messages = history.finish().expect("messages");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.len() <= crate::handoff::IMPORTED_HISTORY_BYTES + 4);
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
}
