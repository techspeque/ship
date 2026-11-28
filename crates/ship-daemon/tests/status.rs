//! Status, deployments, projects and logs (phase-1-ws-1.11) through the
//! daemon's API, with the store seeded directly and a fake `docker` that
//! plays the Engine from control files: `ps` lists `$FAKE_DIR/ids`,
//! `container inspect` prints `$FAKE_DIR/inspect.json`, `container logs`
//! prints `<id>.out` on stdout and `<id>.err` on stderr (with `--follow`,
//! then `<id>.late` a moment later, then it waits), and every command
//! fails as Docker does when its socket is gone while
//! `$FAKE_DIR/unreachable` exists. Every invocation is logged.

mod common;

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{
    DeploymentResult, DesiredState, ErrorCode, OperationPhase, ReleaseId, RevisionKind,
    RouteProvider, RouteStatus, SecretValue, ServiceKind, Slug,
};
use ship_daemon::Redactor;
use ship_docker::Runner;
use ship_net::Target;
use ship_net::client;
use ship_store::{
    Deployment, Environment, NewDeployment, NewOperation, NewRelease, NewRevision, NewRoute,
    NewService, OperationFailure, Project, Service, Store,
};

const TIMEOUT: Duration = Duration::from_secs(30);
const SECRET: &str = "hunter2-logs-secret";

const SCRIPT: &str = r##"#!/bin/sh
d="$FAKE_DIR"
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
if [ -f "$d/unreachable" ]; then
  echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2
  exit 1
fi
case "$1" in
  ps) cat "$d/ids" 2>/dev/null; exit 0;;
  container)
    case "$2" in
      inspect) cat "$d/inspect.json"; exit 0;;
      logs)
        id=""
        follow=""
        prev=""
        for a in "$@"; do
          if [ "$prev" = "--" ]; then id="$a"; fi
          if [ "$a" = "--follow" ]; then follow=1; fi
          prev="$a"
        done
        cat "$d/$id.out" 2>/dev/null
        cat "$d/$id.err" >&2 2>/dev/null
        if [ -n "$follow" ]; then
          sleep 1
          cat "$d/$id.late" 2>/dev/null
          sleep 30
        fi
        exit 0;;
    esac;;
esac
exit 64
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

const WEB_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WORKER_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.fake.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = client::get(&self.target, path, TIMEOUT).await.unwrap();
        let body = String::from_utf8(r.body.to_vec()).unwrap();
        let json = serde_json::from_str(&body).unwrap_or(Value::String(body));
        (r.status, json)
    }

    /// The body of a logs answer, one JSON value per line.
    async fn log_lines(&self, path: &str) -> (u16, Vec<Value>) {
        let r = client::get(&self.target, path, TIMEOUT).await.unwrap();
        let body = String::from_utf8(r.body.to_vec()).unwrap();
        if r.status != 200 {
            return (r.status, vec![serde_json::from_str(&body).unwrap()]);
        }
        let lines = body
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (r.status, lines)
    }
}

async fn fixture(observe_interval: Duration) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake");
    fs::create_dir(&fake).unwrap();
    let mut config = common::config(dir.path());
    config.docker = Runner::new(fake_docker()).env("FAKE_DIR", &fake);
    config.observe_interval = observe_interval;
    let redactor = Redactor::new();
    redactor.register(&SecretValue::new(SECRET));
    config.redactor = redactor;
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

/// A project `my-app` with `web` and `worker` services and one succeeded
/// deployment of release `rel`.
struct Seeded {
    project: Project,
    env: Environment,
    web: Service,
    worker: Service,
    rel: ReleaseId,
    succeeded: Deployment,
}

async fn seed(store: &Store) -> Seeded {
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
    let rev = revision(store, &ensured.project).await;
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
    let op = operation(store, &env, "seed-1").await;
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
    let succeeded = store
        .finish_deployment(dep.id, DeploymentResult::Succeeded)
        .await
        .unwrap();
    store
        .advance_operation(op, OperationPhase::Succeeded)
        .await
        .unwrap();
    Seeded {
        project: ensured.project,
        env,
        web,
        worker,
        rel,
        succeeded,
    }
}

async fn revision(store: &Store, project: &Project) -> ship_core::RevisionId {
    store
        .create_revision(
            project.id,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: "sha256:aa",
                manifest_fingerprint: None,
            },
        )
        .await
        .unwrap()
        .id
}

async fn operation(store: &Store, env: &Environment, key: &str) -> ship_core::OperationId {
    store
        .create_operation(&NewOperation {
            idempotency_key: key,
            kind: "push",
            environment_id: Some(env.id),
        })
        .await
        .unwrap()
        .operation
        .id
}

/// A deployment of a new revision whose build failed, after `s`'s.
async fn fail_a_build(store: &Store, s: &Seeded) -> Deployment {
    let rev = revision(store, &s.project).await;
    let op = operation(store, &s.env, "seed-2").await;
    let dep = store
        .create_deployment(&NewDeployment {
            environment_id: s.env.id,
            revision_id: rev,
            operation_id: op,
            previous_release_id: Some(s.rel),
        })
        .await
        .unwrap();
    store
        .advance_operation(op, OperationPhase::Building)
        .await
        .unwrap();
    store
        .fail_operation(
            op,
            OperationPhase::Failed,
            &OperationFailure {
                code: ErrorCode::BuildFailed,
                message: "the image build exited 1",
            },
        )
        .await
        .unwrap();
    store
        .finish_deployment(dep.id, DeploymentResult::Failed)
        .await
        .unwrap()
}

/// One container's `container inspect` object.
fn container(id: &str, s: &Seeded, service: &Service, state: Value, restarts: u64) -> Value {
    json!({
        "Id": id,
        "Name": format!("/{}-{}-1", s.env.compose_project(), service.name),
        "RestartCount": restarts,
        "State": state,
        "Config": { "Labels": {
            "dev.ship.project": s.project.id.to_string(),
            "dev.ship.environment": s.env.id.to_string(),
            "dev.ship.service": service.id.to_string(),
            "dev.ship.release": s.rel.to_string(),
            "com.docker.compose.service": service.name,
        }},
    })
}

/// `web` running with a passing health check, `worker` running with none
/// and restarted twice.
fn run_both(f: &Fixture, s: &Seeded) {
    f.control("ids", &format!("{WEB_ID}\n{WORKER_ID}\n"));
    let inspect = json!([
        container(
            WEB_ID,
            s,
            &s.web,
            json!({"Status": "running", "ExitCode": 0, "OOMKilled": false,
                   "Health": {"Status": "healthy"}}),
            0,
        ),
        container(
            WORKER_ID,
            s,
            &s.worker,
            json!({"Status": "running", "ExitCode": 0, "OOMKilled": false}),
            2,
        ),
    ]);
    f.control("inspect.json", &inspect.to_string());
}

/// Ids, timestamps and endpoint addresses replaced by placeholders, so
/// output can be compared with a fixture.
fn normalise(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let v = if k == "address" {
                        json!("ADDRESS")
                    } else {
                        normalise(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(normalise).collect()),
        Value::String(s) => {
            if let Some((prefix, rest)) = s.split_once('_')
                && matches!(prefix, "prj" | "env" | "svc" | "rev" | "rel" | "dep" | "op")
                && rest.len() == 26
            {
                json!(format!("{prefix}_ID"))
            } else if s.parse::<ship_core::Timestamp>().is_ok() {
                json!("TIMESTAMP")
            } else {
                v.clone()
            }
        }
        _ => v.clone(),
    }
}

fn fixture_json(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ship/tests/fixtures")
        .join(name);
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A listener for `web`'s route, so its endpoint answers.
fn answering_route() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    (listener, address)
}

async fn route(store: &Store, s: &Seeded, service: &Service, address: &str, status: RouteStatus) {
    store
        .replace_routes(
            s.env.id,
            &[NewRoute {
                service_id: service.id,
                provider: RouteProvider::Port,
                address: address.to_owned(),
                status,
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn status_matches_the_fixture_and_keeps_the_three_facts_apart() {
    let f = fixture(Duration::from_secs(3600)).await;
    let s = seed(f.store()).await;
    let (listener, address) = answering_route();
    route(f.store(), &s, &s.web, &address, RouteStatus::Confirmed).await;
    run_both(&f, &s);

    let (status, got) = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(normalise(&got), normalise(&fixture_json("status.json")));
    assert_eq!(got["services"][0]["status"], "healthy");
    // No health check: running_unverified, never healthy.
    assert_eq!(got["services"][1]["status"], "running_unverified");
    assert_eq!(got["services"][1]["restarts"], 2);
    assert_eq!(got["services"][0]["endpoints"][0]["address"], address);
    assert_eq!(got["last_deployment"]["id"], s.succeeded.id.to_string());

    // The observations were recorded on request.
    let stored = f.store().list_observations(s.env.id).await.unwrap();
    assert_eq!(stored.len(), 2);

    // Observing ran only read-only commands.
    for call in f.invocations() {
        assert!(
            call.starts_with("[ps]") || call.starts_with("[container][inspect]"),
            "{call}"
        );
    }

    // A route that does not answer, or is not confirmed, is not shown.
    drop(listener);
    let (_, got) = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(got["services"][0]["endpoints"], json!([]));
    let (_live_listener, live) = answering_route();
    route(f.store(), &s, &s.web, &live, RouteStatus::Pending).await;
    let (_, got) = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(got["services"][0]["endpoints"], json!([]));

    f.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_project_never_deployed_matches_its_fixture() {
    let f = fixture(Duration::from_secs(3600)).await;
    f.store()
        .get_or_create_project(&Slug::parse("my-app").unwrap())
        .await
        .unwrap();
    let (status, got) = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(
        normalise(&got),
        normalise(&fixture_json("status-never-deployed.json"))
    );
    let (status, got) = f.get("/api/v1/projects/my-app/deployments").await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(got, json!({ "deployments": [] }));

    for path in [
        "/api/v1/projects/nope/status",
        "/api/v1/projects/my-app/status?env=staging",
        "/api/v1/projects/nope/deployments",
        "/api/v1/projects/nope/logs",
    ] {
        let (status, got) = f.get(path).await;
        assert_eq!(status, 404, "{path}: {got}");
        assert_eq!(got["code"], "not_found", "{path}");
    }
    for path in [
        "/api/v1/projects/My%20App/status",
        "/api/v1/projects/my-app/status?service=web",
        "/api/v1/projects/my-app/logs?follow=yes",
    ] {
        let (status, got) = f.get(path).await;
        assert_eq!(status / 100, 4, "{path}: {got}");
        assert_ne!(status, 404, "{path}: {got}");
    }
    f.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_deployment_is_listed_and_the_running_release_stays_visible() {
    let f = fixture(Duration::from_secs(3600)).await;
    let s = seed(f.store()).await;
    let failed = fail_a_build(f.store(), &s).await;
    run_both(&f, &s);

    let (status, got) = f.get("/api/v1/projects/my-app/deployments").await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(
        normalise(&got),
        normalise(&fixture_json("deployments.json"))
    );
    let list = got["deployments"].as_array().unwrap();
    assert_eq!(list[0]["id"], failed.id.to_string());
    assert_eq!(list[0]["release_id"], Value::Null);
    assert_eq!(list[0]["previous_release_id"], s.rel.to_string());
    assert_eq!(
        list[0]["failure"],
        json!({ "code": "build_failed", "phase": "building", "message": "the image build exited 1" })
    );
    assert_eq!(list[1]["id"], s.succeeded.id.to_string());

    let (_, got) = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(got["last_deployment"]["id"], s.succeeded.id.to_string());
    assert_eq!(got["last_deployment"]["result"], "succeeded");
    assert_eq!(got["last_deployment"]["release_id"], s.rel.to_string());
    f.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn docker_unreachable_makes_every_service_unknown_and_changes_nothing() {
    let f = fixture(Duration::from_secs(3600)).await;
    let s = seed(f.store()).await;
    let (_listener, address) = answering_route();
    route(f.store(), &s, &s.web, &address, RouteStatus::Confirmed).await;
    run_both(&f, &s);
    let deployments_before = f.store().list_deployments(s.env.id).await.unwrap();
    let routes_before = f.store().list_routes(s.env.id).await.unwrap();
    f.control("unreachable", "");

    let (status, got) = f.get("/api/v1/projects/my-app/status").await;
    assert_eq!(status, 200, "{got}");
    for svc in got["services"].as_array().unwrap() {
        assert_eq!(svc["status"], "unknown", "{svc}");
        // Nothing is known, so no address is claimed either.
        assert_eq!(svc["endpoints"], json!([]), "{svc}");
    }
    assert_eq!(got["environment"]["desired_state"], "running");
    assert_eq!(got["last_deployment"]["result"], "succeeded");

    let (status, got) = f.get("/api/v1/projects").await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(got["projects"][0]["environments"][0]["status"], "unknown");

    let stored = f.store().list_observations(s.env.id).await.unwrap();
    assert_eq!(stored.len(), 2);
    for o in &stored {
        assert_eq!(o.observation, ship_core::Observation::DockerUnreachable);
    }
    // Nothing was cleaned up or changed.
    let env = f.store().get_environment(s.env.id).await.unwrap();
    assert_eq!(env.desired_state, DesiredState::Running);
    assert_eq!(
        f.store().list_deployments(s.env.id).await.unwrap(),
        deployments_before
    );
    assert_eq!(
        f.store().list_routes(s.env.id).await.unwrap(),
        routes_before
    );
    for call in f.invocations() {
        assert!(call.starts_with("[ps]"), "only the listing ran: {call}");
    }

    // Logs: refused before anything streams.
    let (status, lines) = f.log_lines("/api/v1/projects/my-app/logs").await;
    assert_eq!(status, 503, "{lines:?}");
    assert_eq!(lines[0]["code"], "docker_unavailable");
    f.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn projects_show_each_environment_and_its_overall_status() {
    let f = fixture(Duration::from_secs(3600)).await;
    let s = seed(f.store()).await;
    f.store()
        .get_or_create_project(&Slug::parse("new-app").unwrap())
        .await
        .unwrap();
    // `web` healthy, `worker` running without a check: running_unverified.
    run_both(&f, &s);
    let (status, got) = f.get("/api/v1/projects").await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(normalise(&got), normalise(&fixture_json("projects.json")));
    f.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_interval_observer_records_without_a_request() {
    let f = fixture(Duration::from_millis(100)).await;
    let s = seed(f.store()).await;
    run_both(&f, &s);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let stored = f.store().list_observations(s.env.id).await.unwrap();
        if stored.len() == 2 {
            assert!(
                stored
                    .iter()
                    .all(|o| matches!(o.observation, ship_core::Observation::Reachable { .. }))
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no observation was recorded"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f.running.shutdown().await.unwrap();
}

/// The fake's log files for both containers, as in `logs.jsonl`, plus a
/// secret `web` prints and a line it writes later.
fn write_logs(f: &Fixture) {
    f.control(
        &format!("{WEB_ID}.out"),
        &format!(
            "2026-01-02T03:04:46.120000000Z listening on :8080\n\
             2026-01-02T03:04:48.5Z token={SECRET}\n"
        ),
    );
    f.control(
        &format!("{WEB_ID}.err"),
        "2026-01-02T03:04:47.003Z warning: no cache directory\n",
    );
    f.control(
        &format!("{WORKER_ID}.out"),
        "2026-01-02T03:04:47.5Z ready\n",
    );
    f.control(
        &format!("{WEB_ID}.late"),
        "2026-01-02T03:05:00Z late line\n",
    );
}

#[tokio::test]
async fn logs_tell_streams_apart_narrow_to_a_service_and_redact() {
    let f = fixture(Duration::from_secs(3600)).await;
    let s = seed(f.store()).await;
    run_both(&f, &s);
    write_logs(&f);

    let (status, lines) = f.log_lines("/api/v1/projects/my-app/logs").await;
    assert_eq!(status, 200, "{lines:?}");
    let expected: Vec<Value> = include_str!("../../ship/tests/fixtures/logs.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Sorted by time: the fixture's three lines, then the redacted one.
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(&lines[..3], &expected[..]);
    assert_eq!(lines[3]["service"], "web");
    let text = lines[3]["line"].as_str().unwrap();
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.starts_with("token="), "{text}");

    let (status, lines) = f
        .log_lines("/api/v1/projects/my-app/logs?service=worker")
        .await;
    assert_eq!(status, 200, "{lines:?}");
    assert_eq!(lines, vec![expected[2].clone()]);

    let (status, lines) = f.log_lines("/api/v1/projects/my-app/logs?service=db").await;
    assert_eq!(status, 404, "{lines:?}");

    // `tail` reaches Docker as `--tail`.
    let (status, _) = f.log_lines("/api/v1/projects/my-app/logs?tail=5").await;
    assert_eq!(status, 200);
    assert!(
        f.invocations()
            .iter()
            .any(|c| c.starts_with("[container][logs]") && c.contains("[--tail][5]")),
        "{:?}",
        f.invocations()
    );
    f.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn followed_logs_arrive_as_they_are_written() {
    let f = fixture(Duration::from_secs(3600)).await;
    let s = seed(f.store()).await;
    run_both(&f, &s);
    write_logs(&f);

    let mut stream = client::get_stream(
        &f.target,
        "/api/v1/projects/my-app/logs?service=web&follow=true",
        &[],
        TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(stream.head.status, 200);
    let mut seen = String::new();
    // The fake keeps every reader open for 30 s after its late line, so
    // reading that line well before then shows lines are not held back.
    let read = tokio::time::timeout(Duration::from_secs(15), async {
        while !seen.contains("late line") {
            let chunk = stream.next_chunk().await.expect("stream open").unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await;
    assert!(read.is_ok(), "the late line did not arrive: {seen}");
    let lines: Vec<Value> = seen
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let streams: Vec<&str> = lines
        .iter()
        .map(|l| l["stream"].as_str().unwrap())
        .collect();
    assert!(
        streams.contains(&"stdout") && streams.contains(&"stderr"),
        "{seen}"
    );
    assert!(lines.iter().all(|l| l["service"] == "web"), "{seen}");
    assert!(!seen.contains(SECRET), "{seen}");
    drop(stream);
    f.running.shutdown().await.unwrap();
}
