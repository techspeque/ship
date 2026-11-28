//! `ship push` on the daemon (phase-1-ws-1.9): one `push` operation that
//! receives a snapshot and deploys it, a new project created by its own
//! `project` operation, replays that start nothing, and the read of the
//! deployment an operation recorded, against a fake `docker` that plays
//! the Engine as in `deploy.rs` and `compose_import.rs`.

mod common;

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use ship_core::{OperationId, OperationPhase};
use ship_daemon::push::project_key;
use ship_daemon::{DeploySettings, Redactor};
use ship_docker::Runner;
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{NewOperation, Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The fake `docker`: as `compose_import.rs`'s, with `compose config`
/// printing the first `--file`, and `image inspect` exposing
/// `$FAKE_DIR/exposed` when it exists.
const SCRIPT: &str = r##"#!/bin/sh
d="$FAKE_DIR"
HEX=4f9a1c0d2e3b4a5968778695a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b
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
services() {
  l=$(sed -n 2p "$d/up_files")
  grep -o '"dev.ship.service": "svc_[a-z0-9]*"' "$l" | sed 's/.*: "\([^"]*\)"/\1/' | sort -u
}
case "$first" in
  ps)
    case "$line" in
      *"label=dev.ship.environment="*)
        if [ -f "$d/up_files" ]; then
          n=0
          for s in $(services); do n=$((n+1)); echo "$HEX$n"; done
        fi;;
    esac
    exit 0;;
  network) exit 0;;
  image)
    ports=$(cat "$d/exposed" 2>/dev/null || echo null)
    echo "[{\"Id\":\"sha256:${HEX}0\",\"Config\":{\"ExposedPorts\":$ports,\"Volumes\":null}}]"
    exit 0;;
  container)
    l=$(sed -n 2p "$d/up_files")
    prj=$(grep -m1 '"dev.ship.project": "prj_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(grep -m1 '"dev.ship.environment": "env_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -o '"dev.ship.release": "rel_[a-z0-9]*"' "$l" | sed 's/.*: "\([^"]*\)"/\1/' | sort -u | tail -1)
    out="["
    n=0
    for s in $(services); do
      n=$((n+1))
      if [ "$n" -gt 1 ]; then out="$out,"; fi
      h=$(cat "$d/health" 2>/dev/null)
      health=""
      if [ -n "$h" ]; then health=",\"Health\":{\"Status\":\"$h\"}"; fi
      out="$out$(printf '{"Id":"%s%s","Name":"/ship-s%s-1","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"s%s"}},"State":{"Status":"running","ExitCode":0,"OOMKilled":false%s}}' "$HEX" "$n" "$n" "$prj" "$env" "$s" "$rel" "$n" "$health")"
    done
    echo "$out]"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config) cat "$f1"; exit 0;;
  build)
    echo "#1 building"
    echo "#2 DONE 0.1s" >&2
    exit "$(cat "$d/build_exit" 2>/dev/null || echo 0)";;
  up)
    printf '%s\n%s\n' "$f1" "$f2" > "$d/up_files"
    echo "Container started"
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

    fn builds(&self) -> usize {
        fs::read_to_string(self.fake.join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("[compose]") && l.contains("[build]"))
            .count()
    }
}

async fn fixture() -> Fixture {
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
    config.tailnet = Arc::new(StaticTailnet(None));
    config.redactor = Redactor::new();
    let running = ship_daemon::start(&config).await.unwrap();
    let target = Target::Local {
        socket: running.socket_path().to_path_buf(),
    };
    Fixture {
        _dir: dir,
        fake,
        target,
        running,
    }
}

/// A packed tree of `files`: its stream and SHA-256.
fn snapshot(files: &[(&str, &str)]) -> (Vec<u8>, String) {
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
    (stream, packed.sha256)
}

const DOCKERFILE: &[(&str, &str)] = &[("Dockerfile", "FROM scratch\n")];

struct Answer {
    status: u16,
    json: Value,
}

async fn push(f: &Fixture, project: &str, query: &str, key: Option<&str>) -> Answer {
    let (stream, sha) = snapshot(DOCKERFILE);
    let mut headers = vec![("ship-snapshot-sha256", sha.as_str())];
    if let Some(key) = key {
        headers.push(("idempotency-key", key));
    }
    let path = format!("/api/v1/projects/{project}/pushes{query}");
    let r = client::post(
        &f.target,
        &path,
        &headers,
        RequestBody::bytes(stream),
        TIMEOUT,
    )
    .await
    .unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_slice(&r.body).unwrap_or(Value::Null),
    }
}

async fn get(f: &Fixture, path: &str) -> Answer {
    let r = client::get(&f.target, path, TIMEOUT).await.unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_slice(&r.body).unwrap_or(Value::Null),
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn op_id(a: &Answer) -> OperationId {
    a.json["operation"]["id"].as_str().unwrap().parse().unwrap()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_creates_its_project_and_deploys_in_one_operation() {
    let f = fixture().await;
    let a = push(&f, "fresh", "", Some("push-1")).await;
    assert_eq!(a.status, 202, "{}", a.json);
    assert_eq!(a.json["operation"]["kind"], "push");
    let id = op_id(&a);
    let op = finished(f.store(), id).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    assert_eq!(
        op.environment_id.unwrap().to_string(),
        a.json["environment_id"]
    );

    // Phases only increase: the deployment's own validation continues
    // the snapshot's.
    let events = f.store().events_after(id, 0).await.unwrap();
    let phases: Vec<OperationPhase> = events
        .iter()
        .filter(|e| e.event_type == "phase_started")
        .map(|e| e.phase)
        .collect();
    assert_eq!(
        phases,
        [
            OperationPhase::Uploading,
            OperationPhase::Validating,
            OperationPhase::Building,
            OperationPhase::Preparing,
            OperationPhase::Deploying,
            OperationPhase::Verifying,
        ]
    );
    let sequences: Vec<i64> = events.iter().map(|e| e.sequence).collect();
    assert_eq!(sequences, (1..=events.len() as i64).collect::<Vec<_>>());

    // The project was created by its own operation, under the derived key.
    let project_op = f
        .store()
        .create_operation(&NewOperation {
            idempotency_key: &project_key("push-1"),
            kind: "project",
            environment_id: None,
        })
        .await
        .unwrap();
    assert!(!project_op.created);
    assert_eq!(project_op.operation.phase, OperationPhase::Succeeded);

    // Replaying the key starts nothing new.
    let again = push(&f, "fresh", "", Some("push-1")).await;
    assert_eq!(again.status, 202, "{}", again.json);
    assert_eq!(op_id(&again), id);
    assert_eq!(again.json["revision_id"], a.json["revision_id"]);
    assert_eq!(f.builds(), 1);
    let env = op.environment_id.unwrap();
    assert_eq!(f.store().list_deployments(env).await.unwrap().len(), 1);

    // The project operation recorded no deployment.
    let none = get(
        &f,
        &format!("/api/v1/operations/{}/deployment", project_op.operation.id),
    )
    .await;
    assert_eq!(none.status, 404, "{}", none.json);
    assert_eq!(none.json["code"], "not_found");
    f.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deployment_read_lists_only_answering_endpoints() {
    let f = fixture().await;
    let port = free_port();
    let a = push(&f, "web", &format!("?port={port}"), Some("p-1")).await;
    assert_eq!(a.status, 202, "{}", a.json);
    let id = op_id(&a);
    let op = finished(f.store(), id).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let path = format!("/api/v1/operations/{id}/deployment");

    // Nothing listens on the route: no endpoint.
    let dead = get(&f, &path).await;
    assert_eq!(dead.status, 200, "{}", dead.json);
    assert_eq!(dead.json["result"], "succeeded");
    assert!(
        dead.json["release_id"]
            .as_str()
            .unwrap()
            .starts_with("rel_")
    );
    let web = &dead.json["services"][0];
    assert_eq!(web["name"], "web");
    assert_eq!(web["status"], "running_unverified");
    assert_eq!(web["endpoints"], serde_json::json!([]));

    // Something answers there now: the endpoint is listed.
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let live = get(&f, &path).await;
    assert_eq!(
        live.json["services"][0]["endpoints"],
        serde_json::json!([{ "provider": "port", "address": format!("127.0.0.1:{port}") }])
    );
    drop(listener);
    f.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_build_lists_no_services() {
    let f = fixture().await;
    f.control("build_exit", "1");
    let a = push(&f, "broken", "", Some("b-1")).await;
    assert_eq!(a.status, 202, "{}", a.json);
    let id = op_id(&a);
    let op = finished(f.store(), id).await;
    assert_eq!(op.failure_code.as_deref(), Some("build_failed"), "{op:?}");
    let d = get(&f, &format!("/api/v1/operations/{id}/deployment")).await;
    assert_eq!(d.status, 200, "{}", d.json);
    assert_eq!(d.json["result"], "failed");
    assert_eq!(d.json["release_id"], Value::Null);
    assert_eq!(d.json["services"], serde_json::json!([]));
    f.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_pushes_are_refused_before_any_operation() {
    let f = fixture().await;
    let no_key = push(&f, "app", "", None).await;
    assert_eq!(no_key.status, 400, "{}", no_key.json);
    for query in ["?port=0", "?colour=red", "?file=..%2Fx", "?confirm=maybe"] {
        let a = push(&f, "app", query, Some("k")).await;
        assert_eq!(a.status, 400, "{query}: {}", a.json);
        assert_eq!(a.json["code"], "invalid_request", "{query}");
    }
    assert!(f.store().list_projects().await.unwrap().is_empty());
    f.running.shutdown().await.unwrap();
}
