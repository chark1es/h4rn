//! Registered projects: folders the sidebar lists before their first thread.
//!
//! The list is a small JSON file in the data directory, written atomically.
//! Adding a project can open an existing folder, create a new one (optionally
//! running `git init`), or clone a repository. Every git call runs without a
//! shell, with arguments as an array and `--` before user-supplied values, so
//! a URL or folder name can never become an option.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use h4rn_protocol::{
    DirectoryEntry, DirectoryListing, HostId, ProjectSource, ProtoError, ProtoErrorCode,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;

pub use h4rn_protocol::Project;

const FILE: &str = "projects.json";
/// A clone that has not finished in this long is stopped.
const CLONE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Folders listed per directory; enough for any picker.
const MAX_ENTRIES: usize = 500;

#[derive(Default, Serialize, Deserialize)]
struct ProjectsFile {
    projects: Vec<Project>,
}

/// Owns `projects.json` for this host.
pub struct ProjectService {
    path: PathBuf,
    host: HostId,
    home: PathBuf,
    projects: Mutex<Vec<Project>>,
    fs: Option<Arc<crate::fs::FsService>>,
}

fn invalid(message: impl Into<String>, hint: impl Into<String>) -> ProtoError {
    ProtoError::new(ProtoErrorCode::InvalidRequest, message, hint)
}

fn internal(message: impl Into<String>) -> ProtoError {
    ProtoError::plain(ProtoErrorCode::Internal, message)
}

impl ProjectService {
    /// Loads the list from `data_dir`, treating a missing file as empty.
    pub fn open(data_dir: &Path, host: HostId) -> anyhow::Result<Self> {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self::open_with_home(data_dir, host, home)
    }

    /// Like [`Self::open`] with an explicit home directory (tests).
    pub fn open_with_home(data_dir: &Path, host: HostId, home: PathBuf) -> anyhow::Result<Self> {
        let path = data_dir.join(FILE);
        let file: ProjectsFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "ignoring an unreadable projects file");
                ProjectsFile::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProjectsFile::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            host,
            home,
            projects: Mutex::new(file.projects),
            fs: None,
        })
    }

    /// Lets the IDE browse every registered project folder.
    pub fn with_fs(mut self, fs: Arc<crate::fs::FsService>) -> Self {
        Self::authorise_all(&fs, &self.projects.get_mut().clone());
        self.fs = Some(fs);
        self
    }

    fn authorise_all(fs: &crate::fs::FsService, projects: &[Project]) {
        for project in projects {
            if let Err(error) = fs.allow_root(&project.path) {
                tracing::warn!(path = %project.path, %error, "skipping an unreadable project folder");
            }
        }
    }

    /// The host this service answers for.
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// Registered projects, oldest first.
    pub async fn list(&self) -> Vec<Project> {
        self.projects.lock().await.clone()
    }

    /// Adds a project, creating or cloning its folder first when asked.
    pub async fn add(
        &self,
        source: ProjectSource,
        name: Option<String>,
    ) -> Result<Project, ProtoError> {
        let folder = match source {
            ProjectSource::Existing { path } => {
                let path = self.expand(&path)?;
                if !path.is_dir() {
                    return Err(invalid(
                        format!("{} is not a folder", path.display()),
                        "Pick an existing folder, or choose New folder to create one.",
                    ));
                }
                path
            }
            ProjectSource::NewFolder {
                parent,
                name: folder,
                git_init,
            } => {
                let parent = self.existing_parent(&parent)?;
                let folder = segment(&folder)?;
                let path = parent.join(folder);
                match std::fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        if !is_empty_dir(&path) {
                            return Err(invalid(
                                format!("{} already exists and is not empty", path.display()),
                                "Choose another name, or use Open folder for an existing project.",
                            ));
                        }
                    }
                    Err(error) => {
                        return Err(internal(format!(
                            "could not create {}: {error}",
                            path.display()
                        )));
                    }
                }
                if git_init {
                    git(&path, &["init", "--quiet"], Duration::from_secs(30)).await?;
                }
                path
            }
            ProjectSource::Clone {
                url,
                parent,
                name: folder,
            } => {
                let url = clone_url(&url)?;
                let parent = self.existing_parent(&parent)?;
                let folder = match folder.filter(|value| !value.trim().is_empty()) {
                    Some(folder) => segment(&folder)?.to_owned(),
                    None => repo_name(&url).ok_or_else(|| {
                        invalid(
                            "could not tell the repository name from the URL",
                            "Type a folder name for the clone.",
                        )
                    })?,
                };
                let path = parent.join(&folder);
                if path.exists() && !is_empty_dir(&path) {
                    return Err(invalid(
                        format!("{} already exists", path.display()),
                        "Choose another folder name, or open the existing folder instead.",
                    ));
                }
                let target = path.display().to_string();
                git(
                    &parent,
                    &["clone", "--quiet", "--", &url, &target],
                    CLONE_TIMEOUT,
                )
                .await?;
                path
            }
        };
        let path = std::fs::canonicalize(&folder)
            .map_err(|error| internal(format!("could not open {}: {error}", folder.display())))?;
        self.register(path, name).await
    }

    async fn register(&self, path: PathBuf, name: Option<String>) -> Result<Project, ProtoError> {
        let path_text = path.display().to_string();
        let mut projects = self.projects.lock().await;
        if let Some(existing) = projects.iter().find(|project| project.path == path_text) {
            return Ok(existing.clone());
        }
        let project = Project {
            id: format!("prj_{}", uuid::Uuid::now_v7().simple()),
            host: self.host.clone(),
            name: name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| folder_name(&path)),
            path: path_text,
            created_at: Utc::now(),
            git: path.join(".git").exists(),
        };
        projects.push(project.clone());
        self.save(&projects)?;
        drop(projects);
        if let Some(fs) = &self.fs
            && let Err(error) = fs.allow_root(&project.path)
        {
            tracing::warn!(path = %project.path, %error, "the IDE cannot browse the new project");
        }
        Ok(project)
    }

    /// Renames one project.
    pub async fn rename(&self, project_id: &str, name: &str) -> Result<Project, ProtoError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("a project needs a name", "Type a name."));
        }
        let mut projects = self.projects.lock().await;
        let project = projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .ok_or_else(|| not_found(project_id))?;
        project.name = name.to_owned();
        let project = project.clone();
        self.save(&projects)?;
        Ok(project)
    }

    /// Removes one project from the list. The folder is kept.
    pub async fn remove(&self, project_id: &str) -> Result<(), ProtoError> {
        let mut projects = self.projects.lock().await;
        let before = projects.len();
        projects.retain(|project| project.id != project_id);
        if projects.len() == before {
            return Err(not_found(project_id));
        }
        self.save(&projects)
    }

    /// Lists the folders inside one directory.
    pub fn browse(&self, path: Option<&str>) -> Result<DirectoryListing, ProtoError> {
        let path = match path.map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => self.expand(path)?,
            None => self.home.clone(),
        };
        let path = std::fs::canonicalize(&path).map_err(|error| {
            invalid(
                format!("cannot open {}: {error}", path.display()),
                "Check the path, or start from your home folder.",
            )
        })?;
        if !path.is_dir() {
            return Err(invalid(
                format!("{} is not a folder", path.display()),
                "Pick a folder.",
            ));
        }
        let mut entries = Vec::new();
        let reader = std::fs::read_dir(&path).map_err(|error| {
            invalid(
                format!("cannot read {}: {error}", path.display()),
                "h4rn needs read access to this folder.",
            )
        })?;
        for entry in reader.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            // Follow symlinks so a linked code folder still shows up.
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            entries.push(DirectoryEntry {
                git: child.join(".git").exists(),
                path: child.display().to_string(),
                name,
            });
        }
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        entries.truncate(MAX_ENTRIES);
        Ok(DirectoryListing {
            git: path.join(".git").exists(),
            parent: path.parent().map(|parent| parent.display().to_string()),
            home: self.home.display().to_string(),
            path: path.display().to_string(),
            entries,
        })
    }

    fn existing_parent(&self, parent: &str) -> Result<PathBuf, ProtoError> {
        let parent = self.expand(parent)?;
        if !parent.is_dir() {
            return Err(invalid(
                format!("{} does not exist", parent.display()),
                "Pick an existing folder to create the project in.",
            ));
        }
        Ok(parent)
    }

    /// Expands `~` and requires an absolute result.
    fn expand(&self, path: &str) -> Result<PathBuf, ProtoError> {
        let path = path.trim();
        let expanded = if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(path)
        };
        if !expanded.is_absolute() {
            return Err(invalid(
                format!("{path} is not an absolute path"),
                "Start the path with / or ~.",
            ));
        }
        Ok(expanded)
    }

    fn save(&self, projects: &[Project]) -> Result<(), ProtoError> {
        let body = serde_json::to_vec_pretty(&ProjectsFile {
            projects: projects.to_vec(),
        })
        .map_err(|error| internal(format!("could not encode projects: {error}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|error| internal(format!("could not save projects: {error}")))
    }
}

fn not_found(project_id: &str) -> ProtoError {
    ProtoError::plain(
        ProtoErrorCode::NotFound,
        format!("no project {project_id} on this host"),
    )
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// One plain folder name: no separators, no `.`/`..`, no leading dash.
fn segment(name: &str) -> Result<&str, ProtoError> {
    let name = name.trim();
    let mut components = Path::new(name).components();
    let single = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    if name.is_empty() || !single || name.starts_with('-') || name.contains(['/', '\\']) {
        return Err(invalid(
            format!("“{name}” is not a folder name"),
            "Use one folder name without slashes.",
        ));
    }
    Ok(name)
}

/// Normalises a clone source: `owner/repo` becomes a GitHub HTTPS URL; other
/// values must look like a git URL and never start with `-`.
fn clone_url(url: &str) -> Result<String, ProtoError> {
    let url = url.trim();
    let shorthand = url.split('/').count() == 2
        && !url.contains(':')
        && !url.starts_with('.')
        && url
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || "-_./".contains(char));
    if shorthand {
        return Ok(format!(
            "https://github.com/{}.git",
            url.trim_end_matches(".git")
        ));
    }
    let looks_like_git = url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("ssh://")
        || url.starts_with("git://")
        || url.starts_with("file://")
        || (url.contains('@') && url.contains(':'));
    if url.is_empty() || url.starts_with('-') || !looks_like_git {
        return Err(invalid(
            format!("“{url}” is not a repository URL"),
            "Paste an HTTPS or SSH clone URL, or type owner/repo for GitHub.",
        ));
    }
    Ok(url.to_owned())
}

/// The folder a plain `git clone` would create.
fn repo_name(url: &str) -> Option<String> {
    let tail = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()?
        .trim_end_matches(".git");
    (!tail.is_empty() && tail != "." && tail != "..").then(|| tail.to_owned())
}

async fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<(), ProtoError> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| internal(format!("git {} took too long and was stopped", args[0])))?
        .map_err(|error| {
            ProtoError::new(
                ProtoErrorCode::Internal,
                format!("could not run git: {error}"),
                "Install git on the host and make sure it is on the daemon's PATH.",
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_owned();
    Err(ProtoError::new(
        ProtoErrorCode::InvalidRequest,
        message,
        "Check the URL and that this host can reach it; private repositories need git credentials on the host.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dir: &Path, home: &Path) -> ProjectService {
        ProjectService::open_with_home(dir, HostId::new("host_test"), home.to_path_buf()).unwrap()
    }

    #[tokio::test]
    async fn adds_lists_renames_and_removes_an_existing_folder() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("api")).unwrap();
        let projects = service(data.path(), home.path());

        let project = projects
            .add(
                ProjectSource::Existing {
                    path: "~/api".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(project.name, "api");
        assert!(project.path.ends_with("/api"));
        // The same folder again returns the same project.
        let again = projects
            .add(
                ProjectSource::Existing {
                    path: project.path.clone(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(again.id, project.id);

        let renamed = projects.rename(&project.id, "API").await.unwrap();
        assert_eq!(renamed.name, "API");
        // A reopened service reads the saved list.
        let reopened = service(data.path(), home.path());
        assert_eq!(reopened.list().await[0].name, "API");

        projects.remove(&project.id).await.unwrap();
        assert!(projects.list().await.is_empty());
        assert!(
            home.path().join("api").is_dir(),
            "removing keeps the folder"
        );
    }

    #[tokio::test]
    async fn creates_a_new_folder_with_git_init() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let projects = service(data.path(), home.path());
        let project = projects
            .add(
                ProjectSource::NewFolder {
                    parent: "~".into(),
                    name: "fresh".into(),
                    git_init: true,
                },
                Some("Fresh".into()),
            )
            .await
            .unwrap();
        assert_eq!(project.name, "Fresh");
        assert!(project.git);
        assert!(home.path().join("fresh/.git").exists());

        // A non-empty folder of the same name is refused.
        std::fs::write(home.path().join("fresh/README"), "x").unwrap();
        let error = projects
            .add(
                ProjectSource::NewFolder {
                    parent: "~".into(),
                    name: "fresh".into(),
                    git_init: false,
                },
                None,
            )
            .await;
        // Already registered paths short-circuit before the emptiness check
        // only for Existing; NewFolder must refuse to reuse a full folder.
        assert!(error.is_err());
    }

    #[tokio::test]
    async fn clones_a_local_repository() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let origin = home.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&origin)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("git is not available; skipping the clone test");
            return;
        }
        let projects = service(data.path(), home.path());
        let url = format!("file://{}", origin.display());
        let project = projects
            .add(
                ProjectSource::Clone {
                    url,
                    parent: "~".into(),
                    name: Some("copy".into()),
                },
                None,
            )
            .await
            .unwrap();
        assert!(project.git);
        assert!(home.path().join("copy/.git").exists());
    }

    #[test]
    fn folder_names_and_urls_are_validated() {
        assert!(segment("ok-name").is_ok());
        for bad in ["", "..", "a/b", "-rf", "."] {
            assert!(segment(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(
            clone_url("acme/api").unwrap(),
            "https://github.com/acme/api.git"
        );
        assert!(clone_url("--upload-pack=evil").is_err());
        assert!(clone_url("not a url").is_err());
        assert_eq!(
            clone_url("git@github.com:acme/api.git").unwrap(),
            "git@github.com:acme/api.git"
        );
        assert_eq!(
            repo_name("git@github.com:acme/api.git").as_deref(),
            Some("api")
        );
        assert_eq!(
            repo_name("https://github.com/acme/web/").as_deref(),
            Some("web")
        );
    }

    #[test]
    fn browse_lists_visible_folders() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("code")).unwrap();
        std::fs::create_dir(home.path().join(".hidden")).unwrap();
        std::fs::create_dir_all(home.path().join("repo/.git")).unwrap();
        std::fs::write(home.path().join("file.txt"), "x").unwrap();
        let projects = service(data.path(), home.path());
        let listing = projects.browse(None).unwrap();
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["code", "repo"]);
        assert!(listing.entries[1].git);
        assert!(projects.browse(Some("relative/path")).is_err());
    }
}
