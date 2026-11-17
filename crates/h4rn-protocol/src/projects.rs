//! Projects: folders on a host that the sidebar lists even before they have a
//! thread.
//!
//! A project is a host plus an absolute workspace folder. Threads still group
//! by `(host, workspace_path)`; a registered project only adds a name, a
//! creation time and a sidebar row of its own. Adding one can open an
//! existing folder, create a new folder (optionally `git init`), or clone a
//! repository.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::HostId;

/// One registered project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Project {
    /// Stable id.
    pub id: String,
    /// Host that owns the folder.
    pub host: HostId,
    /// Absolute, canonical folder path on the host.
    pub path: String,
    /// Display name; defaults to the folder name.
    pub name: String,
    /// When the project was added.
    pub created_at: DateTime<Utc>,
    /// Whether the folder is a git work tree.
    pub git: bool,
}

/// How to add a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectSource {
    /// Register a folder that already exists.
    Existing {
        /// Folder path; `~` expands to the host user's home.
        path: String,
    },
    /// Create `parent/name`, optionally running `git init` in it.
    NewFolder {
        /// Existing folder to create the project in; `~` expands.
        parent: String,
        /// New folder name (one path segment).
        name: String,
        /// Run `git init` in the new folder.
        #[serde(default)]
        git_init: bool,
    },
    /// Clone a repository into `parent/<name or repository name>`.
    Clone {
        /// Clone URL, or `owner/repo` for GitHub.
        url: String,
        /// Existing folder to clone into; `~` expands.
        parent: String,
        /// Folder name; defaults to the repository name.
        #[serde(default)]
        name: Option<String>,
    },
}

/// One child folder in a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DirectoryEntry {
    /// Folder name.
    pub name: String,
    /// Absolute path.
    pub path: String,
    /// Whether it is a git work tree (has a `.git` entry).
    pub git: bool,
}

/// The folders inside one directory, for picking a project location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DirectoryListing {
    /// Absolute path that was listed.
    pub path: String,
    /// Parent directory, when there is one.
    pub parent: Option<String>,
    /// The host user's home directory, for `~` display.
    pub home: String,
    /// Child folders, sorted by name; hidden folders are left out.
    pub entries: Vec<DirectoryEntry>,
    /// Whether the listed folder is itself a git work tree.
    pub git: bool,
}
