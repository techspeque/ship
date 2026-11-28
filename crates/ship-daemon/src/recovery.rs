//! What a daemon start does about deployments a previous daemon left in
//! flight (phase-1-ws-1.14), after [`crate::queue::recover`] marked their
//! operations `interrupted`.
//!
//! It runs no Docker command (ADR-0006 rule 11). Each deployment with no
//! result is ended `interrupted`; its record says which release served
//! before it, and that stays the one Ship reports serving. What an
//! interrupted operation staged on disk is removed, and nothing else. The
//! `interrupted` result is what makes it eligible, so every start retries
//! staging an earlier start could not remove:
//!
//! - a release directory it allocated but never recorded (known by its
//!   [`OPERATION_FILE`]) is removed whole: no release names it. It is
//!   first renamed into [`TRASH_DIR`], atomically: before the rename it is
//!   still marked, after it everything in the trash is garbage, and every
//!   start empties the trash first, so a start that dies or fails at any
//!   point leaves the next one something to finish (f-051, f-053);
//! - a release it recorded keeps its files, since a release never changes,
//!   and loses only its env files: no deployment will bring it up again,
//!   so its values need not stay on disk (ADR-0009).
//!
//! In `deploying` and `verifying` the new containers may already run in
//! place of the old (Human, 2026-11-28); Docker is left as it is and
//! `ship status` names the command that puts it right
//! ([`crate::status::interruption`]).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ship_core::{DeploymentResult, OperationId, ReleaseId};
use ship_store::{Deployment, Store, StoreError};

use crate::config::ENV_DIR;
use crate::deploy::{OPERATION_FILE, RELEASES_DIR, release_dir};

/// The directory, in the state directory, that unrecorded release
/// directories are renamed into before they are removed. Everything in it
/// is garbage; [`recover`] empties it at every start.
pub const TRASH_DIR: &str = "trash";

/// What [`recover`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Deployments ended `interrupted`.
    pub deployments: usize,
    /// Release directories removed whole (never recorded).
    pub staged_releases: usize,
    /// Recorded releases whose env files were removed.
    pub pruned_releases: usize,
}

/// Ends every deployment left without a result as `interrupted` and
/// removes what the operation of every `interrupted` deployment staged,
/// including one an earlier start ended but could not clean up. Run at
/// start, after [`crate::queue::recover`] and before anything is served:
/// no operation is running, so a deployment with no result is one a dead
/// daemon left. A file that cannot be removed is logged and retried at the
/// next start; the store failing fails.
pub async fn recover(store: &Store, state_dir: &Path) -> Result<Recovered, StoreError> {
    empty_trash(state_dir);
    let mut open: Vec<Deployment> = Vec::new();
    let mut operations: BTreeSet<OperationId> = BTreeSet::new();
    for project in store.list_projects().await? {
        for environment in store.list_environments(project.id).await? {
            for d in store.list_deployments(environment.id).await? {
                match d.result {
                    None => {
                        operations.insert(d.operation_id);
                        open.push(d);
                    }
                    Some(DeploymentResult::Interrupted) => {
                        operations.insert(d.operation_id);
                    }
                    Some(_) => {}
                }
            }
        }
    }
    let mut done = Recovered::default();
    if operations.is_empty() {
        return Ok(done);
    }
    for d in &open {
        store
            .finish_deployment(d.id, DeploymentResult::Interrupted)
            .await?;
        done.deployments += 1;
        tracing::warn!(
            deployment = %d.id,
            operation = %d.operation_id,
            "deployment interrupted by a daemon restart; Docker is left as it is"
        );
    }
    for (release, dir) in staged_by(state_dir, &operations) {
        // Recorded (the deployment may not name it yet): its files stay.
        match store.get_release(release).await {
            Ok(_) => {
                if prune_env(&dir, release) {
                    done.pruned_releases += 1;
                }
            }
            Err(StoreError::NotFound { .. }) => match remove_staged(state_dir, &dir, release) {
                Ok(()) => done.staged_releases += 1,
                Err(e) => {
                    tracing::error!(release = %release, error = %e,
                        "the staging of an interrupted release was not removed");
                }
            },
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Removes the unrecorded release directory `dir` whole: renames it into
/// [`TRASH_DIR`], then removes it from there. A failed rename leaves `dir`
/// where it was, marked, for the next start; once renamed it is garbage
/// whatever happens, and a removal that fails is finished by the next
/// start's [`empty_trash`].
fn remove_staged(state_dir: &Path, dir: &Path, release: ReleaseId) -> io::Result<()> {
    let trash = state_dir.join(TRASH_DIR);
    create_private_dir(&trash)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let entry = trash.join(format!("{release}-{nanos}"));
    fs::rename(dir, &entry)?;
    if let Err(e) = fs::remove_dir_all(&entry) {
        tracing::error!(release = %release, error = %e,
            "the staging of an interrupted release is in the trash; the next start removes it");
    }
    Ok(())
}

/// Removes everything in [`TRASH_DIR`]. An entry that cannot be removed is
/// logged and left for the next start; the start goes on.
fn empty_trash(state_dir: &Path) {
    let Ok(entries) = fs::read_dir(state_dir.join(TRASH_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(t) if t.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(e) => Err(e),
        };
        if let Err(e) = removed {
            tracing::error!(path = %path.display(), error = %e,
                "a trash entry was not removed; the next start retries");
        }
    }
}

/// Creates `dir`, readable by the daemon's user only, if it is missing.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Removes the env files of `release`, in `dir`; whether there were any.
fn prune_env(dir: &Path, release: ReleaseId) -> bool {
    match fs::remove_dir_all(dir.join(ENV_DIR)) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::error!(release = %release, error = %e,
                "the env files of an interrupted release were not removed");
            false
        }
    }
}

/// The release directories whose [`OPERATION_FILE`] names one of
/// `operations`, with their release ids. A directory without that file
/// (from before it was written) or of another operation is not listed.
fn staged_by(state_dir: &Path, operations: &BTreeSet<OperationId>) -> Vec<(ReleaseId, PathBuf)> {
    let Ok(entries) = fs::read_dir(state_dir.join(RELEASES_DIR)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(release) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<ReleaseId>().ok())
        else {
            continue;
        };
        let dir = release_dir(state_dir, release);
        let made_by = fs::read_to_string(dir.join(OPERATION_FILE))
            .ok()
            .and_then(|s| s.trim().parse::<OperationId>().ok());
        if made_by.is_some_and(|op| operations.contains(&op)) {
            out.push((release, dir));
        }
    }
    out
}
