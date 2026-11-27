//! Snapshot uploads (ADR-0005, daemon side).
//!
//! `POST /api/v1/projects/{project}/uploads` carries a tar + zstd snapshot
//! as its body and the stream's SHA-256 in `Ship-Snapshot-Sha256`. The
//! upload is an `upload` operation (ADR-0011): [`receive`], its job,
//! streams the body over a bounded channel into
//! [`ship_source::unpack`], which validates every entry as it arrives and
//! writes only into `<state>/staging/<operation id>`. A stream that passes
//! every check and matches its declared hash is promoted, under one lock,
//! to `<state>/snapshots/<sha256>` and recorded as the project's
//! `snapshot` revision with that hash; the same tree uploaded again is the
//! same revision. Any failure removes the staging directory, so a failed
//! upload leaves only its failed operation (rule 7).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::HeaderMap;
use futures_util::StreamExt;
use ship_core::api::{GIT_COMMIT_HEADER, GIT_DIRTY_HEADER, SNAPSHOT_SHA256_HEADER, UploadView};
use ship_core::{ApiError, ErrorCode, OperationId, OperationPhase, ProjectId, RevisionKind, Slug};
use ship_source::{UnpackError, UnpackLimits, Unpacked};
use ship_store::{NewRevision, Operation, Revision, Store};
use tokio::sync::{Mutex, mpsc};

use crate::queue::{Context, JobFailure};

/// Directory under the state directory holding in-flight uploads, one
/// directory per operation.
pub const STAGING_DIR: &str = "staging";
/// Directory under the state directory holding promoted snapshots, one
/// directory per content hash.
pub const SNAPSHOTS_DIR: &str = "snapshots";
/// Body chunks buffered between the connection and the unpacker.
const BODY_CHANNEL: usize = 8;

/// Where a snapshot with `sha256` is kept once promoted.
pub fn snapshot_dir(state_dir: &Path, sha256: &str) -> PathBuf {
    state_dir.join(SNAPSHOTS_DIR).join(sha256)
}

/// The staging directory of the upload `operation`.
pub fn staging_dir(state_dir: &Path, operation: OperationId) -> PathBuf {
    state_dir.join(STAGING_DIR).join(operation.to_string())
}

/// The daemon's upload settings and promotion lock.
#[derive(Debug)]
pub struct Uploads {
    state_dir: PathBuf,
    limits: UnpackLimits,
    /// Held from promoting a snapshot until its revision is recorded, so
    /// two uploads of one tree make one directory and one revision.
    promote: Mutex<()>,
}

impl Uploads {
    /// Upload settings under `state_dir`, creating the staging and
    /// snapshot directories (0700) if missing.
    pub fn new(state_dir: &Path, limits: UnpackLimits) -> io::Result<Arc<Uploads>> {
        use std::os::unix::fs::DirBuilderExt;
        for dir in [STAGING_DIR, SNAPSHOTS_DIR] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(state_dir.join(dir))?;
        }
        Ok(Arc::new(Uploads {
            state_dir: state_dir.to_path_buf(),
            limits,
            promote: Mutex::new(()),
        }))
    }

    pub fn limits(&self) -> UnpackLimits {
        self.limits
    }
}

/// Removes every staging directory a previous daemon left behind (it died
/// mid-upload; its operation is interrupted by [`crate::queue::recover`])
/// and returns how many there were.
pub fn clean_staging(state_dir: &Path) -> io::Result<usize> {
    let root = state_dir.join(STAGING_DIR);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        removed += 1;
    }
    Ok(removed)
}

/// What the client told us beside the snapshot. Its Git state is recorded
/// as information only (ADR-0005 rule 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRequest {
    pub project: Slug,
    /// The declared SHA-256 of the compressed stream.
    pub sha256: String,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest, message)
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Reads an upload's path segment and headers. Refused before any
/// operation exists: these are malformed requests, not failed uploads.
pub fn parse_request(project: &str, headers: &HeaderMap) -> Result<UploadRequest, ApiError> {
    let project = Slug::parse(project)?;
    let header = |name: &str| -> Result<Option<&str>, ApiError> {
        match headers.get(name) {
            None => Ok(None),
            Some(v) => v
                .to_str()
                .map(Some)
                .map_err(|_| invalid("upload headers must be visible ASCII")),
        }
    };
    let sha256 = header(SNAPSHOT_SHA256_HEADER)?
        .ok_or_else(|| invalid("the Ship-Snapshot-Sha256 header is required"))?;
    if sha256.len() != 64 || !is_lower_hex(sha256) {
        return Err(invalid(
            "Ship-Snapshot-Sha256 must be 64 lowercase hex digits",
        ));
    }
    let git_commit = header(GIT_COMMIT_HEADER)?;
    if let Some(c) = git_commit
        && (!(7..=64).contains(&c.len()) || !is_lower_hex(c))
    {
        return Err(invalid(
            "Ship-Git-Commit must be 7 to 64 lowercase hex digits",
        ));
    }
    let git_dirty = match header(GIT_DIRTY_HEADER)? {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => return Err(invalid("Ship-Git-Dirty must be `true` or `false`")),
    };
    Ok(UploadRequest {
        project,
        sha256: sha256.to_owned(),
        git_commit: git_commit.map(str::to_owned),
        git_dirty,
    })
}

/// The operation log line for the client's Git state, if it sent any.
fn git_line(request: &UploadRequest) -> Option<String> {
    let dirty = match request.git_dirty {
        Some(true) => " with uncommitted changes",
        Some(false) => ", clean",
        None => "",
    };
    match (&request.git_commit, request.git_dirty) {
        (Some(commit), _) => Some(format!(
            "client reports git commit {commit}{dirty} (information only)"
        )),
        (None, Some(_)) => Some(format!(
            "client reports no git commit{dirty} (information only)"
        )),
        (None, None) => None,
    }
}

/// An upload stored as a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub project_id: ProjectId,
    pub revision: Revision,
    pub new_revision: bool,
    pub unpacked: Unpacked,
}

impl Received {
    pub fn view(&self, operation: OperationId) -> UploadView {
        UploadView {
            operation_id: operation,
            project_id: self.project_id,
            revision_id: self.revision.id,
            sha256: self.revision.source_ref.clone(),
            new_revision: self.new_revision,
        }
    }
}

/// Removes a staging directory when dropped, unless disarmed.
struct StagingGuard(Option<PathBuf>);

impl StagingGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Receives `body` as a snapshot of `request.project` inside the running
/// operation `ctx`: phases `uploading`, then `validating`. On success the
/// snapshot is a revision; on failure nothing of it is left on disk.
///
/// Public so an operation that carries an upload inside it (`ship push`)
/// runs the same steps.
pub async fn receive(
    ctx: &Context,
    store: &Store,
    uploads: &Uploads,
    request: &UploadRequest,
    body: Body,
) -> Result<Received, JobFailure> {
    ctx.enter(OperationPhase::Uploading, "receiving the snapshot")
        .await?;
    if let Some(line) = git_line(request) {
        ctx.log(&line).await?;
    }
    let ensured = store.get_or_create_project(&request.project).await?;
    let project_id = ensured.project.id;

    let staging = staging_dir(&uploads.state_dir, ctx.operation_id());
    let unpacked = stream_unpack(body, staging.clone(), uploads.limits).await?;
    let mut guard = StagingGuard(Some(staging.clone()));

    ctx.enter(
        OperationPhase::Validating,
        &format!(
            "received {} files, {} bytes; checking the digest",
            unpacked.files, unpacked.bytes
        ),
    )
    .await?;
    if unpacked.sha256 != request.sha256 {
        return Err(JobFailure::new(
            ErrorCode::DigestMismatch,
            format!(
                "the snapshot's SHA-256 is {}, not the declared {}",
                unpacked.sha256, request.sha256
            ),
        ));
    }

    let _promoting = uploads.promote.lock().await;
    // Everything fallible but recording the revision happens before the
    // snapshot is published, so a failure leaves nothing new behind.
    let existing = find_revision(store, project_id, &unpacked.sha256).await?;
    match &existing {
        Some(revision) => {
            ctx.log(&format!(
                "revision {} (sha256 {}), unchanged",
                revision.id, revision.source_ref
            ))
            .await?
        }
        None => {
            ctx.log(&format!(
                "recording a new revision (sha256 {})",
                unpacked.sha256
            ))
            .await?
        }
    }
    let target = snapshot_dir(&uploads.state_dir, &unpacked.sha256);
    let moved = {
        let (staging, target) = (staging.clone(), target.clone());
        tokio::task::spawn_blocking(move || promote(&staging, &target)).await
    };
    // Removes the snapshot this upload published (never one that was
    // already there) unless its revision is recorded.
    let mut published = StagingGuard(None);
    match moved {
        Ok(Ok(moved)) => {
            guard.disarm();
            if moved {
                published = StagingGuard(Some(target.clone()));
            }
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, path = %target.display(), "promoting a snapshot failed");
            return Err(storage_failure(
                &e,
                "the daemon could not store the snapshot",
            ));
        }
        Err(e) => {
            tracing::error!(error = %e, "promoting a snapshot failed");
            return Err(JobFailure::new(
                ErrorCode::Internal,
                "the daemon could not store the snapshot",
            ));
        }
    }

    let (revision, new_revision) = match existing {
        Some(revision) => (revision, false),
        None => {
            let revision = store
                .create_revision(
                    project_id,
                    &NewRevision {
                        kind: RevisionKind::Snapshot,
                        source_ref: &unpacked.sha256,
                        manifest_fingerprint: None,
                    },
                )
                .await?;
            (revision, true)
        }
    };
    published.disarm();
    Ok(Received {
        project_id,
        revision,
        new_revision,
        unpacked,
    })
}

/// Moves `staging` to `target` (`true`), or discards it when `target`
/// already holds the same content, same hash (`false`).
fn promote(staging: &Path, target: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(staging).map(|()| false),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the snapshot path exists and is not a directory",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::rename(staging, target).map(|()| true),
        Err(e) => Err(e),
    }
}

/// The project's `snapshot` revision with content hash `sha256`.
async fn find_revision(
    store: &Store,
    project: ProjectId,
    sha256: &str,
) -> Result<Option<Revision>, JobFailure> {
    Ok(store
        .list_revisions(project)
        .await?
        .into_iter()
        .find(|r| r.kind == RevisionKind::Snapshot && r.source_ref == sha256))
}

/// `Internal`, or `InsufficientStorage` when the disk is full.
fn storage_failure(e: &io::Error, message: &str) -> JobFailure {
    if e.kind() == io::ErrorKind::StorageFull {
        JobFailure::new(
            ErrorCode::InsufficientStorage,
            "the state directory is full",
        )
    } else {
        JobFailure::new(ErrorCode::Internal, message)
    }
}

/// The failure an unpack error ends the upload with.
fn unpack_failure(e: &UnpackError) -> JobFailure {
    match e {
        UnpackError::Unsafe { .. } => JobFailure::new(ErrorCode::UnsafeSnapshot, e.to_string()),
        UnpackError::TooLarge { .. } | UnpackError::Expansion { .. } => {
            JobFailure::new(ErrorCode::PayloadTooLarge, e.to_string())
        }
        UnpackError::Malformed(_) | UnpackError::Transfer(_) => {
            JobFailure::new(ErrorCode::InvalidRequest, e.to_string())
        }
        UnpackError::Staging { path, source } => {
            tracing::error!(error = %source, path = %path.display(), "writing a snapshot failed");
            storage_failure(source, "the daemon could not write the snapshot")
        }
    }
}

/// Feeds `body` to [`ship_source::unpack`] on a blocking thread through a
/// channel of [`BODY_CHANNEL`] chunks. The unpacker's end is watched while
/// the body is read, so when it stops early (a refused entry) the upload
/// ends at once and the rest of the body is not read, even if the client
/// holds it open.
async fn stream_unpack(
    body: Body,
    staging: PathBuf,
    limits: UnpackLimits,
) -> Result<Unpacked, JobFailure> {
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(BODY_CHANNEL);
    let mut unpacking = tokio::task::spawn_blocking(move || {
        ship_source::unpack(ChannelReader::new(rx), &staging, &limits)
    });
    let mut chunks = body.into_data_stream();
    // `None` once the body has ended or failed: the unpacker sees the end.
    let mut tx = Some(tx);
    let finished = loop {
        tokio::select! {
            biased;
            finished = &mut unpacking => break finished,
            chunk = chunks.next(), if tx.is_some() => {
                let Some(chunk) = chunk else {
                    tx = None;
                    continue;
                };
                let chunk = chunk
                    .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string()));
                let failed = chunk.is_err();
                // The send fails only once the unpacker has ended; its
                // result is taken on the next turn.
                let sender = tx.as_ref().expect("guarded by the branch");
                if sender.send(chunk).await.is_err() || failed {
                    tx = None;
                }
            }
        }
    };
    drop(tx);
    match finished {
        Ok(Ok(unpacked)) => Ok(unpacked),
        Ok(Err(e)) => Err(unpack_failure(&e)),
        Err(e) => {
            tracing::error!(error = %e, "the unpacker failed");
            Err(JobFailure::new(
                ErrorCode::Internal,
                "the snapshot could not be unpacked",
            ))
        }
    }
}

/// A blocking reader over the body chunks; end of input when the sender
/// is dropped.
struct ChannelReader {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    chunk: Bytes,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<io::Result<Bytes>>) -> Self {
        ChannelReader {
            rx,
            chunk: Bytes::new(),
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.chunk.is_empty() {
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => self.chunk = chunk,
                Some(Err(e)) => return Err(e),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len());
        buf[..n].copy_from_slice(&self.chunk[..n]);
        self.chunk = self.chunk.slice(n..);
        Ok(n)
    }
}

/// How a replayed idempotency key is answered: the body is never read.
/// A succeeded upload answers its revision, found by project and declared
/// hash (none: the key was used for another upload); a failed one its
/// failure; one still running `409`.
pub enum Replay {
    Done(UploadView),
    Failed(ApiError),
}

/// See [`Replay`].
pub async fn replay(store: &Store, op: &Operation, request: &UploadRequest) -> Replay {
    let internal = || {
        Replay::Failed(
            ApiError::new(
                ErrorCode::Internal,
                "the daemon could not read its database",
            )
            .with_operation(op.id),
        )
    };
    match op.phase {
        OperationPhase::Succeeded => {
            let projects = match store.list_projects().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "reading projects failed");
                    return internal();
                }
            };
            let Some(project) = projects.into_iter().find(|p| p.slug == request.project) else {
                return Replay::Failed(conflict(op.id));
            };
            match find_revision(store, project.id, &request.sha256).await {
                Ok(Some(revision)) => Replay::Done(UploadView {
                    operation_id: op.id,
                    project_id: project.id,
                    revision_id: revision.id,
                    sha256: revision.source_ref,
                    new_revision: false,
                }),
                Ok(None) => Replay::Failed(conflict(op.id)),
                Err(_) => internal(),
            }
        }
        phase if phase.is_terminal() => {
            let code = op
                .failure_code
                .as_deref()
                .and_then(|c| c.parse::<ErrorCode>().ok())
                .unwrap_or(ErrorCode::Internal);
            let message = op
                .failure_message
                .clone()
                .unwrap_or_else(|| "the upload failed".to_owned());
            Replay::Failed(ApiError::new(code, message).with_operation(op.id))
        }
        _ => Replay::Failed(
            ApiError::new(
                ErrorCode::Conflict,
                "an upload with this idempotency key is still running",
            )
            .with_operation(op.id),
        ),
    }
}

fn conflict(op: OperationId) -> ApiError {
    ApiError::new(
        ErrorCode::IdempotencyConflict,
        "this idempotency key was already used for a different upload",
    )
    .with_operation(op)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn requests_are_parsed() {
        let r = parse_request("my-app", &headers(&[("ship-snapshot-sha256", SHA)])).unwrap();
        assert_eq!(r.project.as_str(), "my-app");
        assert_eq!(r.sha256, SHA);
        assert_eq!((r.git_commit, r.git_dirty), (None, None));

        let r = parse_request(
            "my-app",
            &headers(&[
                ("ship-snapshot-sha256", SHA),
                ("ship-git-commit", "abc1234"),
                ("ship-git-dirty", "true"),
            ]),
        )
        .unwrap();
        assert_eq!(r.git_commit.as_deref(), Some("abc1234"));
        assert_eq!(r.git_dirty, Some(true));
    }

    #[test]
    fn bad_requests_are_refused() {
        let upper = SHA.to_uppercase();
        let upper: &'static str = Box::leak(upper.into_boxed_str());
        let cases: Vec<(&str, HeaderMap, ErrorCode)> = vec![
            (
                "My-App",
                headers(&[("ship-snapshot-sha256", SHA)]),
                ErrorCode::InvalidSlug,
            ),
            ("my-app", headers(&[]), ErrorCode::InvalidRequest),
            (
                "my-app",
                headers(&[("ship-snapshot-sha256", "abc")]),
                ErrorCode::InvalidRequest,
            ),
            (
                "my-app",
                headers(&[("ship-snapshot-sha256", upper)]),
                ErrorCode::InvalidRequest,
            ),
            (
                "my-app",
                headers(&[("ship-snapshot-sha256", SHA), ("ship-git-commit", "abc")]),
                ErrorCode::InvalidRequest,
            ),
            (
                "my-app",
                headers(&[
                    ("ship-snapshot-sha256", SHA),
                    ("ship-git-commit", "ABC1234"),
                ]),
                ErrorCode::InvalidRequest,
            ),
            (
                "my-app",
                headers(&[("ship-snapshot-sha256", SHA), ("ship-git-dirty", "yes")]),
                ErrorCode::InvalidRequest,
            ),
        ];
        for (project, h, code) in cases {
            let e = parse_request(project, &h).unwrap_err();
            assert_eq!(e.code, code, "{project} {h:?}");
        }
    }

    #[test]
    fn git_lines() {
        let mut r = parse_request("a", &headers(&[("ship-snapshot-sha256", SHA)])).unwrap();
        assert_eq!(git_line(&r), None);
        r.git_commit = Some("abc1234".into());
        assert_eq!(
            git_line(&r).unwrap(),
            "client reports git commit abc1234 (information only)"
        );
        r.git_dirty = Some(true);
        assert_eq!(
            git_line(&r).unwrap(),
            "client reports git commit abc1234 with uncommitted changes (information only)"
        );
        r.git_commit = None;
        r.git_dirty = Some(false);
        assert_eq!(
            git_line(&r).unwrap(),
            "client reports no git commit, clean (information only)"
        );
    }

    #[test]
    fn unpack_errors_map_to_stable_codes() {
        use ship_source::{Limit, UnsafeReason};
        let cases = [
            (
                UnpackError::Unsafe {
                    path: "x".into(),
                    reason: UnsafeReason::Symlink,
                },
                ErrorCode::UnsafeSnapshot,
            ),
            (
                UnpackError::TooLarge {
                    limit: Limit::Bytes,
                    max: 1,
                },
                ErrorCode::PayloadTooLarge,
            ),
            (
                UnpackError::Expansion {
                    expanded: 2,
                    compressed: 1,
                },
                ErrorCode::PayloadTooLarge,
            ),
            (
                UnpackError::Malformed("x".into()),
                ErrorCode::InvalidRequest,
            ),
            (
                UnpackError::Transfer(io::Error::other("reset")),
                ErrorCode::InvalidRequest,
            ),
            (
                UnpackError::Staging {
                    path: "/x".into(),
                    source: io::Error::from(io::ErrorKind::StorageFull),
                },
                ErrorCode::InsufficientStorage,
            ),
            (
                UnpackError::Staging {
                    path: "/x".into(),
                    source: io::Error::from(io::ErrorKind::PermissionDenied),
                },
                ErrorCode::Internal,
            ),
        ];
        for (e, code) in cases {
            assert_eq!(unpack_failure(&e).code, code, "{e}");
        }
    }

    #[test]
    fn channel_reader_reads_chunks_then_ends() {
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Ok(Bytes::from_static(b"hel"))).unwrap();
        tx.try_send(Ok(Bytes::from_static(b"lo"))).unwrap();
        drop(tx);
        let mut out = String::new();
        ChannelReader::new(rx).read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello");

        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Ok(Bytes::from_static(b"x"))).unwrap();
        tx.try_send(Err(io::Error::other("broken"))).unwrap();
        let mut out = Vec::new();
        assert!(ChannelReader::new(rx).read_to_end(&mut out).is_err());
    }
}
