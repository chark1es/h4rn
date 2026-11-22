//! Text generation for the daemon: thread titles written by a model.
//!
//! Two kinds of backend are supported behind one [`TextGeneration`] trait:
//!
//! - an installed coding-agent CLI, run once in an empty temporary directory
//!   with no tools (`claude -p`, or `codex exec`);
//! - a configured text generator, either an OpenAI-compatible chat-completions
//!   API (OpenAI, OpenRouter, Ollama, LM Studio) or Anthropic's messages API.
//!
//! Generator keys live in the OS keychain when one is available and in the
//! transport crate's encrypted `0600` files otherwise (ADR-016), exactly like
//! forge tokens. Nothing that leaves this module carries a key: the protocol
//! reports only whether one exists.
//!
//! The service owns its own JSON files under `text-generation/` in the data
//! directory, written atomically, so a corrupt or missing file never stops the
//! daemon from starting.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use h4rn_harness::HarnessInstall;
use h4rn_protocol::{
    HarnessKind, ProtoError, ProtoErrorCode, TextGenerationSettings, TextGenerator,
    TextGeneratorDraft, TextGeneratorId, TextGeneratorKind, TextGeneratorPatch,
    TextGeneratorTestReport, TitleModelMode, TitleModelSetting,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::create_private_dir;

/// File holding the generator list (never a key).
pub const GENERATORS_FILE: &str = "text-generators.json";
/// File holding the title model choice.
pub const SETTINGS_FILE: &str = "text-generation.json";
/// Directory under the daemon data directory.
pub const DATA_DIR: &str = "text-generation";
/// OS keychain service name for generator keys.
const KEYCHAIN_SERVICE: &str = "h4rn-text-generation";
/// A title request that takes longer than this is abandoned.
pub const TITLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest title the daemon will apply.
pub const TITLE_MAX_CHARS: usize = 60;
/// Output beyond this is a model that ignored the prompt, not a title.
const TITLE_REJECT_CHARS: usize = 240;
/// How much of the first prompt is sent.
const PROMPT_MAX_CHARS: usize = 4_000;
/// How much of an API error body is quoted back to the user.
const ERROR_BODY_MAX_CHARS: usize = 300;

/// Harnesses whose CLI has a one-shot, non-interactive mode h4rn can run.
pub const SUPPORTED_AGENTS: [HarnessKind; 2] = [HarnessKind::ClaudeCode, HarnessKind::Codex];

/// One-shot text generation behind a trait, so later tasks (commit messages,
/// pull request text) can reuse the same service.
#[async_trait]
pub trait TextGeneration: Send + Sync {
    /// Writes a thread title from the first prompt and the workspace name.
    async fn thread_title(
        &self,
        first_prompt: &str,
        workspace: &str,
    ) -> Result<String, TextGenerationError>;
}

/// A text-generation failure at the daemon boundary.
#[derive(Clone, Debug, thiserror::Error)]
pub enum TextGenerationError {
    /// The title model is off.
    #[error("text generation is off")]
    Disabled,
    /// The selected backend is not usable on this host.
    #[error("{0}")]
    NotConfigured(String),
    /// This harness has no one-shot mode h4rn can run.
    #[error("{0} has no non-interactive mode h4rn can run")]
    UnsupportedHarness(String),
    /// The request or stored setting is invalid.
    #[error("{0}")]
    InvalidRequest(String),
    /// A generator or thread id does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The provider rejected the key.
    #[error("the text generator rejected its key")]
    Unauthorized,
    /// The provider refused the request.
    #[error("{0}")]
    Provider(String),
    /// The backend did not answer in time.
    #[error("text generation timed out")]
    Timeout,
    /// The model returned nothing usable.
    #[error("the model returned no title")]
    EmptyOutput,
    /// The model returned a paragraph instead of a title.
    #[error("the model returned more than a title")]
    OversizedOutput,
    /// A local read or write failed.
    #[error("{0}")]
    Local(String),
}

impl TextGenerationError {
    /// Maps the failure onto the protocol error class and a user hint.
    pub fn into_proto(self) -> ProtoError {
        let (code, hint) = match &self {
            Self::Disabled => (
                ProtoErrorCode::Unsupported,
                "Turn title generation back on in Settings → Threads.",
            ),
            Self::NotConfigured(_) => (
                ProtoErrorCode::NotFound,
                "Pick an installed coding agent or add a text generator; the first-prompt title is used until then.",
            ),
            Self::UnsupportedHarness(_) => (
                ProtoErrorCode::Unsupported,
                "Pick Claude Code or Codex, which both have a non-interactive mode.",
            ),
            Self::InvalidRequest(_) => (
                ProtoErrorCode::InvalidRequest,
                "Fix the field the message names and save again.",
            ),
            Self::NotFound(_) => (
                ProtoErrorCode::NotFound,
                "Reload the list; the entry was removed on this host.",
            ),
            Self::Unauthorized => (
                ProtoErrorCode::Unauthorized,
                "Replace the API key for this generator.",
            ),
            Self::Provider(_) => (
                ProtoErrorCode::Internal,
                "Check the model name, base URL and the daemon log.",
            ),
            Self::Timeout => (
                ProtoErrorCode::Internal,
                "The provider did not answer in time; thread titles keep their first line.",
            ),
            Self::EmptyOutput | Self::OversizedOutput => (
                ProtoErrorCode::Internal,
                "The model ignored the title prompt; thread titles keep their first line.",
            ),
            Self::Local(_) => (
                ProtoErrorCode::Internal,
                "Check the daemon data directory and retry.",
            ),
        };
        ProtoError::new(code, self.to_string(), hint)
    }
}

impl From<TextGenerationError> for ProtoError {
    fn from(error: TextGenerationError) -> Self {
        error.into_proto()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct GeneratorsFile {
    #[serde(default)]
    generators: Vec<StoredGenerator>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredGenerator {
    id: TextGeneratorId,
    name: String,
    kind: TextGeneratorKind,
    base_url: String,
    model: String,
}

impl StoredGenerator {
    fn view(&self, has_api_key: bool) -> TextGenerator {
        TextGenerator {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            has_api_key,
        }
    }
}

/// Keychain-first key storage with an encrypted-file fallback (ADR-016).
struct KeyStore {
    dir: PathBuf,
    keychain: bool,
}

struct AgentLocator {
    overrides: HashMap<HarnessKind, PathBuf>,
}

impl AgentLocator {
    fn find(&self, kind: HarnessKind) -> Option<HarnessInstall> {
        if let Some(path) = self.overrides.get(&kind) {
            return Some(HarnessInstall {
                path: path.clone(),
                version: "test".to_owned(),
            });
        }
        match kind {
            HarnessKind::ClaudeCode => h4rn_harness::detect_claude(),
            HarnessKind::Codex => h4rn_harness::detect_codex(),
            _ => None,
        }
    }

    fn first_installed(&self) -> Option<HarnessKind> {
        SUPPORTED_AGENTS
            .into_iter()
            .find(|kind| self.find(*kind).is_some())
    }
}

impl KeyStore {
    fn new(dir: PathBuf, keychain: bool) -> io::Result<Self> {
        create_private_dir(&dir).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self { dir, keychain })
    }

    fn path(&self, id: &TextGeneratorId) -> PathBuf {
        let digest = blake3::hash(id.as_str().as_bytes()).to_hex();
        self.dir.join(format!("{digest}.secret"))
    }

    fn save(&self, id: &TextGeneratorId, key: &str) -> Result<(), TextGenerationError> {
        if self.keychain
            && let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, id.as_str())
            && entry.set_password(key).is_ok()
        {
            return Ok(());
        }
        h4rn_transport::keys::write_secret_file(&self.path(id), key.as_bytes()).map_err(|error| {
            TextGenerationError::Local(format!("store text generator key: {error}"))
        })
    }

    fn load(&self, id: &TextGeneratorId) -> Result<Option<String>, TextGenerationError> {
        if self.keychain
            && let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, id.as_str())
            && let Ok(key) = entry.get_password()
            && !key.is_empty()
        {
            return Ok(Some(key));
        }
        let bytes = h4rn_transport::keys::read_secret_file(&self.path(id)).map_err(|error| {
            TextGenerationError::Local(format!("read text generator key: {error}"))
        })?;
        Ok(bytes.map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned()))
    }

    fn remove(&self, id: &TextGeneratorId) {
        if self.keychain
            && let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, id.as_str())
        {
            let _ = entry.delete_credential();
        }
        if let Err(error) = h4rn_transport::keys::remove_secret_file(&self.path(id)) {
            tracing::warn!(%error, "could not remove a text generator key file");
        }
    }
}

/// The daemon's text-generation service.
pub struct TextGenerationService {
    dir: PathBuf,
    generators_path: PathBuf,
    settings_path: PathBuf,
    generators: Mutex<Vec<StoredGenerator>>,
    settings: Mutex<TextGenerationSettings>,
    keys: KeyStore,
    agents: AgentLocator,
    http: reqwest::Client,
}

impl std::fmt::Debug for TextGenerationService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextGenerationService")
            .field("dir", &self.dir)
            .field("generators", &self.generators())
            .finish_non_exhaustive()
    }
}

impl TextGenerationService {
    /// Opens the service under `data_dir/text-generation`.
    pub fn open(data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = data_dir.as_ref().join(DATA_DIR);
        create_private_dir(&dir)?;
        let keychain = keyring::Entry::store_status().is_ok();
        Self::open_in(&dir, keychain)
    }

    fn open_in(dir: &Path, keychain: bool) -> anyhow::Result<Self> {
        create_private_dir(dir)?;
        let generators_path = dir.join(GENERATORS_FILE);
        let settings_path = dir.join(SETTINGS_FILE);
        let generators: GeneratorsFile = match std::fs::read(&generators_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %generators_path.display(), "ignoring an unreadable text generator list");
                GeneratorsFile::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => GeneratorsFile::default(),
            Err(error) => return Err(error.into()),
        };
        let settings: TextGenerationSettings = match std::fs::read(&settings_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %settings_path.display(), "ignoring unreadable text generation settings");
                TextGenerationSettings::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                TextGenerationSettings::default()
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            generators_path,
            settings_path,
            generators: Mutex::new(generators.generators),
            settings: Mutex::new(settings),
            keys: KeyStore::new(dir.join("keys"), keychain)?,
            agents: AgentLocator {
                overrides: HashMap::new(),
            },
            http: reqwest::Client::builder()
                .timeout(TITLE_TIMEOUT)
                .build()
                .map_err(|error| {
                    anyhow::anyhow!("build the text generation HTTP client: {error}")
                })?,
        })
    }

    /// Points a harness at an explicit executable (tests and fixtures).
    pub fn with_agent_install(mut self, harness: HarnessKind, path: impl Into<PathBuf>) -> Self {
        self.agents.overrides.insert(harness, path.into());
        self
    }

    /// Uses the encrypted-file key backend instead of the keychain (tests).
    pub fn with_file_keys(mut self) -> anyhow::Result<Self> {
        self.keys = KeyStore::new(self.dir.join("keys"), false)?;
        Ok(self)
    }

    /// Configured generators, oldest first. Keys are never included.
    pub fn generators(&self) -> Vec<TextGenerator> {
        let generators = self
            .generators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        generators
            .iter()
            .map(|generator| {
                let has_api_key = self.keys.load(&generator.id).ok().flatten().is_some();
                generator.view(has_api_key)
            })
            .collect()
    }

    /// One configured generator.
    pub fn generator(&self, id: &TextGeneratorId) -> Option<TextGenerator> {
        self.generators()
            .into_iter()
            .find(|generator| &generator.id == id)
    }

    /// Adds a generator and stores its key.
    pub fn add_generator(
        &self,
        draft: TextGeneratorDraft,
    ) -> Result<TextGenerator, TextGenerationError> {
        let generator = valid_generator(
            TextGeneratorId::new(format!("tg_{}", uuid::Uuid::now_v7().simple())),
            draft.name,
            draft.kind,
            draft.base_url,
            draft.model,
        )?;
        if let Some(key) = draft
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            self.keys.save(&generator.id, key)?;
        }
        {
            let mut generators = self
                .generators
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            generators.push(generator.clone());
        }
        self.save_generators()?;
        Ok(generator.view(self.keys.load(&generator.id)?.is_some()))
    }

    /// Changes one generator. `None` leaves a field alone.
    pub fn update_generator(
        &self,
        id: &TextGeneratorId,
        patch: TextGeneratorPatch,
    ) -> Result<TextGenerator, TextGenerationError> {
        let mut generators = self
            .generators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = generators
            .iter()
            .position(|generator| &generator.id == id)
            .ok_or_else(|| TextGenerationError::NotFound("no such text generator".into()))?;
        let current = generators[index].clone();
        let replacement = valid_generator(
            current.id.clone(),
            patch.name.unwrap_or(current.name),
            current.kind,
            patch.base_url.unwrap_or(current.base_url),
            patch.model.unwrap_or(current.model),
        )?;
        generators[index] = replacement.clone();
        drop(generators);

        if patch.clear_api_key {
            self.keys.remove(id);
        } else if let Some(key) = patch
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            self.keys.save(id, key)?;
        }
        self.save_generators()?;
        Ok(replacement.view(self.keys.load(id)?.is_some()))
    }

    /// Removes a generator, its key and any title setting that named it.
    pub fn remove_generator(&self, id: &TextGeneratorId) -> Result<(), TextGenerationError> {
        let mut generators = self
            .generators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = generators.len();
        generators.retain(|generator| &generator.id != id);
        if generators.len() == before {
            return Err(TextGenerationError::NotFound(
                "no such text generator".into(),
            ));
        }
        drop(generators);
        self.keys.remove(id);
        self.save_generators()?;

        let mut settings = self
            .settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if settings.title.generator_id.as_ref() == Some(id) {
            settings.title = TitleModelSetting::default();
            drop(settings);
            self.save_settings()?;
        }
        Ok(())
    }

    /// Which model writes thread titles.
    pub fn settings(&self) -> TextGenerationSettings {
        self.settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the title model after checking it can be satisfied.
    pub fn set_settings(
        &self,
        settings: TextGenerationSettings,
    ) -> Result<TextGenerationSettings, TextGenerationError> {
        let title = &settings.title;
        match title.mode {
            TitleModelMode::Off => {}
            TitleModelMode::CodingAgent => {
                if let Some(harness) = title.harness
                    && !SUPPORTED_AGENTS.contains(&harness)
                {
                    return Err(TextGenerationError::UnsupportedHarness(
                        harness_label(harness).to_owned(),
                    ));
                }
            }
            TitleModelMode::TextGenerator => {
                let id = title.generator_id.as_ref().ok_or_else(|| {
                    TextGenerationError::InvalidRequest(
                        "pick a text generator for thread titles".into(),
                    )
                })?;
                let generator = self.generator(id).ok_or_else(|| {
                    TextGenerationError::NotFound("no such text generator".into())
                })?;
                if title
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .is_none()
                    && generator.model.trim().is_empty()
                {
                    return Err(TextGenerationError::InvalidRequest(
                        "a text generator needs a model".into(),
                    ));
                }
            }
        }
        *self
            .settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings.clone();
        self.save_settings()?;
        Ok(settings)
    }

    /// Sends a tiny prompt through one generator and reports the honest
    /// outcome, including the provider's own message.
    pub async fn test_generator(
        &self,
        id: &TextGeneratorId,
        model: Option<&str>,
    ) -> Result<TextGeneratorTestReport, TextGenerationError> {
        let generator = self
            .generators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|generator| &generator.id == id)
            .cloned()
            .ok_or_else(|| TextGenerationError::NotFound("no such text generator".into()))?;
        let generator = match model.map(str::trim).filter(|model| !model.is_empty()) {
            Some(model) => StoredGenerator {
                model: model.to_owned(),
                ..generator
            },
            None => generator,
        };
        let key = self.keys.load(id)?;
        let started = Instant::now();
        let result = self
            .call_generator(&generator, key.as_deref(), "Reply with the single word: ok")
            .await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(match result {
            Ok(reply) => TextGeneratorTestReport {
                ok: true,
                latency_ms,
                reply: Some(sanitize_title(&reply).unwrap_or_else(|_| reply.trim().to_owned())),
                error: None,
                hint: None,
            },
            Err(error) => {
                let message = match &error {
                    TextGenerationError::Provider(message) => message.clone(),
                    other => other.to_string(),
                };
                let hint = error.into_proto().hint;
                TextGeneratorTestReport {
                    ok: false,
                    latency_ms,
                    reply: None,
                    error: Some(message),
                    hint,
                }
            }
        })
    }

    fn save_generators(&self) -> Result<(), TextGenerationError> {
        let file = GeneratorsFile {
            generators: self
                .generators
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        };
        write_json(&self.generators_path, &file)
    }

    fn save_settings(&self) -> Result<(), TextGenerationError> {
        let settings = self.settings();
        write_json(&self.settings_path, &settings)
    }

    async fn run_agent(
        &self,
        harness: HarnessKind,
        prompt: &str,
        workspace: &str,
    ) -> Result<String, TextGenerationError> {
        let install = self.agents.find(harness).ok_or_else(|| {
            TextGenerationError::NotConfigured(format!(
                "{} is not installed on this host",
                harness_label(harness)
            ))
        })?;
        let scratch = self
            .dir
            .join(format!("run-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&scratch).map_err(|error| {
            TextGenerationError::Local(format!("create a title workspace: {error}"))
        })?;

        let result = self
            .run_agent_in(
                &install.path,
                harness,
                &scratch,
                title_prompt(prompt, workspace),
            )
            .await;
        if let Err(error) = std::fs::remove_dir_all(&scratch) {
            tracing::debug!(%error, "could not remove a text generation workspace");
        }
        result
    }

    async fn run_agent_in(
        &self,
        program: &Path,
        harness: HarnessKind,
        cwd: &Path,
        prompt: String,
    ) -> Result<String, TextGenerationError> {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match harness {
            HarnessKind::ClaudeCode => {
                // `--tools ""` keeps it from reading anything; `dontAsk` stops a
                // permission prompt from waiting on a person who is not there.
                command.args([
                    "-p",
                    "--output-format",
                    "text",
                    "--model",
                    "haiku",
                    "--tools",
                    "",
                    "--disable-slash-commands",
                    "--strict-mcp-config",
                    "--permission-mode",
                    "dontAsk",
                ]);
            }
            HarnessKind::Codex => {
                command.args([
                    "exec",
                    "--ephemeral",
                    "--skip-git-repo-check",
                    "-s",
                    "read-only",
                    "-",
                ]);
            }
            other => {
                return Err(TextGenerationError::UnsupportedHarness(
                    harness_label(other).to_owned(),
                ));
            }
        }
        let mut child = command.spawn().map_err(|error| {
            TextGenerationError::Local(format!("start the coding agent: {error}"))
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await.map_err(|error| {
                TextGenerationError::Local(format!("write the title prompt: {error}"))
            })?;
        }
        let output = tokio::time::timeout(TITLE_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| TextGenerationError::Timeout)?
            .map_err(|error| {
                TextGenerationError::Local(format!("read the coding agent: {error}"))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = excerpt(stderr.trim());
            return Err(TextGenerationError::Provider(if detail.is_empty() {
                format!("{} exited with {}", harness_label(harness), output.status)
            } else {
                format!("{} failed: {detail}", harness_label(harness))
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn call_generator(
        &self,
        generator: &StoredGenerator,
        key: Option<&str>,
        prompt: &str,
    ) -> Result<String, TextGenerationError> {
        let base = generator.base_url.trim_end_matches('/');
        let (url, body) = match generator.kind {
            TextGeneratorKind::OpenAiCompatible => (
                format!("{base}/chat/completions"),
                serde_json::json!({
                    "model": generator.model,
                    "messages": [
                        { "role": "system", "content": TITLE_SYSTEM_PROMPT },
                        { "role": "user", "content": prompt },
                    ],
                    "max_tokens": 64,
                    "temperature": 0.2,
                    "stream": false,
                }),
            ),
            TextGeneratorKind::Anthropic => (
                if base.ends_with("/v1") {
                    format!("{base}/messages")
                } else {
                    format!("{base}/v1/messages")
                },
                serde_json::json!({
                    "model": generator.model,
                    "max_tokens": 64,
                    "system": TITLE_SYSTEM_PROMPT,
                    "messages": [{ "role": "user", "content": prompt }],
                }),
            ),
        };
        let mut request = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
            request = match generator.kind {
                TextGeneratorKind::OpenAiCompatible => request.bearer_auth(key.trim()),
                TextGeneratorKind::Anthropic => request
                    .header("x-api-key", key.trim())
                    .header("anthropic-version", "2023-06-01"),
            };
        } else if generator.kind == TextGeneratorKind::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = tokio::time::timeout(TITLE_TIMEOUT, request.send())
            .await
            .map_err(|_| TextGenerationError::Timeout)?
            .map_err(|error| {
                TextGenerationError::Provider(format!("could not reach the provider: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            TextGenerationError::Provider(format!("read the provider reply: {error}"))
        })?;
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(TextGenerationError::Unauthorized);
            }
            return Err(TextGenerationError::Provider(format!(
                "the provider answered {status}: {}",
                excerpt(body.trim())
            )));
        }
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
            TextGenerationError::Provider(format!("the provider reply was not JSON: {error}"))
        })?;
        let text = match generator.kind {
            TextGeneratorKind::OpenAiCompatible => parsed
                .get("choices")
                .and_then(|choices| choices.get(0))
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_str())
                .map(str::to_owned),
            TextGeneratorKind::Anthropic => {
                let text = parsed
                    .get("content")
                    .and_then(|content| content.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(|kind| kind.as_str()) == Some("text")
                            })
                            .filter_map(|block| block.get("text").and_then(|text| text.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                (!text.trim().is_empty()).then_some(text)
            }
        };
        text.ok_or(TextGenerationError::EmptyOutput)
    }
}

const TITLE_SYSTEM_PROMPT: &str = "You write one short title for a coding thread. Reply with the title only: plain text, one line, at most 60 characters, no quotes, no markdown, no explanation.";

#[async_trait]
impl TextGeneration for TextGenerationService {
    async fn thread_title(
        &self,
        first_prompt: &str,
        workspace: &str,
    ) -> Result<String, TextGenerationError> {
        let title = self.settings().title;
        let raw = match title.mode {
            TitleModelMode::Off => return Err(TextGenerationError::Disabled),
            TitleModelMode::CodingAgent => {
                let harness = title
                    .harness
                    .or_else(|| self.agents.first_installed())
                    .ok_or_else(|| {
                        TextGenerationError::NotConfigured(
                            "no coding agent with a one-shot mode is installed".into(),
                        )
                    })?;
                if !SUPPORTED_AGENTS.contains(&harness) {
                    return Err(TextGenerationError::UnsupportedHarness(
                        harness_label(harness).to_owned(),
                    ));
                }
                self.run_agent(harness, &title_prompt(first_prompt, workspace), workspace)
                    .await?
            }
            TitleModelMode::TextGenerator => {
                let id = title.generator_id.clone().ok_or_else(|| {
                    TextGenerationError::NotConfigured("no text generator is selected".into())
                })?;
                let generator = self
                    .generators
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .find(|generator| generator.id == id)
                    .cloned()
                    .ok_or_else(|| {
                        TextGenerationError::NotConfigured(
                            "the selected text generator is gone".into(),
                        )
                    })?;
                let override_model = title
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty());
                let generator = match override_model {
                    Some(model) => StoredGenerator {
                        model: model.to_owned(),
                        ..generator
                    },
                    None => generator,
                };
                let key = self.keys.load(&generator.id)?;
                self.call_generator(
                    &generator,
                    key.as_deref(),
                    &title_prompt(first_prompt, workspace),
                )
                .await?
            }
        };
        sanitize_title(&raw)
    }
}

/// The exact prompt a title backend receives.
pub fn title_prompt(first_prompt: &str, workspace: &str) -> String {
    let trimmed = first_prompt.trim();
    let prompt: String = trimmed.chars().take(PROMPT_MAX_CHARS).collect();
    let workspace = std::path::Path::new(workspace)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| workspace.trim().to_owned());
    format!(
        "Workspace: {workspace}\n\nFirst prompt:\n{prompt}\n\nThe title names this work. Reply with the title only."
    )
}

fn harness_label(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::ClaudeCode => "Claude Code",
        HarnessKind::Codex => "Codex",
        HarnessKind::Opencode => "OpenCode",
        HarnessKind::Antigravity => "Antigravity",
    }
}

fn valid_generator(
    id: TextGeneratorId,
    name: String,
    kind: TextGeneratorKind,
    base_url: String,
    model: String,
) -> Result<StoredGenerator, TextGenerationError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(TextGenerationError::InvalidRequest(
            "a text generator needs a name".into(),
        ));
    }
    if name.chars().count() > 80 {
        return Err(TextGenerationError::InvalidRequest(
            "a text generator name is at most 80 characters".into(),
        ));
    }
    let base_url = validate_base_url(&base_url)?;
    let model = model.trim().to_owned();
    if model.is_empty() {
        return Err(TextGenerationError::InvalidRequest(
            "a text generator needs a model".into(),
        ));
    }
    Ok(StoredGenerator {
        id,
        name,
        kind,
        base_url,
        model,
    })
}

/// Validates a generator base URL: https anywhere, http only on loopback.
pub fn validate_base_url(value: &str) -> Result<String, TextGenerationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(TextGenerationError::InvalidRequest(
            "a text generator needs a base URL".into(),
        ));
    }
    let parsed = url::Url::parse(trimmed).map_err(|error| {
        TextGenerationError::InvalidRequest(format!("invalid base URL {trimmed:?}: {error}"))
    })?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            if !host_is_loopback(&parsed) {
                return Err(TextGenerationError::InvalidRequest(
                    "plain http is only allowed for a loopback base URL (localhost, 127.0.0.1 or ::1)"
                        .into(),
                ));
            }
        }
        other => {
            return Err(TextGenerationError::InvalidRequest(format!(
                "base URL must use https, or http on loopback; {other:?} is not supported"
            )));
        }
    }
    if parsed.host_str().is_none() {
        return Err(TextGenerationError::InvalidRequest(
            "the base URL has no host".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(TextGenerationError::InvalidRequest(
            "the base URL must not carry a query or fragment".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(TextGenerationError::InvalidRequest(
            "put the API key in the key field, not in the base URL".into(),
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

fn host_is_loopback(parsed: &url::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// Reduces a model reply to one plain title, or explains why it is not one.
pub fn sanitize_title(raw: &str) -> Result<String, TextGenerationError> {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or(TextGenerationError::EmptyOutput)?;
    let mut line = line.to_owned();
    if let Some(rest) = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
    {
        line = rest.trim().to_owned();
    }
    loop {
        let trimmed = strip_wrapping_quotes(&line);
        if trimmed.len() == line.len() {
            break;
        }
        line = trimmed.trim().to_owned();
    }
    let line = line.trim_end_matches(['.', ';', ':']).trim().to_owned();
    if line.is_empty() {
        return Err(TextGenerationError::EmptyOutput);
    }
    if line.chars().count() > TITLE_REJECT_CHARS {
        return Err(TextGenerationError::OversizedOutput);
    }
    let title: String = line.chars().take(TITLE_MAX_CHARS).collect();
    let title = title.trim().to_owned();
    if title.is_empty() {
        return Err(TextGenerationError::EmptyOutput);
    }
    Ok(title)
}

fn strip_wrapping_quotes(value: &str) -> String {
    for (open, close) in [('"', '"'), ('\'', '\''), ('`', '`'), ('“', '”'), ('‘', '’')] {
        if value.starts_with(open) && value.ends_with(close) && value.chars().count() > 1 {
            return value
                .trim_start_matches(open)
                .trim_end_matches(close)
                .to_owned();
        }
    }
    value.to_owned()
}

fn excerpt(value: &str) -> String {
    let value = value.trim();
    let mut excerpt: String = value.chars().take(ERROR_BODY_MAX_CHARS).collect();
    if value.chars().count() > ERROR_BODY_MAX_CHARS {
        excerpt.push('…');
    }
    excerpt
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), TextGenerationError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        TextGenerationError::Local(format!("serialise {}: {error}", path.display()))
    })?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| {
        TextGenerationError::Local(format!("write {}: {error}", temporary.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&temporary)
            .map_err(|error| {
                TextGenerationError::Local(format!("stat {}: {error}", temporary.display()))
            })?
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&temporary, permissions).map_err(|error| {
            TextGenerationError::Local(format!("protect {}: {error}", temporary.display()))
        })?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| TextGenerationError::Local(format!("replace {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, TextGenerationService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = TextGenerationService::open_in(dir.path(), false)
            .expect("open")
            .with_file_keys()
            .expect("file keys");
        (dir, service)
    }

    fn draft(kind: TextGeneratorKind, base_url: &str, api_key: Option<&str>) -> TextGeneratorDraft {
        TextGeneratorDraft {
            name: "Test generator".into(),
            kind,
            base_url: base_url.into(),
            model: "tiny-model".into(),
            api_key: api_key.map(str::to_owned),
        }
    }

    #[test]
    fn titles_are_one_plain_line_with_quotes_stripped_and_a_cap() {
        assert_eq!(
            sanitize_title("  Fix the flaky auth test\nbecause it fails\n").unwrap(),
            "Fix the flaky auth test"
        );
        assert_eq!(
            sanitize_title("\"A quoted title\"").unwrap(),
            "A quoted title"
        );
        assert_eq!(sanitize_title("“Smart quotes”").unwrap(), "Smart quotes");
        assert_eq!(
            sanitize_title("Title: Strip the label.").unwrap(),
            "Strip the label"
        );
        let long = "a".repeat(TITLE_MAX_CHARS + 20);
        assert_eq!(sanitize_title(&long).unwrap(), "a".repeat(TITLE_MAX_CHARS));
        assert!(matches!(
            sanitize_title(""),
            Err(TextGenerationError::EmptyOutput)
        ));
        assert!(matches!(
            sanitize_title("\n  \n"),
            Err(TextGenerationError::EmptyOutput)
        ));
        assert!(matches!(
            sanitize_title(&"word ".repeat(TITLE_REJECT_CHARS)),
            Err(TextGenerationError::OversizedOutput)
        ));
    }

    #[test]
    fn generator_writes_need_admin_scope_and_listing_needs_view() {
        use h4rn_protocol::{ClientRequest, PairScope};

        assert_eq!(
            ClientRequest::ListTextGenerators.required_scope(),
            PairScope::View
        );
        assert_eq!(
            ClientRequest::GetTextGenerationSettings.required_scope(),
            PairScope::View
        );
        assert_eq!(
            ClientRequest::AddTextGenerator {
                generator: draft(
                    TextGeneratorKind::OpenAiCompatible,
                    "https://example.com/v1",
                    Some("sk-test"),
                ),
            }
            .required_scope(),
            PairScope::Admin
        );
        assert_eq!(
            ClientRequest::TestTextGenerator {
                generator_id: TextGeneratorId::new("tg_1"),
                model: None,
            }
            .required_scope(),
            PairScope::Admin
        );
        assert_eq!(
            ClientRequest::RenameThread {
                thread_id: h4rn_protocol::ThreadId::new("th_1"),
                title: "mine".into(),
            }
            .required_scope(),
            PairScope::Approve
        );
    }

    #[test]
    fn base_urls_require_https_except_on_loopback() {
        assert_eq!(
            validate_base_url("https://api.openai.com/v1/").unwrap(),
            "https://api.openai.com/v1"
        );
        assert!(validate_base_url("http://127.0.0.1:11434/v1").is_ok());
        assert!(validate_base_url("http://localhost:1234/v1").is_ok());
        assert!(validate_base_url("http://[::1]:1234/v1").is_ok());
        let error = validate_base_url("http://example.com/v1").unwrap_err();
        assert!(error.to_string().contains("loopback"));
        assert!(validate_base_url("ftp://example.com").is_err());
        assert!(validate_base_url("https://example.com/v1?x=1").is_err());
        assert!(validate_base_url("https://user:pass@example.com/v1").is_err());
        assert!(validate_base_url("  ").is_err());
    }

    #[test]
    fn generator_views_never_carry_a_key_either_as_a_field_or_a_string() {
        let (_dir, service) = service();
        let generator = service
            .add_generator(draft(
                TextGeneratorKind::OpenAiCompatible,
                "https://api.openai.com/v1",
                Some("sk-must-not-leak"),
            ))
            .expect("add");
        assert!(generator.has_api_key);
        let listed = service.generators();
        assert_eq!(listed.len(), 1);
        let rendered = serde_json::to_string(&listed).expect("serialise");
        assert!(!rendered.contains("sk-must-not-leak"));
        assert!(!rendered.contains("\"api_key\":"));
    }

    #[test]
    fn settings_refuse_a_generator_that_does_not_exist_and_an_unsupported_agent() {
        let (_dir, service) = service();
        let error = service
            .set_settings(TextGenerationSettings {
                title: TitleModelSetting {
                    mode: TitleModelMode::TextGenerator,
                    harness: None,
                    generator_id: Some(TextGeneratorId::new("tg_missing")),
                    model: None,
                },
            })
            .unwrap_err();
        assert!(matches!(error, TextGenerationError::NotFound(_)));
        let error = service
            .set_settings(TextGenerationSettings {
                title: TitleModelSetting {
                    mode: TitleModelMode::CodingAgent,
                    harness: Some(HarnessKind::Opencode),
                    generator_id: None,
                    model: None,
                },
            })
            .unwrap_err();
        assert!(matches!(
            error,
            TextGenerationError::UnsupportedHarness(message) if message == "OpenCode"
        ));
    }

    #[test]
    fn removing_the_selected_generator_returns_titles_to_the_default_mode() {
        let (_dir, service) = service();
        let generator = service
            .add_generator(draft(
                TextGeneratorKind::OpenAiCompatible,
                "https://example.com/v1",
                None,
            ))
            .expect("add");
        service
            .set_settings(TextGenerationSettings {
                title: TitleModelSetting {
                    mode: TitleModelMode::TextGenerator,
                    harness: None,
                    generator_id: Some(generator.id.clone()),
                    model: None,
                },
            })
            .expect("select");
        service.remove_generator(&generator.id).expect("remove");
        assert_eq!(service.settings().title.mode, TitleModelMode::CodingAgent);
    }

    #[tokio::test]
    async fn openai_compatible_requests_have_the_expected_shape() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "model": "tiny-model",
                "stream": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": "\"Ship the title\"" } }]
            })))
            .mount(&server)
            .await;

        let (_dir, service) = service();
        let generator = service
            .add_generator(draft(
                TextGeneratorKind::OpenAiCompatible,
                &format!("{}/v1", server.uri()),
                Some("sk-test"),
            ))
            .expect("add");
        let report = service
            .test_generator(&generator.id, None)
            .await
            .expect("test");
        assert!(report.ok, "{report:?}");
        assert_eq!(report.reply.as_deref(), Some("Ship the title"));

        let requests = server.received_requests().await.expect("requests");
        let body: serde_json::Value = requests[0].body_json().expect("json body");
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("ok")
        );
    }

    #[tokio::test]
    async fn anthropic_requests_use_the_messages_path_and_header() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "model": "tiny-model",
                "max_tokens": 64,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{ "type": "text", "text": "Anthropic title" }]
            })))
            .mount(&server)
            .await;

        let (_dir, service) = service();
        let generator = service
            .add_generator(draft(
                TextGeneratorKind::Anthropic,
                &server.uri(),
                Some("sk-ant-test"),
            ))
            .expect("add");
        let report = service
            .test_generator(&generator.id, None)
            .await
            .expect("test");
        assert!(report.ok, "{report:?}");
        assert_eq!(report.reply.as_deref(), Some("Anthropic title"));
    }

    #[tokio::test]
    async fn an_unauthorized_provider_is_reported_honestly_without_the_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key sk-secret"))
            .mount(&server)
            .await;

        let (_dir, service) = service();
        let generator = service
            .add_generator(draft(
                TextGeneratorKind::OpenAiCompatible,
                &format!("{}/v1", server.uri()),
                Some("sk-secret"),
            ))
            .expect("add");
        let report = service
            .test_generator(&generator.id, None)
            .await
            .expect("test");
        assert!(!report.ok);
        assert!(report.error.unwrap().contains("rejected"));
    }

    #[tokio::test]
    async fn an_agent_fixture_supplies_a_title() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-agent");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\nprintf 'Generated by the fixture\\n'\n",
        )
        .expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let service = TextGenerationService::open_in(dir.path().join("data").as_path(), false)
            .expect("open")
            .with_file_keys()
            .expect("file keys")
            .with_agent_install(HarnessKind::ClaudeCode, &script);
        service
            .set_settings(TextGenerationSettings {
                title: TitleModelSetting {
                    mode: TitleModelMode::CodingAgent,
                    harness: Some(HarnessKind::ClaudeCode),
                    generator_id: None,
                    model: None,
                },
            })
            .expect("settings");
        assert_eq!(
            service
                .thread_title("Fix the build", "/tmp/repo")
                .await
                .unwrap(),
            "Generated by the fixture"
        );
    }
}
