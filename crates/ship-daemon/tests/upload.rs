//! Snapshot uploads over the daemon's unix socket (ADR-0005): a packed
//! tree becomes a revision, the same tree again is the same revision, and
//! every failed upload leaves no revision, no staging directory and a
//! failed `upload` operation.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Bytes;
use ship_core::api::UPLOAD_KIND;
use ship_core::{OperationId, OperationPhase, ProjectId, Slug};
use ship_daemon::UnpackLimits;
use ship_daemon::upload::{STAGING_DIR, snapshot_dir};
use ship_net::Target;
use ship_net::client::{self, RequestBody};
use ship_source::{Options, pack, scan};

const TIMEOUT: Duration = Duration::from_secs(30);

struct Daemon {
    _dir: tempfile::TempDir,
    state: PathBuf,
    target: Target,
    running: ship_daemon::Running,
}

async fn daemon(limits: UnpackLimits) -> Daemon {
    daemon_with(limits, ship_daemon::Redactor::new()).await
}

async fn daemon_with(limits: UnpackLimits, redactor: ship_daemon::Redactor) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::config(dir.path());
    config.upload_limits = limits;
    config.redactor = redactor;
    let state = config.state_dir.clone();
    let running = ship_daemon::start(&config).await.unwrap();
    let target = Target::Local {
        socket: running.socket_path().to_path_buf(),
    };
    Daemon {
        _dir: dir,
        state,
        target,
        running,
    }
}

/// A small tree packed by the client side; (stream, sha256).
fn packed_tree(root: &Path, marker: &str) -> (Vec<u8>, String) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Dockerfile"), b"FROM scratch\n").unwrap();
    fs::write(root.join("src/main.txt"), marker.as_bytes()).unwrap();
    let options = Options {
        home: Some(PathBuf::from("/nonexistent/ship-test-home")),
        ..Options::default()
    };
    let selection = scan(root, &options).unwrap();
    let mut stream = Vec::new();
    let packed = pack(&selection, &mut stream).unwrap();
    (stream, packed.sha256)
}

struct Answer {
    status: u16,
    json: serde_json::Value,
}

async fn upload(
    d: &Daemon,
    project: &str,
    key: &str,
    headers: &[(&str, &str)],
    body: RequestBody,
) -> Answer {
    let mut all = vec![("idempotency-key", key)];
    all.extend_from_slice(headers);
    let path = format!("/api/v1/projects/{project}/uploads");
    let r = client::post(&d.target, &path, &all, body, TIMEOUT)
        .await
        .unwrap();
    let json = serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)));
    Answer {
        status: r.status,
        json,
    }
}

async fn upload_bytes(d: &Daemon, project: &str, key: &str, sha: &str, body: &[u8]) -> Answer {
    upload(
        d,
        project,
        key,
        &[("ship-snapshot-sha256", sha)],
        RequestBody::bytes(body.to_vec()),
    )
    .await
}

fn staging_entries(d: &Daemon) -> Vec<PathBuf> {
    fs::read_dir(d.state.join(STAGING_DIR))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

async fn revisions(d: &Daemon, project: &str) -> Vec<ship_store::Revision> {
    let slug = Slug::parse(project).unwrap();
    let projects = d.running.store().list_projects().await.unwrap();
    match projects.into_iter().find(|p| p.slug == slug) {
        Some(p) => d.running.store().list_revisions(p.id).await.unwrap(),
        None => Vec::new(),
    }
}

/// Asserts a failed upload: `status` and `code`, a failed `upload`
/// operation with that code, nothing staged and no revision.
async fn assert_failed(d: &Daemon, a: &Answer, project: &str, status: u16, code: &str) {
    assert_eq!(a.status, status, "{}", a.json);
    assert_eq!(a.json["code"], code, "{}", a.json);
    let op: OperationId = a.json["operation_id"].as_str().unwrap().parse().unwrap();
    let op = d.running.store().get_operation(op).await.unwrap();
    assert_eq!(op.kind, UPLOAD_KIND);
    assert_eq!(op.phase, OperationPhase::Failed);
    assert_eq!(op.failure_code.as_deref(), Some(code));
    assert!(staging_entries(d).is_empty(), "{:?}", staging_entries(d));
    assert!(revisions(d, project).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_tree_is_one_revision_per_project() {
    let d = daemon(UnpackLimits::default()).await;
    let src = tempfile::tempdir().unwrap();
    let (stream, sha) = packed_tree(src.path(), "v1");

    let first = upload(
        &d,
        "my-app",
        "k1",
        &[
            ("ship-snapshot-sha256", sha.as_str()),
            ("ship-git-commit", "abc1234"),
            ("ship-git-dirty", "true"),
        ],
        RequestBody::bytes(stream.clone()),
    )
    .await;
    assert_eq!(first.status, 201, "{}", first.json);
    assert_eq!(first.json["sha256"], sha.as_str());
    assert_eq!(first.json["new_revision"], true);
    let revision = first.json["revision_id"].as_str().unwrap().to_owned();
    let project: ProjectId = first.json["project_id"].as_str().unwrap().parse().unwrap();

    // The snapshot is on disk under its hash, exactly as packed.
    let snapshot = snapshot_dir(&d.state, &sha);
    assert_eq!(
        fs::read(snapshot.join("Dockerfile")).unwrap(),
        b"FROM scratch\n"
    );
    assert_eq!(fs::read(snapshot.join("src/main.txt")).unwrap(), b"v1");
    assert!(staging_entries(&d).is_empty());

    // The operation succeeded and logged the client's git state.
    let op: OperationId = first.json["operation_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stored = d.running.store().get_operation(op).await.unwrap();
    assert_eq!(stored.kind, UPLOAD_KIND);
    assert_eq!(stored.phase, OperationPhase::Succeeded);
    assert_eq!(stored.environment_id, None);
    let log = fs::read_to_string(d.running.queue().log_path(op)).unwrap();
    assert!(
        log.contains("client reports git commit abc1234 with uncommitted changes"),
        "{log}"
    );

    // Again, under a new key: the same revision, not a new one.
    let second = upload_bytes(&d, "my-app", "k2", &sha, &stream).await;
    assert_eq!(second.status, 200, "{}", second.json);
    assert_eq!(second.json["revision_id"], revision.as_str());
    assert_eq!(second.json["new_revision"], false);
    let revs = revisions(&d, "my-app").await;
    assert_eq!(revs.len(), 1);
    assert_eq!(revs[0].source_ref, sha);
    assert_eq!(revs[0].project_id, project);

    // Another project: its own revision of the same content.
    let other = upload_bytes(&d, "other-app", "k3", &sha, &stream).await;
    assert_eq!(other.status, 201, "{}", other.json);
    assert_ne!(other.json["revision_id"], revision.as_str());
    assert_eq!(other.json["sha256"], sha.as_str());

    // A changed tree is a new revision.
    let src2 = tempfile::tempdir().unwrap();
    let (stream2, sha2) = packed_tree(src2.path(), "v2");
    assert_ne!(sha2, sha);
    let third = upload_bytes(&d, "my-app", "k4", &sha2, &stream2).await;
    assert_eq!(third.status, 201, "{}", third.json);
    assert_eq!(revisions(&d, "my-app").await.len(), 2);

    d.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_key_answers_without_reading_the_body() {
    let d = daemon(UnpackLimits::default()).await;
    let src = tempfile::tempdir().unwrap();
    let (stream, sha) = packed_tree(src.path(), "replay");
    let first = upload_bytes(&d, "my-app", "same-key", &sha, &stream).await;
    assert_eq!(first.status, 201, "{}", first.json);

    let replay = upload_bytes(&d, "my-app", "same-key", &sha, b"not a snapshot").await;
    assert_eq!(replay.status, 200, "{}", replay.json);
    assert_eq!(replay.json["operation_id"], first.json["operation_id"]);
    assert_eq!(replay.json["revision_id"], first.json["revision_id"]);

    // The same key for another upload is a conflict.
    let other = "1".repeat(64);
    let conflict = upload_bytes(&d, "my-app", "same-key", &other, b"x").await;
    assert_eq!(conflict.status, 422, "{}", conflict.json);
    assert_eq!(conflict.json["code"], "idempotency_conflict");

    // A failed upload replays its failure.
    let failed = upload_bytes(&d, "my-app", "bad-key", &other, &stream).await;
    assert_eq!(failed.json["code"], "digest_mismatch");
    let again = upload_bytes(&d, "my-app", "bad-key", &other, &stream).await;
    assert_eq!(again.status, failed.status);
    assert_eq!(again.json["code"], "digest_mismatch");
    assert_eq!(again.json["operation_id"], failed.json["operation_id"]);

    d.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_requests_are_refused_before_any_operation() {
    let d = daemon(UnpackLimits::default()).await;
    let sha = "a".repeat(64);
    let sha = sha.as_str();
    let cases = vec![
        ("my-app", vec![("idempotency-key", "k")], "invalid_request"),
        (
            "my-app",
            vec![("idempotency-key", "k"), ("ship-snapshot-sha256", "abc")],
            "invalid_request",
        ),
        (
            "my-app",
            vec![("ship-snapshot-sha256", sha)],
            "invalid_request",
        ),
        (
            "Not_A_Slug",
            vec![("idempotency-key", "k"), ("ship-snapshot-sha256", sha)],
            "invalid_slug",
        ),
        (
            "my-app",
            vec![
                ("idempotency-key", "k"),
                ("ship-snapshot-sha256", sha),
                ("ship-git-dirty", "maybe"),
            ],
            "invalid_request",
        ),
    ];
    for (project, headers, code) in cases {
        let path = format!("/api/v1/projects/{project}/uploads");
        let r = client::post(&d.target, &path, &headers, RequestBody::bytes("x"), TIMEOUT)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(r.status, 400, "{project} {headers:?}: {json}");
        assert_eq!(json["code"], code, "{project} {headers:?}: {json}");
        assert!(json["operation_id"].is_null(), "{json}");
    }
    assert!(d.running.store().list_projects().await.unwrap().is_empty());
    d.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_uploads_leave_only_a_failed_operation() {
    let d = daemon(UnpackLimits {
        max_bytes: 64 * 1024,
        ..UnpackLimits::default()
    })
    .await;
    let src = tempfile::tempdir().unwrap();
    let (stream, sha) = packed_tree(src.path(), "content");

    // The declared hash is not the stream's.
    let wrong = "0".repeat(64);
    let a = upload_bytes(&d, "my-app", "k1", &wrong, &stream).await;
    assert_failed(&d, &a, "my-app", 422, "digest_mismatch").await;
    assert!(!snapshot_dir(&d.state, &sha).exists());

    // A hostile entry.
    let tar = {
        let mut h = [0u8; 512];
        h[..4].copy_from_slice(b"link");
        h[100..108].copy_from_slice(b"0000777\0");
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(b"00000000000\0");
        h[136..148].copy_from_slice(b"00000000000\0");
        h[156] = b'2';
        h[157..161].copy_from_slice(b"/etc");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut t = h.to_vec();
        t.extend_from_slice(&[0u8; 1024]);
        t
    };
    let hostile = ship_source::fuzzing::compress(&tar).unwrap();
    // Refused while streaming, before the hash is compared.
    let a = upload_bytes(&d, "my-app", "k2", &"a".repeat(64), &hostile).await;
    assert_failed(&d, &a, "my-app", 422, "unsafe_snapshot").await;

    // Over the byte limit.
    let big = tempfile::tempdir().unwrap();
    fs::write(big.path().join("big.bin"), vec![1u8; 200 * 1024]).unwrap();
    let selection = scan(
        big.path(),
        &Options {
            home: Some(PathBuf::from("/nonexistent/ship-test-home")),
            ..Options::default()
        },
    )
    .unwrap();
    let mut big_stream = Vec::new();
    let big_sha = pack(&selection, &mut big_stream).unwrap().sha256;
    let a = upload_bytes(&d, "my-app", "k3", &big_sha, &big_stream).await;
    assert_failed(&d, &a, "my-app", 413, "payload_too_large").await;

    // Truncated: the stream ends early.
    let a = upload_bytes(&d, "my-app", "k4", &sha, &stream[..stream.len() / 2]).await;
    assert_failed(&d, &a, "my-app", 400, "invalid_request").await;

    d.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_transfer_leaves_nothing_behind() {
    let d = daemon(UnpackLimits::default()).await;
    let src = tempfile::tempdir().unwrap();
    let (stream, sha) = packed_tree(src.path(), "broken");
    let (tx, body) = RequestBody::channel(2);
    let half = Bytes::from(stream[..stream.len() / 2].to_vec());
    let feeder = tokio::spawn(async move {
        tx.send(Ok(half)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = tx
            .send(Err(std::io::Error::other("the client went away")))
            .await;
    });
    let headers = [
        ("idempotency-key", "k1"),
        ("ship-snapshot-sha256", sha.as_str()),
    ];
    let path = "/api/v1/projects/my-app/uploads";
    // The client aborts its own request; whatever it reports, the daemon
    // must clean up.
    let _ = client::post(&d.target, path, &headers, body, TIMEOUT).await;
    feeder.await.unwrap();

    // The replay of the key reports the stored failure once it is stored.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let failed = loop {
        let a = upload_bytes(&d, "my-app", "k1", &sha, b"").await;
        if a.status != 409 {
            break a;
        }
        assert!(tokio::time::Instant::now() < deadline, "still running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_failed(&d, &failed, "my-app", 400, "invalid_request").await;
    d.running.shutdown().await.unwrap();
}

/// A compressed archive of one symlink entry named `name`, then the end of
/// the archive.
fn symlink_archive(name: &str) -> Vec<u8> {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000777\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(b"00000000000\0");
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = b'2';
    h[157..161].copy_from_slice(b"/etc");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut t = h.to_vec();
    t.extend_from_slice(&[0u8; 1024]);
    ship_source::fuzzing::compress(&t).unwrap()
}

/// A failure naming a registered secret answers redacted, first time and
/// on replay alike (f-026).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_answers_redacted() {
    const SECRET: &str = "hunter2-upload-secret";
    let redactor = ship_daemon::Redactor::new();
    redactor.register(&ship_core::SecretValue::new(SECRET));
    let d = daemon_with(UnpackLimits::default(), redactor).await;
    let hostile = symlink_archive(SECRET);
    let sha = "a".repeat(64);

    let first = upload_bytes(&d, "my-app", "k1", &sha, &hostile).await;
    assert_failed(&d, &first, "my-app", 422, "unsafe_snapshot").await;
    let again = upload_bytes(&d, "my-app", "k1", &sha, b"").await;
    assert_eq!(again.status, 422, "{}", again.json);
    for a in [&first, &again] {
        let text = a.json.to_string();
        assert!(!text.contains(SECRET), "{text}");
        assert!(text.contains(ship_core::secret::REDACTED), "{text}");
    }
    assert_eq!(first.json["message"], again.json["message"]);
    d.running.shutdown().await.unwrap();
}

/// A refused entry ends the upload while the client still holds its body
/// open: the answer does not wait for the end of the body (f-027).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_entry_answers_with_the_body_held_open() {
    let d = daemon(UnpackLimits::default()).await;
    let hostile = symlink_archive("link");
    let sha = "a".repeat(64);
    let (tx, body) = RequestBody::channel(2);
    tx.send(Ok(Bytes::from(hostile))).await.unwrap();
    let headers = [
        ("idempotency-key", "k1"),
        ("ship-snapshot-sha256", sha.as_str()),
    ];
    let path = "/api/v1/projects/my-app/uploads";
    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        client::post(&d.target, path, &headers, body, TIMEOUT),
    )
    .await
    .expect("no answer while the body is held open")
    .unwrap();
    let a = Answer {
        status: answered.status,
        json: serde_json::from_slice(&answered.body).unwrap(),
    };
    assert_failed(&d, &a, "my-app", 422, "unsafe_snapshot").await;
    // The sender was open until here.
    drop(tx);
    d.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leftover_staging_is_removed_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::config(dir.path());
    let leftover = config.state_dir.join(STAGING_DIR).join("op_left_behind");
    fs::create_dir_all(leftover.join("deep")).unwrap();
    fs::write(leftover.join("deep/file"), b"half").unwrap();
    let running = ship_daemon::start(&config).await.unwrap();
    assert!(!leftover.exists());
    assert!(config.state_dir.join(STAGING_DIR).is_dir());
    running.shutdown().await.unwrap();
}
