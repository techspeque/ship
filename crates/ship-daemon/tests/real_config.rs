//! Config values and secrets against the real Docker Engine
//! (phase-1-ws-1.10 acceptance). Ignored by `make verify`; CI's `docker`
//! job runs them with `--ignored`. The container writes its environment to
//! a file its `httpd` serves, so the test reads exactly what the service
//! sees.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{OperationId, OperationPhase, RevisionId, Slug};
use ship_daemon::DeploySettings;
use ship_docker::{ComposeProject, Runner, compose};
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(300);
const SECRET: &str = "hunter2-real-secret";
/// Everything Compose's env-file format and interpolation could misread.
const TRICKY: &str = r#"a "b" \c $HOME ${X} $$ # d 'e' =f"#;

/// Serves its own environment, written at start, as `/env.txt`.
const SERVER: &str = "FROM busybox:1.36\n\
     EXPOSE 3000\n\
     CMD [\"sh\", \"-c\", \"mkdir -p /www; env > /www/env.txt; exec httpd -f -p 3000 -h /www\"]\n";

struct Daemon {
    _dir: tempfile::TempDir,
    target: Target,
    running: ship_daemon::Running,
}

impl Daemon {
    fn store(&self) -> &Store {
        self.running.store()
    }
}

async fn daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::config(dir.path());
    config.docker = Runner::docker();
    config.deploy = DeploySettings {
        settle: Duration::from_secs(2),
        verify_timeout: Duration::from_secs(60),
        step_timeout: Duration::from_secs(300),
        poll: Duration::from_millis(500),
    };
    config.tailnet = Arc::new(StaticTailnet(None));
    let running = ship_daemon::start(&config).await.unwrap();
    let target = Target::Local {
        socket: running.socket_path().to_path_buf(),
    };
    Daemon {
        _dir: dir,
        target,
        running,
    }
}

async fn post(d: &Daemon, path: &str, key: &str, body: Value) -> Value {
    let headers = [
        ("content-type", "application/json"),
        ("idempotency-key", key),
    ];
    let body = RequestBody::bytes(body.to_string().into_bytes());
    let r = client::post(&d.target, path, &headers, body, TIMEOUT)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(r.status, 202, "{json}");
    json
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

/// Posts a change and asserts its operation succeeded.
async fn set(d: &Daemon, path: &str, key: &str, body: Value) {
    let a = post(d, path, key, body).await;
    let id: OperationId = a["id"].as_str().unwrap().parse().unwrap();
    let op = finished(d.store(), id).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
}

/// Uploads a tree with the server's Dockerfile and `files`.
async fn upload(d: &Daemon, files: &[(&str, String)]) -> RevisionId {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), SERVER).unwrap();
    for (name, text) in files {
        fs::write(root.path().join(name), text).unwrap();
    }
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
        &d.target,
        "/api/v1/projects/app/uploads",
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

async fn deploy(d: &Daemon, key: &str, body: Value) -> Operation {
    let path = "/api/v1/projects/app/deployments";
    let a = post(d, path, key, body).await;
    let id: OperationId = a["id"].as_str().unwrap().parse().unwrap();
    let op = finished(d.store(), id).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    op
}

async fn down(d: &Daemon) {
    let slug = Slug::parse("app").unwrap();
    let ensured = d.store().get_or_create_project(&slug).await.unwrap();
    let project = ComposeProject::new(ensured.environment.id);
    let mut sink = |_: ship_docker::Line| {};
    let _ = compose::down(&Runner::docker(), &project, TIMEOUT, &mut sink).await;
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// The service's environment as it serves it, retried for up to 10 s.
fn served_env(port: u16) -> Vec<String> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(body) = get_once(addr, "/env.txt") {
            return body.lines().map(str::to_owned).collect();
        }
        assert!(std::time::Instant::now() < deadline, "nothing on {addr}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn get_once(addr: SocketAddr, path: &str) -> Option<String> {
    let timeout = Duration::from_secs(2);
    let mut s = TcpStream::connect_timeout(&addr, timeout).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    let (head, body) = out.split_once("\r\n\r\n")?;
    head.contains(" 200 ").then(|| body.to_owned())
}

fn has(env: &[String], line: &str) -> bool {
    env.iter().any(|l| l == line)
}

/// `docker ARGS`' stdout.
fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "docker {args:?}: {err}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine; CI's docker job runs it"]
async fn a_dockerfile_service_sees_its_values_by_precedence_and_no_image_holds_a_secret() {
    let d = daemon().await;
    let vars = "/api/v1/projects/app/vars";
    let secrets = "/api/v1/projects/app/secrets";
    let revision = upload(&d, &[]).await;
    let slug = Slug::parse("app").unwrap();
    d.store().get_or_create_project(&slug).await.unwrap();

    let v = |n: &str, val: &str| json!({ "name": n, "value": val });
    set(&d, vars, "v1", v("LEVEL", "project")).await;
    set(&d, vars, "v2", v("TRICKY", TRICKY)).await;
    let mut e = v("LEVEL", "environment");
    e["env"] = json!("development");
    set(&d, vars, "v3", e).await;
    set(&d, secrets, "s1", v("TOKEN", SECRET)).await;

    let port = free_port();
    let body = json!({ "revision_id": revision.to_string(), "port": port.to_string() });
    deploy(&d, "d1", body.clone()).await;
    let env = served_env(port);
    assert!(has(&env, "LEVEL=environment"), "{env:?}");
    assert!(has(&env, &format!("TRICKY={TRICKY}")), "{env:?}");
    assert!(has(&env, &format!("TOKEN={SECRET}")), "{env:?}");

    // A service value wins once redeployed; until then nothing changes.
    let mut s = v("LEVEL", "service");
    s["service"] = json!("web");
    set(&d, vars, "v4", s).await;
    assert!(has(&served_env(port), "LEVEL=environment"));
    let op = deploy(&d, "d2", body).await;
    let env = served_env(port);
    assert!(has(&env, "LEVEL=service"), "{env:?}");

    // Neither the image's history nor its configuration holds the secret.
    let dep = d.store().deployment_for_operation(op.id).await.unwrap();
    let release = dep.unwrap().release_id.unwrap();
    let stored = d.store().get_release(release).await.unwrap();
    let image = stored.images[0].clone();
    let history = docker(&["history", "--no-trunc", &image]);
    assert!(!history.contains("hunter2"), "{history}");
    let inspect = docker(&["image", "inspect", &image]);
    assert!(!inspect.contains("hunter2"), "{inspect}");
    down(&d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine; CI's docker job runs it"]
async fn a_compose_service_gets_values_and_keeps_what_its_file_declares() {
    let d = daemon().await;
    let vars = "/api/v1/projects/app/vars";
    let port = free_port();
    let file = format!(
        "services:\n  web:\n    build: .\n    ports:\n      - \"127.0.0.1:{port}:3000\"\n    \
         environment:\n      FROM_FILE: file\n"
    );
    let revision = upload(&d, &[("compose.yaml", file)]).await;
    let v = |n: &str, val: &str| json!({ "name": n, "value": val });
    set(&d, vars, "v1", v("FROM_FILE", "ship")).await;
    set(&d, vars, "v2", v("FROM_SHIP", "ship")).await;
    let body = json!({ "revision_id": revision.to_string(), "confirm": true });
    deploy(&d, "d1", body).await;
    let env = served_env(port);
    assert!(has(&env, "FROM_SHIP=ship"), "{env:?}");
    // Compose's own rule: the file's environment: entry wins.
    assert!(has(&env, "FROM_FILE=file"), "{env:?}");
    down(&d).await;
}
