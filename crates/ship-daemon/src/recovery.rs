//! What a daemon start does about deployments a previous daemon left in
//! flight (phase-1-ws-1.14), after [`crate::queue::recover`] marked their
//! operations `interrupted`.
//!
//! It runs no Docker command (ADR-0006 rule 11). Each deployment with no
//! result is ended `interrupted`; its record says which release served
//! before it, and that stays the one Ship reports serving. What the
//! interrupted operation staged on disk is removed, and nothing else:
//!
//! - a release directory it allocated but never recorded (known by its
//!   [`OPERATION_FILE`]) is removed whole: no release names it;
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

use ship_core::{DeploymentResult, OperationId, ReleaseId};
use ship_store::{Deployment, Store, StoreError};

use crate::config::ENV_DIR;
use crate::deploy::{OPERATION_FILE, RELEASES_DIR, release_dir};

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
/// removes what its operation staged. Run at start, after
/// [`crate::queue::recover`] and before anything is served: no operation
/// is running, so a deployment with no result is one a dead daemon left.
/// A file that cannot be removed is logged; the store failing fails.
pub async fn recover(store: &Store, state_dir: &Path) -> Result<Recovered, StoreError> {
    let mut open: Vec<Deployment> = Vec::new();
    for project in store.list_projects().await? {
        for environment in store.list_environments(project.id).await? {
            open.extend(
                store
                    .list_deployments(environment.id)
                    .await?
                    .into_iter()
                    .filter(|d| d.result.is_none()),
            );
        }
    }
    let mut done = Recovered::default();
    if open.is_empty() {
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
    let operations: BTreeSet<OperationId> = open.iter().map(|d| d.operation_id).collect();
    for (release, dir) in staged_by(state_dir, &operations) {
        // Recorded (the deployment may not name it yet): its files stay.
        match store.get_release(release).await {
            Ok(_) => {
                if prune_env(&dir, release) {
                    done.pruned_releases += 1;
                }
            }
            Err(StoreError::NotFound { .. }) => match fs::remove_dir_all(&dir) {
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
