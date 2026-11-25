//! Creating a git worktree for a new thread.
//!
//! A thread that asks for isolation gets a checkout of its project's current
//! `HEAD` on its own branch, and the thread's workspace becomes that checkout.
//! The main checkout is only read: every git command here runs `-C` against
//! it, and `git worktree add` never touches its index or working tree.

use std::io::Write;
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
    record: Option<PendingRecordFile>,
    /// The creates still waiting for their thread. A shutdown rolls them back
    /// and waits, instead of losing the rollback with the process.
    pending: Option<Arc<PendingWorktrees>>,
}

/// One create that has not been adopted by a thread yet.
///
/// Written atomically under the daemon's data before `git worktree add` runs,
/// and updated as cleanup progresses. The owner token is the marker in the
/// registration's admin dir, so a sweep only removes a checkout this create
/// made and never a worktree someone else made. `admin` is filled in as soon
/// as git has made the registration; a record without it is an intent whose
/// create was interrupted before ownership could be proved.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct PendingRecord {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    created_branch: bool,
    head: Option<String>,
    owner: String,
    /// The registration's admin dir (`<common>/worktrees/<id>`), once git has
    /// made it and this create has marked it.
    #[serde(default)]
    admin: Option<PathBuf>,
    /// Whether the checkout's removal has started. Written before
    /// `git worktree remove` runs, so a kill between the removal and the flag
    /// below still tells the next startup the checkout is gone.
    #[serde(default)]
    removing: bool,
    /// Whether the checkout and its registration are gone.
    #[serde(default)]
    checkout_removed: bool,
    /// Whether the branch this create made is gone (or was never h4rn's).
    #[serde(default)]
    branch_removed: bool,
}

/// One record on disk, and the data last read or written for it.
#[derive(Clone, Debug)]
struct PendingRecordFile {
    path: PathBuf,
    data: PendingRecord,
}

/// Creates whose thread does not exist yet, shared with the creator so a
/// shutdown can wait for every rollback.
#[derive(Default)]
struct PendingWorktrees {
    created: std::sync::Mutex<Vec<CreatedWorktree>>,
    /// Rollbacks already started in the background, which a shutdown waits for.
    rollbacks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Create admission. Closing it stops new creates; the wait drains the
    /// ones already admitted so shutdown never takes a create mid-adoption.
    admission: std::sync::Mutex<Admission>,
    admission_changed: tokio::sync::Notify,
}

#[derive(Default)]
struct Admission {
    closed: bool,
    active: usize,
}

/// One in-flight thread-creation admission.
///
/// Held from before the checkout is made until the thread has durably adopted
/// it (or its rollback was started), so a shutdown can close admission and
/// wait for either outcome instead of taking the checkout.
pub struct CreateAdmission {
    pending: Arc<PendingWorktrees>,
}

impl Drop for CreateAdmission {
    fn drop(&mut self) {
        let mut admission = lock(&self.pending.admission);
        admission.active = admission.active.saturating_sub(1);
        drop(admission);
        self.pending.admission_changed.notify_waiters();
    }
}

impl std::fmt::Debug for PendingWorktrees {
    /// The created entries hold this set, so printing them would recurse.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingWorktrees")
            .field("created", &lock(&self.created).len())
            .field("rollbacks", &lock(&self.rollbacks).len())
            .field("active", &lock(&self.admission).active)
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

    fn admit(self: &Arc<Self>) -> Result<CreateAdmission, String> {
        let mut admission = lock(&self.admission);
        if admission.closed {
            return Err("the daemon is shutting down; no worktree was created".to_owned());
        }
        admission.active += 1;
        Ok(CreateAdmission {
            pending: Arc::clone(self),
        })
    }

    fn close_admission(&self) {
        lock(&self.admission).closed = true;
        self.admission_changed.notify_waiters();
    }

    async fn wait_for_admissions(&self) {
        loop {
            let notified = self.admission_changed.notified();
            if lock(&self.admission).active == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Panics in a test would poison the lock; the sets are still usable.
fn lock<T>(set: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    set.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What one rollback attempt did.
struct Cleanup {
    /// Nothing is left to retry: the record is retired.
    finished: bool,
    /// This attempt removed a checkout or deleted a branch h4rn made.
    did_work: bool,
}

impl Cleanup {
    /// Nothing was proved enough to act on; the record stays for a retry.
    const NONE: Self = Self {
        finished: false,
        did_work: false,
    };
    /// The record is finished and this attempt had nothing left to remove.
    const FINISHED: Self = Self {
        finished: true,
        did_work: false,
    };
}

impl CreatedWorktree {
    /// The thread now owns this checkout, so neither a shutdown nor the next
    /// startup's sweep may remove it.
    pub fn keep(&self) {
        self.retire();
    }

    /// Starts the rollback without waiting for it, off the async runtime when
    /// there is one, and records it so a shutdown can wait for it.
    pub fn rollback_detached(self) {
        let pending = self.pending.clone();
        // The in-flight task now owns this cleanup; leave it out of the
        // created set so shutdown waits on the task, not on a second rollback
        // of the same checkout.
        if let Some(pending) = &pending {
            pending.remove(&self.path);
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let task = handle.spawn_blocking(move || {
                    let _ = self.rollback();
                });
                if let Some(pending) = pending {
                    pending.track(task);
                }
            }
            // Outside a runtime (a plain unit test) it runs inline.
            Err(_) => {
                let _ = self.rollback();
            }
        }
    }

    /// Best-effort removal after the thread this was made for never started.
    ///
    /// The cleanup is tied to the registration this checkout had when it was
    /// made: if `.git` names a different admin dir, that admin dir's `gitdir`
    /// no longer points back here, its `HEAD` is another branch, or the marker
    /// this call wrote is gone (another thread's checkout now lives at the same
    /// path), nothing is removed. The identity is checked again immediately
    /// before git is asked to remove anything.
    ///
    /// Once the checkout itself is gone, the record is the proof: the record
    /// says this create removed the checkout, so only the branch step is left,
    /// guarded by the branch not being registered, not being checked out, and
    /// still pointing at the commit this create started from.
    ///
    /// Returns `true` when this attempt's record is finished (the checkout is
    /// gone, or the path was positively identified as a replacement). A failed
    /// removal leaves the record for the next startup to retry.
    pub fn rollback(&self) -> bool {
        self.rollback_with(true).finished
    }

    /// [`Self::rollback`], optionally deferring instead of waiting for another
    /// create that holds the folder's claim. Startup uses the deferring form so
    /// a busy create never stalls the daemon.
    fn rollback_with(&self, wait: bool) -> Cleanup {
        #[cfg(test)]
        test_hooks::before_rollback(&self.path);
        // A retry of the same branch may claim this folder the moment the
        // checkout leaves. Hold the claim across the checks, the removal and
        // the branch step, so it can only start before or after this rollback
        // instead of in the middle of it.
        let claim = match self.claim(wait) {
            Ok(Some(claim)) => claim,
            Ok(None) => {
                tracing::info!(
                    path = %self.path.display(),
                    "another create holds the worktree folder; deferring recovery"
                );
                return Cleanup::NONE;
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %self.path.display(),
                    "could not claim the worktree folder; leaving it in place"
                );
                return Cleanup::NONE;
            }
        };
        let _claim = claim;
        if self.checkout_removed() {
            // The checkout this create made is already gone, and the record
            // says so was its own. There is no registration left to prove, so
            // the record itself is the ownership proof; only the branch guards
            // remain.
            return self.finish_after_removal();
        }
        let Some(owner) = self.owner.as_deref() else {
            tracing::warn!(
                path = %self.path.display(),
                "refusing to roll back a worktree whose owner marker was not recorded at creation"
            );
            return Cleanup::NONE;
        };
        let mut admin = self.registration.clone().or_else(|| {
            self.record
                .as_ref()
                .and_then(|record| record.data.admin.clone())
        });
        // A crash after the marker was written but before the record learned
        // the admin id still leaves positive proof: the checkout's `.git` and
        // the marker name the same registration.
        if admin.is_none()
            && let Some(found) = worktree_gitdir(&self.path)
            && registration_owner(&found).as_deref() == Some(owner)
        {
            admin = Some(found);
        }
        // A kill between `git worktree add` and the marker leaves no marker in
        // the admin dir; the claim this create wrote into the destination's
        // lock file before the add is then the ownership proof.
        let claim_owner = claim_names_owner(&self.path, owner);
        let Some(admin) = admin else {
            if claim_owner && self.destination_gone_or_empty() {
                // A kill between recording the owner in the claim and
                // `git worktree add` leaves no registration at all. The claim
                // proves this create made the folder, and an absent (or empty)
                // destination has nothing to remove, so finish the branch step
                // and retire instead of retrying this record forever. Anything
                // in the folder belongs to someone else and is left alone.
                match std::fs::remove_dir(&self.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            path = %self.path.display(),
                            "could not remove an empty worktree folder; leaving it in place"
                        );
                        return Cleanup::NONE;
                    }
                }
                return self.finish_after_removal();
            }
            tracing::warn!(
                path = %self.path.display(),
                "refusing to roll back a worktree whose registration was not proved"
            );
            return Cleanup::NONE;
        };
        if !registration_proves(&admin, &self.path, owner, &self.branch, claim_owner) {
            if self.replacement_now(&admin) {
                // A different checkout lives here now; this attempt has
                // nothing left to remove.
                self.retire();
                return Cleanup::FINISHED;
            }
            tracing::warn!(
                admin = %admin.display(),
                path = %self.path.display(),
                "the registration no longer matches the create; leaving it alone"
            );
            return Cleanup::NONE;
        }
        #[cfg(test)]
        test_hooks::before_rollback_remove(&self.path);
        // Re-check after any hook and immediately before the destructive step:
        // the path or its registration may have been replaced in between.
        if !registration_proves(&admin, &self.path, owner, &self.branch, claim_owner) {
            if self.replacement_now(&admin) {
                self.retire();
                return Cleanup::FINISHED;
            }
            return Cleanup::NONE;
        }
        let mut removed_checkout = false;
        if !self.checkout_removed() {
            // The removal is marked before git runs: a kill between
            // `git worktree remove` and the progress write below must still
            // let the next start finish the branch step instead of retiring
            // the record and leaking the branch.
            if !self.save_removing(&admin) {
                return Cleanup::NONE;
            }
            // The removal is now on record; this is the last point before git
            // acts on it.
            #[cfg(test)]
            test_hooks::before_cleanup_removal(&self.path);
            let removed = git::run_with_timeout(
                &self.repo,
                [
                    std::ffi::OsStr::new("worktree"),
                    std::ffi::OsStr::new("remove"),
                    std::ffi::OsStr::new("--force"),
                    self.path.as_os_str(),
                ],
                CREATE_TIMEOUT,
            );
            if removed.is_err() {
                // A locked worktree is a retry, not a reason to forget the
                // record: once it is unlocked the next startup can finish.
                return Cleanup::NONE;
            }
            // The checkout and its registration are gone; record that before
            // the branch step, so a crash here still tells the next startup
            // the record is the ownership proof.
            if !self.save_progress(true, false) {
                return Cleanup {
                    finished: false,
                    did_work: true,
                };
            }
            removed_checkout = true;
        }
        if self.created_branch && !self.branch_removed() {
            let existed = branch_exists(&self.repo, &self.branch);
            if !delete_created_branch(&self.repo, &self.path, &self.branch, self.head.as_deref()) {
                return Cleanup {
                    finished: false,
                    did_work: removed_checkout,
                };
            }
            if !self.save_progress(true, true) {
                return Cleanup {
                    finished: false,
                    did_work: true,
                };
            }
            removed_checkout |= existed && !branch_exists(&self.repo, &self.branch);
        }
        self.retire();
        Cleanup {
            finished: true,
            did_work: removed_checkout,
        }
    }

    /// The last step of a rollback whose checkout is already gone.
    ///
    /// The record says this create removed its own checkout, so no
    /// registration is left to prove. Only a branch this create made is
    /// deleted, and [`delete_created_branch`] still refuses one that is
    /// registered, checked out, or no longer points at the commit this create
    /// started from.
    fn finish_after_removal(&self) -> Cleanup {
        let mut did_work = false;
        if self.created_branch && !self.branch_removed() {
            let existed = branch_exists(&self.repo, &self.branch);
            if !delete_created_branch(&self.repo, &self.path, &self.branch, self.head.as_deref()) {
                return Cleanup::NONE;
            }
            if !self.save_progress(true, true) {
                return Cleanup {
                    finished: false,
                    did_work,
                };
            }
            did_work = existed && !branch_exists(&self.repo, &self.branch);
        }
        self.retire();
        Cleanup {
            finished: true,
            did_work,
        }
    }

    /// Whether this path's registration was positively replaced by another
    /// checkout, rather than merely becoming unreadable.
    fn replacement_now(&self, expected: &Path) -> bool {
        match worktree_gitdir(&self.path) {
            // The path exists and `.git` names a different admin dir.
            Some(current) if current != expected => true,
            // Git reuses the admin name when the old registration is removed
            // and another checkout takes the folder. A fresh registration at
            // that name does not carry this create's marker.
            Some(_) => self
                .owner
                .as_deref()
                .is_some_and(|owner| registration_owner(expected).as_deref() != Some(owner)),
            // The path is gone. A registration that still exists under our
            // admin id is ours; one that vanished was removed (possibly by
            // git's own incomplete add), so there is nothing left to retry.
            None => !expected.exists(),
        }
    }

    fn checkout_removed(&self) -> bool {
        let Some(record) = &self.record else {
            return false;
        };
        record.data.checkout_removed
            // The removal was marked before git ran. If it was killed, or the
            // progress write failed, right after `git worktree remove`, the
            // record still says the checkout exists while both paths are gone.
            || (record.data.removing
                && !self.path.exists()
                && record
                    .data
                    .admin
                    .as_ref()
                    .is_some_and(|admin| !admin.exists()))
    }

    /// Whether this create's destination is gone or still an empty folder.
    ///
    /// A record interrupted before `git worktree add` never got a registration,
    /// so the only thing left to judge is the folder the claim created. An
    /// empty folder is that one; anything in it belongs to someone else.
    fn destination_gone_or_empty(&self) -> bool {
        match std::fs::read_dir(&self.path) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }

    fn branch_removed(&self) -> bool {
        self.record
            .as_ref()
            .is_some_and(|record| record.data.branch_removed)
    }

    /// Writes cleanup progress before the next destructive step starts.
    fn save_progress(&self, checkout_removed: bool, branch_removed: bool) -> bool {
        let Some(record) = &self.record else {
            return true;
        };
        let mut data = record.data.clone();
        data.checkout_removed |= checkout_removed;
        data.branch_removed |= branch_removed;
        match write_pending_record(&record.path, &data) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %record.path.display(),
                    "could not record worktree cleanup progress"
                );
                false
            }
        }
    }

    /// Marks the checkout's removal before `git worktree remove` runs, and
    /// records the admin dir it is about to remove.
    ///
    /// A failed write keeps the record and skips the removal: a crash after an
    /// unmarked removal would leave a record that says the checkout still
    /// exists, and the branch would leak.
    fn save_removing(&self, admin: &Path) -> bool {
        let Some(record) = &self.record else {
            return true;
        };
        let mut data = record.data.clone();
        data.removing = true;
        data.admin = Some(admin.to_path_buf());
        match write_pending_record(&record.path, &data) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %record.path.display(),
                    "could not record the start of a worktree removal"
                );
                false
            }
        }
    }

    /// Retires this attempt's durable record and its in-flight entry. Called
    /// only once the thread owns the checkout, or cleanup is actually done (or
    /// the path is positively a replacement). A record that cannot be removed
    /// is harmless: the next sweep checks the same identity again.
    fn retire(&self) {
        if let Some(pending) = &self.pending {
            pending.remove(&self.path);
        }
        if let Some(record) = &self.record {
            let _ = std::fs::remove_file(&record.path);
        }
    }

    /// The destination folder's claim, for the length of a rollback.
    ///
    /// The checkout path is always `<location>/<project>/<folder>`, so the
    /// claim is the one a create for the same folder takes.
    fn claim(&self, wait: bool) -> Result<Option<ClaimLock>, String> {
        let Some(project_dir) = self.path.parent() else {
            return Ok(None);
        };
        let Some(location) = project_dir.parent() else {
            return Ok(None);
        };
        let Some(project) = project_dir.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        ClaimLock::acquire_with(location, project, &self.path, wait)
    }
}

/// Whether `admin` is still the registration this create made for `checkout`.
///
/// `claim_owner` says the destination's claim file names this create's owner
/// token; it covers a kill between `git worktree add` and the marker, when the
/// admin dir has no marker yet. A marker that is there but names another owner
/// is a replacement and never falls back to the claim.
fn registration_proves(
    admin: &Path,
    checkout: &Path,
    owner: &str,
    branch: &str,
    claim_owner: bool,
) -> bool {
    match registration_owner(admin) {
        Some(marker) if marker != owner => return false,
        Some(_) => {}
        None if claim_owner => {}
        None => return false,
    }
    // While the checkout exists, its own `.git` must name this admin dir: that
    // is the live proof the registration is still the one this create made. A
    // folder that is gone (deleted by hand, or a kill inside
    // `git worktree remove`) is proved from the admin dir alone: its `gitdir`
    // naming this path and its `HEAD` naming this branch, below.
    if checkout.exists() && worktree_gitdir(checkout).as_deref() != Some(admin) {
        return false;
    }
    let Some(target) = git::read_bounded_following(&admin.join("gitdir")) else {
        return false;
    };
    let Ok(target) = String::from_utf8(target) else {
        return false;
    };
    let target = Path::new(target.trim());
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        admin.join(target)
    };
    let checkout_from_admin = if target.file_name().is_some_and(|name| name == ".git") {
        target.parent().map(Path::to_path_buf)
    } else {
        Some(target)
    };
    if checkout_from_admin.is_none_or(|path| git::norm(&path) != git::norm(checkout)) {
        return false;
    }
    git::read_head(&admin.join("HEAD")) == git::Head::Branch(branch.to_owned())
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

/// Writes this create's ownership token into `admin`.
///
/// The admin dir belongs to this registration, so the marker goes with it when
/// git removes the registration; a replacement checkout at the same folder
/// gets a freshly created admin dir without this token.
fn mark_registration(admin: &Path, owner: &str) -> bool {
    match std::fs::write(admin.join(OWNER_MARKER), owner.as_bytes()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                %error,
                admin = %admin.display(),
                "could not mark a worktree registration; it will not be rolled back"
            );
            false
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

/// Whether the destination's claim file names `owner`.
///
/// A create writes its owner token into the claim file before `git worktree
/// add` runs, so a kill between the add and the owner marker leaves a fact the
/// next start can match: this create made the folder, and the registration at
/// it is the one the add made. The claim file is rewritten only by a create
/// holding the folder's lock, and a folder that exists cannot be claimed
/// again, so a match always names the create that made this checkout.
fn claim_names_owner(dest: &Path, owner: &str) -> bool {
    let Some(project_dir) = dest.parent() else {
        return false;
    };
    let Some(location) = project_dir.parent() else {
        return false;
    };
    let Some(project) = project_dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    std::fs::read_to_string(claim_path(location, project, dest))
        .is_ok_and(|token| token.trim() == owner)
}

/// The folder, under the daemon's data, that one file per unadopted create is
/// written to.
fn pending_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("worktrees-pending")
}

/// The intent a create records before it changes anything in git.
fn new_pending_record(
    data_dir: &Path,
    repo: &Path,
    path: &Path,
    branch: &str,
    created_branch: bool,
    head: Option<&str>,
    owner: &str,
) -> Result<PendingRecordFile, String> {
    let dir = pending_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|error| {
        format!(
            "could not create the worktree recovery folder {}: {error}",
            dir.display()
        )
    })?;
    let data = PendingRecord {
        repo: repo.to_path_buf(),
        path: path.to_path_buf(),
        branch: branch.to_owned(),
        created_branch,
        head: head.map(str::to_owned),
        owner: owner.to_owned(),
        admin: None,
        removing: false,
        checkout_removed: false,
        branch_removed: !created_branch,
    };
    let record = PendingRecordFile {
        path: dir.join(format!("{}.json", uuid::Uuid::now_v7().simple())),
        data,
    };
    write_pending_record(&record.path, &record.data)
        .map_err(|error| format!("could not record the worktree creation intent: {error}"))?;
    Ok(record)
}

/// Writes one recovery record atomically: a synced temporary file in the same
/// folder is renamed over the record, then the folder itself is synced.
///
/// A kill at any point leaves either the previous complete record or the new
/// one, never a half-written JSON file the sweep has to guess about.
fn write_pending_record(path: &Path, record: &PendingRecord) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("{} has no parent folder", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::now_v7().simple()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        // Syncing the folder makes the rename itself durable; not every
        // platform can open a folder, and there the rename is still atomic.
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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
            .any(|used| path == *used || used.starts_with(&path))
        {
            let _ = std::fs::remove_file(&record_file);
            continue;
        }
        let created = CreatedWorktree {
            path: record.path.clone(),
            repo: record.repo.clone(),
            branch: record.branch.clone(),
            created_branch: record.created_branch,
            // A crash after marking but before the record learned the admin id
            // is still safe: adoption below requires the marker to match.
            registration: record
                .admin
                .clone()
                .or_else(|| worktree_gitdir(&record.path)),
            owner: Some(record.owner.clone()),
            head: record.head.clone(),
            record: Some(PendingRecordFile {
                path: record_file.clone(),
                data: record.clone(),
            }),
            pending: None,
        };
        // A record that says its own checkout is gone proves itself: there is
        // no registration left to check, only a branch this create made. Any
        // other record needs the owner marker this create wrote, or — for a
        // kill between the add and the marker — the token it wrote into the
        // destination's claim before the add.
        let proven = created.checkout_removed()
            || created.registration.as_deref().is_some_and(|admin| {
                registration_owner(admin).as_deref() == Some(record.owner.as_str())
            })
            || claim_names_owner(&record.path, &record.owner);
        if !proven {
            tracing::warn!(
                path = %record_file.display(),
                "a create intent has no registration marker yet; leaving it for a later retry"
            );
            continue;
        }
        if created.rollback_with(false).did_work {
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
/// meantime keeps it. Returns `true` when the branch step is finished: either
/// the branch is gone, or it is no longer the ref this create made.
fn delete_created_branch(repo: &Path, dest: &Path, branch: &str, head: Option<&str>) -> bool {
    #[cfg(test)]
    test_hooks::before_branch_delete(dest);
    if registered(repo, dest) || branch_checked_out(repo, branch) {
        return false;
    }
    if !branch_points_at(repo, branch, head) {
        return true;
    }
    git::run_with_timeout(repo, ["branch", "-D", branch], CREATE_TIMEOUT).is_ok()
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
        if lock(&self.pending.admission).closed {
            return Err("the daemon is shutting down; no worktree was created".to_owned());
        }
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

    /// Stops new thread creates from making a checkout.
    ///
    /// Called as soon as the listener stops accepting work. A create that
    /// already passed this gate finishes through adoption or rollback before
    /// [`Self::drain_admissions`] returns.
    pub fn close_admission(&self) {
        self.pending.close_admission();
    }

    /// One in-flight thread-creation admission, held until the thread has
    /// adopted its checkout or its rollback was started.
    pub fn admit(&self) -> Result<CreateAdmission, String> {
        self.pending.admit()
    }

    /// Waits for every admitted thread create to finish adopting or rolling
    /// back its checkout.
    pub async fn drain_admissions(&self) {
        self.pending.wait_for_admissions().await;
    }

    /// Waits for every rollback a shutdown could otherwise lose, then rolls
    /// back each create no thread adopted and waits for that too.
    ///
    /// A checkout in `in_use` is retired without touching it: a thread that
    /// committed while shutdown was draining owns it. The admission drain has
    /// already run, so no new thread can appear here.
    pub async fn shutdown(&self, in_use: &[String]) {
        for handle in self.pending.take_rollbacks() {
            let _ = handle.await;
        }
        let in_use: Vec<PathBuf> = in_use
            .iter()
            .map(|path| git::norm(Path::new(path)))
            .collect();
        let created = self.pending.take_created();
        if created.is_empty() {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || {
            for created in created {
                let path = git::norm(&created.path);
                if in_use
                    .iter()
                    .any(|used| path == *used || used.starts_with(&path))
                {
                    created.keep();
                    continue;
                }
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
    // a bare repository's worktree) is not the main one, while the project is
    // named after the main checkout. The destination must be outside both, or
    // a location that holds the main checkout leaks new checkouts into it. The
    // same check also refuses a location inside a sibling linked worktree.
    // The layout is resolved once, here, so the folder that was checked is the
    // folder every branch below is created in.
    let destination = project_destination(&repo, &location)?;

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
            let dest = destination.checkout(named);
            let Some(claim) = destination.claim(&dest)? else {
                return Err(format!("a folder already exists at {}", dest.display()));
            };
            // An existing branch is attached, not recreated, so its history
            // is the one the person asked for.
            (named.to_owned(), !branches.contains(named), dest, claim)
        }
        _ => claim_generated_branch(settings, prompt, &branches, &worktrees, &destination)?,
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
    let owner = uuid::Uuid::now_v7().simple().to_string();
    // The intent is durable before git changes anything. A kill during the
    // registration, the checkout hook or the submodule update therefore leaves
    // a record that names exactly the attempt and its owner token.
    let record = match new_pending_record(
        data_dir,
        &repo,
        &dest,
        &branch,
        created_branch,
        head.as_deref(),
        &owner,
    ) {
        Ok(record) => record,
        Err(error) => {
            if claim.still_ours() {
                let _ = std::fs::remove_dir_all(&dest);
            }
            drop(claim);
            return Err(error);
        }
    };
    let dest_text = dest.to_string_lossy().into_owned();
    // The owner token goes into the claim, which exists before the add, so a
    // kill between `git worktree add` and the marker below still leaves proof
    // this create made the checkout. Failing to write it only loses that
    // recovery window; the marker still covers every later crash.
    claim.record_owner(&owner);
    // `--no-checkout` splits registration from checkout: the owner marker is
    // written while the registration exists, and only then does the checkout
    // (and its post-checkout hook) run. A kill in that hook leaves both the
    // marker and the record.
    let add = if created_branch {
        git::run_with_timeout(
            &repo,
            [
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                &branch,
                &dest_text,
            ],
            CREATE_TIMEOUT,
        )
    } else {
        git::run_with_timeout(
            &repo,
            ["worktree", "add", "--no-checkout", &dest_text, &branch],
            CREATE_TIMEOUT,
        )
    };
    if let Err(error) = add {
        // Remove only what this call made: a registration git left behind, the
        // folder it claimed (still the same directory), and a branch it
        // created at the commit it started from. The claim is held throughout,
        // so no other create can claim the folder until this cleanup is done.
        let cleaned = clean_failed_add(
            &repo,
            &dest,
            &claim,
            created_branch.then_some(branch.as_str()),
            head.as_deref(),
            before.as_ref(),
        );
        if cleaned {
            let _ = std::fs::remove_file(&record.path);
        }
        drop(claim);
        return Err(format!(
            "could not create the worktree at {}: {error}",
            dest.display()
        ));
    }

    let registration = worktree_gitdir(&dest);
    let marked = registration
        .as_deref()
        .is_some_and(|admin| mark_registration(admin, &owner));
    if !marked {
        let cleaned = clean_failed_add(
            &repo,
            &dest,
            &claim,
            created_branch.then_some(branch.as_str()),
            head.as_deref(),
            before.as_ref(),
        );
        if cleaned {
            let _ = std::fs::remove_file(&record.path);
        }
        drop(claim);
        return Err(format!(
            "could not prove ownership of the worktree at {}; nothing was kept",
            dest.display()
        ));
    }
    let mut record = record;
    record.data.admin = registration.clone();
    if let Err(error) = write_pending_record(&record.path, &record.data) {
        tracing::warn!(
            %error,
            path = %record.path.display(),
            "could not update the worktree recovery record with its registration"
        );
    }
    // Populate the working tree now that ownership is durable. A failing
    // checkout is rolled back through the same verified registration.
    if let Err(error) =
        git::run_with_timeout(&dest, ["checkout", "--quiet", &branch], CREATE_TIMEOUT)
    {
        let created = CreatedWorktree {
            registration,
            owner: Some(owner),
            head: head.clone(),
            record: Some(record),
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
            "could not create the worktree at {}: its checkout failed: {error}",
            created.path.display()
        ));
    }

    // New worktrees start without submodule contents. A repository that
    // declares them gets a full recursive checkout, so the agent does not run
    // against empty folders that look like real ones.
    if dest.join(".gitmodules").is_file()
        && let Err(error) = git::run_with_timeout(&dest, submodule_args(&dest), CREATE_TIMEOUT)
    {
        let created = CreatedWorktree {
            registration,
            owner: Some(owner),
            head: head.clone(),
            record: Some(record),
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

    drop(claim);
    Ok(CreatedWorktree {
        registration,
        owner: Some(owner),
        head,
        record: Some(record),
        pending: None,
        path: dest,
        repo,
        branch,
        created_branch,
    })
}

/// Best-effort cleanup after `git worktree add` failed.
///
/// A failed add can leave a registration, a branch (`-b` ran before the
/// checkout failed), or neither. Each piece is removed only when it was absent
/// before the add and still names this attempt's work, so another checkout is
/// never touched. There is deliberately no repository-wide prune: an unrelated
/// user worktree that is temporarily moved must keep its registration.
///
/// Returns whether every owned piece is gone; a failure leaves the recovery
/// record for the next startup.
fn clean_failed_add(
    repo: &Path,
    dest: &Path,
    claim: &Claim,
    created_branch: Option<&str>,
    head: Option<&str>,
    before: Option<&git::RegistrationSnapshot>,
) -> bool {
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
    let mut removed = true;
    if let Some(registration) = registration {
        if !claim.still_ours() {
            return false;
        }
        removed = git::run_with_timeout(
            repo,
            [
                std::ffi::OsStr::new("worktree"),
                std::ffi::OsStr::new("remove"),
                std::ffi::OsStr::new("--force"),
                registration.path.as_os_str(),
            ],
            CREATE_TIMEOUT,
        )
        .is_ok();
    }
    // The recursive fallback only ever removes the very directory this call
    // created, and only while git holds no registration for it.
    let unregistered = !registered(repo, dest);
    #[cfg(test)]
    test_hooks::before_cleanup_removal(dest);
    if removed && claim.still_ours() && unregistered {
        let _ = std::fs::remove_dir_all(dest);
    }
    if let Some(branch) = created_branch {
        removed = delete_created_branch(repo, dest, branch, head) && removed;
    }
    removed
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
    destination: &ProjectDestination,
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
        let dest = destination.checkout(&candidate);
        // Another create may have taken this folder between the listing and
        // now; the atomic claim is what decides, not the earlier look.
        if let Some(claim) = destination.claim(&dest)? {
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
        let dest = destination.checkout(&candidate);
        if let Some(claim) = destination.claim(&dest)? {
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

    /// Remembers `owner` in the claim file, before `git worktree add` runs.
    ///
    /// A kill between the add and the owner marker leaves a registration this
    /// create made but cannot prove from the admin dir; the token written here
    /// is the durable fact the next start matches instead (see
    /// [`claim_names_owner`]).
    fn record_owner(&self, owner: &str) -> bool {
        self._lock.record_owner(owner)
    }
}

/// One destination's lock file, held for the life of a [`Claim`].
struct ClaimLock {
    #[cfg(any(unix, windows))]
    _file: std::fs::File,
    #[cfg(not(any(unix, windows)))]
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
        Self::acquire_with(location, project, dest, true)?
            .ok_or_else(|| format!("another worktree create is still using {}", dest.display()))
    }

    /// Writes `owner` into the claim file, durably.
    ///
    /// The os lock is on the file itself, so rewriting its contents keeps the
    /// claim held. A reader only uses the token when `git worktree add` has
    /// already registered the checkout and no owner marker names another
    /// create, so a stale token from an earlier create is never proof.
    fn record_owner(&self, owner: &str) -> bool {
        #[cfg(any(unix, windows))]
        let result = {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = &self._file;
            (|| {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(owner.as_bytes())?;
                file.sync_all()
            })()
        };
        #[cfg(not(any(unix, windows)))]
        let result = std::fs::write(&self.path, owner);
        if let Err(error) = result {
            tracing::warn!(%error, "could not record the worktree owner in its claim file");
            return false;
        }
        true
    }

    /// Takes the folder's claim, or answers `None` when `wait` is false and
    /// another create holds it.
    ///
    /// Unix and Windows use an OS lock released when the process exits, so a
    /// crash never leaves a stale marker that makes the next startup wait.
    fn acquire_with(
        location: &Path,
        project: &str,
        dest: &Path,
        wait: bool,
    ) -> Result<Option<Self>, String> {
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
            // A forked child briefly inherits every open descriptor until it
            // execs. Give a nonblocking sweep that short window before calling
            // the folder busy; a real create still leaves it deferred.
            let deferral_deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(40);
            loop {
                let operation = if wait {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_EX | libc::LOCK_NB
                };
                let locked = unsafe { libc::flock(file.as_raw_fd(), operation) };
                if locked == 0 {
                    return Ok(Some(Self { _file: file }));
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if !wait
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
                    )
                {
                    if std::time::Instant::now() >= deferral_deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                return Err(format!("could not lock {}: {error}", path.display()));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            loop {
                let opened = std::fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .share_mode(0)
                    .open(&path);
                match opened {
                    Ok(file) => return Ok(Some(Self { _file: file })),
                    Err(error) if !wait => {
                        let _ = (dest, error);
                        return Ok(None);
                    }
                    Err(_) if std::time::Instant::now() >= deadline => {
                        return Err(format!(
                            "another worktree create is still using {}",
                            dest.display()
                        ));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            if !wait && path.exists() {
                return Ok(None);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            loop {
                match std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                {
                    Ok(mut file) => {
                        let _ = writeln!(file, "{}", std::process::id());
                        return Ok(Some(Self { path }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        if !wait || std::time::Instant::now() >= deadline {
                            return Ok(None);
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

#[cfg(not(any(unix, windows)))]
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
#[cfg(test)]
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

/// Where one location holds this project's checkouts, from one layout.
///
/// The folder name and the destination both come from the same
/// [`git::Layout`], so a create can never check one destination and make
/// another: the project folder is named after the layout's main checkout, and
/// `<location>/<project>` is the exact folder the refusal check saw.
struct ProjectDestination {
    location: PathBuf,
    project: String,
    /// `<location>/<project>`, the folder this project's checkouts go in.
    folder: PathBuf,
}

impl ProjectDestination {
    /// The checkout folder for `branch`.
    fn checkout(&self, branch: &str) -> PathBuf {
        self.folder.join(folder_name(branch))
    }

    /// Claims this branch's folder, creating its parents first.
    fn claim(&self, dest: &Path) -> Result<Option<Claim>, String> {
        claim_dir(&self.location, &self.project, dest)
    }
}

/// The project folder name: the main worktree's folder.
fn project_name_of(main: &Path) -> String {
    main.file_name()
        .and_then(|name| name.to_str())
        .map(folder_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned())
}

/// Why `location` may not hold this project's checkouts, when it may not, or
/// the destination it holds when it may.
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
fn project_destination(workspace: &Path, location: &Path) -> Result<ProjectDestination, String> {
    let layout = git::layout(workspace).map_err(|error| {
        format!(
            "the worktree location {} could not be checked against {}: {error}; choose a folder after git can tell which folder is the project's main checkout",
            location.display(),
            workspace.display()
        )
    })?;
    let main = &layout.main;
    let project = project_name_of(main);
    let destination = location.join(&project);
    if location_inside_repo(workspace, &destination) {
        return Err(destination_message(location, &destination, workspace));
    }
    if location_inside_repo(main, &destination) {
        if git::is_bare(main, git::OBSERVATION_GIT_TIMEOUT).ok() == Some(true) {
            return Err(format!(
                "the worktree location {} would put new checkouts (under {}) inside the bare repository's git directory {}, which has no working tree to hold them; choose a folder outside it",
                location.display(),
                destination.display(),
                main.display()
            ));
        }
        return Err(destination_message(location, &destination, main));
    }
    let workspace_norm = git::norm(workspace);
    let main_norm = git::norm(main);
    let common_norm = git::norm(&layout.common);
    for registration in layout
        .snapshot
        .iter()
        .filter(|registration| !registration.bare)
    {
        if registration.path == workspace_norm
            || registration.path == main_norm
            // A submodule's main registration names its git dir; it has no
            // working tree of its own, and the working tree was checked above.
            || registration.path == common_norm
        {
            continue;
        }
        if location_inside_repo(&registration.path, &destination) {
            return Err(format!(
                "the worktree location {} would put new checkouts (under {}) inside another worktree of this repository, {}; choose a folder outside it",
                location.display(),
                destination.display(),
                registration.path.display()
            ));
        }
    }
    // Backstop: refuse a destination inside a working tree of this same
    // repository even when that checkout is not registered and never proves
    // itself as the main one — a folder whose `.git` is gone, a metadata
    // directory named `.git`, an unregistered linked worktree. The deepest
    // folder that exists is the one git is asked about, so a destination that
    // does not exist yet is still judged by the folder it would go into.
    if let Some(ancestor) = deepest_existing_ancestor(&destination)
        && git::common_dir(&ancestor, git::OBSERVATION_GIT_TIMEOUT)
            .is_ok_and(|common| common == common_norm)
    {
        return Err(destination_message(location, &destination, &ancestor));
    }
    Ok(ProjectDestination {
        location: location.to_path_buf(),
        project,
        folder: destination,
    })
}

/// The refusal [`project_destination`] gives for a location it cannot use.
pub(crate) fn destination_refusal(workspace: &Path, location: &Path) -> Option<String> {
    project_destination(workspace, location).err()
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

/// The deepest folder on `path`'s way up that exists, `path` included.
fn deepest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
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
    static PAUSE_BEFORE_ROLLBACK_REMOVE: Mutex<Vec<Pause>> = Mutex::new(Vec::new());
    static HOLD_ROLLBACK: Mutex<Option<(PathBuf, Duration)>> = Mutex::new(None);
    static CLAIM_IDENTITY: Mutex<Vec<(PathBuf, Option<super::DirIdentity>)>> =
        Mutex::new(Vec::new());

    /// Holds a cleanup of `dest` just before its destructive removal, until a
    /// test releases it. A rollback has already recorded the removal here; a
    /// failed add has no record and pauses before its recursive removal.
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

    /// Holds a rollback for `dest` after its first identity check and before
    /// the re-check and destructive command, until a test releases it.
    pub(crate) fn pause_before_rollback_remove(
        dest: PathBuf,
        paused: Arc<Barrier>,
        release: Arc<Barrier>,
    ) {
        install(
            &PAUSE_BEFORE_ROLLBACK_REMOVE,
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
            &PAUSE_BEFORE_ROLLBACK_REMOVE,
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

    pub(super) fn before_rollback_remove(dest: &std::path::Path) {
        pause(&PAUSE_BEFORE_ROLLBACK_REMOVE, dest);
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

    /// The folder h4rn names a project's checkouts in.
    fn project_name(repo: &Path) -> String {
        git::layout(repo)
            .map(|layout| project_name_of(&layout.main))
            .unwrap_or_else(|_| project_name_of(repo))
    }

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
        assert!(
            !error.contains("admin-meta") && !error.contains("api.git"),
            "the refusal never names the metadata directory: {error}"
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

    /// A linked worktree inside the metadata directory's parent proves nothing
    /// about where the main checkout is: `repo_of` names that parent after the
    /// common dir's `.git`. A location that is an ignored repository inside the
    /// real main checkout must still be refused instead of accepted because the
    /// workspace happens to sit inside the folder git calls the main worktree.
    #[test]
    fn a_linked_worktree_inside_the_metadata_parent_never_proves_the_main_checkout() {
        let root = tempfile::tempdir().expect("root");
        let meta = root.path().join("meta");
        std::fs::create_dir(&meta).expect("meta");
        let main = root.path().join("code").join("api");
        std::fs::create_dir_all(&main).expect("api");
        run(
            root.path(),
            &[
                "init",
                "-q",
                "-b",
                "main",
                "--separate-git-dir",
                meta.join(".git").to_str().unwrap(),
                main.to_str().unwrap(),
            ],
        );
        run(&main, &["config", "user.email", "wt@example.invalid"]);
        run(&main, &["config", "user.name", "worktree test"]);
        std::fs::write(main.join(".gitignore"), "vendor/\n").expect("ignore");
        run(&main, &["add", ".gitignore"]);
        run(&main, &["commit", "-q", "-m", "init"]);
        let workspace = meta.join("wt");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                workspace.to_str().unwrap(),
            ],
        );
        assert_eq!(
            git::repo_of(&workspace),
            Some(meta.clone()),
            "git names the metadata parent as the main worktree"
        );

        // An ignored repository nested inside the real main checkout keeps the
        // destination's deepest existing ancestor out of this repository, so
        // only the main-checkout proof can refuse the location.
        let nested = main.join("vendor").join("lib");
        std::fs::create_dir_all(&nested).expect("nested repository");
        init_repo(&nested);

        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &workspace,
            &WorktreeRequest::Named("nested/x".into()),
            "nested",
            &settings(Some(&nested)),
            data.path(),
            None,
        )
        .expect_err("the location is inside the real main checkout");
        assert!(
            error.contains("main checkout could not be found"),
            "the refusal says the main checkout is unknown: {error}"
        );
        assert!(
            !nested.join("meta").exists(),
            "nothing was created under the location"
        );
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/nested/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
    }

    /// A linked worktree nested inside its own main checkout (`repo/.wt/one`,
    /// the common `.worktrees/` layout) still resolves the real main checkout:
    /// being inside it is not proof enough on its own, but the main checkout
    /// holds a tracked path, so creating a thread checkout from there works.
    #[test]
    fn a_linked_worktree_nested_in_its_own_main_checkout_can_create() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("repo");
        std::fs::create_dir(&main).expect("repo");
        init_repo(&main);
        std::fs::write(main.join(".git").join("info").join("exclude"), ".wt/\n").expect("exclude");
        let workspace = main.join(".wt").join("one");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "one",
                workspace.to_str().unwrap(),
            ],
        );

        let location = tempfile::tempdir().expect("location");
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            &workspace,
            &WorktreeRequest::Named("nested-ok/x".into()),
            "nested",
            &settings(Some(location.path())),
            data.path(),
            None,
        )
        .expect("a worktree nested in its own main checkout can create");
        // The project folder is named after the real main checkout, which is
        // only known once the main checkout is resolved.
        assert!(
            git::norm(&created.path).starts_with(git::norm(&location.path().join("repo"))),
            "the checkout lands under the main checkout's project folder: {}",
            created.path.display()
        );
    }

    /// A repository whose `HEAD` tree is empty cannot prove its main checkout
    /// from a linked worktree: no tracked path shows which folder holds the
    /// working tree, and a metadata parent has the same `.git` shape. The
    /// location is refused rather than guessed (docs/worktrees.md says so); one
    /// tracked file is the proof that makes the same request work.
    #[test]
    fn an_empty_head_tree_refuses_a_location_until_the_repository_tracks_a_file() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        run(&main, &["init", "-q", "-b", "main"]);
        run(&main, &["config", "user.email", "wt@example.invalid"]);
        run(&main, &["config", "user.name", "worktree test"]);
        run(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let workspace = root.path().join("linked");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                workspace.to_str().unwrap(),
            ],
        );
        let location = tempfile::tempdir().expect("location");
        let data = tempfile::tempdir().expect("data");

        let error = create_blocking(
            &workspace,
            &WorktreeRequest::Named("empty/x".into()),
            "empty",
            &settings(Some(location.path())),
            data.path(),
            None,
        )
        .expect_err("an empty HEAD tree cannot prove the main checkout");
        assert!(
            error.contains("main checkout could not be found"),
            "the refusal says the main checkout is unknown: {error}"
        );
        assert!(
            !location.path().join("api").exists(),
            "nothing was created under the location"
        );

        std::fs::write(main.join("README.md"), "hello\n").expect("readme");
        run(&main, &["add", "README.md"]);
        run(&main, &["commit", "-q", "-m", "readme"]);
        let created = create_blocking(
            &workspace,
            &WorktreeRequest::Named("empty/x".into()),
            "empty",
            &settings(Some(location.path())),
            data.path(),
            None,
        )
        .expect("the tracked file proves the main checkout");
        assert_eq!(
            created.path,
            checkout_path(location.path(), "api", "empty/x")
        );
        created.keep();
    }

    /// A `--separate-git-dir` repository whose metadata dir sits under an
    /// unrelated parent, seen from a linked worktree, leaves nothing that
    /// proves which folder is the main checkout. h4rn refuses every location
    /// rather than guess, and never puts a checkout in the real main checkout.
    ///
    /// The metadata dir is tried under its own name and under `.git`: git
    /// reports the parent of a metadata dir called `.git` as the main worktree,
    /// and h4rn must not believe that either.
    #[test]
    fn an_unprovable_main_checkout_refuses_every_location() {
        refuses_every_location_when_the_main_checkout_cannot_be_proved("admin-meta");
        refuses_every_location_when_the_main_checkout_cannot_be_proved(".git");
    }

    /// [`an_unprovable_main_checkout_refuses_every_location`] for one metadata
    /// directory name.
    fn refuses_every_location_when_the_main_checkout_cannot_be_proved(metadata: &str) {
        let root = tempfile::tempdir().expect("root");
        let meta = root.path().join("meta");
        std::fs::create_dir(&meta).expect("meta");
        let main = root.path().join("code").join("api");
        std::fs::create_dir_all(&main).expect("api");
        let gitdir = meta.join(metadata);
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
        let linked = root.path().join("code").join("linked");
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
        if metadata == ".git" {
            assert_eq!(
                git::repo_of(&linked),
                Some(meta.clone()),
                "git names the metadata dir's parent as the main worktree"
            );
        }

        let data = tempfile::tempdir().expect("data");
        let locations = [main.clone(), main.join("nested")];
        for location in &locations {
            let error = create_blocking(
                &linked,
                &WorktreeRequest::Named("guessed/x".into()),
                "guessed",
                &settings(Some(location)),
                data.path(),
                None,
            )
            .expect_err("the main checkout cannot be proved");
            assert!(
                error.contains("main checkout could not be found"),
                "the refusal says the main checkout is unknown: {error}"
            );
            assert!(
                !error.contains(metadata) && !error.contains("api.git"),
                "the refusal never names the metadata directory: {error}"
            );
            assert!(
                !main.join(folder_name("guessed/x")).exists()
                    && !linked.join(folder_name("guessed/x")).exists(),
                "no checkout was made inside the main checkout or the workspace"
            );
        }
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/guessed/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
        assert!(
            git::list_worktrees(&main)
                .expect("list")
                .iter()
                .all(|worktree| worktree.branch.as_deref() != Some("guessed/x")),
            "only the linked checkout that was already there is registered"
        );
    }

    /// A checkout of the same repository that git no longer registers is still
    /// refused as a location: the deepest existing folder is asked for its
    /// common dir, which is this repository's.
    #[test]
    fn an_unregistered_checkouts_folder_is_still_refused_as_a_location() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        init_repo(&main);
        let stale = root.path().join("stale");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "unlisted",
                stale.to_str().unwrap(),
            ],
        );
        // Deleting the admin entry's `gitdir` is what makes git stop listing
        // a registration whose checkout is still a working tree.
        let admin = worktree_gitdir(&stale).expect("admin dir");
        std::fs::remove_file(admin.join("gitdir")).expect("remove gitdir");
        assert!(
            git::list_worktrees(&main).expect("list").is_empty(),
            "git no longer lists the checkout"
        );

        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &main,
            &WorktreeRequest::Named("guessed/x".into()),
            "guessed",
            &settings(Some(&stale)),
            data.path(),
            None,
        )
        .expect_err("the destination is a working tree of the same repository");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            error.contains(&stale.to_string_lossy().into_owned()),
            "the refusal names the checkout: {error}"
        );
        assert!(
            !stale
                .join(project_name(&main))
                .join(folder_name("guessed/x"))
                .exists(),
            "no checkout was made inside the unregistered checkout"
        );
    }

    /// The folder a location holds for a project is named from the same layout
    /// the refusal check used, so a create from a linked worktree makes its
    /// checkout under the main checkout's name and nowhere else.
    #[test]
    fn a_linked_worktree_creates_under_the_main_checkouts_project_folder() {
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
        let location = root.path().join("checkouts");
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            &linked,
            &WorktreeRequest::Named("layout/x".into()),
            "layout",
            &settings(Some(&location)),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(
            created.path,
            location.join("api").join(folder_name("layout/x")),
            "the checkout is in the checked project folder"
        );
        assert!(
            !location.join("linked").exists(),
            "nothing was created under the workspace's own folder name"
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

    /// Holds `branch`'s deletion closed, as a crash between the checkout's
    /// removal and `git branch -D` would.
    fn block_branch_delete(repo: &Path, branch: &str) -> PathBuf {
        let lock = repo
            .join(".git")
            .join("refs")
            .join("heads")
            .join(format!("{branch}.lock"));
        std::fs::create_dir_all(lock.parent().expect("ref folder")).expect("ref folder");
        std::fs::write(&lock, b"").expect("ref lock");
        lock
    }

    /// The checkout is removed, then the process dies before its branch is
    /// deleted. The record is the proof at the next startup: it deletes the
    /// branch this create made and retires itself.
    #[test]
    fn a_removed_checkout_with_its_branch_pending_is_finished_at_the_next_start() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("crashed/x".into()),
            "crashed",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let lock = block_branch_delete(repo.path(), "crashed/x");
        assert!(!created.rollback(), "the branch step could not finish yet");
        assert!(!created.path.exists(), "the checkout was removed");
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_some(),
            "the record stays until the branch is gone"
        );

        std::fs::remove_file(&lock).expect("unblock the branch");
        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(
            swept,
            vec![created.path.clone()],
            "the next startup deletes the branch"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/crashed/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is gone"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_none(),
            "the record is retired"
        );
    }

    /// A checkout folder deleted by hand leaves its registration and marker
    /// behind. The admin dir alone then proves ownership, and the next start
    /// finishes the removal (git accepts a registration whose folder is gone)
    /// and deletes the branch h4rn made.
    #[test]
    fn a_deleted_checkout_folder_with_its_registration_is_finished_at_the_next_start() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("deleted/x".into()),
            "deleted",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        std::fs::remove_dir_all(&created.path).expect("remove the checkout folder");
        assert_eq!(
            git::list_worktrees(repo.path()).expect("list").len(),
            1,
            "the registration is still there"
        );

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![created.path.clone()]);
        assert!(
            git::list_worktrees(repo.path()).expect("list").is_empty(),
            "the registration is gone"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/deleted/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is gone"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_none(),
            "the record is retired"
        );
    }

    /// A kill right after `git worktree add`, before the owner marker, leaves
    /// no marker to match. The token the create wrote into the destination's
    /// claim before the add proves the checkout is its own, so the next start
    /// removes it.
    #[test]
    fn a_create_killed_right_after_its_add_is_swept() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let location = data.path().join("worktrees");
        let project = project_name(repo.path());
        let dest = checkout_path(&location, &project, "killed/x");
        std::fs::create_dir_all(dest.parent().expect("project folder")).expect("project folder");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                "killed/x",
                dest.to_str().unwrap(),
            ],
        );
        let head = git::run_with_timeout(repo.path(), ["rev-parse", "HEAD"], CREATE_TIMEOUT)
            .expect("head");
        let owner = "killed-create-owner";
        let record = PendingRecord {
            repo: repo.path().to_path_buf(),
            path: dest.clone(),
            branch: "killed/x".into(),
            created_branch: true,
            head: Some(head),
            owner: owner.into(),
            admin: None,
            removing: false,
            checkout_removed: false,
            branch_removed: false,
        };
        let pending = data.path().join("worktrees-pending");
        std::fs::create_dir_all(&pending).expect("pending dir");
        write_pending_record(&pending.join("killed.json"), &record).expect("record");
        let claim = claim_path(&location, &project, &dest);
        std::fs::create_dir_all(claim.parent().expect("claims folder")).expect("claims folder");
        std::fs::write(&claim, owner).expect("claim token");
        assert!(
            worktree_gitdir(&dest).is_some(),
            "the add registered the checkout"
        );
        assert!(
            registration_owner(&worktree_gitdir(&dest).expect("admin dir")).is_none(),
            "the kill came before the owner marker"
        );

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![dest.clone()], "the leftover checkout is swept");
        assert!(!dest.exists(), "the checkout is gone");
        assert!(
            git::list_worktrees(repo.path()).expect("list").is_empty(),
            "the registration is gone"
        );
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/killed/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is gone"
        );
        assert!(
            std::fs::read_dir(&pending)
                .expect("pending dir")
                .next()
                .is_none(),
            "the record is retired"
        );
    }

    /// A kill between `git worktree remove` and the progress write leaves a
    /// record that still says the checkout exists. The "removing" mark written
    /// before the removal is what lets the next start delete the branch
    /// instead of retiring the record and leaking it.
    #[test]
    fn a_removal_marked_before_git_runs_finishes_the_branch_at_the_next_start() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("unsaved/x".into()),
            "unsaved",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        run(
            repo.path(),
            &[
                "worktree",
                "remove",
                "--force",
                created.path.to_str().unwrap(),
            ],
        );
        let pending = data.path().join("worktrees-pending");
        let record_file = std::fs::read_dir(&pending)
            .expect("pending dir")
            .next()
            .expect("a record")
            .expect("record entry")
            .path();
        let mut record: PendingRecord =
            serde_json::from_slice(&std::fs::read(&record_file).expect("read the record"))
                .expect("parse the record");
        assert!(record.admin.is_some(), "the create recorded its admin dir");
        // The state a kill after the removal, before the progress write, leaves.
        record.removing = true;
        record.checkout_removed = false;
        record.branch_removed = false;
        write_pending_record(&record_file, &record).expect("record");
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/unsaved/x"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "the branch is still there when the process dies"
        );

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![created.path.clone()]);
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/unsaved/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the next start deletes the branch this create made"
        );
        assert!(
            std::fs::read_dir(&pending)
                .expect("pending dir")
                .next()
                .is_none(),
            "the record is retired"
        );
    }

    /// A create killed before it registered anything leaves a record proved only
    /// by the claim token. With no admin dir and an empty destination, the next
    /// start finishes it: the branch h4rn made and the empty folder are removed,
    /// and the record is retired instead of retrying forever.
    #[test]
    fn a_claim_proven_record_without_a_registration_finishes_at_the_next_start() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("claimonly/x".into()),
            "claim only",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        // What a kill inside a failed add's cleanup leaves: no registration and
        // no checkout, but an empty folder and a claim that names the create.
        run(
            repo.path(),
            &[
                "worktree",
                "remove",
                "--force",
                created.path.to_str().unwrap(),
            ],
        );
        std::fs::create_dir(&created.path).expect("empty destination");
        let pending = data.path().join("worktrees-pending");
        let record_file = std::fs::read_dir(&pending)
            .expect("pending dir")
            .next()
            .expect("a record")
            .expect("record entry")
            .path();
        let mut record: PendingRecord =
            serde_json::from_slice(&std::fs::read(&record_file).expect("read the record"))
                .expect("parse the record");
        assert!(
            claim_names_owner(&record.path, &record.owner),
            "the claim proves this create made the folder"
        );
        record.admin = None;
        record.removing = false;
        record.checkout_removed = false;
        record.branch_removed = false;
        write_pending_record(&record_file, &record).expect("record");

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(
            swept,
            vec![created.path.clone()],
            "the branch deletion is real work"
        );
        assert!(!created.path.exists(), "the empty destination is removed");
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/claimonly/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the branch this create made is deleted"
        );
        assert!(
            std::fs::read_dir(&pending)
                .expect("pending dir")
                .next()
                .is_none(),
            "the record is retired"
        );
    }

    /// The owner token written into the destination's claim before
    /// `git worktree add` is the proof a kill between the add and the
    /// registration marker still has, so it must name the create's record.
    #[test]
    fn a_create_writes_its_owner_into_the_destination_claim() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("claimed/x".into()),
            "claimed",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let record = created.record.as_ref().expect("a recovery record");
        let folder = created.path.parent().expect("project folder");
        let location = folder.parent().expect("location");
        let project = folder
            .file_name()
            .expect("project name")
            .to_str()
            .expect("utf-8 project name");
        let claim = claim_path(location, project, &created.path);
        assert_eq!(
            std::fs::read_to_string(&claim)
                .expect("the claim file")
                .trim(),
            record.data.owner,
            "the claim names the create that owns the folder"
        );
        created.keep();
    }

    /// The record marks the removal and names the admin dir before git runs, so
    /// a kill between `git worktree remove` and the progress write still lets the
    /// next start finish the branch step instead of retiring the record.
    #[test]
    fn a_rollback_records_the_removal_and_admin_before_git_removes_the_checkout() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let mut created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("marked/x".into()),
            "marked",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let admin = created.registration.clone().expect("a registration");
        let record = created.record.as_mut().expect("a recovery record");
        // Drop the admin from the record file, so only the removal write can
        // put it back before git acts.
        record.data.admin = None;
        let record_path = record.path.clone();
        write_pending_record(&record_path, &record.data).expect("record without an admin");

        let paused = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        test_hooks::pause_before_cleanup_removal(
            created.path.clone(),
            paused.clone(),
            release.clone(),
        );
        let path = created.path.clone();
        let handle = std::thread::spawn(move || created.rollback());
        paused.wait();
        let record: PendingRecord =
            serde_json::from_slice(&std::fs::read(&record_path).expect("read the record"))
                .expect("parse the record");
        assert!(record.removing, "the removal is marked before git runs");
        assert_eq!(
            record.admin.as_deref(),
            Some(admin.as_path()),
            "the record names the admin dir git is about to remove"
        );
        release.wait();
        assert!(handle.join().expect("join"), "the rollback finishes");
        test_hooks::clear_failed_add_hooks(&path);
        assert!(!path.exists(), "the checkout is removed");
    }

    /// A branch deletion that fails keeps the record and the branch for a later
    /// retry; the sweep reports only what it actually removed.
    #[test]
    fn a_failed_branch_delete_is_retried_on_a_later_start() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("blocked/x".into()),
            "blocked",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let lock = block_branch_delete(repo.path(), "blocked/x");

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(
            swept,
            vec![created.path.clone()],
            "the checkout removal is real work"
        );
        assert!(!created.path.exists(), "the checkout is gone");
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/blocked/x"],
                CREATE_TIMEOUT,
            )
            .is_ok(),
            "the branch is kept while it cannot be deleted"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_some(),
            "the record is kept for the next startup"
        );

        std::fs::remove_file(&lock).expect("unblock the branch");
        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![created.path.clone()], "the branch is deleted");
        assert!(
            git::run_with_timeout(
                repo.path(),
                ["rev-parse", "--verify", "refs/heads/blocked/x"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "the retried startup deletes the branch"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_none(),
            "the record is retired"
        );
    }

    /// A record with nothing left to remove is retired silently: it is not
    /// reported as a checkout that was swept.
    #[test]
    fn a_record_with_nothing_left_to_remove_is_not_reported_as_swept() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let path = data.path().join("checkouts").join("api").join("gone-x");
        let record = PendingRecord {
            repo: repo.path().to_path_buf(),
            path: path.clone(),
            branch: "gone/x".into(),
            created_branch: true,
            head: None,
            owner: "already-cleaned".into(),
            admin: None,
            removing: false,
            checkout_removed: true,
            branch_removed: true,
        };
        std::fs::create_dir_all(data.path().join("worktrees-pending")).expect("pending dir");
        write_pending_record(
            &data.path().join("worktrees-pending").join("done.json"),
            &record,
        )
        .expect("record");

        assert!(
            sweep_pending(data.path(), &[]).is_empty(),
            "nothing was removed"
        );
        assert!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .next()
                .is_none(),
            "the finished record is retired"
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
        let record = PendingRecord {
            repo: main.clone(),
            path: user.clone(),
            branch: "mine".into(),
            created_branch: false,
            head: None,
            owner: "not-h4rns-owner".into(),
            admin: None,
            removing: false,
            checkout_removed: false,
            branch_removed: true,
        };
        std::fs::create_dir_all(data.path().join("worktrees-pending")).expect("pending dir");
        write_pending_record(
            &data.path().join("worktrees-pending").join("user.json"),
            &record,
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

    /// A thread that happens to work in a parent folder does not own a pending
    /// checkout inside it. Only the checkout itself, or a folder inside it, is
    /// "in use".
    #[test]
    fn a_thread_opened_at_a_parent_folder_does_not_protect_a_pending_checkout() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("in-use/parent".into()),
            "in use",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let project_dir = created.path.parent().expect("project folder");
        let location = project_dir.parent().expect("location");
        let above = vec![location.to_string_lossy().into_owned()];

        let swept = sweep_pending(data.path(), &above);
        assert_eq!(swept, vec![created.path.clone()]);
        assert!(
            !created.path.exists(),
            "the checkout was not mistaken for the thread's own"
        );
    }

    /// A thread whose workspace is inside a pending checkout does protect it.
    #[test]
    fn a_thread_inside_a_pending_checkout_protects_it() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("in-use/inside".into()),
            "in use",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let inside = created.path.join("src");
        std::fs::create_dir(&inside).expect("src");
        let in_use = vec![inside.to_string_lossy().into_owned()];

        assert!(sweep_pending(data.path(), &in_use).is_empty());
        assert!(created.path.is_dir(), "the thread's checkout stays");
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

        creator.shutdown(&[]).await;
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

    /// A thread working above a create's folder is not "in use" for it: the
    /// create is still rolled back at shutdown.
    #[tokio::test]
    async fn shutdown_rolls_back_a_create_below_a_threads_folder() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let creator = WorktreeCreator::new(
            data.path().to_path_buf(),
            Arc::new(crate::worktrees::settings::WorktreeSettingsStore::in_memory()),
        );
        let created = creator
            .create_for_thread(
                repo.path().to_str().unwrap(),
                &WorktreeRequest::Named("shutdown/below".into()),
                "shutdown",
            )
            .await
            .expect("create");
        let project_dir = created.path.parent().expect("project folder");
        let location = project_dir.parent().expect("location");
        let above = vec![location.to_string_lossy().into_owned()];

        creator.shutdown(&above).await;
        assert!(
            !created.path.exists(),
            "a parent folder is not the thread's own checkout"
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

        creator.shutdown(&[]).await;
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

    /// Recovery cleans only the registration it proved it made. A user
    /// worktree that was moved by hand is stale to git, but its registration
    /// must survive another create's rollback.
    #[test]
    fn a_moved_user_worktree_keeps_its_registration_after_recovery() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let user = data.path().join("user-worktree");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mine",
                user.to_str().expect("path"),
            ],
        );
        let moved = data.path().join("moved-user-worktree");
        std::fs::rename(&user, &moved).expect("move the user worktree");

        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("h4rn/other".into()),
            "other",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert!(created.rollback(), "the h4rn create is cleaned up");

        let registrations = git::registrations(repo.path()).expect("registrations");
        assert!(
            registrations
                .iter()
                .any(|registration| registration.branch() == Some("mine")),
            "the moved user worktree still has its registration"
        );
    }

    /// A replacement registered at the same path after the first identity
    /// check survives the rollback.
    #[test]
    fn a_replacement_after_verification_survives_the_rollback() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let first = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("race/x".into()),
            "first",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let dest = first.path.clone();
        let paused = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        test_hooks::pause_before_rollback_remove(dest.clone(), paused.clone(), release.clone());

        let repo_path = repo.path().to_path_buf();
        let rollback = std::thread::spawn(move || first.rollback());
        paused.wait();
        // Another actor removes the verified checkout and registers a
        // different branch at the same path.
        run(
            &repo_path,
            &[
                "worktree",
                "remove",
                "--force",
                dest.to_str().expect("path"),
            ],
        );
        let _ = std::fs::remove_dir_all(&dest);
        run(
            &repo_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "replacement",
                dest.to_str().expect("path"),
            ],
        );
        std::fs::write(dest.join("replacement.txt"), "kept\n").expect("replacement file");
        release.wait();

        assert!(rollback.join().expect("rollback"), "the old record retires");
        assert!(
            dest.join("replacement.txt").is_file(),
            "the replacement's files are untouched"
        );
        assert!(
            git::list_worktrees(&repo_path)
                .expect("list")
                .iter()
                .any(|worktree| worktree.branch.as_deref() == Some("replacement")),
            "the replacement's registration is untouched"
        );
        test_hooks::clear_failed_add_hooks(&dest);
    }

    /// A locked worktree keeps its recovery record until it can be removed.
    #[test]
    fn a_locked_worktree_is_retried_after_it_is_unlocked() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("locked/x".into()),
            "locked",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        run(
            repo.path(),
            &["worktree", "lock", created.path.to_str().expect("path")],
        );

        assert!(!created.rollback(), "a locked worktree is not removed");
        assert!(created.path.is_dir());
        assert_eq!(
            std::fs::read_dir(data.path().join("worktrees-pending"))
                .expect("pending dir")
                .count(),
            1,
            "the recovery record remains"
        );

        run(
            repo.path(),
            &["worktree", "unlock", created.path.to_str().expect("path")],
        );
        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![created.path.clone()]);
        assert!(!created.path.exists(), "the unlocked checkout is removed");
    }

    /// A create interrupted after its owner marker was written but before its
    /// checkout completed is finished at the next startup.
    #[test]
    fn a_create_interrupted_after_its_marker_is_swept() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let location = data.path().join("worktrees");
        let project = project_name(repo.path());
        let dest = checkout_path(&location, &project, "interrupted/after-marker");
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("parent");
        let head = git::run_with_timeout(repo.path(), ["rev-parse", "HEAD"], CREATE_TIMEOUT)
            .expect("head");
        let record = new_pending_record(
            data.path(),
            repo.path(),
            &dest,
            "interrupted/after-marker",
            true,
            Some(&head),
            "owner-after-marker",
        )
        .expect("intent");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                "interrupted/after-marker",
                dest.to_str().expect("path"),
            ],
        );
        let admin = worktree_gitdir(&dest).expect("registration");
        assert!(mark_registration(&admin, "owner-after-marker"));
        let mut saved = record.data.clone();
        saved.admin = Some(admin);
        write_pending_record(&record.path, &saved).expect("record admin");

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![dest.clone()]);
        assert!(!dest.exists(), "the interrupted checkout is gone");
        assert!(
            git::run_with_timeout(
                repo.path(),
                [
                    "rev-parse",
                    "--verify",
                    "refs/heads/interrupted/after-marker"
                ],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "its branch is gone"
        );
    }

    /// A busy folder is deferred instead of blocking startup on a claim.
    #[test]
    fn a_busy_recovery_is_deferred_not_waited_for() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("busy/x".into()),
            "busy",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        let project_dir = created.path.parent().expect("project dir");
        let location = project_dir.parent().expect("location");
        let project = project_dir.file_name().unwrap().to_str().unwrap();
        let held = ClaimLock::acquire(location, project, &created.path).expect("claim");

        let started = std::time::Instant::now();
        let swept = sweep_pending(data.path(), &[]);
        assert!(swept.is_empty(), "a busy record is not swept yet");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the sweep did not wait for the claim"
        );
        assert!(created.path.is_dir(), "the checkout is untouched");
        drop(held);

        let swept = sweep_pending(data.path(), &[]);
        assert_eq!(swept, vec![created.path.clone()]);
    }

    /// A `--separate-git-dir` repository whose main checkout sits at the
    /// destination is refused from the main checkout itself, with the real
    /// working-tree name rather than the metadata directory.
    #[test]
    fn a_separate_git_dir_main_checkout_refuses_a_location_holding_it() {
        let root = tempfile::tempdir().expect("root");
        let main = root.path().join("api");
        std::fs::create_dir(&main).expect("api");
        let gitdir = root.path().join("admin-meta");
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

        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &main,
            &WorktreeRequest::Named("separate-main/x".into()),
            "separate",
            &settings(Some(root.path())),
            data.path(),
            None,
        )
        .expect_err("the location holds the main checkout");
        assert!(error.contains("inside the project checkout"), "{error}");
        assert!(
            error.contains("api"),
            "the refusal names the checkout: {error}"
        );
        assert!(
            !main.join(folder_name("separate-main/x")).exists(),
            "nothing was created in the main checkout"
        );
    }

    /// An unreadable sibling admin entry means containment cannot be proved,
    /// so the location is refused rather than allowing a nested add.
    #[test]
    fn an_unreadable_sibling_registration_refuses_the_location() {
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
        let common = git::common_dir(&main, git::OBSERVATION_GIT_TIMEOUT).expect("common");
        let admin = std::fs::read_dir(common.join("worktrees"))
            .expect("admin entries")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                std::fs::read_to_string(path.join("gitdir"))
                    .ok()
                    .is_some_and(|text| text.contains("feature"))
            })
            .expect("sibling admin");
        std::fs::remove_file(admin.join("gitdir")).expect("remove gitdir");
        std::fs::create_dir(admin.join("gitdir")).expect("unreadable gitdir");

        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            &main,
            &WorktreeRequest::Named("sibling/unreadable".into()),
            "sibling",
            &settings(Some(&sibling)),
            data.path(),
            None,
        )
        .expect_err("containment cannot be established");
        assert!(error.contains("could not be checked"), "{error}");
        assert!(
            !sibling
                .join(project_name(&main))
                .join(folder_name("sibling/unreadable"))
                .exists(),
            "nothing was created inside the unreadable sibling"
        );
        assert!(
            git::run_with_timeout(
                &main,
                ["rev-parse", "--verify", "refs/heads/sibling/unreadable"],
                CREATE_TIMEOUT,
            )
            .is_err(),
            "no branch was made"
        );
    }
}
