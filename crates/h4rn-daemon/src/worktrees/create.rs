//! Creating a git worktree for a new thread.
//!
//! A thread that asks for isolation gets a checkout of its project's current
//! `HEAD` on its own branch, and the thread's workspace becomes that checkout.
//! The main checkout is only read: every git command here runs `-C` against
//! it, and `git worktree add` never touches its index or working tree.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use h4rn_protocol::WorktreeRequest;

use super::git;
use super::settings::{
    WorktreeSettingsStore, expand_location, location_inside_repo, validate_location,
};

/// How long `git worktree add` and submodule initialisation may take.
const CREATE_TIMEOUT: Duration = Duration::from_secs(120);

/// A worktree created for a thread.
#[derive(Clone, Debug)]
pub struct CreatedWorktree {
    /// Absolute checkout path.
    pub path: PathBuf,
    /// Main worktree of the repository.
    pub repo: PathBuf,
    /// The branch checked out there.
    pub branch: String,
    /// Whether h4rn made the branch, and so may delete it when rolling back.
    pub created_branch: bool,
    /// The admin dir (`<common>/worktrees/<id>`) this checkout's `.git` names
    /// when it was created. A rollback only acts while `.git` still names it,
    /// so a replacement checkout at the same path is never removed.
    registration: Option<PathBuf>,
    /// The token this call wrote into the registration's admin dir. A
    /// replacement registration at the same path (git names the admin dir after
    /// the checkout path, so it would be reused) has no such marker.
    owner: Option<String>,
    /// The commit `HEAD` named before the add, for verifying a branch this
    /// call made before deleting it.
    head: Option<String>,
    /// The durable record that survives a crash before the thread adopts this
    /// checkout, so the next startup can finish the rollback.
    record: Option<PathBuf>,
    /// The creates still waiting for their thread. A shutdown rolls them back
    /// and waits, instead of losing the rollback with the process.
    pending: Option<Arc<PendingWorktrees>>,
}

/// One create that has not been adopted by a thread yet.
///
/// Written under the daemon's data before a create returns. The owner token is
/// the marker in the registration's admin dir, so a sweep only removes a
/// checkout this create made and never a worktree someone else made.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PendingRecord {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    created_branch: bool,
    head: Option<String>,
    owner: String,
}

/// Creates whose thread does not exist yet, shared with the creator so a
/// shutdown can wait for every rollback.
#[derive(Default)]
struct PendingWorktrees {
    created: std::sync::Mutex<Vec<CreatedWorktree>>,
    /// Rollbacks already started in the background, which a shutdown waits for.
    rollbacks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for PendingWorktrees {
    /// The created entries hold this set, so printing them would recurse.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingWorktrees")
            .field("created", &lock(&self.created).len())
            .field("rollbacks", &lock(&self.rollbacks).len())
            .finish()
    }
}

impl PendingWorktrees {
    fn add(&self, created: CreatedWorktree) {
        lock(&self.created).push(created);
    }

    fn remove(&self, path: &Path) {
        lock(&self.created).retain(|created| created.path != path);
    }

    fn track(&self, handle: tokio::task::JoinHandle<()>) {
        let mut rollbacks = lock(&self.rollbacks);
        rollbacks.retain(|handle| !handle.is_finished());
        rollbacks.push(handle);
    }

    fn take_created(&self) -> Vec<CreatedWorktree> {
        std::mem::take(&mut *lock(&self.created))
    }

    fn take_rollbacks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut *lock(&self.rollbacks))
    }
}

/// Panics in a test would poison the lock; the sets are still usable.
fn lock<T>(set: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    set.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl CreatedWorktree {
    /// The thread now owns this checkout, so neither a shutdown nor the next
    /// startup's sweep may remove it.
    pub fn keep(&self) {
        self.forget();
    }

    /// Starts the rollback without waiting for it, off the async runtime when
    /// there is one, and records it so a shutdown can wait for it.
    pub fn rollback_detached(self) {
        let pending = self.pending.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let task = handle.spawn_blocking(move || self.rollback());
                if let Some(pending) = pending {
                    pending.track(task);
                }
            }
            // Outside a runtime (a plain unit test) it runs inline.
            Err(_) => self.rollback(),
        }
    }

    /// Best-effort removal after the thread this was made for never started.
    ///
    /// The cleanup is tied to the registration this checkout had when it was
    /// made: if git no longer lists it, `.git` names a different admin dir, or
    /// the marker this call wrote is gone (another thread's checkout now lives
    /// at the same path), nothing is removed.
    pub fn rollback(&self) {
        #[cfg(test)]
        test_hooks::before_rollback(&self.path);
        // A retry of the same branch may claim this folder the moment the
        // checkout leaves. Hold the claim across the checks, the removal and
        // the branch step, so it can only start before or after this rollback
        // instead of in the middle of it.
        let Some(_claim) = self.claim() else {
            tracing::warn!(
                path = %self.path.display(),
                "could not claim the worktree folder; leaving it in place"
            );
            return;
        };
        let (Some(registration), Some(owner)) =
            (self.registration.as_deref(), self.owner.as_deref())
        else {
            tracing::warn!(
                path = %self.path.display(),
                "refusing to roll back a worktree whose registration was not recorded at creation"
            );
            self.forget();
            return;
        };
        if worktree_gitdir(&self.path).as_deref() != Some(registration)
            || registration_owner(registration).as_deref() != Some(owner)
        {
            // The checkout at this path is no longer the one this call made, so
            // this attempt has nothing left to remove.
            self.forget();
            return;
        }
        let _ = git::run_with_timeout(
            &self.repo,
            [
                std::ffi::OsStr::new("worktree"),
                std::ffi::OsStr::new("remove"),
                std::ffi::OsStr::new("--force"),
                self.path.as_os_str(),
            ],
            CREATE_TIMEOUT,
        );
        let _ = git::run_with_timeout(&self.repo, ["worktree", "prune"], CREATE_TIMEOUT);
        if self.created_branch {
            delete_created_branch(&self.repo, &self.path, &self.branch, self.head.as_deref());
        }
        self.forget();
    }

    /// Drops this attempt's pending record and in-flight entry. Best effort for
    /// the file: a stale one is swept by the next startup, and the sweep checks
    /// the owner marker again.
    fn forget(&self) {
        if let Some(pending) = &self.pending {
            pending.remove(&self.path);
        }
        if let Some(record) = &self.record {
            let _ = std::fs::remove_file(record);
        }
    }

    /// The destination folder's claim, for the length of a rollback.
    ///
    /// The checkout path is always `<location>/<project>/<folder>`, so the
    /// claim is the one a create for the same folder takes.
    fn claim(&self) -> Option<ClaimLock> {
        let project_dir = self.path.parent()?;
        let location = project_dir.parent()?;
        let project = project_dir.file_name()?.to_str()?;
        ClaimLock::acquire(location, project, &self.path).ok()
    }
}

/// The admin dir a checkout's `.git` file names, resolved against the
/// checkout when git wrote it relatively.
fn worktree_gitdir(checkout: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(checkout.join(".git")).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(super::git::norm(&if target.is_absolute() {
        target.to_path_buf()
    } else {
        checkout.join(target)
    }))
}

/// The name of the marker a create writes into its registration's admin dir.
const OWNER_MARKER: &str = "h4rn-owner";

/// Writes this call's ownership token into `admin`, returning it.
///
/// The admin dir belongs to this registration, so the marker goes with it when
/// git removes the registration; a replacement checkout at the same folder
/// gets a freshly created admin dir without this token.
fn mark_registration(admin: &Path) -> Option<String> {
    let owner = uuid::Uuid::now_v7().simple().to_string();
    match std::fs::write(admin.join(OWNER_MARKER), owner.as_bytes()) {
        Ok(()) => Some(owner),
        Err(error) => {
            tracing::warn!(
                %error,
                admin = %admin.display(),
                "could not mark a worktree registration; it will not be rolled back"
            );
            None
        }
    }
}

/// The token [`mark_registration`] wrote into `admin`.
fn registration_owner(admin: &Path) -> Option<String> {
    std::fs::read_to_string(admin.join(OWNER_MARKER))
        .ok()
        .map(|owner| owner.trim().to_owned())
        .filter(|owner| !owner.is_empty())
}

/// The folder, under the daemon's data, that one file per unadopted create is
/// written to.
fn pending_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("worktrees-pending")
}

/// Writes the record that lets a later startup finish this create's rollback.
///
/// Best effort: without it a crash leaves the registration for a person to
/// clear in Settings, which is what happened before the sweep existed. The
/// owner token is required, because the sweep only removes a checkout whose
/// registration still carries it.
fn write_pending_record(
    data_dir: &Path,
    repo: &Path,
    path: &Path,
    branch: &str,
    created_branch: bool,
    head: Option<&str>,
    owner: &str,
) -> Option<PathBuf> {
    let dir = pending_dir(data_dir);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            %error,
            dir = %dir.display(),
            "could not write a create's recovery record; a crash could leave its worktree behind"
        );
        return None;
    }
    let record = PendingRecord {
        repo: repo.to_path_buf(),
        path: path.to_path_buf(),
        branch: branch.to_owned(),
        created_branch,
        head: head.map(str::to_owned),
        owner: owner.to_owned(),
    };
    let bytes = match serde_json::to_vec(&record) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "could not encode a create's recovery record");
            return None;
        }
    };
    let record_path = dir.join(format!("{}.json", uuid::Uuid::now_v7().simple()));
    match std::fs::write(&record_path, bytes) {
        Ok(()) => Some(record_path),
        Err(error) => {
            tracing::warn!(
                %error,
                path = %record_path.display(),
                "could not write a create's recovery record; a crash could leave its worktree behind"
            );
            None
        }
    }
}

/// Finishes the rollback of creates that never reached a thread, at startup.
///
/// A create that fails after its checkout exists rolls back immediately; a
/// shutdown during that rollback would otherwise leave an h4rn-owned
/// registration nothing uses. The records are written before a create returns,
/// so whatever is left here belongs to an attempt no thread adopted. A record
/// that names a checkout a thread now uses is dropped without touching it, and
/// the rollback itself still refuses a registration whose owner marker does not
/// match, so a worktree h4rn did not make is never removed.
///
/// Returns the checkout paths that are gone afterwards. Best effort: every
/// failure is logged, and a record that cannot be handled now is retried at the
/// next startup.
pub fn sweep_pending(data_dir: &Path, in_use: &[String]) -> Vec<PathBuf> {
    let dir = pending_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let in_use: Vec<PathBuf> = in_use
        .iter()
        .map(|path| git::norm(Path::new(path)))
        .collect();
    let mut swept = Vec::new();
    for entry in entries.flatten() {
        let record_file = entry.path();
        if record_file.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&record_file) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<PendingRecord>(&bytes) else {
            tracing::warn!(
                path = %record_file.display(),
                "skipping a create recovery record that could not be read"
            );
            continue;
        };
        let path = git::norm(&record.path);
        // A thread may have been created just before the crash; it owns the
        // checkout now, so the record is finished, not swept.
        if in_use
            .iter()
            .any(|used| path == *used || path.starts_with(used) || used.starts_with(&path))
        {
            let _ = std::fs::remove_file(&record_file);
            continue;
        }
        let created = CreatedWorktree {
            path: record.path.clone(),
            repo: record.repo.clone(),
            branch: record.branch.clone(),
            created_branch: record.created_branch,
            registration: worktree_gitdir(&record.path),
            owner: Some(record.owner),
            head: record.head,
            record: Some(record_file.clone()),
            pending: None,
        };
        created.rollback();
        if !record.path.exists() {
            swept.push(record.path);
        }
    }
    swept
}

/// Whether `repo` still has a registration whose checkout is `path`.
///
/// A snapshot git could not answer proves nothing, so it reads as registered:
/// the caller then leaves the checkout alone.
fn registered(repo: &Path, path: &Path) -> bool {
    let path = super::git::norm(path);
    super::git::registrations(repo)
        .map(|snapshot| {
            snapshot
                .iter()
                .any(|registration| registration.path == path)
        })
        .unwrap_or(true)
}

/// Whether `refs/heads/<branch>` exists in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    let reference = format!("refs/heads/{branch}");
    super::git::run_with_timeout(
        repo,
        ["rev-parse", "--verify", reference.as_str()],
        CREATE_TIMEOUT,
    )
    .is_ok()
}

/// Whether `branch` exists in `repo` and still points at `head`.
///
/// The full ref is asked for: `git rev-parse` prefers a tag over a branch of
/// the same name, so a tag would otherwise hide a branch h4rn created.
fn branch_points_at(repo: &Path, branch: &str, head: Option<&str>) -> bool {
    let Some(head) = head else {
        return false;
    };
    let reference = format!("refs/heads/{branch}");
    super::git::run_with_timeout(
        repo,
        ["rev-parse", "--verify", reference.as_str()],
        CREATE_TIMEOUT,
    )
    .is_ok_and(|commit| commit == head)
}

/// Whether a registration of `repo` has `branch` checked out right now.
///
/// Read fresh rather than from a snapshot taken earlier: a retry of the same
/// branch can register its checkout while a cleanup is running. A snapshot
/// that is not complete (or cannot be read) reads as "in use", so a branch is
/// never deleted on the strength of a partial answer.
fn branch_checked_out(repo: &Path, branch: &str) -> bool {
    super::git::registrations(repo)
        .map(|snapshot| {
            !snapshot.complete
                || snapshot
                    .iter()
                    .any(|registration| match &registration.head {
                        super::git::Head::Branch(held) => held == branch,
                        // An unreadable HEAD may name the branch; leave it.
                        super::git::Head::Unknown => true,
                        super::git::Head::Detached(_) => false,
                    })
        })
        .unwrap_or(true)
}

/// Deletes a branch this call created after a failed add or a rollback.
///
/// Runs while the destination's claim is held. It re-reads the registrations
/// at the last moment, so a checkout a retry registered for this branch in the
/// meantime keeps it.
fn delete_created_branch(repo: &Path, dest: &Path, branch: &str, head: Option<&str>) {
    #[cfg(test)]
    test_hooks::before_branch_delete(dest);
    if !registered(repo, dest)
        && !branch_checked_out(repo, branch)
        && branch_points_at(repo, branch, head)
    {
        let _ = git::run_with_timeout(repo, ["branch", "-D", branch], CREATE_TIMEOUT);
    }
}

/// Creates thread worktrees with this machine's saved settings.
#[derive(Clone)]
pub struct WorktreeCreator {
    data_dir: PathBuf,
    settings: Arc<WorktreeSettingsStore>,
    pending: Arc<PendingWorktrees>,
}

impl WorktreeCreator {
    /// A creator rooted at the daemon's data directory.
    pub fn new(data_dir: PathBuf, settings: Arc<WorktreeSettingsStore>) -> Self {
        Self {
            data_dir,
            settings,
            pending: Arc::default(),
        }
    }

    /// Creates the requested worktree, off the async runtime.
    pub async fn create_for_thread(
        &self,
        workspace: &str,
        request: &WorktreeRequest,
        prompt: &str,
    ) -> Result<CreatedWorktree, String> {
        let workspace = PathBuf::from(workspace);
        let settings = self.settings.get();
        let data_dir = self.data_dir.clone();
        let prompt = prompt.to_owned();
        let request = request.clone();
        // Every probe and `git worktree add` shells out and blocks.
        let mut created = tokio::task::spawn_blocking(move || {
            create_blocking(
                &workspace,
                &request,
                &prompt,
                &settings,
                &data_dir,
                dirs::home_dir().as_deref(),
            )
        })
        .await
        .map_err(|error| format!("the worktree setup task failed: {error}"))??;
        created.pending = Some(Arc::clone(&self.pending));
        self.pending.add(created.clone());
        Ok(created)
    }

    /// Waits for every rollback a shutdown could otherwise lose, then rolls
    /// back each create no thread adopted and waits for that too.
    ///
    /// Called after the thread manager has stopped, so a create that was still
    /// running has already dropped its guard and registered its rollback.
    pub async fn shutdown(&self) {
        for handle in self.pending.take_rollbacks() {
            let _ = handle.await;
        }
        let created = self.pending.take_created();
        if created.is_empty() {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || {
            for created in created {
                created.rollback();
            }
        })
        .await;
    }
}

/// [`WorktreeCreator::create_for_thread`] as a blocking call.
///
/// `home` expands a `~` location; tests pass their own so no real home folder
/// is touched.
pub fn create_blocking(
    workspace: &Path,
    request: &WorktreeRequest,
    prompt: &str,
    settings: &h4rn_protocol::WorktreeSettings,
    data_dir: &Path,
    home: Option<&Path>,
) -> Result<CreatedWorktree, String> {
    if !workspace.is_dir() {
        return Err(format!(
            "the workspace {} does not exist on this host",
            workspace.display()
        ));
    }
    // Distinguish "git is missing" from "this is not a repository": both
    // probes fail, but only one is something the person can fix by installing
    // git.
    if let Err(error) = git::run_with_timeout(workspace, ["--version"], CREATE_TIMEOUT)
        && error.contains("could not run git")
    {
        return Err("git is not installed or could not be run on this host".to_owned());
    }
    if !git::is_git_repo(workspace) {
        return Err(format!("{} is not a git repository", workspace.display()));
    }
    let repo = git::toplevel(workspace)
        .ok_or_else(|| format!("{} is not a git repository", workspace.display()))?;

    let branches = git::run_with_timeout(
        &repo,
        ["for-each-ref", "--format=%(refname)", "refs/heads"],
        CREATE_TIMEOUT,
    )
    .map_err(|error| format!("could not list the repository's branches: {error}"))?;
    // Full ref names are requested and the prefix stripped here: git shortens
    // a name that a tag shares (`release` becomes `heads/release`), which would
    // make the branch look free.
    let branches: std::collections::HashSet<&str> = branches
        .lines()
        .filter_map(|line| line.strip_prefix("refs/heads/"))
        .collect();
    let worktrees = git::list_worktrees(&repo)
        .map_err(|error| format!("could not read the repository's worktrees: {error}"))?;

    let location = match settings.location.as_deref() {
        Some(location) => {
            validate_location(location)?;
            expand_location(location, home)?
        }
        None => data_dir.join("worktrees"),
    };
    // A checkout inside the main checkout would make the main worktree dirty,
    // and `git worktree add` may refuse it outright; refuse before claiming.
    //
    // `repo` is the workspace's own checkout, which for a linked worktree (or
    // a bare repository's worktree) is not the main one, while `project` is
    // named after the main checkout. The destination must be outside both, or
    // a location that holds the main checkout leaks new checkouts into it. The
    // same check also refuses a location inside a sibling linked worktree.
    let project = project_name(&repo);
    if let Some(message) = destination_refusal(&repo, &location) {
        return Err(message);
    }

    // The folder is claimed atomically before any git call (see [`claim_dir`]),
    // so two creates for one folder can never both own it and then remove each
    // other's checkout after the loser's `git worktree add` fails. The claim is
    // held until this call's cleanup finishes, so a failed add that removes the
    // folder cannot let a second create register a checkout before the first
    // has finished deleting it.
    let (branch, mut created_branch, dest, claim) = match request.branch().map(str::trim) {
        Some(named) if !named.is_empty() => {
            if !valid_branch(&repo, named)? {
                return Err(format!("`{named}` is not a valid branch name"));
            }
            // `list_worktrees` names linked checkouts only, so the main
            // checkout's own branch is asked for separately.
            let main_branch = git::run_with_timeout(
                &repo,
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
                CREATE_TIMEOUT,
            )
            .ok();
            if main_branch.as_deref() == Some(named) {
                return Err(format!(
                    "the branch `{named}` is already checked out at {}",
                    repo.display()
                ));
            }
            if let Some(existing) = worktrees
                .iter()
                .find(|worktree| worktree.branch.as_deref() == Some(named))
            {
                return Err(format!(
                    "the branch `{named}` is already checked out at {}",
                    existing.path.display()
                ));
            }
            let dest = checkout_path(&location, &project, named);
            let Some(claim) = claim_dir(&location, &project, &dest)? else {
                return Err(format!("a folder already exists at {}", dest.display()));
            };
            // An existing branch is attached, not recreated, so its history
            // is the one the person asked for.
            (named.to_owned(), !branches.contains(named), dest, claim)
        }
        _ => claim_generated_branch(settings, prompt, &branches, &worktrees, &location, &project)?,
    };

    // The branch was read before the folder was claimed. A rollback of the same
    // named branch can delete it while this call waits for the claim, and a
    // retry can check the branch out. Re-read it under the claim so the add
    // attaches only a branch that is really there and really free.
    if let Some(named) = request
        .branch()
        .map(str::trim)
        .filter(|named| !named.is_empty())
        && let Ok(snapshot) = git::registrations(&repo)
    {
        if let Some(holder) = snapshot
            .iter()
            .find(|registration| registration.branch() == Some(named))
        {
            // Leave nothing behind: the folder this call claimed is empty and
            // still ours.
            if claim.still_ours() {
                let _ = std::fs::remove_dir_all(&dest);
            }
            return Err(format!(
                "the branch `{named}` is already checked out at {}",
                holder.path.display()
            ));
        }
        created_branch = !branch_exists(&repo, named);
    }

    // What a failed add leaves behind is judged against the state before it:
    // this attempt owns only a registration and ref that were not there then.
    let before = git::registrations(&repo).ok();
    let head = git::run_with_timeout(&repo, ["rev-parse", "HEAD"], CREATE_TIMEOUT).ok();
    let dest_text = dest.to_string_lossy().into_owned();
    let add = if created_branch {
        git::run_with_timeout(
            &repo,
            ["worktree", "add", "-b", &branch, &dest_text],
            CREATE_TIMEOUT,
        )
    } else {
        git::run_with_timeout(
            &repo,
            ["worktree", "add", &dest_text, &branch],
            CREATE_TIMEOUT,
        )
    };
    if let Err(error) = add {
        // Remove only what this call made: a registration git left behind, the
        // folder it claimed (still the same directory), and a branch it
        // created at the commit it started from. The claim is held throughout,
        // so no other create can claim the folder until this cleanup is done.
        clean_failed_add(
            &repo,
            &dest,
            &claim,
            created_branch.then_some(branch.as_str()),
            head.as_deref(),
            before.as_ref(),
        );
        drop(claim);
        return Err(format!(
            "could not create the worktree at {}: {error}",
            dest.display()
        ));
    }

    // New worktrees start without submodule contents. A repository that
    // declares them gets a full recursive checkout, so the agent does not run
    // against empty folders that look like real ones.
    let registration = worktree_gitdir(&dest);
    let owner = registration.as_deref().and_then(mark_registration);
    let record = owner.as_deref().and_then(|owner| {
        write_pending_record(
            data_dir,
            &repo,
            &dest,
            &branch,
            created_branch,
            head.as_deref(),
            owner,
        )
    });
    if dest.join(".gitmodules").is_file()
        && let Err(error) = git::run_with_timeout(&dest, submodule_args(&dest), CREATE_TIMEOUT)
    {
        let created = CreatedWorktree {
            registration,
            owner,
            head: head.clone(),
            record,
            pending: None,
            path: dest,
            repo,
            branch,
            created_branch,
        };
        // The claim cannot be held while `rollback` takes it again.
        drop(claim);
        created.rollback();
        return Err(format!(
            "the worktree was created but its submodules could not be initialised: {error}"
        ));
    }

    Ok(CreatedWorktree {
        registration,
        owner,
        head,
        record,
        pending: None,
        path: dest,
        repo,
        branch,
        created_branch,
    })
}

/// Best-effort cleanup after `git worktree add` failed.
///
/// A failed add can leave a registration (a failing `post-checkout` hook), a
/// branch (`-b` ran before the checkout failed), or neither (a required smudge
/// filter makes git remove the folder and registration itself). Each piece is
/// removed only when it was absent before the add and still names this
/// attempt's work, so another checkout is never touched.
fn clean_failed_add(
    repo: &Path,
    dest: &Path,
    claim: &Claim,
    created_branch: Option<&str>,
    head: Option<&str>,
    before: Option<&git::RegistrationSnapshot>,
) {
    let dest_norm = git::norm(dest);
    let before_has_dest = match before {
        // A snapshot that could not read every registration cannot prove the
        // one at this path was not another thread's.
        Some(snapshot) if snapshot.complete => snapshot
            .iter()
            .any(|registration| registration.path == dest_norm),
        _ => true,
    };
    let after = git::registrations(repo).ok();
    let registration = after.as_ref().and_then(|snapshot| {
        (!before_has_dest)
            .then(|| {
                snapshot
                    .iter()
                    .find(|registration| registration.path == dest_norm)
            })
            .flatten()
    });
    if let Some(registration) = registration {
        let _ = git::run_with_timeout(
            repo,
            [
                std::ffi::OsStr::new("worktree"),
                std::ffi::OsStr::new("remove"),
                std::ffi::OsStr::new("--force"),
                registration.path.as_os_str(),
            ],
            CREATE_TIMEOUT,
        );
    }
    // `prune` drops a registration whose folder git already removed; it never
    // touches a live checkout.
    let _ = git::run_with_timeout(repo, ["worktree", "prune"], CREATE_TIMEOUT);
    // The recursive fallback only ever removes the very directory this call
    // created, and only while git holds no registration for it.
    let unregistered = !registered(repo, dest);
    #[cfg(test)]
    test_hooks::before_cleanup_removal(dest);
    if claim.still_ours() && unregistered {
        let _ = std::fs::remove_dir_all(dest);
    }
    if let Some(branch) = created_branch {
        delete_created_branch(repo, dest, branch, head);
    }
}

/// `git submodule update` with the local-path permission a checkout of a
/// repository with local submodules needs.
///
/// Git refuses the `file` transport for submodules unless told otherwise
/// (CVE-2022-39253). A repository whose `.gitmodules` names a relative,
/// absolute or `file:` URL is a local checkout the person already has, so that
/// one command may use local paths; a repository with only network remotes is
/// left at git's stricter default.
fn submodule_args(dest: &Path) -> Vec<&'static str> {
    let needs_file = std::fs::read_to_string(dest.join(".gitmodules"))
        .map(|text| {
            text.lines().any(|line| {
                let Some((key, value)) = line.split_once('=') else {
                    return false;
                };
                if key.trim() != "url" {
                    return false;
                }
                let url = value.trim();
                url.starts_with("file:")
                    || url.starts_with("./")
                    || url.starts_with("../")
                    || Path::new(url).is_absolute()
            })
        })
        .unwrap_or(false);
    if needs_file {
        vec![
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ]
    } else {
        vec!["submodule", "update", "--init", "--recursive"]
    }
}

/// The daemon-generated branch for `prompt`, with a short suffix when the
/// obvious name is taken, and its checkout folder claimed for this call.
///
/// A name whose folder another create claimed first gets the next suffix. The
/// fallback never trusts the prefix: a prefix saved before prefix validation
/// existed can still start an invalid name, so a candidate git would refuse is
/// replaced by one that needs no prefix at all.
fn claim_generated_branch(
    settings: &h4rn_protocol::WorktreeSettings,
    prompt: &str,
    branches: &std::collections::HashSet<&str>,
    worktrees: &[git::GitWorktree],
    location: &Path,
    project: &str,
) -> Result<(String, bool, std::path::PathBuf, Claim), String> {
    let prefix = branch_prefix(&settings.branch_prefix);
    let slug = prompt_slug(prompt);
    let base = format!("{prefix}{slug}");
    for suffix in 1..=1000 {
        let candidate = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        if !free_branch_name(&candidate, branches, worktrees) {
            continue;
        }
        let dest = checkout_path(location, project, &candidate);
        // Another create may have taken this folder between the listing and
        // now; the atomic claim is what decides, not the earlier look.
        if let Some(claim) = claim_dir(location, project, &dest)? {
            return Ok((candidate, true, dest, claim));
        }
    }
    // Every readable name in the window is taken; name it by the folder the
    // person is likely to recognise instead of failing.
    for _ in 0..100 {
        let unique = format!("thread-{}", uuid::Uuid::now_v7().simple());
        let with_prefix = format!("{prefix}{unique}");
        let candidate = with_prefix
            .chars()
            .take(80)
            .collect::<String>()
            .trim_end_matches(['/', '.'])
            .to_owned();
        let candidate = if is_valid_branch_name(&candidate) {
            candidate
        } else {
            unique
        };
        let dest = checkout_path(location, project, &candidate);
        if let Some(claim) = claim_dir(location, project, &dest)? {
            return Ok((candidate, true, dest, claim));
        }
    }
    Err("could not find a free name for the new worktree".to_owned())
}

/// Whether `candidate` is a name git accepts and no branch or checkout uses.
fn free_branch_name(
    candidate: &str,
    branches: &std::collections::HashSet<&str>,
    worktrees: &[git::GitWorktree],
) -> bool {
    is_valid_branch_name(candidate)
        && !branches.contains(candidate)
        && worktrees
            .iter()
            .all(|worktree| worktree.branch.as_deref() != Some(candidate))
}

/// Claims `dest` for this call, creating its parent first.
///
/// The claim has two parts, and both last until this call's cleanup is done:
///
/// * a lock file in `<location>/.claims`, so a second create for the same
///   folder waits for this call instead of registering a checkout while a
///   failed add is still being cleaned up (a failed add can remove the folder,
///   and `create_dir` alone would then let the second create in);
/// * the folder itself, created with `create_dir`, which is atomic and decides
///   which call owns it.
///
/// Returns `Ok(None)` when the folder was already there: git accepts an empty
/// folder, but an existing one may hold another call's checkout, so it is never
/// reused.
fn claim_dir(location: &Path, project: &str, dest: &Path) -> Result<Option<Claim>, String> {
    #[cfg(test)]
    test_hooks::before_claim(dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create the worktree folder {}: {error}",
                parent.display()
            )
        })?;
    }
    let lock = ClaimLock::acquire(location, project, dest)?;
    match std::fs::create_dir(dest) {
        Ok(()) => {
            let identity = dir_identity(dest);
            #[cfg(test)]
            test_hooks::record_claim_identity(dest, identity);
            Ok(Some(Claim {
                dest: dest.to_path_buf(),
                identity,
                _lock: lock,
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(format!(
            "could not create the worktree folder {}: {error}",
            dest.display()
        )),
    }
}

/// The identity of the folder a claim created.
///
/// Used before the recursive fallback: only the same directory (same device and
/// inode) is ever removed. On platforms without device and inode numbers the
/// value is a unit, so this cannot tell one directory from another; there the
/// claim's lock file is the only guard against a second create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// The identity of `path` as a directory; `None` when it is missing or a file.
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        meta.is_dir().then(|| DirIdentity {
            device: meta.dev(),
            inode: meta.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        path.is_dir().then_some(DirIdentity {})
    }
}

/// A hold on one destination folder.
struct Claim {
    dest: PathBuf,
    identity: Option<DirIdentity>,
    /// Released when this is dropped; on Unix the file itself is left in
    /// `.claims` so every create locks the same inode.
    _lock: ClaimLock,
}

impl Claim {
    /// Whether `dest` is still the very directory this claim created.
    fn still_ours(&self) -> bool {
        self.identity.is_some() && dir_identity(&self.dest) == self.identity
    }
}

/// One destination's lock file, held for the life of a [`Claim`].
struct ClaimLock {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

/// The lock file one destination uses, inside the location's `.claims` folder.
fn claim_path(location: &Path, project: &str, dest: &Path) -> PathBuf {
    let folder = dest
        .file_name()
        .map(|name| folder_name(&name.to_string_lossy()))
        .unwrap_or_else(|| "worktree".to_owned());
    location
        .join(".claims")
        .join(format!("{}--{folder}.lock", folder_name(project)))
}

impl ClaimLock {
    fn acquire(location: &Path, project: &str, dest: &Path) -> Result<Self, String> {
        let path = claim_path(location, project, dest);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| {
                format!(
                    "could not create the worktree claim folder {}: {error}",
                    dir.display()
                )
            })?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .map_err(|error| format!("could not open {}: {error}", path.display()))?;
            loop {
                // A blocking lock, released by the kernel if this process dies
                // mid-create, so a crash never wedges the folder for good.
                let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if locked == 0 {
                    return Ok(Self { _file: file });
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("could not lock {}: {error}", path.display()));
            }
        }
        #[cfg(not(unix))]
        {
            // Without `flock`, a create-new marker serialises creates; a marker
            // older than ten minutes is a crash's leftover and is replaced.
            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            loop {
                match std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        let _ = writeln!(file, "{}", std::process::id());
                        return Ok(Self { path });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let stale = path
                            .metadata()
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|modified| modified.elapsed().ok())
                            .is_some_and(|age| age > Duration::from_secs(600));
                        if stale {
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err(format!(
                                "another worktree create is still using {}",
                                dest.display()
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(error) => {
                        return Err(format!("could not create {}: {error}", path.display()));
                    }
                }
            }
        }
    }
}

#[cfg(not(unix))]
impl Drop for ClaimLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Normalises the configured prefix: invalid characters become hyphens, the
/// trailing slash is optional, and an empty prefix adds nothing.
pub fn branch_prefix(prefix: &str) -> String {
    let mut cleaned = String::new();
    for character in prefix.trim().chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/') {
            cleaned.push(character);
        } else {
            cleaned.push('-');
        }
    }
    while cleaned.contains("//") {
        cleaned = cleaned.replace("//", "/");
    }
    let cleaned = cleaned.trim_start_matches('-').trim_matches('/');
    if cleaned.is_empty() {
        return String::new();
    }
    format!("{cleaned}/")
}

/// A short, readable slug for the branch from the first prompt.
fn prompt_slug(prompt: &str) -> String {
    let mut slug = String::new();
    for word in prompt.split_whitespace().take(8) {
        let cleaned: String = word
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let cleaned = cleaned.trim_matches('-');
        if cleaned.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(cleaned);
        if slug.len() >= 32 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "thread".to_owned()
    } else {
        slug.chars()
            .take(40)
            .collect::<String>()
            .trim_end_matches(['-', '.'])
            .to_owned()
    }
}

/// Where a branch's checkout lives: one folder per branch, so a branch with a
/// `/` does not become a surprise directory tree.
fn checkout_path(location: &Path, project: &str, branch: &str) -> PathBuf {
    location.join(project).join(folder_name(branch))
}

/// A folder-safe name for a branch.
fn folder_name(branch: &str) -> String {
    let mut name = String::new();
    for character in branch.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            name.push(character);
        } else {
            name.push('-');
        }
    }
    let name = name.trim_matches(['-', '.', ' ']);
    if name.is_empty() {
        "worktree".to_owned()
    } else {
        name.to_owned()
    }
}

/// The project folder name: the main worktree's folder.
fn project_name(repo: &Path) -> String {
    let main = git::main_checkout(repo).unwrap_or_else(|| repo.to_path_buf());
    project_name_of(&main)
}

/// [`project_name`] from an already resolved main checkout.
fn project_name_of(main: &Path) -> String {
    main.file_name()
        .and_then(|name| name.to_str())
        .map(folder_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned())
}

/// Why `location` may not hold this project's checkouts, when it may not.
///
/// Checkouts go to `<location>/<project>/<branch>`, so the location itself may
/// be outside every checkout while the folder it holds for this project is one
/// (Location `~/code` for the project `~/code/api`). The destination is refused
/// inside the workspace's own checkout, its main checkout — which a linked
/// worktree, a `--separate-git-dir` repository, a submodule or a bare
/// repository's worktree keep somewhere else — and inside any other linked
/// worktree of the same repository.
///
/// Both the creator and Settings validation use this, so a location they would
/// disagree about cannot be saved and then fail on the first thread.
pub(crate) fn destination_refusal(workspace: &Path, location: &Path) -> Option<String> {
    let main = git::main_checkout(workspace).unwrap_or_else(|| workspace.to_path_buf());
    let project = project_name_of(&main);
    let destination = location.join(&project);
    if location_inside_repo(workspace, &destination) {
        return Some(destination_message(location, &destination, workspace));
    }
    if location_inside_repo(&main, &destination) {
        if git::is_bare(&main, git::OBSERVATION_GIT_TIMEOUT).ok() == Some(true) {
            return Some(format!(
                "the worktree location {} would put new checkouts (under {}) inside the bare repository's git directory {}, which has no working tree to hold them; choose a folder outside it",
                location.display(),
                destination.display(),
                main.display()
            ));
        }
        return Some(destination_message(location, &destination, &main));
    }
    let Ok(snapshot) = git::registrations(workspace) else {
        return None;
    };
    let common = git::common_dir(workspace, git::OBSERVATION_GIT_TIMEOUT).ok();
    for registration in snapshot.iter().filter(|registration| !registration.bare) {
        if registration.path == workspace
            || registration.path == main
            // A submodule's main registration names its git dir; it has no
            // working tree of its own, and the working tree was checked above.
            || common.as_ref() == Some(&registration.path)
        {
            continue;
        }
        if location_inside_repo(&registration.path, &destination) {
            return Some(format!(
                "the worktree location {} would put new checkouts (under {}) inside another worktree of this repository, {}; choose a folder outside it",
                location.display(),
                destination.display(),
                registration.path.display()
            ));
        }
    }
    None
}

/// The refusal shared by every checkout that would hold the destination.
fn destination_message(location: &Path, destination: &Path, checkout: &Path) -> String {
    format!(
        "the worktree location {} would put new checkouts (under {}) inside the project checkout {}; choose a folder outside it",
        location.display(),
        destination.display(),
        checkout.display()
    )
}

/// Whether `git` accepts `name` as a branch.
fn valid_branch(repo: &Path, name: &str) -> Result<bool, String> {
    if name.starts_with('-') {
        return Ok(false);
    }
    match git::run_with_timeout(repo, ["check-ref-format", "--branch", name], CREATE_TIMEOUT) {
        Ok(_) => Ok(true),
        // `check-ref-format` reports an invalid name as a plain failure.
        Err(error) if error.contains("could not run git") => Err(error),
        Err(_) => Ok(false),
    }
}

/// Generated names are built from safe pieces; this is the final guard that
/// the result is still a name git will accept.
///
/// Settings validation uses it too, so a saved prefix is refused at the point
/// the person can still fix it instead of failing at creation.
pub(crate) fn is_valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with(".lock")
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
                })
        })
}

#[cfg(test)]
pub(crate) mod test_hooks {
    //! Hooks that let a test hold one create's cleanup or one rollback open, so
    //! the race against a second create (and the async runtime) is deterministic.

    use std::path::PathBuf;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    struct Pause {
        dest: PathBuf,
        paused: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    // Keyed by destination (a list, not one slot), because tests in one binary
    // run in parallel and several may watch different folders at once.
    static PAUSE_BEFORE_REMOVE: Mutex<Vec<Pause>> = Mutex::new(Vec::new());
    static PAUSE_BEFORE_BRANCH: Mutex<Vec<Pause>> = Mutex::new(Vec::new());
    static PAUSE_BEFORE_CLAIM: Mutex<Vec<Pause>> = Mutex::new(Vec::new());
    static HOLD_ROLLBACK: Mutex<Option<(PathBuf, Duration)>> = Mutex::new(None);
    static CLAIM_IDENTITY: Mutex<Vec<(PathBuf, Option<super::DirIdentity>)>> =
        Mutex::new(Vec::new());

    /// Holds a failed add's cleanup for `dest` just before its recursive
    /// removal, until a test releases it.
    pub(crate) fn pause_before_cleanup_removal(
        dest: PathBuf,
        paused: Arc<Barrier>,
        release: Arc<Barrier>,
    ) {
        install(
            &PAUSE_BEFORE_REMOVE,
            Pause {
                dest,
                paused,
                release,
            },
        );
    }

    /// Holds a cleanup for `dest` just before it deletes a branch it created,
    /// until a test releases it.
    pub(crate) fn pause_before_branch_delete(
        dest: PathBuf,
        paused: Arc<Barrier>,
        release: Arc<Barrier>,
    ) {
        install(
            &PAUSE_BEFORE_BRANCH,
            Pause {
                dest,
                paused,
                release,
            },
        );
    }

    /// Holds a create for `dest` before it claims the folder, until a test
    /// releases it, so a test can act on the branch it read a moment earlier.
    pub(crate) fn pause_before_claim(dest: PathBuf, paused: Arc<Barrier>, release: Arc<Barrier>) {
        install(
            &PAUSE_BEFORE_CLAIM,
            Pause {
                dest,
                paused,
                release,
            },
        );
    }

    /// Whether `dest`'s claim is held right now (by any call in this process).
    pub(crate) fn claim_held(
        location: &std::path::Path,
        project: &str,
        dest: &std::path::Path,
    ) -> bool {
        let path = super::claim_path(location, project, dest);
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let Ok(file) = std::fs::File::open(&path) else {
                return false;
            };
            let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if locked == 0 {
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                return false;
            }
            true
        }
        #[cfg(not(unix))]
        {
            let _ = (location, project);
            path.exists()
        }
    }

    /// Holds a rollback of `dest` for `hold` so a test can watch the runtime.
    pub(crate) fn hold_rollback(dest: PathBuf, hold: Duration) {
        *HOLD_ROLLBACK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((dest, hold));
    }

    /// Adds a pause for its destination, replacing an earlier one for the same
    /// folder.
    fn install(slot: &Mutex<Vec<Pause>>, pause: Pause) {
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.retain(|installed| installed.dest != pause.dest);
        slot.push(pause);
    }

    /// Forgets the pause hooks this test installed for `dest`.
    ///
    /// Scoped by destination, because tests in one binary run in parallel and
    /// one test must never erase another's hook.
    pub(crate) fn clear_failed_add_hooks(dest: &std::path::Path) {
        for slot in [
            &PAUSE_BEFORE_REMOVE,
            &PAUSE_BEFORE_BRANCH,
            &PAUSE_BEFORE_CLAIM,
        ] {
            slot.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|pause| pause.dest != dest);
        }
        CLAIM_IDENTITY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(recorded, _)| recorded != dest);
    }

    /// The directory identity a claim for `dest` recorded when it made the
    /// folder, so a test can put a different directory there on purpose.
    #[cfg(unix)]
    pub(super) fn claim_identity(dest: &std::path::Path) -> Option<super::DirIdentity> {
        CLAIM_IDENTITY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
            .find(|(recorded, _)| recorded == dest)
            .and_then(|(_, identity)| *identity)
    }

    pub(super) fn record_claim_identity(
        dest: &std::path::Path,
        identity: Option<super::DirIdentity>,
    ) {
        let mut recorded = CLAIM_IDENTITY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        recorded.retain(|(path, _)| path != dest);
        recorded.push((dest.to_path_buf(), identity));
    }

    /// Forgets the rollback hold this test installed.
    pub(crate) fn clear_rollback() {
        *HOLD_ROLLBACK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(super) fn before_cleanup_removal(dest: &std::path::Path) {
        pause(&PAUSE_BEFORE_REMOVE, dest);
    }

    pub(super) fn before_branch_delete(dest: &std::path::Path) {
        pause(&PAUSE_BEFORE_BRANCH, dest);
    }

    pub(super) fn before_claim(dest: &std::path::Path) {
        pause(&PAUSE_BEFORE_CLAIM, dest);
    }

    /// Waits on the pause a test installed for `dest`, when there is one.
    fn pause(slot: &Mutex<Vec<Pause>>, dest: &std::path::Path) {
        let waiting = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|pause| pause.dest == dest)
            .map(|pause| (Arc::clone(&pause.paused), Arc::clone(&pause.release)));
        if let Some((paused, release)) = waiting {
            paused.wait();
            release.wait();
        }
    }

    pub(super) fn before_rollback(dest: &std::path::Path) {
        let hold = HOLD_ROLLBACK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|(held, _)| held == dest)
            .map(|(_, hold)| *hold);
        if let Some(hold) = hold {
            let until = Instant::now() + hold;
            while Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("repo");
        init_repo(dir.path());
        dir
    }

    /// Makes `dir` a repository with one commit, the way a project folder is.
    fn init_repo(dir: &Path) {
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "user.email", "wt@example.invalid"]);
        run(dir, &["config", "user.name", "worktree test"]);
        std::fs::write(dir.join("README.md"), "hello\n").expect("readme");
        run(dir, &["add", "README.md"]);
        run(dir, &["commit", "-q", "-m", "init"]);
    }

    fn settings(location: Option<&Path>) -> h4rn_protocol::WorktreeSettings {
        h4rn_protocol::WorktreeSettings {
            location: location.map(|path| path.to_string_lossy().into_owned()),
            ..h4rn_protocol::WorktreeSettings::default()
        }
    }

    #[test]
    fn generated_branch_uses_the_prefix_and_prompt() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "Fix the login loop please",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "h4rn/fix-the-login-loop-please");
        assert!(created.path.is_dir());
        assert_eq!(
            created.path,
            checkout_path(
                &data.path().join("worktrees"),
                &project_name(repo.path()),
                "h4rn/fix-the-login-loop-please"
            )
        );
        assert_eq!(
            git::toplevel(&created.path).as_deref(),
            Some(created.path.as_path())
        );
    }

    #[test]
    fn a_named_branch_is_attached_as_written() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature/readme".into()),
            "anything",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "feature/readme");
        assert!(created.created_branch);
        let branch = git::run_with_timeout(
            &created.path,
            ["rev-parse", "--abbrev-ref", "HEAD"],
            CREATE_TIMEOUT,
        )
        .expect("branch");
        assert_eq!(branch, "feature/readme");
    }

    #[test]
    fn an_empty_prefix_generates_a_bare_slug() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "No prefix here",
            &h4rn_protocol::WorktreeSettings {
                branch_prefix: String::new(),
                ..h4rn_protocol::WorktreeSettings::default()
            },
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "no-prefix-here");
    }

    #[test]
    fn a_taken_generated_name_gets_a_suffix() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        run(repo.path(), &["branch", "h4rn/same-task"]);
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "same task",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "h4rn/same-task-2");
    }

    #[test]
    fn a_branch_checked_out_elsewhere_is_refused() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        run(repo.path(), &["branch", "busy-branch"]);
        let other = data.path().join("other");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                other.to_str().unwrap(),
                "busy-branch",
            ],
        );
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("busy-branch".into()),
            "task",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("refused");
        assert!(error.contains("already checked out"), "{error}");
    }

    #[test]
    fn a_branch_checked_out_in_the_main_checkout_is_refused() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("main".into()),
            "task",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("the main checkout's own branch cannot be checked out twice");
        assert!(error.contains("already checked out"), "{error}");
    }

    #[test]
    fn a_non_git_folder_is_refused() {
        let folder = tempfile::tempdir().expect("folder");
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            folder.path(),
            &WorktreeRequest::Generated(true),
            "task",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("refused");
        assert!(error.contains("not a git repository"), "{error}");
    }

    #[test]
    fn the_custom_location_is_used_and_tilde_expands_against_home() {
        let repo = repo();
        let custom = tempfile::tempdir().expect("custom");
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "custom place",
            &settings(Some(custom.path())),
            data.path(),
            None,
        )
        .expect("create");
        assert!(created.path.starts_with(custom.path()));
        assert!(!created.path.starts_with(data.path()));

        let home = tempfile::tempdir().expect("home");
        let settings = h4rn_protocol::WorktreeSettings {
            location: Some("~/checkouts".into()),
            ..h4rn_protocol::WorktreeSettings::default()
        };
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "home place",
            &settings,
            data.path(),
            Some(home.path()),
        )
        .expect("create");
        assert!(
            created.path.starts_with(home.path().join("checkouts")),
            "{}",
            created.path.display()
        );
    }

    #[test]
    fn a_repository_with_submodules_gets_them_checked_out() {
        let sub = tempfile::tempdir().expect("submodule");
        run(sub.path(), &["init", "-q", "-b", "main"]);
        run(sub.path(), &["config", "user.email", "wt@example.invalid"]);
        run(sub.path(), &["config", "user.name", "worktree test"]);
        std::fs::write(sub.path().join("inner.txt"), "inside\n").expect("inner");
        run(sub.path(), &["add", "inner.txt"]);
        run(sub.path(), &["commit", "-q", "-m", "sub init"]);

        let repo = repo();
        // The submodule lives in another temp folder, so git needs to be told
        // this repository's local `file` remotes are trusted.
        run(repo.path(), &["config", "protocol.file.allow", "always"]);
        run(
            repo.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.path().to_str().unwrap(),
                "sub",
            ],
        );
        run(repo.path(), &["commit", "-q", "-m", "add submodule"]);

        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "with a submodule",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert!(
            created.path.join("sub").join("inner.txt").is_file(),
            "the submodule is checked out in the worktree"
        );
    }

    #[test]
    fn a_missing_git_program_is_reported_as_not_installed() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let missing = data.path().join("no-such-git");
        git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = Some(missing));
        let result = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "task",
            &settings(None),
            data.path(),
            None,
        );
        git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = None);
        let error = result.expect_err("git cannot be run");
        assert_eq!(
            error,
            "git is not installed or could not be run on this host"
        );
    }

    #[test]
    fn an_invalid_prefix_never_generates_an_invalid_branch() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        for (suffix, prefix) in [("dot", ".hidden"), ("dots", "v1..2")] {
            let created = create_blocking(
                repo.path(),
                &WorktreeRequest::Generated(true),
                &format!("prefix {suffix}"),
                &h4rn_protocol::WorktreeSettings {
                    branch_prefix: prefix.into(),
                    ..h4rn_protocol::WorktreeSettings::default()
                },
                data.path(),
                None,
            )
            .expect("create with a prefix the generator must not trust");
            assert!(
                is_valid_branch_name(&created.branch),
                "`{prefix}` produced `{}`",
                created.branch
            );
            assert!(
                !created.branch.starts_with('.') && !created.branch.contains(".."),
                "`{prefix}` produced `{}`",
                created.branch
            );
            assert!(created.path.is_dir());
        }
    }

    /// Two creates that pick the same folder never delete each other's
    /// checkout: the loser refuses, or moves to the next generated suffix.
    #[test]
    fn concurrent_creates_for_one_folder_leave_the_winners_checkout_intact() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let settings = settings(None);

        // A named branch has one folder; exactly one call can own it.
        for round in 0..30 {
            let branch = format!("shared-topic-{round}");
            let barrier = std::sync::Barrier::new(2);
            let results = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..2)
                    .map(|_| {
                        let barrier = &barrier;
                        let branch = branch.as_str();
                        let repo = repo.path();
                        let data = data.path();
                        let settings = &settings;
                        scope.spawn(move || {
                            barrier.wait();
                            create_blocking(
                                repo,
                                &WorktreeRequest::Named(branch.into()),
                                "same task",
                                settings,
                                data,
                                None,
                            )
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("join"))
                    .collect::<Vec<_>>()
            });

            let (winners, losers): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
            assert_eq!(
                winners.len(),
                1,
                "round {round}: exactly one create owns the folder"
            );
            let winner = winners.into_iter().next().expect("winner").expect("ok");
            assert!(winner.path.is_dir(), "round {round}: the checkout exists");
            assert!(
                winner.path.join("README.md").is_file(),
                "round {round}: the checkout still has its files"
            );
            let loser = losers
                .into_iter()
                .next()
                .expect("loser")
                .expect_err("the loser is refused");
            assert!(
                loser.contains("a folder already exists")
                    || loser.contains("already checked out at"),
                "round {round}: {loser}"
            );
        }

        // A generated name may use the next suffix instead of failing, but no
        // successful call may lose the checkout another call made.
        for round in 0..30 {
            let barrier = std::sync::Barrier::new(2);
            let results = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..2)
                    .map(|_| {
                        let barrier = &barrier;
                        let repo = repo.path();
                        let data = data.path();
                        let settings = &settings;
                        scope.spawn(move || {
                            barrier.wait();
                            create_blocking(
                                repo,
                                &WorktreeRequest::Generated(true),
                                "same task",
                                settings,
                                data,
                                None,
                            )
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("join"))
                    .collect::<Vec<_>>()
            });

            let winners: Vec<_> = results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .collect();
            assert!(
                !winners.is_empty(),
                "round {round}: one create must win: {results:?}"
            );
            for winner in &winners {
                assert!(
                    winner.path.join("README.md").is_file(),
                    "round {round}: {} lost its files: {results:?}",
                    winner.path.display()
                );
            }
            let mut paths: Vec<_> = winners.iter().map(|winner| winner.path.clone()).collect();
            paths.sort();
            paths.dedup();
            assert_eq!(
                paths.len(),
                winners.len(),
                "round {round}: each create owns a distinct checkout"
            );
        }
    }

    /// A failed `git worktree add` that removes its own folder must not delete
    /// a checkout a second create made in that window.
    ///
    /// A required smudge filter fails the first create's checkout (git removes
    /// the folder itself), while the second create attaches a branch made
    /// before the filter existed and checks out the same folder successfully.
    #[test]
    fn a_failed_add_never_deletes_a_checkout_a_second_create_made() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let base = git::run_with_timeout(repo.path(), ["rev-parse", "HEAD"], CREATE_TIMEOUT)
            .expect("base commit");
        // The filter only exists from the next commit on, so a branch made from
        // `base` still checks out.
        std::fs::write(repo.path().join(".gitattributes"), "*.md filter=broken\n")
            .expect("attributes");
        run(repo.path(), &["config", "filter.broken.smudge", "false"]);
        run(repo.path(), &["config", "filter.broken.clean", "cat"]);
        run(repo.path(), &["config", "filter.broken.required", "true"]);
        run(repo.path(), &["add", ".gitattributes"]);
        run(repo.path(), &["commit", "-q", "-m", "attributes"]);

        let location = data.path().join("worktrees");
        let project = project_name(repo.path());
        for round in 0..30 {
            // `race/<n>` (new branch at the failing HEAD) and `race-<n>` (an
            // existing branch from before the filter) share one folder.
            let failing = format!("race/{round}");
            let safe = format!("race-{round}");
            run(repo.path(), &["branch", &safe, &base]);
            let dest = checkout_path(&location, &project, &failing);
            let paused = std::sync::Arc::new(std::sync::Barrier::new(2));
            let release = std::sync::Arc::new(std::sync::Barrier::new(2));
            test_hooks::pause_before_cleanup_removal(dest.clone(), paused.clone(), release.clone());

            let settings = settings(None);
            let (first, second) = std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    create_blocking(
                        repo.path(),
                        &WorktreeRequest::Named(failing.clone()),
                        "racing create",
                        &settings,
                        data.path(),
                        None,
                    )
                });
                // The first create has failed and its cleanup is paused where
                // the old code would delete next. The claim must still be held
                // there, or a second create could register a checkout in the
                // gap.
                paused.wait();
                let held = test_hooks::claim_held(&location, &project, &dest);
                if !held {
                    // Let the paused cleanup and its thread finish before
                    // reporting, so a failed check never deadlocks the scope.
                    release.wait();
                    let _ = first.join();
                    panic!(
                        "round {round}: the failed create still holds the folder while its cleanup is paused"
                    );
                }
                let second = scope.spawn(|| {
                    create_blocking(
                        repo.path(),
                        &WorktreeRequest::Named(safe.clone()),
                        "second create",
                        &settings,
                        data.path(),
                        None,
                    )
                });
                // The second create is blocked on the claim, so it can only
                // start once the first cleanup has released it.
                release.wait();
                (
                    first.join().expect("first join"),
                    second.join().expect("second join"),
                )
            });
            assert!(
                first.is_err(),
                "round {round}: the failing add is reported: {first:?}"
            );
            let second = second.unwrap_or_else(|error| {
                panic!("round {round}: the second create succeeds: {error}")
            });
            assert_eq!(second.path, dest, "round {round}: one folder");
            assert!(
                second.path.join("README.md").is_file(),
                "round {round}: the second create's checkout survives"
            );
            assert!(
                git::list_worktrees(repo.path())
                    .expect("list")
                    .iter()
                    .any(|worktree| worktree.path == second.path),
                "round {round}: the second create's registration survives"
            );
            test_hooks::clear_failed_add_hooks(&dest);
        }
    }

    /// Rolling back a thread's worktree never removes a replacement checkout
    /// that another thread created at the same folder.
    #[test]
    fn a_rollback_removes_only_its_own_checkout() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let first = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature/login".into()),
            "first",
            &settings(None),
            data.path(),
            None,
        )
        .expect("first create");
        // The original checkout disappears (an external delete and prune), so
        // the folder is free for another thread.
        std::fs::remove_dir_all(&first.path).expect("remove first");
        let _ = git::run_with_timeout(
            repo.path(),
            ["worktree", "prune", "--expire", "now"],
            CREATE_TIMEOUT,
        );
        let replacement = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature-login".into()),
            "replacement",
            &settings(None),
            data.path(),
            None,
        )
        .expect("replacement create");
        assert_eq!(replacement.path, first.path);
        std::fs::write(replacement.path.join("replacement.txt"), "kept\n").expect("marker");

        first.rollback();

        assert!(
            replacement.path.join("replacement.txt").is_file(),
            "the replacement's files are untouched"
        );
        assert!(
            replacement.path.join("README.md").is_file(),
            "the replacement is still a checkout"
        );
        assert!(
            git::list_worktrees(repo.path())
                .expect("list")
                .iter()
                .any(|worktree| worktree.path == replacement.path),
            "the replacement's registration is untouched"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/feature-login"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "the replacement's branch is untouched"
        );
    }

    /// The admin-dir half of the rollback guard: a checkout moved with git keeps
    /// its admin dir name, so a replacement at the old path gets `…1`. Only
    /// comparing the admin dir can tell that replacement from the original;
    /// the owner marker alone still names this rollback.
    #[test]
    fn a_rollback_ignores_a_replacement_under_another_admin_dir() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let first = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature/login".into()),
            "first",
            &settings(None),
            data.path(),
            None,
        )
        .expect("first create");
        // Moving a checkout updates its registration in place; its admin dir
        // keeps the old name (`feature-login`) and this create's owner marker.
        let moved = data.path().join("moved");
        std::fs::create_dir_all(&moved).expect("moved");
        let moved_path = moved.join("feature-login");
        run(
            repo.path(),
            &[
                "worktree",
                "move",
                first.path.to_str().expect("path"),
                moved_path.to_str().expect("path"),
            ],
        );
        // Registering a checkout at the old path now needs a different admin
        // dir, so `.git` names `feature-login1` while `feature-login` still
        // names this rollback and holds its marker.
        let replacement = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature-login".into()),
            "replacement",
            &settings(None),
            data.path(),
            None,
        )
        .expect("replacement create");
        assert_eq!(replacement.path, first.path);
        let admin = repo.path().join(".git/worktrees/feature-login1");
        assert!(
            admin.is_dir(),
            "the replacement registered under a new admin dir: {}",
            admin.display()
        );
        std::fs::write(replacement.path.join("replacement.txt"), "kept\n").expect("marker");

        first.rollback();

        assert!(
            replacement.path.join("replacement.txt").is_file(),
            "the replacement's files are untouched"
        );
        assert!(
            git::list_worktrees(repo.path())
                .expect("list")
                .iter()
                .any(|worktree| worktree.path == replacement.path),
            "the replacement's registration is untouched"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/feature-login"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "the replacement's branch is untouched"
        );
        assert!(
            moved_path.join("README.md").is_file(),
            "the moved checkout is untouched"
        );
    }

    /// Cleanup removes only the very directory the claim created: a folder
    /// replaced by a different directory before the removal is left alone.
    #[cfg(unix)]
    #[test]
    fn a_failed_add_leaves_a_replacement_directory_alone() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        std::fs::write(repo.path().join(".gitattributes"), "*.md filter=broken\n")
            .expect("attributes");
        run(repo.path(), &["config", "filter.broken.smudge", "false"]);
        run(repo.path(), &["config", "filter.broken.clean", "cat"]);
        run(repo.path(), &["config", "filter.broken.required", "true"]);
        run(repo.path(), &["add", ".gitattributes"]);
        run(repo.path(), &["commit", "-q", "-m", "attributes"]);

        let location = data.path().join("worktrees");
        let project = project_name(repo.path());
        let dest = checkout_path(&location, &project, "broken/add");
        let paused = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        test_hooks::pause_before_cleanup_removal(dest.clone(), paused.clone(), release.clone());

        let settings = settings(None);
        let repo_path = repo.path().to_path_buf();
        let data_path = data.path().to_path_buf();
        let failing_data = data_path.clone();
        let failing_dest = dest.clone();
        let handle = std::thread::spawn(move || {
            create_blocking(
                &repo_path,
                &WorktreeRequest::Named("broken/add".into()),
                "broken",
                &settings,
                &failing_data,
                None,
            )
        });
        paused.wait();
        // Another process put a different directory at the same path in the
        // window before cleanup's recursive removal. Its inode must differ from
        // the one the claim recorded, so build candidates until one does (a
        // freshly freed inode can be handed out again) and move it into place.
        let claimed =
            test_hooks::claim_identity(&failing_dest).expect("the claim recorded its folder");
        let scratch = data_path.join("replacement");
        std::fs::create_dir_all(&scratch).expect("scratch");
        let mut replacement = None;
        let mut keep_alive = Vec::new();
        for attempt in 0..8 {
            let candidate = scratch.join(format!("dir-{attempt}"));
            std::fs::create_dir(&candidate).expect("candidate");
            if dir_identity(&candidate) == Some(claimed) {
                // Hold this one so its inode cannot be reused by the next.
                keep_alive.push(candidate);
            } else {
                replacement = Some(candidate);
                break;
            }
        }
        let replacement = replacement.expect("a directory with a different identity");
        let _ = std::fs::remove_dir_all(&failing_dest);
        std::fs::rename(&replacement, &failing_dest).expect("put the replacement in place");
        std::fs::write(failing_dest.join("new.txt"), "kept\n").expect("replacement file");
        release.wait();
        let result = handle.join().expect("join");
        assert!(result.is_err(), "the failing add is reported: {result:?}");
        assert!(
            failing_dest.join("new.txt").is_file(),
            "cleanup never removes a directory it did not create"
        );
        drop(keep_alive);
        test_hooks::clear_failed_add_hooks(&dest);
    }

    /// A retry of the same named branch that read the branch before the
    /// cleanup waited for the folder still creates it: the cleanup holds the
    /// claim while it deletes the old branch, and the retry re-reads the branch
    /// once it holds the claim.
    #[test]
    fn a_retry_of_the_same_branch_survives_a_racing_rollback() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let location = data.path().join("worktrees");
        let project = project_name(repo.path());
        let dest = checkout_path(&location, &project, "feature/login");
        let first = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature/login".into()),
            "first",
            &settings(None),
            data.path(),
            None,
        )
        .expect("first create");
        assert_eq!(first.path, dest);

        let branch_pause = Arc::new(std::sync::Barrier::new(2));
        let branch_release = Arc::new(std::sync::Barrier::new(2));
        test_hooks::pause_before_branch_delete(
            dest.clone(),
            branch_pause.clone(),
            branch_release.clone(),
        );
        let claim_pause = Arc::new(std::sync::Barrier::new(2));
        let claim_release = Arc::new(std::sync::Barrier::new(2));
        test_hooks::pause_before_claim(dest.clone(), claim_pause.clone(), claim_release.clone());

        let repo_path = repo.path().to_path_buf();
        let data_path = data.path().to_path_buf();
        let settings = settings(None);
        let rollback = std::thread::spawn(move || first.rollback());
        // The rollback is between removing the checkout and deleting the
        // branch; it must still hold the claim so no retry can start here.
        branch_pause.wait();
        assert!(
            test_hooks::claim_held(&location, &project, &dest),
            "the rollback holds the folder while it deletes the branch"
        );
        // The retry lists the branch while it still exists, then waits for the
        // claim. It is paused just before taking it.
        let retry = std::thread::spawn(move || {
            create_blocking(
                &repo_path,
                &WorktreeRequest::Named("feature/login".into()),
                "retry",
                &settings,
                &data_path,
                None,
            )
        });
        claim_pause.wait();
        // Let the rollback delete the branch the retry read a moment ago.
        branch_release.wait();
        rollback.join().expect("rollback");
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/feature/login"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the rollback removed the old branch"
        );
        claim_release.wait();
        let retry = retry.join().expect("join");
        let retry = retry.unwrap_or_else(|error| panic!("the retry succeeds: {error}"));
        assert_eq!(retry.path, dest);
        assert!(
            retry.path.join("README.md").is_file(),
            "the retry's checkout has its files"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/feature/login"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "the retry recreated the branch it was going to attach"
        );
        test_hooks::clear_failed_add_hooks(&dest);
    }

    /// A tag that shares a branch's name must not hide the branch from the
    /// cleanup: the full ref is checked, and the branch this create made is
    /// deleted even when the tag points at an older commit.
    #[test]
    fn a_tag_with_a_branch_name_does_not_block_cleanup() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        // The tag points at the first commit; the branch this create makes
        // starts at the second one.
        run(repo.path(), &["tag", "release"]);
        std::fs::write(repo.path().join("README.md"), "second\n").expect("second");
        run(repo.path(), &["commit", "-q", "-am", "second"]);
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("release".into()),
            "release",
            &settings(None),
            data.path(),
            None,
        )
        .expect("the branch is made next to the tag");
        assert!(created.created_branch);

        created.rollback();

        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/release"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is removed"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/tags/release"],
                CREATE_TIMEOUT
            )
            .is_ok(),
            "the tag is left alone"
        );
    }

    /// A failing `post-checkout` hook leaves the branch and registration git
    /// made; a failed create removes both, and the empty folder it claimed.
    #[cfg(unix)]
    #[test]
    fn a_failed_post_checkout_hook_leaves_no_branch_or_registration() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let dest = checkout_path(
            &data.path().join("worktrees"),
            &project_name(repo.path()),
            "hook/x",
        );
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("hook/x".into()),
            "hook",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("the post-checkout hook fails the add");
        assert!(error.contains("could not create the worktree"), "{error}");
        assert!(
            git::list_worktrees(repo.path()).expect("list").is_empty(),
            "no registration is left behind"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/hook/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch the failed add made is removed"
        );
        assert!(!dest.exists(), "the claimed folder is removed");
    }

    /// With a symlinked location git lists the resolved path; the cleanup still
    /// finds and removes the registration and branch it made.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_location_still_cleans_up_a_failed_add() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let real = data.path().join("real");
        std::fs::create_dir(&real).expect("real");
        let link = data.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("link/x".into()),
            "symlink",
            &settings(Some(&link)),
            data.path(),
            None,
        )
        .expect_err("the post-checkout hook fails the add");
        assert!(error.contains("could not create the worktree"), "{error}");
        assert!(
            git::list_worktrees(repo.path()).expect("list").is_empty(),
            "the registration is found through the resolved path and removed"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/link/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch is removed too"
        );
        let dest = real
            .join(project_name(repo.path()))
            .join(folder_name("link/x"));
        assert!(!dest.exists(), "{} is gone", dest.display());
    }

    /// A location inside the main checkout is refused before anything exists.
    #[test]
    fn a_location_inside_the_checkout_is_refused() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let inside = repo.path().join("inside");
        std::fs::create_dir(&inside).expect("inside");
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("refused/x".into()),
            "inside",
            &settings(Some(&inside)),
            data.path(),
            None,
        )
        .expect_err("a checkout inside the main checkout is refused");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            !inside.join(project_name(repo.path())).exists(),
            "nothing was created inside the main checkout"
        );
        assert!(
            !git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/refused/x"],
                CREATE_TIMEOUT
            )
            .is_ok(),
            "no branch was made"
        );
    }

    /// A location that is the project's parent folder puts the checkout
    /// (`<location>/<project>/<branch>`) inside the main checkout, so it is
    /// refused before anything exists.
    #[test]
    fn a_location_that_is_the_projects_parent_is_refused() {
        let root = tempfile::tempdir().expect("root");
        let repo = root.path().join("api");
        std::fs::create_dir(&repo).expect("api");
        init_repo(&repo);
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &repo,
            &WorktreeRequest::Named("parent/x".into()),
            "parent",
            &settings(Some(root.path())),
            data.path(),
            None,
        )
        .expect_err("the destination is the main checkout");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            !git::run_with_timeout(
                &repo,
                ["rev-parse", "--verify", "refs/heads/parent/x"],
                CREATE_TIMEOUT
            )
            .is_ok(),
            "no branch was made"
        );
        assert!(
            !root.path().join("api").join("parent-x").exists(),
            "nothing was created inside the main checkout"
        );
    }

    /// A thread can start from a linked worktree, whose main checkout lives
    /// elsewhere. The project folder is named after the main checkout, so a
    /// location holding the main checkout's parent would land the new checkout
    /// in it even though it is nowhere near the workspace.
    #[test]
    fn a_linked_worktree_refuses_a_location_inside_the_main_checkout() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        init_repo(&main);
        let linked = root.path().join("linked");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &linked,
            &WorktreeRequest::Named("linked/x".into()),
            "linked",
            &settings(Some(root.path())),
            data.path(),
            None,
        )
        .expect_err("the destination is the main checkout, not the linked one");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            error.contains(&main.to_string_lossy().into_owned()),
            "{error}"
        );
        let dest = main.join(folder_name("linked/x"));
        assert!(
            !dest.exists(),
            "nothing was created inside the main checkout"
        );
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/linked/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
        assert_eq!(
            git::list_worktrees(&main).expect("list").len(),
            1,
            "the checkout that was already there is still the only one"
        );
    }

    /// A bare repository's worktree has its git directory (the common dir, and
    /// the folder the project is named after) elsewhere. A location holding
    /// that directory would put the new checkout inside it, so it is refused.
    #[test]
    fn a_bare_repository_worktree_refuses_a_location_inside_its_git_directory() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        std::fs::create_dir(&source).expect("source");
        init_repo(&source);
        let bare = root.path().join("repo.git");
        run(
            root.path(),
            &["clone", "-q", "--bare", "source", "repo.git"],
        );
        let linked = root.path().join("linked");
        run(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &linked,
            &WorktreeRequest::Named("bare/x".into()),
            "bare",
            &settings(Some(root.path())),
            data.path(),
            None,
        )
        .expect_err("the destination is the bare repository's git directory");
        assert!(
            error.contains("inside the bare repository's git directory"),
            "{error}"
        );
        assert!(
            error.contains("choose a folder outside it"),
            "the refusal says what to do: {error}"
        );
        assert!(
            error.contains(&bare.to_string_lossy().into_owned()),
            "{error}"
        );
        let dest = bare.join(folder_name("bare/x"));
        assert!(
            !dest.exists(),
            "nothing was created inside the git directory"
        );
        assert!(
            git::run_with_timeout(
                &bare,
                ["rev-parse", "--verify", "refs/heads/bare/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
        assert_eq!(
            git::list_worktrees(&bare).expect("list").len(),
            1,
            "the checkout that was already there is still the only one"
        );
    }

    /// A tag that shares a name with a branch must not hide the branch: the
    /// branch is attached, not recreated.
    #[test]
    fn a_tag_with_a_branch_name_does_not_hide_the_branch() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        run(repo.path(), &["branch", "release"]);
        run(repo.path(), &["tag", "release"]);
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("release".into()),
            "release",
            &settings(None),
            data.path(),
            None,
        )
        .expect("the existing branch is attached");
        assert!(
            !created.created_branch,
            "the branch was seen next to its same-named tag"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/release"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "the existing branch is still there"
        );
    }

    #[test]
    fn invalid_prefix_characters_become_hyphens() {
        assert_eq!(branch_prefix("feat:"), "feat-/");
        assert_eq!(branch_prefix("feat"), "feat/");
        assert_eq!(branch_prefix("feat/"), "feat/");
        assert_eq!(branch_prefix(""), "");
        assert_eq!(branch_prefix("  "), "");
        assert_eq!(branch_prefix("a  b"), "a--b/");
    }

    /// A `--separate-git-dir` repository names its common dir something other
    /// than `.git`, so the main checkout is not found by the common dir's name.
    /// The registration still names it, and a location that holds it must be
    /// refused from a linked worktree of that repository.
    #[test]
    fn a_separate_git_dir_worktree_refuses_a_location_inside_the_main_checkout() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        let gitdir = root.path().join("api.git");
        run(
            root.path(),
            &[
                "init",
                "-q",
                "-b",
                "main",
                "--separate-git-dir",
                gitdir.to_str().unwrap(),
                main.to_str().unwrap(),
            ],
        );
        run(&main, &["config", "user.email", "wt@example.invalid"]);
        run(&main, &["config", "user.name", "worktree test"]);
        std::fs::write(main.join("README.md"), "hello\n").expect("readme");
        run(&main, &["add", "README.md"]);
        run(&main, &["commit", "-q", "-m", "init"]);
        assert_eq!(git::repo_of(&main), None, "the common dir is not `.git`");

        let linked = root.path().join("linked");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );

        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &linked,
            &WorktreeRequest::Named("separate/x".into()),
            "separate",
            &settings(Some(root.path())),
            data.path(),
            None,
        )
        .expect_err("the destination is the separate git dir repository's main checkout");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            error.contains(&main.to_string_lossy().into_owned()),
            "the refusal names the main checkout: {error}"
        );
        let dest = main.join(folder_name("separate/x"));
        assert!(
            !dest.exists(),
            "nothing was created inside the main checkout"
        );
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/separate/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
        assert!(
            git::list_worktrees(&main)
                .expect("list")
                .iter()
                .all(|worktree| worktree.branch.as_deref() != Some("separate/x")),
            "only the linked checkout that was already there is registered"
        );
    }

    /// A submodule's common dir lives under its superproject, so the main
    /// checkout is not found by the common dir's name either. Its registration
    /// names it, and a location that holds it must be refused from a linked
    /// worktree of the submodule.
    #[test]
    fn a_submodule_worktree_refuses_a_location_inside_its_main_checkout() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        std::fs::create_dir(&source).expect("source");
        init_repo(&source);
        let host = root.path().join("host");
        std::fs::create_dir(&host).expect("host");
        init_repo(&host);
        run(
            &host,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                source.to_str().unwrap(),
                "sub",
            ],
        );
        run(&host, &["commit", "-q", "-m", "add a submodule"]);

        let sub = host.join("sub");
        let linked = root.path().join("linked");
        run(
            &sub,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        assert_eq!(git::repo_of(&linked), None, "the common dir is not `.git`");

        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &linked,
            &WorktreeRequest::Named("submodule/x".into()),
            "submodule",
            &settings(Some(host.as_path())),
            data.path(),
            None,
        )
        .expect_err("the destination is the submodule's main checkout");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            error.contains(&sub.to_string_lossy().into_owned()),
            "the refusal names the submodule checkout: {error}"
        );
        let dest = sub.join(folder_name("submodule/x"));
        assert!(
            !dest.exists(),
            "nothing was created inside the submodule checkout"
        );
        assert!(
            git::run_with_timeout(
                &sub,
                ["rev-parse", "--verify", "refs/heads/submodule/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
        assert!(
            git::list_worktrees(&sub)
                .expect("list")
                .iter()
                .all(|worktree| worktree.branch.as_deref() != Some("submodule/x")),
            "only the linked checkout that was already there is registered"
        );
    }

    /// A location inside a sibling linked worktree puts the new checkout in a
    /// working tree that belongs to another thread, so it is refused as well.
    #[test]
    fn a_location_inside_a_sibling_worktree_is_refused() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        init_repo(&main);
        let sibling = root.path().join("feature");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                sibling.to_str().unwrap(),
            ],
        );
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &main,
            &WorktreeRequest::Named("sibling/x".into()),
            "sibling",
            &settings(Some(&sibling)),
            data.path(),
            None,
        )
        .expect_err("the destination is inside another worktree");
        assert!(error.contains("another worktree"), "{error}");
        assert!(
            error.contains(&sibling.to_string_lossy().into_owned()),
            "the refusal names the sibling: {error}"
        );
        let dest = sibling
            .join(project_name(&main))
            .join(folder_name("sibling/x"));
        assert!(!dest.exists(), "nothing was created inside the sibling");
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/sibling/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
        assert_eq!(
            git::list_worktrees(&main).expect("list").len(),
            1,
            "the sibling is still the only linked checkout"
        );
    }

    /// A create that never reaches its thread leaves a record; the next startup
    /// removes the checkout, its registration and the branch h4rn made.
    #[test]
    fn an_interrupted_create_is_swept_before_the_next_start() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("interrupted/x".into()),
            "interrupted",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert!(created.path.is_dir());
        assert!(
            data.path().join("worktrees-pending").is_dir(),
            "the create wrote a recovery record"
        );

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![created.path.clone()]);
        assert!(!created.path.exists(), "the checkout is gone");
        assert!(
            git::list_worktrees(repo.path()).expect("list").is_empty(),
            "the registration is gone"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/interrupted/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is gone"
        );
    }

    /// A worktree h4rn did not make is never removed, even when a recovery
    /// record happens to name its path: the owner marker does not match.
    #[test]
    fn a_worktree_h4rn_did_not_make_survives_the_sweep() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        init_repo(&main);
        let user = root.path().join("user-worktree");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mine",
                user.to_str().unwrap(),
            ],
        );
        let data = tempfile::tempdir().expect("data");
        write_pending_record(
            data.path(),
            &main,
            &user,
            "mine",
            false,
            None,
            "not-h4rns-owner",
        )
        .expect("record");

        sweep_pending(data.path(), &[]);
        assert!(user.is_dir(), "the user's checkout is left alone");
        assert_eq!(
            git::list_worktrees(&main).expect("list").len(),
            1,
            "its registration is left alone"
        );
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/mine"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "its branch is left alone"
        );
    }

    /// A record whose checkout a thread now uses is finished, not swept: the
    /// thread owns it, so both stay.
    #[test]
    fn an_in_use_worktree_survives_the_sweep() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("in-use/x".into()),
            "in use",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");

        let in_use = vec![created.path.to_string_lossy().into_owned()];
        assert!(sweep_pending(data.path(), &in_use).is_empty());
        assert!(created.path.is_dir(), "the thread's checkout stays");
        assert_eq!(
            git::list_worktrees(repo.path()).expect("list").len(),
            1,
            "its registration stays"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/in-use/x"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "its branch stays"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_none(),
            "the recovered record is gone"
        );
    }

    /// A shutdown rolls back every create no thread adopted before it returns.
    #[tokio::test]
    async fn shutdown_rolls_back_every_create_no_thread_kept() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let creator = WorktreeCreator::new(
            data.path().to_path_buf(),
            Arc::new(crate::worktrees::settings::WorktreeSettingsStore::in_memory()),
        );
        let created = creator
            .create_for_thread(
                repo.path().to_str().unwrap(),
                &WorktreeRequest::Named("shutdown/x".into()),
                "shutdown",
            )
            .await
            .expect("create");
        assert!(created.path.is_dir());
        assert_eq!(
            git::list_worktrees(repo.path()).expect("list").len(),
            1,
            "the checkout exists before shutdown"
        );

        creator.shutdown().await;
        assert!(!created.path.exists(), "the checkout is rolled back");
        assert!(
            git::list_worktrees(repo.path()).expect("list").is_empty(),
            "the registration is rolled back"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/shutdown/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is rolled back"
        );
    }

    /// A checkout its thread kept is not rolled back by a later shutdown.
    #[tokio::test]
    async fn shutdown_leaves_a_create_its_thread_kept() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let creator = WorktreeCreator::new(
            data.path().to_path_buf(),
            Arc::new(crate::worktrees::settings::WorktreeSettingsStore::in_memory()),
        );
        let created = creator
            .create_for_thread(
                repo.path().to_str().unwrap(),
                &WorktreeRequest::Named("kept/x".into()),
                "kept",
            )
            .await
            .expect("create");
        created.keep();

        creator.shutdown().await;
        assert!(created.path.is_dir(), "the thread's checkout stays");
        assert_eq!(
            git::list_worktrees(repo.path()).expect("list").len(),
            1,
            "its registration stays"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/kept/x"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "its branch stays"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_none(),
            "its recovery record is gone"
        );
    }
}
