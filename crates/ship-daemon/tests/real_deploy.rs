//! Dockerfile deployments against the real Docker Engine (phase-1-ws-1.7
//! acceptance). Ignored by `make verify`; CI's `docker` job runs them with
//! `--ignored`. Each test uses its own free host ports and takes its
//! Compose project down at the end (volumes stay, as always).

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{DeploymentResult, EnvironmentId, OperationId, OperationPhase, RevisionId, Slug};
use ship_daemon::DeploySettings;
use ship_docker::{ComposeProject, Runner, compose};
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(300);

/// Serves `/index.html` (the revision's marker) and `/starts` (one line
/// per start, kept in the `VOLUME`) on port 3000.
const SERVER: &str = "FROM busybox:1.36\n\
     COPY index.html /www/index.html\n\
     EXPOSE 3000\n\
     VOLUME /data\n\
     CMD [\"sh\", \"-c\", \"echo start >> /data/starts; cp /data/starts /www/starts; exec httpd -f -p 3000 -h /www\"]\n";

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

async fn daemon(tailnet: Option<Ipv4Addr>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::config(dir.path());
    config.docker = Runner::docker();
    config.deploy = DeploySettings {
        settle: Duration::from_secs(2),
        verify_timeout: Duration::from_secs(60),
        step_timeout: Duration::from_secs(300),
        poll: Duration::from_millis(500),
    };
    config.tailnet = Arc::new(StaticTailnet(tailnet));
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

/// Uploads a tree with `dockerfile` and an `index.html` of `marker`.
async fn upload(d: &Daemon, dockerfile: &str, marker: &str) -> RevisionId {
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

async fn deploy(d: &Daemon, key: &str, revision: RevisionId, port: Option<String>) -> Operation {
    let body = json!({ "revision_id": revision.to_string(), "port": port });
    let a = post(d, "/api/v1/projects/app/deployments", key, body).await;
    let id: OperationId = a["id"].as_str().unwrap().parse().unwrap();
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

/// Takes the project down at the end of a test; its volumes stay.
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

/// `GET path` from `addr`, retried for up to 10 s while nothing answers
/// (a container just recreated may not listen yet).
fn http_get(addr: SocketAddr, path: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let answer = get_once(addr, path);
        if answer.is_some() || std::time::Instant::now() >= deadline {
            return answer;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Whether nothing answers on `addr` now.
fn refused(addr: SocketAddr) -> bool {
    get_once(addr, "/index.html").is_none()
}

/// `GET path` from `addr`: the body, or `None` when nothing answers.
fn get_once(addr: SocketAddr, path: &str) -> Option<String> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    let (_, body) = out.split_once("\r\n\r\n")?;
    Some(body.to_owned())
}

fn host_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn assert_failed(op: &Operation, code: &str, phase: OperationPhase) {
    assert_eq!(op.phase, OperationPhase::Failed, "{op:?}");
    assert_eq!(op.failure_code.as_deref(), Some(code), "{op:?}");
    assert_eq!(op.failure_phase, Some(phase), "{op:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
async fn real_dockerfile_deployments_and_their_failures() {
    let d = daemon(None).await;
    let port = free_port();
    let at = loopback(port);

    // Deployed and reachable on 127.0.0.1 only; host N maps to EXPOSE 3000.
    let v1 = upload(&d, SERVER, "v1").await;
    let op = deploy(&d, "v1", v1, Some(port.to_string())).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    assert_eq!(http_get(at, "/index.html").as_deref(), Some("v1"));
    if let Some(lan) = host_address() {
        let elsewhere = SocketAddr::new(IpAddr::V4(lan), port);
        assert!(refused(elsewhere), "{elsewhere}");
    }
    let env = environment(&d).await;
    let routes = d.store().list_routes(env).await.unwrap();
    assert_eq!(routes[0].address, at.to_string());
    let said: Vec<String> = d
        .store()
        .events_after(op.id, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.message)
        .collect();
    assert!(said.contains(&format!("Endpoint: {at}")), "{said:?}");

    // A redeploy replaces it; the named volume keeps its data.
    let v2 = upload(&d, SERVER, "v2").await;
    let op = deploy(&d, "v2", v2, Some(port.to_string())).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    assert_eq!(http_get(at, "/index.html").as_deref(), Some("v2"));
    assert_eq!(http_get(at, "/starts").as_deref(), Some("start\nstart\n"));

    // A failing build: v2 still serves, untouched.
    let broken = upload(&d, "FROM busybox:1.36\nRUN false\n", "v3").await;
    let op = deploy(&d, "v3", broken, Some(port.to_string())).await;
    assert_failed(&op, "build_failed", OperationPhase::Building);
    assert_eq!(http_get(at, "/index.html").as_deref(), Some("v2"));

    // A failing health check: v2 is brought back.
    let unhealthy =
        format!("{SERVER}HEALTHCHECK --interval=1s --timeout=1s --retries=1 CMD false\n");
    let v4 = upload(&d, &unhealthy, "v4").await;
    let op = deploy(&d, "v4", v4, Some(port.to_string())).await;
    assert_failed(&op, "health_check_failed", OperationPhase::Verifying);
    assert_eq!(http_get(at, "/index.html").as_deref(), Some("v2"));

    // A port another process holds: refused before anything starts.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = held.local_addr().unwrap().port();
    let op = deploy(&d, "v5", v2, Some(taken.to_string())).await;
    assert_failed(&op, "port_conflict", OperationPhase::Preparing);
    assert_eq!(http_get(at, "/index.html").as_deref(), Some("v2"));
    drop(held);

    // Explicit HOST:CONTAINER, and no port at all: no route.
    let other = free_port();
    let op = deploy(&d, "v6", v2, Some(format!("{other}:3000"))).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    assert_eq!(
        http_get(loopback(other), "/index.html").as_deref(),
        Some("v2")
    );
    let op = deploy(&d, "v7", v2, None).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    assert_eq!(d.store().list_routes(env).await.unwrap(), []);

    let history = d.store().list_deployments(env).await.unwrap();
    let failed = history
        .iter()
        .filter(|x| x.result == Some(DeploymentResult::Failed))
        .count();
    assert_eq!(failed, 3);
    down(&d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine on Linux; run by CI's docker job"]
async fn real_tailnet_address_is_the_only_one_bound() {
    // 127.0.0.2 stands for the tailnet address: on Linux every 127/8
    // address is the host's, so it is bindable and distinct.
    let tailnet = Ipv4Addr::new(127, 0, 0, 2);
    let d = daemon(Some(tailnet)).await;
    let port = free_port();
    let v1 = upload(&d, SERVER, "tailnet").await;
    let op = deploy(&d, "t1", v1, Some(port.to_string())).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let on_tailnet = SocketAddr::new(IpAddr::V4(tailnet), port);
    assert_eq!(
        http_get(on_tailnet, "/index.html").as_deref(),
        Some("tailnet")
    );
    assert!(refused(loopback(port)));
    let env = environment(&d).await;
    let routes = d.store().list_routes(env).await.unwrap();
    assert_eq!(routes[0].address, on_tailnet.to_string());
    down(&d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Docker Engine; run by CI's docker job"]
async fn real_fixed_publish_address_is_used() {
    let Some(lan) = host_address() else {
        eprintln!("skipped: the host has no non-loopback IPv4 address");
        return;
    };
    let d = daemon(None).await;
    let a = post(
        &d,
        "/api/v1/settings/publish.address",
        "s1",
        json!({ "value": lan.to_string() }),
    )
    .await;
    let id: OperationId = a["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        finished(d.store(), id).await.phase,
        OperationPhase::Succeeded
    );
    let port = free_port();
    let v1 = upload(&d, SERVER, "fixed").await;
    let op = deploy(&d, "f1", v1, Some(port.to_string())).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let on_lan = SocketAddr::new(IpAddr::V4(lan), port);
    assert_eq!(http_get(on_lan, "/index.html").as_deref(), Some("fixed"));
    assert!(refused(loopback(port)));
    down(&d).await;
}
