//! `ship status`, `ship deployments`, `ship logs` and `ship projects` from
//! the built binary against a daemon running in-process, with its store
//! seeded directly and a fake `docker` playing the Engine (as in
//! `ship-daemon`'s `tests/status.rs`): each `-o json` against its contract
//! fixture, the text forms, and `--follow`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ship_core::{
    DeploymentResult, ErrorCode, OperationPhase, ReleaseId, RevisionKind, RouteProvider,
    RouteStatus, ServiceKind, Slug,
};
use ship_store::{
    Environment, NewDeployment, NewOperation, NewRelease, NewRevision, NewRoute, NewService,
    OperationFailure, Project, Service, Store,
};

const SHIP: &str = env!("CARGO_BIN_EXE_ship");

const SCRIPT: &str = r##"#!/bin/sh
d="$FAKE_DIR"
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
        if [ -n "$follow" ]; then sleep 30; fi
        if [ -f "$d/logs-fail" ]; then echo "Error response from daemon: read failed" >&2; exit 1; fi
        exit 0;;
    esac;;
esac
exit 64
"##;

const WEB_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WORKER_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Daemon {
    rt: tokio::runtime::Runtime,
    running: Option<ship_daemon::Running>,
    /// Keeps `web`'s route answering.
    _route: TcpListener,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = self.rt.block_on(running.shutdown());
        }
    }
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(SHIP);
        c.env(
            "SHIP_CONFIG",
            self.dir.path().join("cfg").join("config.json"),
        )
        .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Starts a daemon whose store holds `my-app` (two services, a
    /// succeeded deployment, then a failed build) and `new-app` (nothing
    /// deployed), with the fake `docker` reporting both services running,
    /// and makes it the current target.
    fn daemon(&self) -> Daemon {
        let fake = self.dir.path().join("fake");
        fs::create_dir_all(&fake).unwrap();
        let script = self.dir.path().join("docker");
        fs::write(&script, SCRIPT).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let config = ship_daemon::Config {
            state_dir: self.dir.path().join("state"),
            socket_path: self.dir.path().join("ship.sock"),
            port: 0,
            docker: ship_docker::Runner::new(&script).env("FAKE_DIR", &fake),
            limits: ship_daemon::Limits {
                disk_watermark_bytes: 0,
                ..ship_daemon::Limits::default()
            },
            tailnet: std::sync::Arc::new(ship_net::StaticTailnet(None)),
            observe_interval: Duration::from_secs(3600),
            ..ship_daemon::Config::new(ship_daemon::Redactor::new())
        };
        let running = rt.block_on(ship_daemon::start(&config)).unwrap();
        let route = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = route.local_addr().unwrap().to_string();
        rt.block_on(seed(running.store(), &fake, &address));
        let socket = running.socket_path().display().to_string();
        let out = self.run(&["target", "add", "here", "local", "--socket", &socket]);
        assert!(out.status.success(), "{}", show(&out));
        Daemon {
            rt,
            running: Some(running),
            _route: route,
        }
    }
}

async fn seed(store: &Store, fake: &Path, address: &str) {
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
    store
        .finish_deployment(dep.id, DeploymentResult::Succeeded)
        .await
        .unwrap();
    store
        .advance_operation(op, OperationPhase::Succeeded)
        .await
        .unwrap();

    let rev2 = revision(store, &ensured.project).await;
    let op2 = operation(store, &env, "seed-2").await;
    let dep2 = store
        .create_deployment(&NewDeployment {
            environment_id: env.id,
            revision_id: rev2,
            operation_id: op2,
            previous_release_id: Some(rel),
        })
        .await
        .unwrap();
    store
        .advance_operation(op2, OperationPhase::Building)
        .await
        .unwrap();
    store
        .fail_operation(
            op2,
            OperationPhase::Failed,
            &OperationFailure {
                code: ErrorCode::BuildFailed,
                message: "the image build exited 1",
            },
        )
        .await
        .unwrap();
    store
        .finish_deployment(dep2.id, DeploymentResult::Failed)
        .await
        .unwrap();

    store
        .replace_routes(
            env.id,
            &[NewRoute {
                service_id: web.id,
                provider: RouteProvider::Port,
                address: address.to_owned(),
                status: RouteStatus::Confirmed,
            }],
        )
        .await
        .unwrap();
    store
        .get_or_create_project(&Slug::parse("new-app").unwrap())
        .await
        .unwrap();

    fs::write(fake.join("ids"), format!("{WEB_ID}\n{WORKER_ID}\n")).unwrap();
    let inspect = json!([
        container(
            WEB_ID,
            &ensured.project,
            &env,
            &web,
            rel,
            json!({"Status": "running", "ExitCode": 0, "Health": {"Status": "healthy"}}),
            0
        ),
        container(
            WORKER_ID,
            &ensured.project,
            &env,
            &worker,
            rel,
            json!({"Status": "running", "ExitCode": 0}),
            2
        ),
    ]);
    fs::write(fake.join("inspect.json"), inspect.to_string()).unwrap();
    fs::write(
        fake.join(format!("{WEB_ID}.out")),
        "2026-01-02T03:04:46.120000000Z listening on :8080\n",
    )
    .unwrap();
    fs::write(
        fake.join(format!("{WEB_ID}.err")),
        "2026-01-02T03:04:47.003Z warning: no cache directory\n",
    )
    .unwrap();
    fs::write(
        fake.join(format!("{WORKER_ID}.out")),
        "2026-01-02T03:04:47.5Z ready\n",
    )
    .unwrap();
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

fn container(
    id: &str,
    project: &Project,
    env: &Environment,
    service: &Service,
    rel: ReleaseId,
    state: Value,
    restarts: u64,
) -> Value {
    json!({
        "Id": id,
        "Name": format!("/{}-{}-1", env.compose_project(), service.name),
        "RestartCount": restarts,
        "State": state,
        "Config": { "Labels": {
            "dev.ship.project": project.id.to_string(),
            "dev.ship.environment": env.id.to_string(),
            "dev.ship.service": service.id.to_string(),
            "dev.ship.release": rel.to_string(),
            "com.docker.compose.service": service.name,
        }},
    })
}

fn show(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn fixture(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

/// Ids, timestamps and endpoint addresses replaced by placeholders.
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

/// Asserts `out` succeeded and its stdout, as JSON, matches the fixture
/// `name` apart from ids, timestamps and addresses, keys in the same order.
fn matches_fixture(out: &Output, name: &str) {
    assert!(out.status.success(), "{}", show(out));
    let got: Value = serde_json::from_slice(&out.stdout).unwrap();
    let want: Value = serde_json::from_str(&fixture(name)).unwrap();
    assert_eq!(normalise(&got), normalise(&want), "{name}");
    // Pretty-printed like the fixture, so a reordered key shows too.
    let pretty = |v: &Value| serde_json::to_string_pretty(&normalise(v)).unwrap();
    assert_eq!(pretty(&got), pretty(&want), "{name}");
}

#[test]
fn json_output_matches_the_fixtures() {
    let env = Env::new();
    let _daemon = env.daemon();
    matches_fixture(&env.run(&["status", "my-app", "-o", "json"]), "status.json");
    matches_fixture(
        &env.run(&["deployments", "my-app", "-o", "json"]),
        "deployments.json",
    );
    matches_fixture(&env.run(&["projects", "-o", "json"]), "projects.json");
    // The fixture's project is `my-app`; here the one never deployed is
    // `new-app`.
    let out = env.run(&["status", "new-app", "-o", "json"]);
    assert!(out.status.success(), "{}", show(&out));
    let mut got: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(got["project"]["slug"], "new-app");
    got["project"]["slug"] = json!("my-app");
    let want: Value = serde_json::from_str(&fixture("status-never-deployed.json")).unwrap();
    assert_eq!(normalise(&got), normalise(&want));
    // Logs: exactly, the fake's timestamps being the fixture's.
    let out = env.run(&["logs", "my-app", "-o", "json"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(stdout(&out), fixture("logs.jsonl"));
    assert_eq!(stderr(&out), "");
}

#[test]
fn text_output() {
    let env = Env::new();
    let _daemon = env.daemon();

    let out = env.run(&["status", "my-app"]);
    assert!(out.status.success(), "{}", show(&out));
    let text = stdout(&out);
    assert!(text.contains("Desired:     running\n"), "{text}");
    assert!(text.contains("Deployment:  succeeded at "), "{text}");
    let worker = text.lines().find(|l| l.starts_with("worker")).unwrap();
    assert!(worker.contains("running_unverified"), "{text}");
    assert!(worker.ends_with("not configured"), "{text}");
    let web = text.lines().find(|l| l.starts_with("web")).unwrap();
    assert!(web.contains("healthy"), "{text}");
    assert!(web.contains("127.0.0.1:"), "{text}");

    let out = env.run(&["deployments", "my-app"]);
    assert!(out.status.success(), "{}", show(&out));
    let text = stdout(&out);
    assert!(text.starts_with("STARTED"), "{text}");
    assert!(
        text.contains("  build_failed in building: the image build exited 1\n"),
        "{text}"
    );

    let out = env.run(&["projects"]);
    assert!(out.status.success(), "{}", show(&out));
    let text = stdout(&out);
    assert!(text.contains("running_unverified"), "{text}");
    assert!(text.contains("no services"), "{text}");

    // stdout lines on stdout, stderr lines on stderr.
    let out = env.run(&["logs", "my-app"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(stdout(&out), "web | listening on :8080\nworker | ready\n");
    assert_eq!(stderr(&out), "web | warning: no cache directory\n");

    let out = env.run(&["logs", "my-app/worker"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(stdout(&out), "worker | ready\n");
    assert_eq!(stderr(&out), "");
}

#[test]
fn follow_prints_lines_before_the_log_ends() {
    let env = Env::new();
    let _daemon = env.daemon();
    let mut child = env
        .command(&["logs", "my-app/web", "--follow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // The fake keeps the log open for 30 s after its lines.
    let first = lines.next().unwrap().unwrap();
    assert_eq!(first, "web | listening on :8080");
    assert!(started.elapsed() < Duration::from_secs(20));
    let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
    assert_eq!(
        err.next().unwrap().unwrap(),
        "web | warning: no cache directory"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn a_failed_log_read_exits_nonzero() {
    let env = Env::new();
    let _daemon = env.daemon();
    fs::write(env.dir.path().join("fake").join("logs-fail"), "").unwrap();
    for args in [
        &["logs", "my-app/worker"][..],
        &["logs", "my-app", "-o", "json"],
    ] {
        let out = env.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", show(&out));
    }
}

#[test]
fn refusals() {
    let env = Env::new();
    let daemon = env.daemon();
    for args in [
        &["status", "My App"][..],
        &["deployments", "-x-"],
        &["logs", "my-app/"],
        &["logs", "my-app/web/x"],
        &["logs", "my-app", "--tail", "-1"],
    ] {
        let out = env.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", show(&out));
    }
    for args in [
        &["status", "nope"][..],
        &["deployments", "nope"],
        &["logs", "nope"],
        &["logs", "my-app/db"],
        &["status", "my-app", "--env", "staging"],
    ] {
        let out = env.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", show(&out));
        assert!(
            stderr(&out).contains("not_found"),
            "{args:?}: {}",
            show(&out)
        );
        assert!(out.stdout.is_empty(), "{args:?}: {}", show(&out));
    }
    drop(daemon);
}
