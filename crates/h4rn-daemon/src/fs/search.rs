//! Streamed workspace search over `ignore` + `grep-searcher`.
//!
//! The walker uses the same ignore stack as the file tree (`.gitignore`,
//! global excludes, hidden files shown), and `grep-searcher` handles line
//! numbers, binary detection and bounded buffers. Results are pushed in bounded
//! batches so a huge hit set cannot stall the socket or allocate the world:
//! the client renders them as they arrive and can dismiss the search.

use std::io;
use std::path::PathBuf;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use h4rn_protocol::FsSearchMatch;
use tokio::sync::mpsc;

use super::paths::FsError;

/// How many matches travel in one batch.
const BATCH: usize = 50;

/// One search request after the service has authorised the root.
#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// Canonical workspace root.
    pub workspace: PathBuf,
    /// Literal text or regular expression.
    pub query: String,
    /// Treat `query` as a regular expression.
    pub regex: bool,
    /// Match case exactly.
    pub case_sensitive: bool,
    /// Optional glob filter.
    pub glob: Option<String>,
    /// Hard result cap.
    pub max_results: usize,
}

/// Builds the matcher and starts the worker thread.
///
/// A malformed regular expression is rejected here, before any work starts, so
/// the client gets an error response rather than an empty result set.
pub fn spawn(
    search_id: &str,
    options: SearchOptions,
) -> Result<mpsc::Receiver<h4rn_protocol::FsSearchBatch>, FsError> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .fixed_strings(!options.regex)
        .build(&options.query)
        .map_err(|error| FsError::Invalid(format!("invalid search pattern: {error}")))?;
    let (tx, rx) = mpsc::channel(32);
    let id = search_id.to_owned();

    std::thread::Builder::new()
        .name("h4rn-fssearch".into())
        .spawn(move || {
            let mut builder = ignore::WalkBuilder::new(&options.workspace);
            builder
                .hidden(false)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .parents(true)
                .filter_entry(|entry| entry.file_name() != ".git");
            if let Some(glob) = options.glob.as_deref() {
                match ignore::overrides::OverrideBuilder::new(&options.workspace).add(glob) {
                    Ok(overrides) => {
                        if let Ok(built) = overrides.build() {
                            builder.overrides(built);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(pattern = glob, %error, "ignoring invalid search glob");
                    }
                }
            }

            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                .binary_detection(BinaryDetection::quit(b'\0'))
                .build();
            let mut batch = Vec::with_capacity(BATCH);
            let mut total = 0usize;
            let mut truncated = false;

            for entry in builder.build() {
                if truncated {
                    break;
                }
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                let path = entry.path();
                let relative = path
                    .strip_prefix(&options.workspace)
                    .unwrap_or(path)
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let mut sink = MatchSink {
                    relative: relative.clone(),
                    query: options.query.clone(),
                    regex: options.regex,
                    case_sensitive: options.case_sensitive,
                    matches: Vec::new(),
                };
                if searcher.search_path(&matcher, path, &mut sink).is_err() {
                    continue;
                }
                for found in sink.matches {
                    if total >= options.max_results {
                        truncated = true;
                        break;
                    }
                    total += 1;
                    batch.push(found);
                    if batch.len() >= BATCH
                        && tx
                            .blocking_send(h4rn_protocol::FsSearchBatch {
                                search_id: id.clone(),
                                results: std::mem::take(&mut batch),
                                done: false,
                                truncated: false,
                            })
                            .is_err()
                    {
                        return;
                    }
                }
            }

            let _ = tx.blocking_send(h4rn_protocol::FsSearchBatch {
                search_id: id,
                results: batch,
                done: true,
                truncated,
            });
        })
        .map_err(|error| FsError::Io(format!("spawn search worker: {error}")))?;

    Ok(rx)
}

struct MatchSink {
    relative: String,
    query: String,
    regex: bool,
    case_sensitive: bool,
    matches: Vec<FsSearchMatch>,
}

impl Sink for MatchSink {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let bytes = mat.bytes();
        let Some(text) = std::str::from_utf8(bytes).ok() else {
            return Ok(true);
        };
        let trimmed = text.trim_end_matches(['\n', '\r']);
        let preview = truncate(trimmed, 240);
        let column = if self.regex {
            1
        } else {
            column_of(trimmed, &self.query, self.case_sensitive)
        };
        self.matches.push(FsSearchMatch {
            path: self.relative.clone(),
            line_number: mat.line_number().unwrap_or(1).min(u32::MAX as u64) as u32,
            column,
            preview,
        });
        // Keep collecting; the worker enforces the global cap.
        Ok(true)
    }
}

fn column_of(line: &str, query: &str, case_sensitive: bool) -> u32 {
    let found = if case_sensitive {
        line.find(query)
    } else {
        line.to_lowercase().find(&query.to_lowercase())
    };
    found
        .map(|index| line[..index].chars().count() as u32 + 1)
        .unwrap_or(1)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}
