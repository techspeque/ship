//! Compose deployments against the real Docker Engine (phase-1-ws-1.8
//! acceptance). Ignored by `make verify`; CI's `docker` job runs them with
//! `--ignored`. Each test uses its own free host port and takes its
//! Compose project down at the end (volumes stay, as always). A GPU
//! reservation needs a GPU host; it is covered with the fake `docker` in
//! `compose_import.rs` and by the adoption gate.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{EnvironmentId, OperationId, OperationPhase, RevisionId, Slug};
use ship_daemon::DeploySettings;
use ship_docker::{ComposeProject, Runner, compose};
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(300);

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
        verify_timeout: Duration::from_secs(90),
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

async fn upload(d: &Daemon, files: &[(&str, &str)]) -> RevisionId {
    let root = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
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

async fn deploy(d: &Daemon, key: &str, revision: RevisionId, confirm: bool) -> Operation {
    let headers = [
        ("content-type", "application/json"),
        ("idempotency-key", key),
    ];
    let body = json!({ "revision_id": revision.to_string(), "confirm": confirm });
    let r = client::post(
        &d.target,
        "/api/v1/projects/app/deployments",
        &headers,
        RequestBody::bytes(body.to_string().into_bytes()),
        TIMEOUT,
    )
    .await
    .unwrap();
    let json: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(r.status, 202, "{json}");
    let id: OperationId = json["id"].as_str().unwrap().parse().unwrap();
    finished(d.store(), id).await
}

async fn environment(d: &Daemon) -> EnvironmentId {
    d.store()
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment
        .id
}

async fn down(d: &Daemon) {
    let project = ComposeProject::new(environment(d).await);
    let _ = compose::down(
        &Runner::docker(),
        &project,
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

/// `GET path` from `addr`, retried for up to 10 s while nothing answers.
fn http_get(addr: SocketAddr, path: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let answer = (|| {
            let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
            s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
            write!(s, "GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").ok()?;
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

/// `docker` with `args`: its stdout, trimmed.
fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The ids of the environment's containers.
fn containers_of(env: EnvironmentId) -> String {
    docker(&[
        "ps",
        "--all",
        "--quiet",
        "--filter",
        &format!("label=dev.ship.environment={env}"),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn a_multi_service_file_with_a_volume_and_a_health_check_deploys() {
    let d = daemon().await;
    let port = free_port();
    let compose_file = format!(
        "services:
  web:
    build: .
    ports:
      - \"127.0.0.1:{port}:3000\"
    volumes:
      - ./site:/www
    healthcheck:
      test: [\"CMD\", \"wget\", \"-q\", \"-O-\", \"http://127.0.0.1:3000/index.html\"]
      interval: 1s
      retries: 30
  worker:
    image: busybox:1.36
    command: [\"sh\", \"-c\", \"echo up >> /data/log; exec sleep 3600\"]
    volumes:
      - data:/data
volumes:
  data: {{}}
"
    );
    let rev = upload(
        &d,
        &[
            ("compose.yaml", &compose_file),
            (
                "Dockerfile",
                "FROM busybox:1.36\nCMD [\"httpd\", \"-f\", \"-p\", \"3000\", \"-h\", \"/www\"]\n",
            ),
            ("site/index.html", "compose-marker"),
        ],
    )
    .await;
    // The published port needs confirmation.
    let asked = deploy(&d, "d1", rev, false).await;
    assert_eq!(asked.failure_code.as_deref(), Some("confirmation_required"));
    let env = environment(&d).await;
    assert_eq!(
        containers_of(env),
        "",
        "nothing started before confirmation"
    );

    let op = deploy(&d, "d2", rev, true).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    assert_eq!(
        http_get(addr, "/index.html").as_deref(),
        Some("compose-marker")
    );
    let services = d.store().list_services(env).await.unwrap();
    assert_eq!(services.len(), 2);
    let routes = d.store().list_routes(env).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].address, format!("127.0.0.1:{port}"));

    // The snapshot is mounted read-only.
    let project = ComposeProject::new(env);
    let web = format!("{}-web-1", project.name());
    let rw = docker(&[
        "inspect",
        "--format",
        "{{range .Mounts}}{{if eq .Destination \"/www\"}}{{.RW}}{{end}}{{end}}",
        &web,
    ]);
    assert_eq!(rw, "false");
    // The named volume is the project's and survives a redeploy.
    let again = deploy(&d, "d3", rev, false).await;
    assert_eq!(again.phase, OperationPhase::Succeeded, "{again:?}");
    let worker = format!("{}-worker-1", project.name());
    let log = docker(&["exec", &worker, "cat", "/data/log"]);
    assert!(log.lines().count() >= 1, "{log}");
    down(&d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn a_host_path_bind_deploys_after_confirmation() {
    let d = daemon().await;
    let host = tempfile::tempdir().unwrap();
    fs::write(host.path().join("marker"), "host-marker").unwrap();
    let host_path = fs::canonicalize(host.path()).unwrap();
    let compose_file = format!(
        "services:
  reader:
    image: busybox:1.36
    command: [\"sleep\", \"3600\"]
    volumes:
      - {}:/host:ro
",
        host_path.display()
    );
    let rev = upload(&d, &[("compose.yaml", &compose_file)]).await;
    let asked = deploy(&d, "d1", rev, false).await;
    assert_eq!(asked.failure_code.as_deref(), Some("confirmation_required"));
    assert!(
        asked
            .failure_message
            .as_deref()
            .unwrap()
            .contains(&format!("mounts the host path {}", host_path.display())),
        "{asked:?}"
    );
    let env = environment(&d).await;
    assert_eq!(containers_of(env), "");

    let op = deploy(&d, "d2", rev, true).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let reader = format!("{}-reader-1", ComposeProject::new(env).name());
    assert_eq!(
        docker(&["exec", &reader, "cat", "/host/marker"]),
        "host-marker"
    );
    down(&d).await;
}
