//! Dockerfile deployments (phase-1-ws-1.7) through the daemon's API,
//! against a fake `docker` that records every invocation and plays the
//! Engine's part from control files: the generated Compose file, the
//! published address, the route, each failure with its phase, and the
//! previous release brought back. The same scenarios against the real
//! Engine are in `real_deploy.rs`.

mod common;

use std::fs;
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{
    DeploymentResult, OperationId, OperationPhase, ReleaseId, RevisionId, RouteStatus, SecretValue,
    Slug,
};
use ship_daemon::deploy::{COMPOSE_FILE, LABELS_FILE, release_dir};
use ship_daemon::upload::snapshot_dir;
use ship_daemon::{DeploySettings, Redactor};
use ship_docker::Runner;
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Deployment, Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(30);
const SECRET: &str = "hunter2-deploy-secret";

/// A fake `docker`. Every invocation is appended to `$FAKE_DIR/log` as
/// one line of `[arg]`s. It answers:
/// - `compose … config`: the first `--file`, as Compose would print it;
/// - `compose … build`: two lines, exiting with `$FAKE_DIR/build_exit`;
/// - `image inspect`: an image exposing `$FAKE_DIR/exposed` and declaring
///   `$FAKE_DIR/volumes` (JSON objects; `null` when absent);
/// - `compose … up`: fails once with Docker's port-allocation error when
///   `$FAKE_DIR/up_fail` exists (and removes it); otherwise records its
///   two files in `$FAKE_DIR/up_files`;
/// - `ps` by Ship's environment label: one container once something is
///   up; `container inspect`: that container, labelled from the files last
///   brought up (the release from the image tag), running, with the health
///   status in `$FAKE_DIR/health` if any;
/// - anything else: success with no output (an empty project).
const SCRIPT: &str = r##"#!/bin/sh
d="$FAKE_DIR"
ID=4f9a1c0d2e3b4a5968778695a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
first="$1"
cmd=""
f1=""
f2=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--file" ]; then
    if [ -z "$f1" ]; then f1="$a"; else f2="$a"; fi
  fi
  if [ -z "$cmd" ]; then
    case "$a" in config|build|up|stop|start|restart|down) cmd="$a";; esac
  fi
  prev="$a"
done
case "$first" in
  ps)
    case "$line" in
      *"label=dev.ship.environment="*) if [ -f "$d/up_files" ]; then echo "$ID"; fi;;
    esac
    exit 0;;
  network) exit 0;;
  image)
    ports=$(cat "$d/exposed" 2>/dev/null || echo null)
    vols=$(cat "$d/volumes" 2>/dev/null || echo null)
    echo "[{\"Id\":\"sha256:$ID\",\"Config\":{\"ExposedPorts\":$ports,\"Volumes\":$vols}}]"
    exit 0;;
  container)
    c=$(sed -n 1p "$d/up_files")
    l=$(sed -n 2p "$d/up_files")
    prj=$(grep -m1 '"dev.ship.project": "prj_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(grep -m1 '"dev.ship.environment": "env_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    svc=$(grep -m1 '"dev.ship.service": "svc_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -m1 '"image"' "$c" | sed 's/.*:\(rel_[a-z0-9]*\)".*/\1/')
    health=""
    h=$(cat "$d/health" 2>/dev/null)
    if [ -n "$h" ]; then health=",\"Health\":{\"Status\":\"$h\"}"; fi
    printf '[{"Id":"%s","Name":"/ship-web-1","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"web"}},"State":{"Status":"running","ExitCode":0,"OOMKilled":false%s}}]\n' "$ID" "$prj" "$env" "$svc" "$rel" "$health"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config) cat "$f1"; exit 0;;
  build)
    echo "#1 building web"
    echo "#2 DONE 0.1s" >&2
    exit "$(cat "$d/build_exit" 2>/dev/null || echo 0)";;
  up)
    if [ -f "$d/up_fail" ]; then
      rm -f "$d/up_fail"
      echo "Error response from daemon: Bind for 127.0.0.1:8080 failed: port is already allocated" >&2
      exit 1
    fi
    printf '%s\n%s\n' "$f1" "$f2" > "$d/up_files"
    echo "Container ship-web-1 Started"
    exit 0;;
esac
exit 0
"##;

fn fake_docker() -> &'static Path {
    static PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PATH
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("docker");
            fs::write(&path, SCRIPT).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: PathBuf,
    /// The fake's control files and log.
    fake: PathBuf,
    target: Target,
    running: ship_daemon::Running,
}

impl Fixture {
    fn store(&self) -> &Store {
        self.running.store()
    }

    fn control(&self, name: &str, content: &str) {
        fs::write(self.fake.join(name), content).unwrap();
    }

    fn clear(&self, name: &str) {
        let _ = fs::remove_file(self.fake.join(name));
    }

    /// The fake's invocations, oldest first.
    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.fake.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

async fn fixture(tailnet: Option<Ipv4Addr>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake");
    fs::create_dir(&fake).unwrap();
    let mut config = common::config(dir.path());
    config.docker = Runner::new(fake_docker()).env("FAKE_DIR", &fake);
    config.deploy = DeploySettings {
        settle: Duration::from_millis(200),
        verify_timeout: Duration::from_secs(5),
        step_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(50),
    };
    config.tailnet = Arc::new(StaticTailnet(tailnet));
    let redactor = Redactor::new();
    redactor.register(&SecretValue::new(SECRET));
    config.redactor = redactor;
    let state = config.state_dir.clone();
    let running = ship_daemon::start(&config).await.unwrap();
    let target = Target::Local {
        socket: running.socket_path().to_path_buf(),
    };
    Fixture {
        _dir: dir,
        state,
        fake,
        target,
        running,
    }
}

struct Answer {
    status: u16,
    json: Value,
}

async fn post(f: &Fixture, path: &str, key: Option<&str>, body: Vec<u8>) -> Answer {
    let mut headers = vec![("content-type", "application/json")];
    if let Some(key) = key {
        headers.push(("idempotency-key", key));
    }
    let r = client::post(&f.target, path, &headers, RequestBody::bytes(body), TIMEOUT)
        .await
        .unwrap();
    let json = serde_json::from_slice(&r.body)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)));
    Answer {
        status: r.status,
        json,
    }
}

async fn get(f: &Fixture, path: &str) -> Answer {
    let r = client::get(&f.target, path, TIMEOUT).await.unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_slice(&r.body).unwrap(),
    }
}

/// Uploads a tree with a Dockerfile (and `extra` files) to `project`.
async fn upload(f: &Fixture, project: &str, extra: &[(&str, &str)]) -> RevisionId {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
    for (name, content) in extra {
        fs::write(root.path().join(name), content).unwrap();
    }
    let options = Options {
        home: Some(PathBuf::from("/nonexistent/ship-test-home")),
        ..Options::default()
    };
    let selection = scan(root.path(), &options).unwrap();
    let mut stream = Vec::new();
    let packed = pack(&selection, &mut stream).unwrap();
    let key = format!("upload-{project}-{}", packed.sha256);
    let headers = [
        ("idempotency-key", key.as_str()),
        ("ship-snapshot-sha256", packed.sha256.as_str()),
    ];
    let path = format!("/api/v1/projects/{project}/uploads");
    let r = client::post(
        &f.target,
        &path,
        &headers,
        RequestBody::bytes(stream),
        TIMEOUT,
    )
    .await
    .unwrap();
    let json: Value = serde_json::from_slice(&r.body).unwrap();
    assert!(r.status == 201 || r.status == 200, "{json}");
    json["revision_id"].as_str().unwrap().parse().unwrap()
}

/// Submits a deployment and waits for its operation to end.
async fn deploy(f: &Fixture, project: &str, key: &str, body: Value) -> Operation {
    let path = format!("/api/v1/projects/{project}/deployments");
    let a = post(f, &path, Some(key), body.to_string().into_bytes()).await;
    assert_eq!(a.status, 202, "{}", a.json);
    assert_eq!(a.json["kind"], "deploy");
    let id: OperationId = a.json["id"].as_str().unwrap().parse().unwrap();
    finished(f.store(), id).await
}

async fn finished(store: &Store, id: OperationId) -> Operation {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let op = store.get_operation(id).await.unwrap();
        if op.phase.is_terminal() {
            return op;
        }
        assert!(tokio::time::Instant::now() < deadline, "{op:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The operation's event messages.
async fn messages(store: &Store, op: OperationId) -> Vec<String> {
    store
        .events_after(op, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.message)
        .collect()
}

/// The phases the operation entered, in order.
async fn phases(store: &Store, op: OperationId) -> Vec<OperationPhase> {
    store
        .events_after(op, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == "phase_started")
        .map(|e| e.phase)
        .collect()
}

async fn environment(f: &Fixture, project: &str) -> ship_store::Environment {
    f.store()
        .get_or_create_project(&Slug::parse(project).unwrap())
        .await
        .unwrap()
        .environment
}

/// The deployment of `op`.
async fn deployment_of(f: &Fixture, project: &str, op: OperationId) -> Deployment {
    let env = environment(f, project).await;
    f.store()
        .list_deployments(env.id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.operation_id == op)
        .expect("the operation has a deployment")
}

fn generated(f: &Fixture, release: ReleaseId) -> Value {
    let text = fs::read_to_string(release_dir(&f.state, release).join(COMPOSE_FILE)).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// A port nothing listens on now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn assert_failed(op: &Operation, code: &str, phase: OperationPhase) {
    assert_eq!(op.phase, OperationPhase::Failed, "{op:?}");
    assert_eq!(op.failure_code.as_deref(), Some(code), "{op:?}");
    assert_eq!(op.failure_phase, Some(phase), "{op:?}");
}

fn ups(invocations: &[String]) -> Vec<&String> {
    invocations.iter().filter(|l| l.contains("[up]")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dockerfile_deploys_and_records_its_route() {
    let f = fixture(None).await;
    f.control("exposed", r#"{"3000/tcp":{}}"#);
    let revision = upload(&f, "app", &[("secret.txt", SECRET)]).await;
    let port = free_port();
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");

    assert_eq!(
        phases(f.store(), op.id).await,
        [
            OperationPhase::Validating,
            OperationPhase::Building,
            OperationPhase::Preparing,
            OperationPhase::Deploying,
            OperationPhase::Verifying,
        ]
    );
    let d = deployment_of(&f, "app", op.id).await;
    assert_eq!(d.result, Some(DeploymentResult::Succeeded));
    assert_eq!(d.previous_release_id, None);
    let release = d.release_id.unwrap();
    let r = f.store().get_release(release).await.unwrap();
    assert_eq!(r.revision_id, revision);
    assert_eq!(r.images.len(), 1);
    assert!(r.images[0].starts_with("sha256:"), "{:?}", r.images);
    assert!(r.config_fingerprint.starts_with("sha256:"));

    // The generated file: outside the snapshot, building from it, on
    // 127.0.0.1 (no tailnet), host port to the one EXPOSE port.
    let env = environment(&f, "app").await;
    let rev = f.store().get_revision(revision).await.unwrap();
    let snapshot = snapshot_dir(&f.state, &rev.source_ref);
    let dir = release_dir(&f.state, release);
    assert!(!dir.starts_with(&snapshot));
    let doc = generated(&f, release);
    let web = &doc["services"]["web"];
    assert_eq!(web["build"]["context"], snapshot.to_str().unwrap());
    assert_eq!(web["build"]["dockerfile"], "Dockerfile");
    assert_eq!(web["image"], format!("ship_{}-web:{release}", env.id));
    assert_eq!(web["restart"], "unless-stopped");
    assert_eq!(
        web["ports"],
        json!([{
            "target": 3000, "published": port.to_string(), "host_ip": "127.0.0.1", "protocol": "tcp",
        }])
    );
    for name in [COMPOSE_FILE, LABELS_FILE] {
        let text = fs::read_to_string(dir.join(name)).unwrap();
        assert!(!text.contains(SECRET), "{name}: {text}");
    }
    let labels = fs::read_to_string(dir.join(LABELS_FILE)).unwrap();
    assert!(labels.contains(&release.to_string()), "{labels}");

    // The route is the address bound, and the log says so.
    let routes = f.store().list_routes(env.id).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].address, format!("127.0.0.1:{port}"));
    assert_eq!(routes[0].status, RouteStatus::Confirmed);
    let said = messages(f.store(), op.id).await;
    assert!(
        said.contains(&format!("Endpoint: 127.0.0.1:{port}")),
        "{said:?}"
    );
    assert!(said.iter().any(|m| m == "#1 building web"), "{said:?}");
    assert!(
        said.iter().any(|m| m.contains("running_unverified")),
        "{said:?}"
    );

    // Built with the labels, brought up with both files.
    let calls = f.invocations();
    let build = calls.iter().find(|l| l.contains("[build]")).unwrap();
    assert!(build.contains(&format!("[--file][{}]", dir.join(LABELS_FILE).display())));
    let up = ups(&calls);
    assert_eq!(up.len(), 1);
    assert!(up[0].contains("[--remove-orphans]"), "{}", up[0]);
    assert!(up[0].contains(&dir.join(COMPOSE_FILE).display().to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthy_service_is_healthy() {
    let f = fixture(None).await;
    f.control("health", "healthy");
    let revision = upload(&f, "app", &[]).await;
    let body = json!({ "revision_id": revision.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let said = messages(f.store(), op.id).await;
    assert!(said.iter().any(|m| m == "web is healthy"), "{said:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_published_port_records_no_route() {
    let f = fixture(None).await;
    f.control("exposed", r#"{"3000/tcp":{}}"#);
    let revision = upload(&f, "app", &[]).await;
    let body = json!({ "revision_id": revision.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let env = environment(&f, "app").await;
    assert_eq!(f.store().list_routes(env.id).await.unwrap(), []);
    let d = deployment_of(&f, "app", op.id).await;
    let doc = generated(&f, d.release_id.unwrap());
    assert!(doc["services"]["web"].get("ports").is_none(), "{doc}");
    let said = messages(f.store(), op.id).await;
    assert!(
        said.iter()
            .any(|m| m.starts_with("Endpoint: not configured")),
        "{said:?}"
    );
}

/// The host's own non-loopback IPv4 address, if it has one: the address a
/// datagram to TEST-NET-1 would leave from (nothing is sent).
fn host_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_publishes_on_the_tailnet_address() {
    // The faked tailnet address must be one of the host's, since the port
    // is probed on it before anything starts.
    let Some(tailnet) = host_address() else {
        eprintln!("skipped: the host has no non-loopback IPv4 address");
        return;
    };
    let f = fixture(Some(tailnet)).await;
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let d = deployment_of(&f, "app", op.id).await;
    let doc = generated(&f, d.release_id.unwrap());
    let ports = &doc["services"]["web"]["ports"][0];
    assert_eq!(ports["host_ip"], tailnet.to_string());
    // No EXPOSE: N to N.
    assert_eq!(ports["target"], port);
    let env = environment(&f, "app").await;
    let routes = f.store().list_routes(env.id).await.unwrap();
    assert_eq!(routes[0].address, format!("{tailnet}:{port}"));
    let said = messages(f.store(), op.id).await;
    assert!(
        said.iter()
            .any(|m| m.contains("the host's tailnet address")),
        "{said:?}"
    );
}

/// Sets `publish.address` and waits for the operation.
async fn set_publish_address(f: &Fixture, key: &str, value: &str) -> (Answer, Option<Operation>) {
    let body = json!({ "value": value }).to_string().into_bytes();
    let a = post(f, "/api/v1/settings/publish.address", Some(key), body).await;
    if a.status != 202 {
        return (a, None);
    }
    let id: OperationId = a.json["id"].as_str().unwrap().parse().unwrap();
    let op = finished(f.store(), id).await;
    (a, Some(op))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_publish_address_setting() {
    // A tailnet that is not the host's: a fixed address must win over it.
    let f = fixture(Some(Ipv4Addr::new(192, 0, 2, 7))).await;
    let unset = get(&f, "/api/v1/settings/publish.address").await;
    assert_eq!(unset.status, 200);
    assert_eq!(
        unset.json,
        json!({ "key": "publish.address", "value": "auto", "updated_at": null })
    );
    assert_eq!(get(&f, "/api/v1/settings/nope").await.status, 404);

    // Malformed values and unknown keys never become operations.
    for bad in ["", "Auto", "localhost", "224.0.0.1", "127.0.0.1:80"] {
        let (a, _) = set_publish_address(&f, &format!("bad-{bad}"), bad).await;
        assert_eq!(a.status, 400, "{bad:?}: {}", a.json);
        assert_eq!(a.json["code"], "invalid_request");
    }
    let body = json!({ "value": "auto" }).to_string().into_bytes();
    let a = post(&f, "/api/v1/settings/nope", Some("nope"), body.clone()).await;
    assert_eq!(a.status, 404);
    let a = post(&f, "/api/v1/settings/publish.address", None, body).await;
    assert_eq!(a.status, 400);
    let a = post(
        &f,
        "/api/v1/settings/publish.address",
        Some("x"),
        b"{".to_vec(),
    )
    .await;
    assert_eq!(a.status, 400);

    // An address that is not the host's is refused by the operation.
    let (a, op) = set_publish_address(&f, "s1", "192.0.2.1").await;
    assert_eq!(a.json["kind"], "settings");
    let op = op.unwrap();
    assert_failed(&op, "invalid_request", OperationPhase::Validating);
    assert_eq!(
        get(&f, "/api/v1/settings/publish.address").await.json["value"],
        "auto"
    );

    let (_, op) = set_publish_address(&f, "s2", "127.0.0.1").await;
    assert_eq!(op.unwrap().phase, OperationPhase::Succeeded);
    let set = get(&f, "/api/v1/settings/publish.address").await.json;
    assert_eq!(set["value"], "127.0.0.1");
    assert!(set["updated_at"].is_string(), "{set}");
    let audit = f.store().list_audit_events().await.unwrap();
    assert!(
        audit
            .iter()
            .any(|e| e.action == "set publish.address" && e.target == "127.0.0.1"),
        "{audit:?}"
    );

    // The fixed address is what a deployment binds, and records.
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let d = deployment_of(&f, "app", op.id).await;
    let doc = generated(&f, d.release_id.unwrap());
    assert_eq!(doc["services"]["web"]["ports"][0]["host_ip"], "127.0.0.1");
    let env = environment(&f, "app").await;
    assert_eq!(
        f.store().list_routes(env.id).await.unwrap()[0].address,
        format!("127.0.0.1:{port}")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_expose_ports_need_the_explicit_form() {
    let f = fixture(None).await;
    f.control("exposed", r#"{"3000/tcp":{},"8080/tcp":{}}"#);
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_failed(&op, "invalid_request", OperationPhase::Preparing);
    let message = op.failure_message.unwrap();
    assert!(
        message.contains("3000/tcp") && message.contains("8080/tcp"),
        "{message}"
    );
    assert!(ups(&f.invocations()).is_empty());
    let d = deployment_of(&f, "app", op.id).await;
    assert_eq!(d.result, Some(DeploymentResult::Failed));

    let body = json!({ "revision_id": revision.to_string(), "port": format!("{port}:3000") });
    let op = deploy(&f, "app", "d2", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let d = deployment_of(&f, "app", op.id).await;
    let doc = generated(&f, d.release_id.unwrap());
    assert_eq!(doc["services"]["web"]["ports"][0]["target"], 3000);
    assert_eq!(
        doc["services"]["web"]["ports"][0]["published"],
        port.to_string()
    );
}

/// A first deployment that succeeds on `port`; returns its release.
async fn first_release(f: &Fixture, revision: RevisionId, port: u16) -> ReleaseId {
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(f, "app", "first", body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    deployment_of(f, "app", op.id).await.release_id.unwrap()
}

/// Asserts the environment still has `previous` serving: its route is the
/// one recorded, the last `up` used its files, and the failed deployment
/// names it as its previous release.
async fn assert_previous_serves(f: &Fixture, op: &Operation, previous: ReleaseId, port: u16) {
    let d = deployment_of(f, "app", op.id).await;
    assert_eq!(d.result, Some(DeploymentResult::Failed));
    assert_eq!(d.previous_release_id, Some(previous));
    let env = environment(f, "app").await;
    let routes = f.store().list_routes(env.id).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].address, format!("127.0.0.1:{port}"));
    let history = f.store().list_deployments(env.id).await.unwrap();
    let serving = history
        .iter()
        .find(|x| x.result == Some(DeploymentResult::Succeeded))
        .unwrap();
    assert_eq!(serving.release_id, Some(previous));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_build_starts_nothing() {
    let f = fixture(None).await;
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let previous = first_release(&f, revision, port).await;
    f.control("build_exit", "1");
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "second", body).await;
    assert_failed(&op, "build_failed", OperationPhase::Building);
    assert_eq!(
        op.failure_message.as_deref(),
        Some("the image build exited 1")
    );
    // Nothing was brought up for it, and nothing needed bringing back.
    assert_eq!(ups(&f.invocations()).len(), 1);
    assert_previous_serves(&f, &op, previous, port).await;
    let d = deployment_of(&f, "app", op.id).await;
    assert_eq!(d.release_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_conflict_is_found_before_anything_starts() {
    let f = fixture(None).await;
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let previous = first_release(&f, revision, port).await;

    // Another process holds the new port: refused while preparing.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = held.local_addr().unwrap().port();
    let body = json!({ "revision_id": revision.to_string(), "port": taken.to_string() });
    let op = deploy(&f, "app", "second", body).await;
    assert_failed(&op, "port_conflict", OperationPhase::Preparing);
    assert_eq!(ups(&f.invocations()).len(), 1);
    assert_previous_serves(&f, &op, previous, port).await;
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_conflict_at_start_brings_the_previous_release_back() {
    let f = fixture(None).await;
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let previous = first_release(&f, revision, port).await;

    // The same port again (the serving release holds it, so it is not
    // probed), and Docker refuses it.
    f.control("up_fail", "");
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "second", body).await;
    assert_failed(&op, "port_conflict", OperationPhase::Deploying);
    let up = ups(&f.invocations())
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(up.len(), 3, "{up:#?}");
    let previous_file = release_dir(&f.state, previous).join(COMPOSE_FILE);
    assert!(
        up[2].contains(&previous_file.display().to_string()),
        "{}",
        up[2]
    );
    assert_previous_serves(&f, &op, previous, port).await;
    let said = messages(f.store(), op.id).await;
    assert!(
        said.contains(&format!("release {previous} is running again")),
        "{said:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_health_check_brings_the_previous_release_back() {
    let f = fixture(None).await;
    let revision = upload(&f, "app", &[]).await;
    let port = free_port();
    let previous = first_release(&f, revision, port).await;

    f.control("health", "unhealthy");
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    let op = deploy(&f, "app", "second", body).await;
    assert_failed(&op, "health_check_failed", OperationPhase::Verifying);
    let up = ups(&f.invocations())
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(up.len(), 3, "{up:#?}");
    let previous_file = release_dir(&f.state, previous).join(COMPOSE_FILE);
    assert!(up[2].contains(&previous_file.display().to_string()));
    assert_previous_serves(&f, &op, previous, port).await;
    // The failed release is kept on record with its files, as evidence.
    let d = deployment_of(&f, "app", op.id).await;
    let failed = d.release_id.unwrap();
    assert_ne!(failed, previous);
    assert!(release_dir(&f.state, failed).join(COMPOSE_FILE).is_file());
    f.clear("health");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_previous_release_the_failed_one_is_stopped() {
    let f = fixture(None).await;
    f.control("health", "unhealthy");
    let revision = upload(&f, "app", &[]).await;
    let body = json!({ "revision_id": revision.to_string() });
    let op = deploy(&f, "app", "d1", body).await;
    assert_failed(&op, "health_check_failed", OperationPhase::Verifying);
    let calls = f.invocations();
    let up_at = calls.iter().position(|l| l.contains("[up]")).unwrap();
    assert!(
        calls[up_at..].iter().any(|l| l.contains("[stop]")),
        "{calls:#?}"
    );
    // Stopped, never taken down.
    assert!(!calls.iter().any(|l| l.contains("[down]")), "{calls:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volumes_are_named_and_kept() {
    let f = fixture(None).await;
    f.control("volumes", r#"{"/var/lib/data":{}}"#);
    let revision = upload(&f, "app", &[]).await;
    let body = json!({ "revision_id": revision.to_string() });
    let first = deploy(&f, "app", "d1", body.clone()).await;
    assert_eq!(first.phase, OperationPhase::Succeeded, "{first:?}");
    let second = deploy(&f, "app", "d2", body).await;
    assert_eq!(second.phase, OperationPhase::Succeeded, "{second:?}");
    let a = deployment_of(&f, "app", first.id).await.release_id.unwrap();
    let b = deployment_of(&f, "app", second.id)
        .await
        .release_id
        .unwrap();
    // Named from the path and the start of its SHA-256, the same on every
    // release.
    let name = "var-lib-data-94388feac72b";
    for release in [a, b] {
        let doc = generated(&f, release);
        assert_eq!(
            doc["services"]["web"]["volumes"],
            json!([{ "type": "volume", "source": name, "target": "/var/lib/data" }])
        );
        assert_eq!(doc["volumes"], json!({ name: {} }));
        // The volume's labels name the first release on every redeploy, so
        // Compose never sees it as changed.
        let labels: Value = serde_json::from_str(
            &fs::read_to_string(release_dir(&f.state, release).join(LABELS_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            labels["volumes"][name]["labels"]["dev.ship.release"],
            a.to_string()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_requests_are_refused_before_any_operation() {
    let f = fixture(None).await;
    let revision = upload(&f, "app", &[]).await;
    let other = upload(&f, "other", &[("x.txt", "x")]).await;
    let good = json!({ "revision_id": revision.to_string() });
    // Project, idempotency key, body, status, code.
    type Case<'a> = (&'a str, Option<&'a str>, Vec<u8>, u16, &'a str);
    let cases: Vec<Case> = vec![
        (
            "My-App",
            Some("k1"),
            good.to_string().into_bytes(),
            400,
            "invalid_slug",
        ),
        (
            "ghost",
            Some("k2"),
            good.to_string().into_bytes(),
            404,
            "not_found",
        ),
        (
            "app",
            Some("k3"),
            json!({ "revision_id": other.to_string() })
                .to_string()
                .into_bytes(),
            404,
            "not_found",
        ),
        (
            "app",
            Some("k4"),
            json!({ "revision_id": "rev_01aryz6s41tsv4rrffq69g5fav" })
                .to_string()
                .into_bytes(),
            404,
            "not_found",
        ),
        (
            "app",
            Some("k5"),
            json!({ "revision_id": revision.to_string(), "port": "0" })
                .to_string()
                .into_bytes(),
            400,
            "invalid_request",
        ),
        (
            "app",
            Some("k6"),
            json!({ "revision_id": revision.to_string(), "file": "../Dockerfile" })
                .to_string()
                .into_bytes(),
            400,
            "invalid_request",
        ),
        (
            "app",
            Some("k7"),
            b"not json".to_vec(),
            400,
            "invalid_request",
        ),
        (
            "app",
            None,
            good.to_string().into_bytes(),
            400,
            "invalid_request",
        ),
    ];
    for (project, key, body, status, code) in cases {
        let path = format!("/api/v1/projects/{project}/deployments");
        let a = post(&f, &path, key, body).await;
        assert_eq!(a.status, status, "{project} {key:?}: {}", a.json);
        assert_eq!(a.json["code"], code, "{project} {key:?}: {}", a.json);
        assert!(a.json["operation_id"].is_null(), "{}", a.json);
    }
    assert!(f.invocations().is_empty(), "{:#?}", f.invocations());

    // A file the revision does not have fails the operation, validating.
    let body = json!({ "revision_id": revision.to_string(), "file": "Dockerfile.prod" });
    let op = deploy(&f, "app", "k8", body).await;
    assert_failed(&op, "invalid_request", OperationPhase::Validating);
    assert!(f.invocations().is_empty(), "{:#?}", f.invocations());
}
