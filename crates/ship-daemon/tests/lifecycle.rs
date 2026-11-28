//! Lifecycle commands (phase-1-ws-1.12) through the daemon's API, against
//! two fake `docker`s that record every invocation in `$FAKE_DIR/log`:
//!
//! - [`SERVICES`] plays an environment of several services seeded in the
//!   store: `ps` lists `$FAKE_DIR/ids`; `container inspect` prints, for
//!   each name in `$FAKE_DIR/services`, `<name>.running.json`, or
//!   `<name>.exited.json` while `<name>.stopped` exists; `compose stop`
//!   creates `<name>.stopped` for the services named (every one for none),
//!   `start` and `restart` remove it, `down` removes the ids.
//! - [`PIPELINE`] builds and runs a Dockerfile project as `deploy.rs`'s
//!   fake does, `stop` making its container exit 143 and `down` removing
//!   it.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::api::RECREATE_REQUIRED;
use ship_core::{
    DeploymentAction, DeploymentResult, DesiredState, OperationId, OperationPhase, ReleaseId,
    RevisionKind, RouteProvider, RouteStatus, ServiceKind, Slug,
};
use ship_daemon::{Config, DeploySettings};
use ship_docker::Runner;
use ship_net::Target;
use ship_net::client::{self, RequestBody};
use ship_source::{Options, pack, scan};
use ship_store::{
    Environment, NewDeployment, NewOperation, NewRelease, NewRevision, NewRoute, NewService,
    Operation, Project, Service, Store,
};

const TIMEOUT: Duration = Duration::from_secs(30);

const SERVICES: &str = r##"#!/bin/sh
d="$FAKE_DIR"
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
inspect() {
  printf '['
  sep=''
  for s in $(cat "$d/services"); do
    if [ -f "$d/$s.stopped" ]; then f="$d/$s.exited.json"; else f="$d/$s.running.json"; fi
    printf '%s' "$sep"
    cat "$f"
    sep=','
  done
  printf ']\n'
}
case "$1" in
  ps) cat "$d/ids" 2>/dev/null; exit 0;;
  network) exit 0;;
  container)
    if [ "$2" = inspect ]; then inspect; exit 0; fi
    exit 64;;
  compose) ;;
  *) exit 64;;
esac
cmd=""
after=""
named=""
for a in "$@"; do
  if [ -n "$after" ]; then named="$named $a"; continue; fi
  if [ "$a" = "--" ]; then after=1; continue; fi
  if [ -z "$cmd" ]; then
    case "$a" in stop|start|restart|down|up|config|build) cmd="$a";; esac
  fi
done
if [ -z "$named" ]; then named=$(cat "$d/services"); fi
case "$cmd" in
  stop) for s in $named; do touch "$d/$s.stopped"; done;;
  start|restart) for s in $named; do rm -f "$d/$s.stopped"; done;;
  down) rm -f "$d/ids";;
esac
exit 0
"##;

/// `deploy.rs`'s fake, with `stop`, `start`, `restart` and `down`.
const PIPELINE: &str = r##"#!/bin/sh
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
    if [ -z "$f1" ]; then f1="$a"; elif [ -z "$f2" ]; then f2="$a"; fi
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
    echo "[{\"Id\":\"sha256:$ID\",\"Config\":{\"ExposedPorts\":$ports,\"Volumes\":{\"/data\":{}}}}]"
    exit 0;;
  container)
    c=$(sed -n 1p "$d/up_files")
    l=$(sed -n 2p "$d/up_files")
    prj=$(grep -m1 '"dev.ship.project": "prj_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(grep -m1 '"dev.ship.environment": "env_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    svc=$(grep -m1 '"dev.ship.service": "svc_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -m1 '"image"' "$c" | sed 's/.*:\(rel_[a-z0-9]*\)".*/\1/')
    state='"Status":"running","ExitCode":0'
    if [ -f "$d/stopped" ]; then state='"Status":"exited","ExitCode":143'; fi
    printf '[{"Id":"%s","Name":"/ship-web-1","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"web"}},"State":{%s,"OOMKilled":false}}]\n' "$ID" "$prj" "$env" "$svc" "$rel" "$state"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config) cat "$f1"; exit 0;;
  build) echo "#1 building web"; exit 0;;
  up)
    printf '%s\n%s\n' "$f1" "$f2" > "$d/up_files"
    rm -f "$d/stopped"
    exit 0;;
  stop) touch "$d/stopped"; exit 0;;
  start|restart) rm -f "$d/stopped"; exit 0;;
  down) rm -f "$d/up_files" "$d/stopped"; exit 0;;
esac
exit 0
"##;

/// Which fake a fixture runs.
#[derive(Debug, Clone, Copy)]
enum Fake {
    Services,
    Pipeline,
}

fn script(which: Fake) -> &'static Path {
    static SERVICES_PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    static PIPELINE_PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (cell, text) = match which {
        Fake::Services => (&SERVICES_PATH, SERVICES),
        Fake::Pipeline => (&PIPELINE_PATH, PIPELINE),
    };
    &cell
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("docker");
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    fake: PathBuf,
    target: Target,
    running: Option<ship_daemon::Running>,
}

impl Fixture {
    fn store(&self) -> &Store {
        self.running.as_ref().unwrap().store()
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

    /// The daemon stopped and started again on the same state directory.
    async fn restart_daemon(&mut self) {
        self.running.take().unwrap().shutdown().await.unwrap();
        let running = ship_daemon::start(&self.config).await.unwrap();
        self.target = Target::Local {
            socket: running.socket_path().to_path_buf(),
        };
        self.running = Some(running);
    }

    async fn post(&self, path: &str, key: &str, body: Value) -> (u16, Value) {
        let headers = [
            ("content-type", "application/json"),
            ("idempotency-key", key),
        ];
        let body = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        let r = client::post(
            &self.target,
            path,
            &headers,
            RequestBody::bytes(body),
            TIMEOUT,
        )
        .await
        .unwrap();
        (r.status, serde_json::from_slice(&r.body).unwrap())
    }

    async fn get(&self, path: &str) -> Value {
        let r = client::get(&self.target, path, TIMEOUT).await.unwrap();
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        serde_json::from_slice(&r.body).unwrap()
    }

    /// Posts `action` (with `query`) and waits for its operation to end.
    async fn act(&self, action: &str, query: &str, key: &str) -> Operation {
        let path = format!("/api/v1/projects/my-app/{action}{query}");
        let (status, json) = self.post(&path, key, Value::Null).await;
        assert_eq!(status, 202, "{json}");
        assert_eq!(json["kind"], action);
        finished(self.store(), op_id(&json)).await
    }
}

fn op_id(json: &Value) -> OperationId {
    json["id"].as_str().unwrap().parse().unwrap()
}

async fn fixture(which: Fake) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake");
    fs::create_dir(&fake).unwrap();
    let mut config = common::config(dir.path());
    config.docker = Runner::new(script(which)).env("FAKE_DIR", &fake);
    config.deploy = DeploySettings {
        settle: Duration::from_millis(200),
        verify_timeout: Duration::from_secs(5),
        step_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(50),
    };
    let running = ship_daemon::start(&config).await.unwrap();
    let target = Target::Local {
        socket: running.socket_path().to_path_buf(),
    };
    Fixture {
        _dir: dir,
        config,
        fake,
        target,
        running: Some(running),
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

fn succeeded(op: &Operation) {
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
}

fn refused(op: &Operation, code: &str) {
    assert_eq!(op.phase, OperationPhase::Failed, "{op:?}");
    assert_eq!(op.failure_code.as_deref(), Some(code), "{op:?}");
}

// ---- several seeded services -------------------------------------------

const WEB_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WORKER_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Seeded {
    env: Environment,
    web: Service,
    worker: Service,
    rel: ReleaseId,
}

/// `my-app` with `web` and `worker`, one succeeded deployment of `rel`, a
/// route for `web`, and both containers running in the fake.
async fn seed(f: &Fixture) -> Seeded {
    let store = f.store();
    let ensured = store
        .get_or_create_project(&Slug::parse("my-app").unwrap())
        .await
        .unwrap();
    let env = ensured.environment;
    let service = |name: &str, kind| NewService {
        name: name.to_owned(),
        kind,
        port: None,
    };
    let web = store
        .create_service(env.id, &service("web", ServiceKind::Web))
        .await
        .unwrap();
    let worker = store
        .create_service(env.id, &service("worker", ServiceKind::Worker))
        .await
        .unwrap();
    let rev = store
        .create_revision(
            ensured.project.id,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: "sha256:aa",
                manifest_fingerprint: None,
            },
        )
        .await
        .unwrap()
        .id;
    let rel = store
        .create_release(
            rev,
            &NewRelease {
                images: &["sha256:img".to_owned()],
                config_fingerprint: "cfg",
            },
        )
        .await
        .unwrap()
        .id;
    let op = store
        .create_operation(&NewOperation {
            idempotency_key: "seed",
            kind: "push",
            environment_id: Some(env.id),
        })
        .await
        .unwrap()
        .operation
        .id;
    let dep = store
        .create_deployment(&NewDeployment {
            environment_id: env.id,
            revision_id: rev,
            operation_id: op,
            previous_release_id: None,
        })
        .await
        .unwrap();
    store.set_deployment_release(dep.id, rel).await.unwrap();
    store
        .finish_deployment(dep.id, DeploymentResult::Succeeded)
        .await
        .unwrap();
    store
        .advance_operation(op, OperationPhase::Succeeded)
        .await
        .unwrap();
    store
        .replace_routes(
            env.id,
            &[NewRoute {
                service_id: web.id,
                provider: RouteProvider::Port,
                address: "127.0.0.1:1".to_owned(),
                status: RouteStatus::Confirmed,
            }],
        )
        .await
        .unwrap();
    let s = Seeded {
        env,
        web,
        worker,
        rel,
    };
    let project = ensured.project;
    f.control("ids", &format!("{WEB_ID}\n{WORKER_ID}\n"));
    f.control("services", "web worker");
    for (id, svc) in [(WEB_ID, &s.web), (WORKER_ID, &s.worker)] {
        let running = json!({ "Status": "running", "ExitCode": 0, "OOMKilled": false });
        let exited = json!({ "Status": "exited", "ExitCode": 143, "OOMKilled": false });
        let running = container(id, &project, &s, svc, running);
        let exited = container(id, &project, &s, svc, exited);
        f.control(&format!("{}.running.json", svc.name), &running.to_string());
        f.control(&format!("{}.exited.json", svc.name), &exited.to_string());
    }
    s
}

fn container(id: &str, project: &Project, s: &Seeded, service: &Service, state: Value) -> Value {
    json!({
        "Id": id,
        "Name": format!("/{}-{}-1", s.env.compose_project(), service.name),
        "RestartCount": 0,
        "State": state,
        "Config": { "Labels": {
            "dev.ship.project": project.id.to_string(),
            "dev.ship.environment": s.env.id.to_string(),
            "dev.ship.service": service.id.to_string(),
            "dev.ship.release": s.rel.to_string(),
            "com.docker.compose.service": service.name,
        }},
    })
}

/// `(name, desired_state, status)` of each service in `ship status`.
async fn statuses(f: &Fixture) -> (String, Vec<(String, String, String)>) {
    let v = f.get("/api/v1/projects/my-app/status").await;
    let services = v["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_owned(),
                s["desired_state"].as_str().unwrap().to_owned(),
                s["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    (
        v["environment"]["desired_state"]
            .as_str()
            .unwrap()
            .to_owned(),
        services,
    )
}

fn row(name: &str, desired: &str, status: &str) -> (String, String, String) {
    (name.to_owned(), desired.to_owned(), status.to_owned())
}

/// The fake's invocations that change containers.
fn mutations(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| {
            ["[up]", "[start]", "[stop]", "[restart]", "[down]"]
                .iter()
                .any(|c| l.contains(c))
        })
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_one_service_of_two_keeps_it_stopped_across_a_restart() {
    let mut f = fixture(Fake::Services).await;
    let s = seed(&f).await;
    let op = f.act("stop", "?service=worker", "stop-worker").await;
    succeeded(&op);
    let stops: Vec<String> = mutations(&f.invocations());
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert!(stops[0].ends_with("[stop][--][worker]"), "{stops:?}");
    let worker = f.store().get_service(s.worker.id).await.unwrap();
    assert_eq!(worker.desired_state, DesiredState::Stopped);
    // Exit 143 after a stop asked for is `stopped`, never `failed`.
    let expected = (
        "running".to_owned(),
        vec![
            row("web", "running", "running_unverified"),
            row("worker", "stopped", "stopped"),
        ],
    );
    assert_eq!(statuses(&f).await, expected);

    let before = f.invocations().len();
    f.restart_daemon().await;
    assert_eq!(statuses(&f).await, expected);
    let after = f.invocations()[before..].to_vec();
    assert!(mutations(&after).is_empty(), "{after:?}");

    let op = f.act("start", "?service=worker", "start-worker").await;
    succeeded(&op);
    let last = mutations(&f.invocations()).last().unwrap().clone();
    assert!(last.ends_with("[start][--][worker]"), "{last}");
    assert_eq!(
        statuses(&f).await,
        (
            "running".to_owned(),
            vec![
                row("web", "running", "running_unverified"),
                row("worker", "running", "running_unverified"),
            ]
        )
    );
    // One audit event per action, naming the service.
    let audit = f.store().list_audit_events().await.unwrap();
    let actions: Vec<(&str, &str)> = audit
        .iter()
        .map(|a| (a.action.as_str(), a.target.as_str()))
        .collect();
    let worker_id = s.worker.id.to_string();
    assert_eq!(
        actions,
        [("stop", worker_id.as_str()), ("start", worker_id.as_str())]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_environment_is_stopped_not_failed_and_stays_so() {
    let mut f = fixture(Fake::Services).await;
    let s = seed(&f).await;
    succeeded(&f.act("stop", "", "stop-all").await);
    let env = f.store().get_environment(s.env.id).await.unwrap();
    assert_eq!(env.desired_state, DesiredState::Stopped);
    assert!(
        mutations(&f.invocations())[0].ends_with("[stop]"),
        "{:?}",
        f.invocations()
    );
    let expected = (
        "stopped".to_owned(),
        vec![
            row("web", "running", "stopped"),
            row("worker", "running", "stopped"),
        ],
    );
    assert_eq!(statuses(&f).await, expected);
    let before = f.invocations().len();
    f.restart_daemon().await;
    assert_eq!(statuses(&f).await, expected);
    assert!(mutations(&f.invocations()[before..]).is_empty());

    // One service cannot be started or restarted while the environment
    // is stopped, and nothing runs.
    let before = f.invocations().len();
    refused(
        &f.act("start", "?service=web", "start-web").await,
        "conflict",
    );
    refused(&f.act("restart", "", "restart-all").await, "conflict");
    assert!(mutations(&f.invocations()[before..]).is_empty());

    succeeded(&f.act("start", "", "start-all").await);
    assert!(
        mutations(&f.invocations())
            .last()
            .unwrap()
            .ends_with("[start]")
    );
    assert_eq!(
        statuses(&f).await,
        (
            "running".to_owned(),
            vec![
                row("web", "running", "running_unverified"),
                row("worker", "running", "running_unverified"),
            ]
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_keeps_the_release_and_skips_stopped_services() {
    let f = fixture(Fake::Services).await;
    let s = seed(&f).await;
    succeeded(&f.act("stop", "?service=worker", "stop-worker").await);
    succeeded(&f.act("restart", "", "restart").await);
    let last = mutations(&f.invocations()).last().unwrap().clone();
    assert!(last.ends_with("[restart][--][web]"), "{last}");
    refused(
        &f.act("restart", "?service=worker", "restart-worker").await,
        "conflict",
    );
    let history = f.store().list_deployments(s.env.id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].release_id, Some(s.rel));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redeploy_and_remove_of_one_service_are_refused_before_any_operation() {
    let f = fixture(Fake::Services).await;
    seed(&f).await;
    for action in ["redeploy", "remove"] {
        let path = format!("/api/v1/projects/my-app/{action}?service=web");
        let (status, json) = f.post(&path, &format!("{action}-web"), Value::Null).await;
        assert_eq!(status, 400, "{json}");
        assert_eq!(json["code"], "invalid_request");
        let message = json["message"].as_str().unwrap();
        assert!(message.contains("whole environment"), "{message}");
        assert!(json["operation_id"].is_null(), "{json}");
    }
    assert!(mutations(&f.invocations()).is_empty());
    assert!(f.store().list_audit_events().await.unwrap().is_empty());
    assert!(f.store().unfinished_operations().await.unwrap().is_empty());
    // Unknown names are `404` before any operation too.
    let (status, _) = f
        .post(
            "/api/v1/projects/my-app/stop?service=nope",
            "k1",
            Value::Null,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = f
        .post("/api/v1/projects/nope/stop", "k2", Value::Null)
        .await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_keeps_volumes_and_history_and_then_refuses_the_rest() {
    let f = fixture(Fake::Services).await;
    let s = seed(&f).await;
    succeeded(&f.act("remove", "", "remove").await);
    let downs: Vec<String> = mutations(&f.invocations());
    assert_eq!(downs.len(), 1, "{downs:?}");
    assert!(downs[0].ends_with("[down]"), "{downs:?}");
    for line in f.invocations() {
        assert!(
            !line.contains("[-v]") && !line.contains("--volumes") && !line.contains("[rm]"),
            "{line}"
        );
    }
    let env = f.store().get_environment(s.env.id).await.unwrap();
    assert_eq!(env.desired_state, DesiredState::Removed);
    assert!(f.store().list_routes(s.env.id).await.unwrap().is_empty());
    let history = f.store().list_deployments(s.env.id).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].action, DeploymentAction::Remove);
    assert_eq!(history[0].result, Some(DeploymentResult::Succeeded));
    assert_eq!(history[0].release_id, None);
    assert_eq!(history[0].previous_release_id, Some(s.rel));
    assert_eq!(history[1].action, DeploymentAction::Deploy);

    let listed = f.get("/api/v1/projects/my-app/deployments").await;
    let actions: Vec<&str> = listed["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["remove", "deploy"]);
    let status = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status["environment"]["desired_state"], "removed");
    assert_eq!(status["last_deployment"]["action"], "remove");

    let before = f.invocations().len();
    for (action, query) in [
        ("start", ""),
        ("stop", ""),
        ("restart", ""),
        ("redeploy", ""),
        ("stop", "?service=web"),
    ] {
        let op = f
            .act(action, query, &format!("after-{action}{query}"))
            .await;
        refused(&op, "conflict");
        let message = op.failure_message.unwrap();
        assert!(message.contains("--recreate"), "{message}");
    }
    // Removing again changes nothing.
    succeeded(&f.act("remove", "", "remove-again").await);
    assert!(mutations(&f.invocations()[before..]).is_empty());
    assert_eq!(f.store().list_deployments(s.env.id).await.unwrap().len(), 2);
    let removes = f
        .store()
        .list_audit_events()
        .await
        .unwrap()
        .iter()
        .filter(|a| a.action == "remove")
        .count();
    assert_eq!(removes, 1);
}

/// A failed deployment of a new revision in `env`, recorded in the store.
async fn failed_deployment(f: &Fixture, env: &Environment, key: &str) -> ship_store::Deployment {
    let store = f.store();
    let rev = store
        .create_revision(
            env.project_id,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: key,
                manifest_fingerprint: None,
            },
        )
        .await
        .unwrap()
        .id;
    let op = store
        .create_operation(&NewOperation {
            idempotency_key: key,
            kind: "push",
            environment_id: Some(env.id),
        })
        .await
        .unwrap()
        .operation
        .id;
    let dep = store
        .create_deployment(&NewDeployment {
            environment_id: env.id,
            revision_id: rev,
            operation_id: op,
            previous_release_id: None,
        })
        .await
        .unwrap();
    let dep = store
        .finish_deployment(dep.id, DeploymentResult::Failed)
        .await
        .unwrap();
    let failure = ship_store::OperationFailure {
        code: ship_core::ErrorCode::DeployFailed,
        message: "failed",
    };
    store
        .fail_operation(op, OperationPhase::Failed, &failure)
        .await
        .unwrap();
    dep
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_names_the_revision_and_release_serving() {
    // Revision A serving, a later deploy of revision B failed: the removal
    // names A and A's release.
    let f = fixture(Fake::Services).await;
    let s = seed(&f).await;
    let serving = f.store().list_deployments(s.env.id).await.unwrap()[0].clone();
    let failed = failed_deployment(&f, &s.env, "b").await;
    assert_ne!(failed.revision_id, serving.revision_id);
    succeeded(&f.act("remove", "", "remove").await);
    let removal = f.store().list_deployments(s.env.id).await.unwrap()[0].clone();
    assert_eq!(removal.action, DeploymentAction::Remove);
    assert_eq!(removal.revision_id, serving.revision_id);
    assert_eq!(removal.previous_release_id, Some(s.rel));

    // Nothing ever served: the latest deployment's revision, no release.
    let f = fixture(Fake::Services).await;
    let env = development(&f).await;
    let only = failed_deployment(&f, &env, "only").await;
    succeeded(&f.act("remove", "", "remove").await);
    let removal = f.store().list_deployments(env.id).await.unwrap()[0].clone();
    assert_eq!(removal.action, DeploymentAction::Remove);
    assert_eq!(removal.revision_id, only.revision_id);
    assert_eq!(removal.previous_release_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_action_waits_for_the_operation_holding_its_environment() {
    let f = fixture(Fake::Services).await;
    let s = seed(&f).await;
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let submission = ship_daemon::queue::Submission {
        idempotency_key: "holder",
        kind: "config",
        environment_id: Some(s.env.id),
    };
    let holder = f
        .running
        .as_ref()
        .unwrap()
        .queue()
        .submit(submission, move |_ctx| async move {
            let _ = held.await;
            Ok(())
        })
        .await
        .unwrap()
        .operation
        .id;
    let path = "/api/v1/projects/my-app/stop";
    let (status, json) = f.post(path, "stop-queued", Value::Null).await;
    assert_eq!(status, 202, "{json}");
    let stop = op_id(&json);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let waiting = f.store().get_operation(stop).await.unwrap();
    assert_eq!(waiting.phase, OperationPhase::Queued, "{waiting:?}");
    assert!(mutations(&f.invocations()).is_empty());
    release.send(()).unwrap();
    succeeded(&finished(f.store(), holder).await);
    succeeded(&finished(f.store(), stop).await);
    assert_eq!(mutations(&f.invocations()).len(), 1);
}

// ---- the Dockerfile pipeline -------------------------------------------

/// Uploads a tree with a Dockerfile to `my-app`.
async fn upload(f: &Fixture, marker: &str) -> String {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
    fs::write(root.path().join("marker.txt"), marker).unwrap();
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
        &f.target,
        "/api/v1/projects/my-app/uploads",
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

async fn deploy(f: &Fixture, key: &str, body: Value) -> Operation {
    let (status, json) = f
        .post("/api/v1/projects/my-app/deployments", key, body)
        .await;
    assert_eq!(status, 202, "{json}");
    finished(f.store(), op_id(&json)).await
}

async fn development(f: &Fixture) -> Environment {
    f.store()
        .get_or_create_project(&Slug::parse("my-app").unwrap())
        .await
        .unwrap()
        .environment
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redeploy_applies_new_config_and_restart_does_not() {
    let f = fixture(Fake::Pipeline).await;
    f.control("exposed", r#"{"3000/tcp":{}}"#);
    let revision = upload(&f, "one").await;
    let port = free_port().to_string();
    let op = deploy(&f, "d1", json!({ "revision_id": revision, "port": port })).await;
    succeeded(&op);
    let env = development(&f).await;

    let (status, json) = f
        .post(
            "/api/v1/projects/my-app/vars",
            "var-1",
            json!({ "name": "GREETING", "value": "hello" }),
        )
        .await;
    assert_eq!(status, 202, "{json}");
    succeeded(&finished(f.store(), op_id(&json)).await);
    let stale = |v: &Value| v["services"][0]["stale_config"].as_bool().unwrap();
    assert!(stale(&f.get("/api/v1/projects/my-app/status").await));

    // A restart leaves the release and the config as they are.
    succeeded(&f.act("restart", "", "restart").await);
    assert_eq!(f.store().list_deployments(env.id).await.unwrap().len(), 1);
    assert!(stale(&f.get("/api/v1/projects/my-app/status").await));

    let op = f.act("redeploy", "", "redeploy").await;
    succeeded(&op);
    let history = f.store().list_deployments(env.id).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].operation_id, op.id);
    assert_eq!(history[0].revision_id, history[1].revision_id);
    assert_ne!(history[0].release_id, history[1].release_id);
    assert_eq!(history[0].options, history[1].options);
    assert!(
        history[0].options.as_deref().unwrap().contains(&port),
        "{:?}",
        history[0].options
    );
    assert!(!stale(&f.get("/api/v1/projects/my-app/status").await));
    // The port asked for at the first deploy is published again.
    let routes = f.store().list_routes(env.id).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert!(
        routes[0].address.ends_with(&format!(":{port}")),
        "{routes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_environment_comes_back_only_when_recreated() {
    let f = fixture(Fake::Pipeline).await;
    let revision = upload(&f, "one").await;
    succeeded(&deploy(&f, "d1", json!({ "revision_id": revision })).await);
    let env = development(&f).await;
    succeeded(&f.act("remove", "", "remove").await);
    assert_eq!(
        f.store()
            .get_environment(env.id)
            .await
            .unwrap()
            .desired_state,
        DesiredState::Removed
    );
    // The store alone does not bring it back.
    assert!(
        f.store()
            .set_desired_state(env.id, DesiredState::Running)
            .await
            .is_err()
    );

    let ups_before = f
        .invocations()
        .iter()
        .filter(|l| l.contains("[up]"))
        .count();
    let op = deploy(&f, "d2", json!({ "revision_id": revision })).await;
    refused(&op, "confirmation_required");
    assert_eq!(op.failure_phase, Some(OperationPhase::Validating));
    let message = op.failure_message.unwrap();
    assert!(message.starts_with(RECREATE_REQUIRED), "{message}");
    assert!(message.contains("--recreate"), "{message}");
    assert!(message.contains("data"), "names the volume: {message}");
    assert_eq!(f.store().list_deployments(env.id).await.unwrap().len(), 2);
    let ups = |f: &Fixture| {
        f.invocations()
            .iter()
            .filter(|l| l.contains("[up]"))
            .count()
    };
    assert_eq!(ups(&f), ups_before);

    let op = deploy(
        &f,
        "d3",
        json!({ "revision_id": revision, "recreate": true }),
    )
    .await;
    succeeded(&op);
    assert_eq!(ups(&f), ups_before + 1);
    assert_eq!(
        f.store()
            .get_environment(env.id)
            .await
            .unwrap()
            .desired_state,
        DesiredState::Running
    );
    let listed = f.get("/api/v1/projects/my-app/deployments").await;
    let rows: Vec<(&str, &str)> = listed["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["action"].as_str().unwrap(), d["result"].as_str().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [
            ("deploy", "succeeded"),
            ("remove", "succeeded"),
            ("deploy", "succeeded"),
        ]
    );
    // Nothing served after the removal, so the recreating deployment has
    // no previous release.
    let history = f.store().list_deployments(env.id).await.unwrap();
    assert_eq!(history[0].previous_release_id, None);
    let audit = f.store().list_audit_events().await.unwrap();
    assert!(audit.iter().any(|a| a.action == "recreate"), "{audit:?}");
    let status = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status["environment"]["desired_state"], "running");
    assert_eq!(status["services"][0]["status"], "running_unverified");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_runs_a_stopped_environment_again() {
    let f = fixture(Fake::Pipeline).await;
    let revision = upload(&f, "one").await;
    succeeded(&deploy(&f, "d1", json!({ "revision_id": revision })).await);
    let env = development(&f).await;
    succeeded(&f.act("stop", "", "stop").await);
    let status = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status["services"][0]["status"], "stopped");
    succeeded(&deploy(&f, "d2", json!({ "revision_id": revision })).await);
    assert_eq!(
        f.store()
            .get_environment(env.id)
            .await
            .unwrap()
            .desired_state,
        DesiredState::Running
    );
    let status = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status["services"][0]["status"], "running_unverified");
}
