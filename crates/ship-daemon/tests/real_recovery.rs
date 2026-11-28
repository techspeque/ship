//! Recovery and invariants against the real Docker Engine
//! (phase-1-ws-1.14). Ignored by `make verify`; CI's `docker` job runs them
//! with `--ignored`.
//!
//! - The daemon killed in each phase of a deployment (in-process: the
//!   runtime dropped under it), with a `docker` wrapper that holds a
//!   command open so the kill lands in the phase wanted: in `building` and
//!   `preparing` the previous release still answers; in `deploying` and
//!   `verifying` the new one does, the new daemon runs no Docker command
//!   that changes anything, `ship status` names `ship redeploy`, and that
//!   brings the previous revision back.
//! - No test secret in any image's history or config.
//! - A container, network and volume Ship did not create, and Docker's
//!   whole unlabelled inventory, unchanged across a scenario.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{DeploymentResult, OperationId, OperationPhase, OperationStatus, Slug};
use ship_daemon::{Config, DeploySettings};
use ship_docker::{ComposeProject, Runner, compose};
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Deployment, Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(300);
const SECRET: &str = "hunter2-real-recovery-5e12";

/// Serves `/index.html` (the revision's marker) on port 3000.
const SERVER: &str = "FROM busybox:1.36\n\
     COPY index.html /www/index.html\n\
     EXPOSE 3000\n\
     CMD [\"httpd\", \"-f\", \"-p\", \"3000\", \"-h\", \"/www\"]\n";

/// Real `docker`, every invocation first appended to `$FAKE_DIR/log`, one
/// command held open while `$FAKE_DIR/hang_<what>` exists (having touched
/// `$FAKE_DIR/at_<what>`): `build` before a `compose … build`, `image`
/// before an `image inspect`, `up` after a `compose … up` returned.
const WRAPPER: &str = r##"#!/bin/sh
d="$FAKE_DIR"
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
hold() {
  if [ -f "$d/hang_$1" ]; then
    touch "$d/at_$1"
    while [ -f "$d/hang_$1" ]; do sleep 0.05; done
  fi
}
case "$1" in
  image) [ "$2" = inspect ] && hold image;;
  compose)
    case "$line" in
      *"[build]"*) hold build;;
      *"[up]"*) docker "$@"; s=$?; hold up; exit $s;;
    esac;;
esac
exec docker "$@"
"##;

fn wrapper() -> &'static Path {
    static PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PATH
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("docker");
            fs::write(&path, WRAPPER).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

struct Host {
    _dir: tempfile::TempDir,
    fake: PathBuf,
    config: Config,
}

impl Host {
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        fs::create_dir(&fake).unwrap();
        let mut config = common::config(dir.path());
        config.docker = Runner::new(wrapper()).env("FAKE_DIR", &fake);
        config.deploy = DeploySettings {
            settle: Duration::from_secs(2),
            verify_timeout: Duration::from_secs(90),
            step_timeout: Duration::from_secs(300),
            poll: Duration::from_millis(500),
        };
        config.tailnet = Arc::new(StaticTailnet(None));
        Host {
            _dir: dir,
            fake,
            config,
        }
    }

    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.fake.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn target(running: &ship_daemon::Running) -> Target {
    Target::Local {
        socket: running.socket_path().to_path_buf(),
    }
}

async fn post(t: &Target, path: &str, key: &str, body: Value) -> OperationId {
    let headers = [
        ("content-type", "application/json"),
        ("idempotency-key", key),
    ];
    let body = RequestBody::bytes(body.to_string().into_bytes());
    let r = client::post(t, path, &headers, body, TIMEOUT)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(r.status, 202, "{json}");
    json["id"].as_str().unwrap().parse().unwrap()
}

async fn get(t: &Target, path: &str) -> Value {
    let r = client::get(t, path, TIMEOUT).await.unwrap();
    serde_json::from_slice(&r.body).unwrap()
}

async fn finished(store: &Store, id: OperationId) -> Operation {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let op = store.get_operation(id).await.unwrap();
        if op.phase.is_terminal() {
            return op;
        }
        assert!(tokio::time::Instant::now() < deadline, "{op:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn succeeds(t: &Target, store: &Store, path: &str, key: &str, body: Value) {
    let id = post(t, path, key, body).await;
    let op = finished(store, id).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{path}: {op:?}");
}

async fn upload(t: &Target, dockerfile: &str, marker: &str) -> String {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), dockerfile).unwrap();
    fs::write(root.path().join("index.html"), marker).unwrap();
    let options = Options {
        home: Some(PathBuf::from("/nonexistent/ship-test-home")),
        ..Options::default()
    };
    let selection = scan(root.path(), &options).unwrap();
    let mut stream = Vec::new();
    let packed = pack(&selection, &mut stream).unwrap();
    let key = format!("upload-{}", packed.sha256);
    let headers = [
        ("idempotency-key", key.as_str()),
        ("ship-snapshot-sha256", packed.sha256.as_str()),
    ];
    let r = client::post(
        t,
        "/api/v1/projects/app/uploads",
        &headers,
        RequestBody::bytes(stream),
        TIMEOUT,
    )
    .await
    .unwrap();
    let json: Value = serde_json::from_slice(&r.body).unwrap();
    assert!(r.status == 201 || r.status == 200, "{json}");
    json["revision_id"].as_str().unwrap().to_owned()
}

async fn deployments(store: &Store) -> Vec<Deployment> {
    let env = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment;
    store.list_deployments(env.id).await.unwrap()
}

/// Takes the project down at the end of a test; its volumes stay.
async fn down(store: &Store) {
    let env = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment;
    let _ = compose::down(
        &Runner::docker(),
        &ComposeProject::new(env.id),
        TIMEOUT,
        &mut |_: ship_docker::Line| {},
    )
    .await;
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `GET /index.html` on 127.0.0.1:`port`, retried for up to 20 s.
fn served(port: u16) -> Option<String> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let answer = (|| {
            let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
            s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
            write!(s, "GET /index.html HTTP/1.0\r\nHost: x\r\n\r\n").ok()?;
            let mut out = String::new();
            s.read_to_string(&mut out).ok()?;
            out.split_once("\r\n\r\n").map(|(_, b)| b.to_owned())
        })();
        if answer.is_some() || std::time::Instant::now() >= deadline {
            return answer;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `docker ARGS`' stdout, from the real `docker`.
fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "docker {args:?}: {err}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// After a kill and a new start.
struct Restarted {
    host: Host,
    rt: tokio::runtime::Runtime,
    running: ship_daemon::Running,
    port: u16,
    /// v1's revision id.
    v1: String,
    killed: OperationId,
    /// The wrapper's log length when the new daemon started.
    before: usize,
}

/// Deploys v1 with a secret on a fresh port, then v2 (`v2_dockerfile`)
/// with `hang` held if any, kills the daemon once the operation is in
/// `phase` (and the held command reached), and starts a new one.
fn real_kill(phase: OperationPhase, hang: Option<&str>, v2_dockerfile: &str) -> Restarted {
    let host = Host::new();
    let port = free_port();
    let rt = runtime();
    let (running, v1, killed) = rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        let t = target(&running);
        let store = running.store();
        store
            .get_or_create_project(&Slug::parse("app").unwrap())
            .await
            .unwrap();
        let secret = json!({ "name": "TOKEN", "value": SECRET });
        succeeds(&t, store, "/api/v1/projects/app/secrets", "s1", secret).await;
        let v1 = upload(&t, SERVER, "v1").await;
        let body = json!({ "revision_id": v1, "port": port.to_string() });
        succeeds(&t, store, "/api/v1/projects/app/deployments", "d1", body).await;
        assert_eq!(served(port).as_deref(), Some("v1"));

        let v2 = upload(&t, v2_dockerfile, "v2").await;
        if let Some(hang) = hang {
            fs::write(host.fake.join(format!("hang_{hang}")), "").unwrap();
        }
        let body = json!({ "revision_id": v2, "port": port.to_string() });
        let killed = post(&t, "/api/v1/projects/app/deployments", "d2", body).await;
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let op = store.get_operation(killed).await.unwrap();
            let held = hang.is_none_or(|h| host.fake.join(format!("at_{h}")).exists());
            if op.phase == phase && held {
                break;
            }
            assert!(!op.phase.is_terminal(), "{op:?}");
            assert!(tokio::time::Instant::now() < deadline, "{op:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (running, v1, killed)
    });
    rt.shutdown_background();
    std::mem::forget(running);
    let before = host.invocations().len();
    for h in ["build", "image", "up"] {
        let _ = fs::remove_file(host.fake.join(format!("hang_{h}")));
    }
    let rt = runtime();
    let running = rt.block_on(ship_daemon::start(&host.config)).unwrap();
    Restarted {
        host,
        rt,
        running,
        port,
        v1,
        killed,
        before,
    }
}

fn assert_interrupted(rt: &tokio::runtime::Runtime, store: &Store, id: OperationId) {
    rt.block_on(async {
        let op = store.get_operation(id).await.unwrap();
        assert_eq!(op.status, OperationStatus::Interrupted, "{op:?}");
        let d = deployments(store)
            .await
            .into_iter()
            .find(|d| d.operation_id == id)
            .unwrap();
        assert_eq!(d.result, Some(DeploymentResult::Interrupted), "{d:?}");
    });
}

/// Only reads since the restart: `ps` and `container inspect`.
fn assert_only_reads(host: &Host, before: usize) {
    for line in &host.invocations()[before..] {
        assert!(
            line.starts_with("[ps]") || line.starts_with("[container][inspect]"),
            "a Docker command that is not a read after the restart: {line}"
        );
    }
}

fn previous_serves(phase: OperationPhase, hang: &str) {
    let Restarted {
        host,
        rt,
        running,
        port,
        killed,
        before,
        ..
    } = real_kill(phase, Some(hang), SERVER);
    assert_interrupted(&rt, running.store(), killed);
    assert_eq!(served(port).as_deref(), Some("v1"));
    let status = rt.block_on(get(&target(&running), "/api/v1/projects/app/status"));
    assert_eq!(status["interruption"], Value::Null, "{status}");
    assert_only_reads(&host, before);
    rt.block_on(async {
        down(running.store()).await;
        running.shutdown().await.unwrap();
    });
}

fn redeploy_recovers(phase: OperationPhase, hang: Option<&str>, v2: &str, name: &str) {
    let Restarted {
        host,
        rt,
        running,
        port,
        v1,
        killed,
        before,
    } = real_kill(phase, hang, v2);
    assert_interrupted(&rt, running.store(), killed);
    // Docker runs the new release: Ship left it as it was.
    assert_eq!(served(port).as_deref(), Some("v2"));
    let t = target(&running);
    let status = rt.block_on(get(&t, "/api/v1/projects/app/status"));
    assert_eq!(status["interruption"]["phase"], name, "{status}");
    assert_eq!(status["interruption"]["command"], "ship redeploy app");
    assert_eq!(status["last_deployment"]["result"], "succeeded");
    assert_ne!(status["services"][0]["status"], "unknown", "{status}");
    assert_only_reads(&host, before);

    rt.block_on(async {
        succeeds(
            &t,
            running.store(),
            "/api/v1/projects/app/redeploy",
            "r1",
            json!({}),
        )
        .await;
        let latest = deployments(running.store()).await.remove(0);
        assert_eq!(latest.revision_id.to_string(), v1);
    });
    assert_eq!(served(port).as_deref(), Some("v1"));
    let status = rt.block_on(get(&t, "/api/v1/projects/app/status"));
    assert_eq!(status["interruption"], Value::Null, "{status}");
    rt.block_on(async {
        down(running.store()).await;
        running.shutdown().await.unwrap();
    });
}

#[test]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
fn real_killed_while_building() {
    previous_serves(OperationPhase::Building, "build");
}

#[test]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
fn real_killed_while_preparing() {
    previous_serves(OperationPhase::Preparing, "image");
}

#[test]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
fn real_killed_while_deploying() {
    redeploy_recovers(OperationPhase::Deploying, Some("up"), SERVER, "deploying");
}

#[test]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
fn real_killed_while_verifying() {
    // A health check whose first run is 30 s away keeps the deployment in
    // `verifying` long enough to kill it there.
    let slow = format!("{SERVER}HEALTHCHECK --interval=30s --timeout=5s --retries=1 CMD true\n");
    redeploy_recovers(OperationPhase::Verifying, None, &slow, "verifying");
}

/// Docker's containers, networks and volumes with no `dev.ship.project`
/// label, each with what would show a change.
fn unlabelled() -> Vec<String> {
    let label = "{{.Label \"dev.ship.project\"}}";
    let mut out = Vec::new();
    for (kind, args) in [
        (
            "container",
            vec![
                "container".to_owned(),
                "ls".to_owned(),
                "--all".to_owned(),
                "--no-trunc".to_owned(),
                "--format".to_owned(),
                format!(
                    "{label}\t{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{.Image}}}}\t{{{{.State}}}}\t{{{{.CreatedAt}}}}"
                ),
            ],
        ),
        (
            "network",
            vec![
                "network".to_owned(),
                "ls".to_owned(),
                "--no-trunc".to_owned(),
                "--format".to_owned(),
                format!("{label}\t{{{{.ID}}}}\t{{{{.Name}}}}\t{{{{.Driver}}}}"),
            ],
        ),
        (
            "volume",
            vec![
                "volume".to_owned(),
                "ls".to_owned(),
                "--format".to_owned(),
                format!("{label}\t{{{{.Name}}}}\t{{{{.Driver}}}}"),
            ],
        ),
    ] {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        for line in docker(&args).lines() {
            if let Some(rest) = line.strip_prefix('\t') {
                out.push(format!("{kind}\t{rest}"));
            }
        }
    }
    out.sort();
    out
}

#[test]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
fn real_no_secret_in_images_and_unlabelled_objects_untouched() {
    let tag = format!("ship-test-sentinel-{}", std::process::id());
    docker(&["volume", "create", &tag]);
    docker(&["network", "create", &tag]);
    docker(&["pull", "busybox:1.36"]);
    docker(&[
        "container",
        "create",
        "--name",
        &tag,
        "--network",
        &tag,
        "--volume",
        &format!("{tag}:/data"),
        "busybox:1.36",
        "true",
    ]);
    let before = unlabelled();
    for kind in ["container", "network", "volume"] {
        assert!(
            before
                .iter()
                .any(|l| l.starts_with(kind) && l.contains(&tag)),
            "{kind} {tag} not listed: {before:?}"
        );
    }

    // Deploy, redeploy, restart, stop, start, a kill while deploying and
    // its recovery, and remove.
    // `_host` holds the state directory until the end.
    let Restarted {
        host: _host,
        rt,
        running,
        port,
        ..
    } = real_kill(OperationPhase::Deploying, Some("up"), SERVER);
    let t = target(&running);
    let store = running.store().clone();
    rt.block_on(async {
        for (action, key) in [
            ("redeploy", "a1"),
            ("restart", "a2"),
            ("stop", "a3"),
            ("start", "a4"),
        ] {
            succeeds(
                &t,
                &store,
                &format!("/api/v1/projects/app/{action}"),
                key,
                json!({}),
            )
            .await;
        }
    });
    assert_eq!(served(port).as_deref(), Some("v1"));

    // No image of any release holds the secret, in its history or config.
    let images: Vec<String> = rt.block_on(async {
        let project = store
            .get_or_create_project(&Slug::parse("app").unwrap())
            .await
            .unwrap()
            .project;
        store
            .list_releases(project.id)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|r| r.images)
            .collect()
    });
    assert!(!images.is_empty());
    for image in &images {
        let history = docker(&[
            "image",
            "history",
            "--no-trunc",
            "--format",
            "{{.CreatedBy}}",
            image,
        ]);
        assert!(!history.contains(SECRET), "{image}: {history}");
        let inspect = docker(&["image", "inspect", image]);
        assert!(!inspect.contains(SECRET), "{image}");
    }

    rt.block_on(async {
        succeeds(&t, &store, "/api/v1/projects/app/remove", "a5", json!({})).await;
        running.shutdown().await.unwrap();
    });
    assert_eq!(unlabelled(), before);

    docker(&["container", "rm", &tag]);
    docker(&["network", "rm", &tag]);
    docker(&["volume", "rm", &tag]);
}
