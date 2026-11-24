//! Harness CLI versions and self-updates.
//!
//! The daemon reports what each harness CLI on this host is installed at, what
//! the npm registry publishes, and how it would update it. Nothing here names a
//! vendor protocol: a harness version is an installation fact, not a harness
//! event, so it lives beside [`crate::HarnessKind`] rather than in
//! `h4rn-harness`'s normalized event vocabulary.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::HarnessKind;

/// How h4rn would install a newer harness CLI.
///
/// A display command is carried for every actionable method so the view never
/// reconstructs product copy, and `args` is present only where h4rn itself runs
/// the command (always without a shell).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessUpdateMethod {
    /// The CLI updates itself (`claude update`, `opencode upgrade`).
    SelfUpdate {
        /// Absolute path of the CLI, or the bare name when detection failed.
        program: String,
        /// Arguments passed as an array, never through a shell.
        args: Vec<String>,
        /// The command as a person would type it.
        command: String,
    },
    /// Reinstall the npm package globally.
    Npm {
        /// Registry package, for example `@openai/codex`.
        package: String,
        /// The command as a person would type it.
        command: String,
    },
    /// Reinstall the package with bun, for a bun-global install.
    Bun {
        /// Registry package, for example `@openai/codex`.
        package: String,
        /// The command as a person would type it.
        command: String,
    },
    /// h4rn will not run this; show the command to the person instead.
    Manual {
        /// The command as a person would type it.
        command: String,
    },
    /// There is no known update source (for example Antigravity).
    Unavailable,
}

/// What one harness adapter can do beyond the common trait.
///
/// Advertised per harness so a client can offer an action only where the
/// adapter really implements it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HarnessCapabilities {
    /// The adapter can take a user message into the turn that is already
    /// running, without starting a new turn.
    #[serde(default)]
    pub steer: bool,
}

/// One harness capability row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HarnessCapabilityEntry {
    /// Which CLI this row describes.
    pub harness: HarnessKind,
    /// What the adapter can do.
    pub capabilities: HarnessCapabilities,
}

/// One harness installation and its available update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HarnessVersionInfo {
    /// Which CLI this row describes.
    pub harness: HarnessKind,
    /// Version the installed CLI reports; `None` when it is not installed.
    pub installed: Option<String>,
    /// Newest version the registry publishes; `None` when unknown or offline.
    pub latest: Option<String>,
    /// Whether `latest` is strictly newer than `installed`.
    pub update_available: bool,
    /// How h4rn would update it.
    pub method: HarnessUpdateMethod,
    /// Absolute path of the detected CLI, for provenance.
    pub path: Option<String>,
}

/// Which output stream one line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum HarnessOutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One step of a harness update, streamed while it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessUpdateEvent {
    /// One line of child-process output.
    Output {
        /// Harness being updated.
        harness: HarnessKind,
        /// Stream the line came from.
        stream: HarnessOutputStream,
        /// The complete line, without its newline.
        line: String,
    },
    /// The update finished and the version was re-detected.
    Finished {
        /// Harness that was updated.
        harness: HarnessKind,
        /// Whether the command exited successfully.
        ok: bool,
        /// A sentence for the person, safe to render verbatim.
        message: String,
        /// Version before the update, when known.
        from_version: Option<String>,
        /// Version after the update, when known.
        to_version: Option<String>,
    },
}
