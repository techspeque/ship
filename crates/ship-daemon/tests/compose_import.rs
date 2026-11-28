//! Compose deployments under the trust profile (phase-1-ws-1.8) through
//! the daemon's API, against a fake `docker` that records every
//! invocation. `compose config` prints the canonical model each test
//! writes to `$FAKE_DIR/config`, standing for what Compose would print
//! after interpolation; everything else plays the Engine's part as in
//! `deploy.rs`, with one container per Compose service. The multi-service
//! and host-path cases against the real Engine are in
//! `real_compose_import.rs`.

mod common;

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{
    DeploymentResult, OperationId, OperationPhase, ReleaseId, RevisionId, SecretValue, ServiceKind,
    Slug,
};
use ship_daemon::compose_import::{ACCEPT_ACTION, CONFIRM_ACTION, POLICY_ACTOR};
use ship_daemon::deploy::{FILES_FILE, LABELS_FILE, release_dir};
use ship_daemon::upload::snapshot_dir;
use ship_daemon::{DeploySettings, Redactor};
use ship_docker::Runner;
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{AuditEvent, Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(30);
const SECRET: &str = "hunter2-compose-secret";
/// A value only interpolation puts into the model: it is in no pushed file.
const CANARY: &str = "canary-from-interpolation-7f3e";

/// The fake `docker`. Invocations go to `$FAKE_DIR/log`, one line of
/// `[arg]`s each. It answers:
/// - `compose … config`: `$FAKE_DIR/config`;
/// - `compose … build`: two lines, exiting with `$FAKE_DIR/build_exit`;
/// - `image inspect`: an image with an id;
/// - `compose … up`: fails with Docker's port-allocation error once when
///   `$FAKE_DIR/up_fail` exists; else records its two `--file`s in
///   `$FAKE_DIR/up_files`;
/// - `ps` by Ship's environment label: one container per Ship service id
///   in the override last brought up, or `$FAKE_DIR/replicas` of them
///   when that file exists; `container inspect`: those
///   containers, labelled with that override's newest release, running,
///   with the health status in `$FAKE_DIR/health` if any; when
///   `$FAKE_DIR/health_seq` exists, each inspect instead takes its first
///   line, one status per container in order, and drops it while another
///   line follows;
/// - anything else: success with no output.
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
replicas() {
  cat "$d/replicas" 2>/dev/null || echo 1
}
case "$first" in
  ps)
    case "$line" in
      *"label=dev.ship.environment="*)
        if [ -f "$d/up_files" ]; then
          n=0
          for s in $(services); do
            for j in $(seq 1 "$(replicas)"); do n=$((n+1)); echo "$HEX$n"; done
          done
        fi;;
    esac
    exit 0;;
  network) exit 0;;
  image)
    echo "[{\"Id\":\"sha256:${HEX}0\",\"Config\":{\"ExposedPorts\":null,\"Volumes\":null}}]"
    exit 0;;
  container)
    l=$(sed -n 2p "$d/up_files")
    prj=$(grep -m1 '"dev.ship.project": "prj_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(grep -m1 '"dev.ship.environment": "env_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -o '"dev.ship.release": "rel_[a-z0-9]*"' "$l" | sed 's/.*: "\([^"]*\)"/\1/' | sort -u | tail -1)
    seq=""
    if [ -f "$d/health_seq" ]; then
      seq=$(sed -n 1p "$d/health_seq")
      if [ "$(wc -l < "$d/health_seq")" -gt 1 ]; then
        sed 1d "$d/health_seq" > "$d/health_seq.next"
        mv "$d/health_seq.next" "$d/health_seq"
      fi
    fi
    out="["
    n=0
    k=0
    for s in $(services); do
      k=$((k+1))
      for j in $(seq 1 "$(replicas)"); do
        n=$((n+1))
        if [ "$n" -gt 1 ]; then out="$out,"; fi
        if [ -n "$seq" ]; then
          h=$(echo "$seq" | cut -d' ' -f"$n")
        else
          h=$(cat "$d/health" 2>/dev/null)
        fi
        health=""
        if [ -n "$h" ]; then health=",\"Health\":{\"Status\":\"$h\"}"; fi
        out="$out$(printf '{"Id":"%s%s","Name":"/ship-s%s-%s","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"s%s"}},"State":{"Status":"running","ExitCode":0,"OOMKilled":false%s}}' "$HEX" "$n" "$k" "$j" "$prj" "$env" "$s" "$rel" "$k" "$health")"
      done
    done
    echo "$out]"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config) cat "$d/config"; exit 0;;
  build)
    echo "#1 building"
    echo "#2 DONE 0.1s" >&2
    exit "$(cat "$d/build_exit" 2>/dev/null || echo 0)";;
  up)
    if [ -f "$d/up_fail" ]; then
      rm -f "$d/up_fail"
      echo "Error response from daemon: Bind for 127.0.0.1:8080 failed: port is already allocated" >&2
      exit 1
    fi
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
    state: PathBuf,
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

    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.fake.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn count(&self, word: &str) -> usize {
        let needle = format!("[{word}]");
        self.invocations()
            .iter()
            .filter(|l| l.starts_with("[compose]") && l.contains(&needle))
            .count()
    }

    /// The model `compose config` prints next, with `@SNAP@` standing for
    /// the snapshot of `revision`.
    async fn config(&self, revision: RevisionId, model: &Value) {
        let snap = self.snapshot(revision).await;
        let text = model.to_string().replace("@SNAP@", snap.to_str().unwrap());
        self.control("config", &text);
    }

    async fn snapshot(&self, revision: RevisionId) -> PathBuf {
        let rev = self.store().get_revision(revision).await.unwrap();
        snapshot_dir(&self.state, &rev.source_ref)
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
    body: Vec<u8>,
}

async fn post(f: &Fixture, path: &str, key: &str, body: &Value) -> Answer {
    let headers = [
        ("content-type", "application/json"),
        ("idempotency-key", key),
    ];
    let bytes = body.to_string().into_bytes();
    let r = client::post(
        &f.target,
        path,
        &headers,
        RequestBody::bytes(bytes),
        TIMEOUT,
    )
    .await
    .unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_slice(&r.body).unwrap_or(Value::Null),
        body: r.body.to_vec(),
    }
}

async fn get(f: &Fixture, path: &str) -> Answer {
    let r = client::get(&f.target, path, TIMEOUT).await.unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_slice(&r.body).unwrap_or(Value::Null),
        body: r.body.to_vec(),
    }
}

/// Uploads a tree of `files` to `project`.
async fn upload(f: &Fixture, project: &str, files: &[(&str, &str)]) -> RevisionId {
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

async fn deploy(f: &Fixture, project: &str, key: &str, body: Value) -> Operation {
    let path = format!("/api/v1/projects/{project}/deployments");
    let a = post(f, &path, key, &body).await;
    assert_eq!(a.status, 202, "{}", a.json);
    let id: OperationId = a.json["id"].as_str().unwrap().parse().unwrap();
    finished(f.store(), id).await
}

async fn deploy_revision(
    f: &Fixture,
    project: &str,
    key: &str,
    rev: RevisionId,
    confirm: bool,
) -> Operation {
    let body = json!({ "revision_id": rev.to_string(), "confirm": confirm });
    deploy(f, project, key, body).await
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

async fn messages(store: &Store, op: OperationId) -> Vec<String> {
    store
        .events_after(op, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.message)
        .collect()
}

async fn environment(f: &Fixture, project: &str) -> ship_store::Environment {
    f.store()
        .get_or_create_project(&Slug::parse(project).unwrap())
        .await
        .unwrap()
        .environment
}

async fn project_id(f: &Fixture, project: &str) -> String {
    f.store()
        .get_or_create_project(&Slug::parse(project).unwrap())
        .await
        .unwrap()
        .project
        .id
        .to_string()
}

async fn release_of(f: &Fixture, project: &str, op: OperationId) -> ReleaseId {
    let env = environment(f, project).await;
    f.store()
        .list_deployments(env.id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.operation_id == op)
        .and_then(|d| d.release_id)
        .expect("the operation recorded a release")
}

fn assert_failed(op: &Operation, code: &str, phase: OperationPhase) {
    assert_eq!(op.phase, OperationPhase::Failed, "{op:?}");
    assert_eq!(op.failure_code.as_deref(), Some(code), "{op:?}");
    assert_eq!(op.failure_phase, Some(phase), "{op:?}");
}

async fn audit(f: &Fixture, action: &str) -> Vec<AuditEvent> {
    f.store()
        .list_audit_events()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.action == action)
        .collect()
}

fn detail(e: &AuditEvent) -> Value {
    serde_json::from_str(e.detail.as_deref().unwrap()).unwrap()
}

/// Sets `compose.confirm` on the host (`project` `None`) or a project, and
/// waits for the operation.
async fn set_policy(f: &Fixture, project: Option<&str>, value: &str, key: &str) {
    let path = match project {
        Some(p) => format!("/api/v1/projects/{p}/settings/compose.confirm"),
        None => "/api/v1/settings/compose.confirm".to_owned(),
    };
    let a = post(f, &path, key, &json!({ "value": value })).await;
    assert_eq!(a.status, 202, "{}", a.json);
    let op = finished(f.store(), a.json["id"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const PUBLISHING: &str = "\
services:
  web:
    image: nginx:1
    ports:
      - \"8080:80\"
    volumes:
      - /srv/data:/data
";

fn publishing_model() -> Value {
    json!({ "name": "x", "services": { "web": {
        "image": "nginx:1",
        "ports": [{ "mode": "ingress", "target": 80, "published": "8080", "protocol": "tcp" }],
        "volumes": [{ "type": "bind", "source": "/srv/data", "target": "/data" }],
        "environment": { "TOKEN": CANARY },
    }}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_file_starts_nothing_and_names_its_lines() {
    let f = fixture().await;
    let source = "\
services:
  web:
    image: busybox
    privileged: true
    volumes:
      - ./conf:/conf
      - /var/run/docker.sock:/var/run/docker.sock
    ports:
      - \"8080:80\"
";
    let rev = upload(&f, "app", &[("compose.yaml", source), ("conf/a", "a")]).await;
    f.config(
        rev,
        &json!({ "services": { "web": {
            "image": "busybox",
            "privileged": true,
            "volumes": [
                { "type": "bind", "source": "@SNAP@/conf", "target": "/conf" },
                { "type": "bind", "source": "/var/run/docker.sock", "target": "/var/run/docker.sock" },
            ],
            "ports": [{ "target": 80, "published": "8080", "protocol": "tcp" }],
        }}}),
    )
    .await;
    // Neither confirming nor a policy waives a reject item.
    set_policy(&f, None, "accept", "policy").await;
    let op = deploy_revision(&f, "app", "d1", rev, true).await;
    assert_failed(&op, "compose_rejected", OperationPhase::Validating);
    let message = op.failure_message.clone().unwrap();
    assert!(
        message.contains("line 4: services.web.privileged: privileged: true"),
        "{message}"
    );
    assert!(
        message.contains("line 7: services.web.volumes[1]: mounts /var/run/docker.sock"),
        "{message}"
    );
    assert!(message.contains("no isolation"), "{message}");
    let said = messages(f.store(), op.id).await;
    assert!(
        said.iter()
            .any(|m| m.starts_with("rejected: line 4: services.web.privileged")),
        "{said:?}"
    );
    assert_eq!(f.count("build"), 0);
    assert_eq!(f.count("up"), 0);
    let env = environment(&f, "app").await;
    assert_eq!(f.store().list_deployments(env.id).await.unwrap(), []);
    assert_eq!(f.store().list_services(env.id).await.unwrap(), []);
    assert!(audit(&f, CONFIRM_ACTION).await.is_empty());
    assert!(audit(&f, ACCEPT_ACTION).await.is_empty());

    // `--port` is for a Dockerfile; a Compose file keeps its own ports.
    let body = json!({ "revision_id": rev.to_string(), "port": "9000" });
    let op = deploy(&f, "app", "d2", body).await;
    assert_failed(&op, "invalid_request", OperationPhase::Validating);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmation_is_asked_recorded_and_remembered() {
    let f = fixture().await;
    let rev = upload(&f, "app", &[("compose.yaml", PUBLISHING)]).await;
    f.config(rev, &publishing_model()).await;

    // Unconfirmed: nothing recorded or started; the items and the
    // fingerprint are in the events.
    let op = deploy_revision(&f, "app", "d1", rev, false).await;
    assert_failed(&op, "confirmation_required", OperationPhase::Validating);
    let said = messages(f.store(), op.id).await;
    let asked: Vec<&String> = said
        .iter()
        .filter(|m| m.starts_with("needs confirmation: "))
        .collect();
    assert_eq!(
        asked,
        [
            "needs confirmation: line 7: services.web.volumes[0]: mounts the host path /srv/data \
             at /data (read-write)",
            "needs confirmation: line 5: services.web.ports[0]: publishes 0.0.0.0:8080 to 80/tcp",
        ],
        "{said:?}"
    );
    let fingerprint_line = said
        .iter()
        .find(|m| m.starts_with("compose.yaml fingerprint sha256:"))
        .expect("the fingerprint is stated")
        .clone();
    let fingerprint = fingerprint_line.rsplit(' ').next().unwrap().to_owned();
    assert!(
        said.iter()
            .any(|m| m.starts_with("Ship changes: name: ship_env_")),
        "{said:?}"
    );
    assert_eq!((f.count("build"), f.count("up")), (0, 0));
    let env = environment(&f, "app").await;
    assert_eq!(f.store().list_deployments(env.id).await.unwrap(), []);

    // Confirmed: deployed, and the confirmation is in the audit log.
    let op = deploy_revision(&f, "app", "d2", rev, true).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let confirmed = audit(&f, CONFIRM_ACTION).await;
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0].actor, "operator");
    assert_eq!(confirmed[0].target, project_id(&f, "app").await);
    let d = detail(&confirmed[0]);
    assert_eq!(d["fingerprint"], fingerprint.as_str());
    assert_eq!(d["file"], "compose.yaml");
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    let routes = f.store().list_routes(env.id).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].address, "0.0.0.0:8080");
    assert!(
        messages(f.store(), op.id)
            .await
            .contains(&"Endpoint: 0.0.0.0:8080 (web)".to_owned())
    );

    // The same file, in a new revision too, deploys without asking.
    let op = deploy_revision(&f, "app", "d3", rev, false).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let again = upload(&f, "app", &[("compose.yaml", PUBLISHING), ("README", "x")]).await;
    assert_ne!(again, rev);
    f.config(again, &publishing_model()).await;
    let op = deploy_revision(&f, "app", "d4", again, false).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    assert_eq!(audit(&f, CONFIRM_ACTION).await.len(), 1);

    // A changed file asks again.
    let changed = PUBLISHING.replace("nginx:1", "nginx:2");
    let rev2 = upload(&f, "app", &[("compose.yaml", &changed)]).await;
    let mut model = publishing_model();
    model["services"]["web"]["image"] = json!("nginx:2");
    f.config(rev2, &model).await;
    let op = deploy_revision(&f, "app", "d5", rev2, false).await;
    assert_failed(&op, "confirmation_required", OperationPhase::Validating);
    // Another project with the same file asks too.
    let other = upload(&f, "other", &[("compose.yaml", PUBLISHING)]).await;
    f.config(other, &publishing_model()).await;
    let op = deploy_revision(&f, "other", "d6", other, false).await;
    assert_failed(&op, "confirmation_required", OperationPhase::Validating);
}

const GPU: &str = "\
services:
  train:
    image: pytorch:2
    cap_add:
      - SYS_NICE
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: [gpu]
";

fn gpu_model() -> Value {
    json!({ "services": { "train": {
        "image": "pytorch:2",
        "cap_add": ["SYS_NICE"],
        "deploy": { "resources": { "reservations": { "devices": [
            { "driver": "nvidia", "count": -1, "capabilities": ["gpu"] },
        ]}}},
    }}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_accept_policy_is_the_daemons_and_is_audited() {
    let f = fixture().await;
    let rev = upload(&f, "lab", &[("compose.yaml", GPU)]).await;
    set_policy(&f, Some("lab"), "accept", "p1").await;
    let got = get(&f, "/api/v1/projects/lab/settings/compose.confirm").await;
    assert_eq!(
        (got.status, got.json["value"].clone()),
        (200, json!("accept"))
    );
    let host = get(&f, "/api/v1/settings/compose.confirm").await;
    assert_eq!(host.json["value"], "ask");

    // The project with `accept` deploys a GPU file without asking.
    f.config(rev, &gpu_model()).await;
    let op = deploy_revision(&f, "lab", "d1", rev, false).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let said = messages(f.store(), op.id).await;
    assert!(
        said.iter()
            .any(|m| m.starts_with("accepted by policy: compose.confirm is accept for the project")),
        "{said:?}"
    );
    let accepted = audit(&f, ACCEPT_ACTION).await;
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].actor, POLICY_ACTOR);
    assert_eq!(accepted[0].target, project_id(&f, "lab").await);
    let d = detail(&accepted[0]);
    let items: Vec<&str> = d["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.as_str().unwrap())
        .collect();
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(items[0].starts_with("line 4: services.train.cap_add: adds the capabilities SYS_NICE"));
    assert!(
        items[1].starts_with("line 10: services.train.deploy.resources.reservations.devices[0]"),
        "{items:?}"
    );
    assert_eq!(d["policy"]["scope"], "project");
    assert_eq!(d["policy"]["value"], "accept");
    assert_eq!(d["policy"]["set_by"], "operator");
    assert!(d["policy"]["set_at"].as_str().unwrap().ends_with('Z'));
    assert!(audit(&f, CONFIRM_ACTION).await.is_empty());
    let env = environment(&f, "lab").await;
    let services = f.store().list_services(env.id).await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].kind, ServiceKind::Compose);

    // Another project still asks.
    let other = upload(&f, "other", &[("compose.yaml", GPU)]).await;
    f.config(other, &gpu_model()).await;
    let op = deploy_revision(&f, "other", "d2", other, false).await;
    assert_failed(&op, "confirmation_required", OperationPhase::Validating);

    // A rejected file is still rejected under `accept`.
    let bad = "services:\n  train:\n    image: x\n    pid: host\n";
    let rejected = upload(&f, "lab", &[("compose.yaml", bad)]).await;
    f.config(
        rejected,
        &json!({ "services": { "train": { "image": "x", "pid": "host" } } }),
    )
    .await;
    let op = deploy_revision(&f, "lab", "d3", rejected, false).await;
    assert_failed(&op, "compose_rejected", OperationPhase::Validating);

    // The host's `accept` covers the other project, until it says `ask`.
    set_policy(&f, None, "accept", "p2").await;
    f.config(other, &gpu_model()).await;
    let op = deploy_revision(&f, "other", "d4", other, false).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let host_accepted = audit(&f, ACCEPT_ACTION).await;
    assert_eq!(
        detail(host_accepted.last().unwrap())["policy"]["scope"],
        "host"
    );
    set_policy(&f, Some("other"), "ask", "p3").await;
    let changed = upload(
        &f,
        "other",
        &[("compose.yaml", &GPU.replace("pytorch:2", "pytorch:3"))],
    )
    .await;
    f.config(changed, &gpu_model()).await;
    let op = deploy_revision(&f, "other", "d5", changed, false).await;
    assert_failed(&op, "confirmation_required", OperationPhase::Validating);

    // Bad settings requests are refused before any operation.
    let a = post(
        &f,
        "/api/v1/projects/lab/settings/compose.confirm",
        "bad1",
        &json!({ "value": "yes" }),
    )
    .await;
    assert_eq!(a.status, 400, "{}", a.json);
    let a = post(
        &f,
        "/api/v1/projects/lab/settings/publish.address",
        "bad2",
        &json!({ "value": "auto" }),
    )
    .await;
    assert_eq!(a.status, 404, "{}", a.json);
    let a = post(
        &f,
        "/api/v1/projects/ghost/settings/compose.confirm",
        "bad3",
        &json!({ "value": "ask" }),
    )
    .await;
    assert_eq!(a.status, 404, "{}", a.json);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_policy_in_the_pushed_files_is_ignored() {
    let f = fixture().await;
    let source = format!("x-ship:\n  compose.confirm: accept\n{PUBLISHING}");
    let rev = upload(
        &f,
        "app",
        &[
            ("compose.yaml", &source),
            (
                "ship.toml",
                "[settings]\n\"compose.confirm\" = \"accept\"\n",
            ),
            (".ship/settings.json", r#"{"compose.confirm":"accept"}"#),
        ],
    )
    .await;
    let mut model = publishing_model();
    model["x-ship"] = json!({ "compose.confirm": "accept" });
    f.config(rev, &model).await;
    let op = deploy_revision(&f, "app", "d1", rev, false).await;
    assert_failed(&op, "confirmation_required", OperationPhase::Validating);
    let got = get(&f, "/api/v1/projects/app/settings/compose.confirm").await;
    assert_eq!(got.json["value"], "ask");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_services_deploy_with_read_only_snapshot_binds() {
    let f = fixture().await;
    let port = free_port();
    let source = format!(
        "services:\n  web:\n    build: .\n    ports:\n      - \"127.0.0.1:{port}:80\"\n    \
         volumes:\n      - ./conf:/etc/app\n  db:\n    image: postgres:16\n    volumes:\n      \
         - data:/var/lib/postgresql/data\n    healthcheck:\n      test: [\"CMD\", \"true\"]\n\
         volumes:\n  data: {{}}\n"
    );
    let rev = upload(
        &f,
        "app",
        &[
            ("compose.yaml", &source),
            ("Dockerfile", "FROM scratch\n"),
            ("conf/app.conf", "x"),
        ],
    )
    .await;
    f.config(
        rev,
        &json!({
            "name": "x",
            "services": {
                "web": {
                    "build": { "context": "@SNAP@", "dockerfile": "Dockerfile" },
                    "ports": [{ "target": 80, "published": port.to_string(), "host_ip": "127.0.0.1", "protocol": "tcp" }],
                    "volumes": [{ "type": "bind", "source": "@SNAP@/conf", "target": "/etc/app", "bind": { "create_host_path": true } }],
                    "networks": { "default": null },
                },
                "db": {
                    "image": "postgres:16",
                    "volumes": [{ "type": "volume", "source": "data", "target": "/var/lib/postgresql/data" }],
                    "healthcheck": { "test": ["CMD", "true"] },
                    "networks": { "default": null },
                },
            },
            "networks": { "default": { "name": "x_default" } },
            "volumes": { "data": { "name": "x_data" } },
        }),
    )
    .await;
    f.control("health", "healthy");
    let op = deploy_revision(&f, "app", "d1", rev, true).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let said = messages(f.store(), op.id).await;
    assert!(said.contains(&"web is healthy".to_owned()), "{said:?}");
    assert!(said.contains(&"db is healthy".to_owned()), "{said:?}");
    assert!(
        said.contains(&format!("Endpoint: 127.0.0.1:{port} (web)")),
        "{said:?}"
    );
    assert!(said.contains(&"both compose.yaml and Dockerfile are present; deploying compose.yaml (pass --file Dockerfile to deploy the Dockerfile)".to_owned()), "{said:?}");

    let env = environment(&f, "app").await;
    let mut names: Vec<(String, ServiceKind)> = f
        .store()
        .list_services(env.id)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.name, s.kind))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        names,
        [
            ("db".to_owned(), ServiceKind::Compose),
            ("web".to_owned(), ServiceKind::Compose)
        ]
    );
    let routes = f.store().list_routes(env.id).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].address, format!("127.0.0.1:{port}"));

    // The override: labels, the snapshot bind read-only, the built image's
    // tag; nothing about ports, networks or privileges.
    let release = release_of(&f, "app", op.id).await;
    let dir = release_dir(&f.state, release);
    let snap = f.snapshot(rev).await;
    let over: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(LABELS_FILE)).unwrap()).unwrap();
    let web = &over["services"]["web"];
    assert_eq!(web["image"], format!("ship_{}-web:{release}", env.id));
    assert_eq!(
        web["volumes"],
        json!([{ "type": "bind", "source": format!("{}/conf", snap.display()), "target": "/etc/app",
                 "read_only": true, "bind": { "create_host_path": true } }])
    );
    let keys: Vec<&String> = over["services"]["db"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["labels"]);
    let text = over.to_string();
    for word in ["ports", "privileged", "network_mode", "cap_add"] {
        assert!(!text.contains(word), "{word} in {text}");
    }
    assert!(over["networks"]["default"]["labels"].is_object());
    assert!(over["volumes"]["data"]["labels"].is_object());

    // The release is defined by the snapshot's file and the override.
    let files: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(FILES_FILE)).unwrap()).unwrap();
    assert_eq!(files["directory"], snap.to_str().unwrap());
    assert_eq!(
        files["files"],
        json!([
            snap.join("compose.yaml").to_str().unwrap(),
            dir.join(LABELS_FILE).to_str().unwrap()
        ])
    );
    let calls = f.invocations();
    let build = calls.iter().find(|l| l.contains("[build]")).unwrap();
    assert!(build.ends_with("[build][--][web]"), "{build}");
    let up = calls.iter().find(|l| l.contains("[up]")).unwrap();
    assert!(
        up.contains(&format!(
            "[--file][{}]",
            snap.join("compose.yaml").display()
        )),
        "{up}"
    );
    assert!(
        up.contains(&format!("[--file][{}]", dir.join(LABELS_FILE).display())),
        "{up}"
    );
    assert!(up.contains("[--remove-orphans]"), "{up}");
    let r = f.store().get_release(release).await.unwrap();
    assert_eq!(r.images.len(), 2);
    assert!(
        r.images.contains(&"postgres:16".to_owned()),
        "{:?}",
        r.images
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_health_check_brings_the_previous_release_back() {
    let f = fixture().await;
    let source = "services:\n  web:\n    image: nginx:1\n  worker:\n    image: busybox\n";
    let model =
        json!({ "services": { "web": { "image": "nginx:1" }, "worker": { "image": "busybox" } } });
    let first = upload(&f, "app", &[("compose.yaml", source)]).await;
    f.config(first, &model).await;
    let op = deploy_revision(&f, "app", "d1", first, false).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let previous = release_of(&f, "app", op.id).await;

    let second = upload(
        &f,
        "app",
        &[("compose.yaml", &source.replace("nginx:1", "nginx:2"))],
    )
    .await;
    f.config(second, &model).await;
    f.control("health", "unhealthy");
    let op = deploy_revision(&f, "app", "d2", second, false).await;
    assert_failed(&op, "health_check_failed", OperationPhase::Verifying);
    let said = messages(f.store(), op.id).await;
    assert!(
        said.contains(&format!("release {previous} is running again")),
        "{said:?}"
    );
    let up_files = fs::read_to_string(f.fake.join("up_files")).unwrap();
    let prev_dir = release_dir(&f.state, previous);
    let first_snap = f.snapshot(first).await;
    assert_eq!(
        up_files,
        format!(
            "{}\n{}\n",
            first_snap.join("compose.yaml").display(),
            prev_dir.join(LABELS_FILE).display()
        )
    );
    let env = environment(&f, "app").await;
    let deployments = f.store().list_deployments(env.id).await.unwrap();
    assert_eq!(deployments[0].result, Some(DeploymentResult::Failed));
    assert_eq!(deployments[0].previous_release_id, Some(previous));
}

/// f-037: a service that passed on one poll and fails on a later one, while
/// another service is still starting, fails the deployment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_that_fails_after_passing_fails_the_deployment() {
    let f = fixture().await;
    let source = "services:\n  a:\n    image: nginx:1\n  b:\n    image: busybox\n";
    let model = json!({ "services": { "a": { "image": "nginx:1" }, "b": { "image": "busybox" } } });
    let rev = upload(&f, "app", &[("compose.yaml", source)]).await;
    f.config(rev, &model).await;
    f.control("health_seq", "healthy starting\nunhealthy healthy\n");
    let op = deploy_revision(&f, "app", "d1", rev, false).await;
    assert_failed(&op, "health_check_failed", OperationPhase::Verifying);
    let said = messages(f.store(), op.id).await;
    assert!(!said.iter().any(|m| m.ends_with(" is healthy")), "{said:?}");
}

/// f-038: one unhealthy replica of a service fails the deployment though
/// another replica of it is healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unhealthy_replica_fails_the_deployment() {
    let f = fixture().await;
    let source = "services:\n  web:\n    image: nginx:1\n    deploy:\n      replicas: 2\n";
    let model =
        json!({ "services": { "web": { "image": "nginx:1", "deploy": { "replicas": 2 } } } });
    let rev = upload(&f, "app", &[("compose.yaml", source)]).await;
    f.config(rev, &model).await;
    f.control("replicas", "2");
    f.control("health_seq", "healthy unhealthy\n");
    let op = deploy_revision(&f, "app", "d1", rev, false).await;
    assert_failed(&op, "health_check_failed", OperationPhase::Verifying);
    let said = messages(f.store(), op.id).await;
    assert!(!said.contains(&"web is healthy".to_owned()), "{said:?}");
    assert_eq!(
        op.failure_message.as_deref(),
        Some("the health check of web failed"),
        "{op:?}"
    );
}

/// Every file under `root`, recursively.
fn files_under(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_interpolated_model_is_kept_nowhere() {
    let f = fixture().await;
    let rev = upload(
        &f,
        "app",
        &[(
            "compose.yaml",
            &PUBLISHING.replace(
                "    ports:",
                "    environment:\n      TOKEN: ${TOKEN}\n    ports:",
            ),
        )],
    )
    .await;
    f.config(rev, &publishing_model()).await;
    let asked = deploy_revision(&f, "app", "d1", rev, false).await;
    assert_failed(&asked, "confirmation_required", OperationPhase::Validating);
    let done = deploy_revision(&f, "app", "d2", rev, true).await;
    assert_eq!(done.phase, OperationPhase::Succeeded, "{done:?}");
    // A model Compose refuses: its error is not passed on either.
    f.control("config", "not json");
    let broken = deploy_revision(&f, "app", "d3", rev, false).await;
    assert_eq!(broken.phase, OperationPhase::Failed, "{broken:?}");

    let mut answers = Vec::new();
    for op in [&asked, &done, &broken] {
        answers.push(get(&f, &format!("/api/v1/operations/{}", op.id)).await.body);
        answers.push(
            get(&f, &format!("/api/v1/operations/{}/events", op.id))
                .await
                .body,
        );
    }
    for body in &answers {
        let text = String::from_utf8_lossy(body);
        assert!(!text.contains(CANARY), "{text}");
    }
    // The database (and its WAL), the operation logs and the release
    // files: nothing under the state directory holds the value.
    let mut all = Vec::new();
    files_under(&f.state, &mut all);
    assert!(all.iter().any(|p| p.ends_with(LABELS_FILE)), "{all:?}");
    for path in all {
        let bytes = fs::read(&path).unwrap_or_default();
        let found = bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes());
        assert!(!found, "{} holds the interpolated value", path.display());
    }
}
