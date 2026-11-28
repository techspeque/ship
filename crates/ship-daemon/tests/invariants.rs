//! No known test secret anywhere (OVERVIEW §3.3, ADR-0009 rule 7) over a
//! full scenario through the daemon's API, against a fake `docker`: a
//! secret stored through the API and a value registered with the redactor
//! at start must not appear in the daemon's stderr log, any API response,
//! any operation event or log file, any generated Compose or release file,
//! the database, or any argv Docker was given. They live only where they
//! must: the secret store and the env files of a release (ADR-0009).
//!
//! The parent test re-runs this test binary as a child that installs the
//! daemon's real global logger and plays the scenario, writing every API
//! response it received to a file; the parent then scans the child's
//! stderr and stdout, those responses, the store's events and every file
//! of the state directory. The scanner is shown to fail on a deliberate
//! leak of each kind. Image history needs a real Engine:
//! `real_recovery.rs`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{OperationId, OperationPhase, SecretValue};
use ship_daemon::config::ENV_DIR;
use ship_daemon::deploy::RELEASES_DIR;
use ship_daemon::secrets::SECRETS_DIR;
use ship_daemon::{DeploySettings, Redactor};
use ship_docker::Runner;
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::Store;

const CHILD_ENV: &str = "SHIP_INVARIANTS_CHILD";
const DIR_ENV: &str = "SHIP_INVARIANTS_DIR";
/// Stored through the API as a project secret.
const STORED: &str = "hunter2-stored-invariant-9d41";
/// Registered with the redactor at start, as a value the daemon must never
/// show; logged on purpose through the real logger.
const REGISTERED: &str = "hunter2-registered-invariant-73c0";
const SECRETS: [&str; 2] = [STORED, REGISTERED];
const TIMEOUT: Duration = Duration::from_secs(30);
/// Where the child writes every API response body, one per line.
const RESPONSES: &str = "responses.jsonl";

/// A fake `docker` as `deploy.rs`'s: every invocation appended to
/// `$FAKE_DIR/log`; `compose up` records its files; `ps` and `container
/// inspect` report one running container of the files last brought up.
const SCRIPT: &str = r##"#!/bin/sh
d="$FAKE_DIR"
ID=4f9a1c0d2e3b4a5968778695a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
first="$1"
cmd=""
f1=""
all=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--file" ]; then
    if [ -z "$f1" ]; then f1="$a"; fi
    all="$all $a"
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
    echo "[{\"Id\":\"sha256:$ID\",\"Config\":{\"ExposedPorts\":{\"3000/tcp\":{}},\"Volumes\":null}}]"
    exit 0;;
  container)
    c=$(sed -n 1p "$d/up_files")
    l=$(sed 1d "$d/up_files")
    prj=$(cat $l | grep -m1 '"dev.ship.project": "prj_' | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(cat $l | grep -m1 '"dev.ship.environment": "env_' | sed 's/.*: "\([^"]*\)".*/\1/')
    svc=$(cat $l | grep -m1 '"dev.ship.service": "svc_' | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -m1 '"image"' "$c" | sed 's/.*:\(rel_[a-z0-9]*\)".*/\1/')
    printf '[{"Id":"%s","Name":"/ship-web-1","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"web"}},"State":{"Status":"running","ExitCode":0,"OOMKilled":false}}]\n' "$ID" "$prj" "$env" "$svc" "$rel"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config) cat "$f1"; exit 0;;
  build) echo "#1 building web"; exit 0;;
  up)
    printf '%s\n' $all > "$d/up_files"
    echo "Container ship-web-1 Started"
    exit 0;;
esac
exit 0
"##;

/// The child's scenario; does nothing unless run as the child.
#[test]
fn child_scenario() {
    let Some(dir) = std::env::var_os(DIR_ENV).filter(|_| std::env::var_os(CHILD_ENV).is_some())
    else {
        return;
    };
    let dir = PathBuf::from(dir);
    let redactor = Redactor::new();
    redactor.register(&SecretValue::new(REGISTERED));
    ship_daemon::log::init(redactor.clone()).unwrap();

    let fake = dir.join("fake");
    fs::create_dir_all(&fake).unwrap();
    let script = dir.join("docker");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(&script, SCRIPT).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut config = common::config(&dir);
    config.docker = Runner::new(&script).env("FAKE_DIR", &fake);
    config.deploy = DeploySettings {
        settle: Duration::from_millis(200),
        verify_timeout: Duration::from_secs(5),
        step_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(50),
    };
    config.tailnet = Arc::new(StaticTailnet(None));
    config.redactor = redactor;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let running = ship_daemon::start(&config).await.unwrap();
        let s = Scenario {
            target: Target::Local {
                socket: running.socket_path().to_path_buf(),
            },
            store: running.store().clone(),
            responses: dir.join(RESPONSES),
            ops: std::sync::Mutex::new(Vec::new()),
        };
        // Logged on purpose through the real logger: redacted.
        tracing::warn!(value = REGISTERED, "invariant scenario logs {}", REGISTERED);
        let revision = s.upload().await;

        s.operation(
            "/api/v1/projects/app/secrets",
            "s1",
            json!({ "name": "TOKEN", "value": STORED }),
        )
        .await;
        s.operation(
            "/api/v1/projects/app/vars",
            "v1",
            json!({ "name": "LEVEL", "value": "plain" }),
        )
        .await;
        tracing::info!(token = STORED, "invariant scenario logs {}", STORED);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        s.operation(
            "/api/v1/projects/app/deployments",
            "d1",
            json!({ "revision_id": revision, "port": port.to_string() }),
        )
        .await;
        for (action, key) in [
            ("redeploy", "r1"),
            ("restart", "r2"),
            ("stop", "r3"),
            ("start", "r4"),
        ] {
            s.operation(&format!("/api/v1/projects/app/{action}"), key, json!({}))
                .await;
        }
        for path in [
            "/api/v1/host",
            "/api/v1/projects",
            "/api/v1/projects/app/status",
            "/api/v1/projects/app/deployments",
            "/api/v1/projects/app/vars",
            "/api/v1/projects/app/secrets",
        ] {
            s.get(path).await;
        }
        // Every event of every operation, as the event stream sends them.
        let ops = s.ops.lock().unwrap().clone();
        assert_eq!(ops.len(), 7);
        for id in ops {
            for e in s.store.events_after(id, 0).await.unwrap() {
                s.record(&e.message);
            }
        }
        running.shutdown().await.unwrap();
    });
}

struct Scenario {
    target: Target,
    store: Store,
    responses: PathBuf,
    /// Every operation the scenario submitted.
    ops: std::sync::Mutex<Vec<OperationId>>,
}

impl Scenario {
    fn record(&self, text: &str) {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.responses)
            .unwrap();
        writeln!(f, "{text}").unwrap();
    }

    async fn get(&self, path: &str) -> Value {
        let r = client::get(&self.target, path, TIMEOUT).await.unwrap();
        let body = String::from_utf8(r.body.to_vec()).unwrap();
        self.record(&body);
        assert_eq!(r.status, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Submits a mutation and waits for it to succeed, recording the
    /// acceptance and the operation's view.
    async fn operation(&self, path: &str, key: &str, body: Value) {
        let headers = [
            ("content-type", "application/json"),
            ("idempotency-key", key),
        ];
        let r = client::post(
            &self.target,
            path,
            &headers,
            RequestBody::bytes(body.to_string().into_bytes()),
            TIMEOUT,
        )
        .await
        .unwrap();
        let text = String::from_utf8(r.body.to_vec()).unwrap();
        self.record(&text);
        assert_eq!(r.status, 202, "{path}: {text}");
        let id: OperationId = serde_json::from_str::<Value>(&text).unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        self.ops.lock().unwrap().push(id);
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let op = self.store.get_operation(id).await.unwrap();
            if op.phase.is_terminal() {
                assert_eq!(op.phase, OperationPhase::Succeeded, "{path}: {op:?}");
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{op:?}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.get(&format!("/api/v1/operations/{id}")).await;
    }

    async fn upload(&self) -> String {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
        fs::write(root.path().join("index.html"), b"invariants").unwrap();
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
            &self.target,
            "/api/v1/projects/app/uploads",
            &headers,
            RequestBody::bytes(stream),
            TIMEOUT,
        )
        .await
        .unwrap();
        let text = String::from_utf8(r.body.to_vec()).unwrap();
        self.record(&text);
        assert!(r.status == 201 || r.status == 200, "{text}");
        serde_json::from_str::<Value>(&text).unwrap()["revision_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

/// One place a secret must not be: a name and its bytes.
type Haystack = (String, Vec<u8>);

/// The names of the haystacks holding any of `secrets`.
fn leaks(haystacks: &[Haystack], secrets: &[&str]) -> Vec<String> {
    haystacks
        .iter()
        .filter(|(_, bytes)| {
            secrets
                .iter()
                .any(|s| bytes.windows(s.len()).any(|w| w == s.as_bytes()))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Every file under `root`, skipping the directories where secrets live
/// by design: the secret store and each release's env files.
fn files(root: &Path, out: &mut Vec<Haystack>) {
    let mut entries: Vec<_> = fs::read_dir(root).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            let name = entry.file_name();
            // `state/secrets`, and `state/releases/<id>/env`.
            let is_store = name == SECRETS_DIR && root.join(ship_store::DATABASE_FILE).exists();
            let in_release = root
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == RELEASES_DIR);
            let is_env = name == ENV_DIR && in_release;
            if !is_store && !is_env {
                files(&path, out);
            }
        } else if kind.is_file() {
            out.push((path.display().to_string(), fs::read(&path).unwrap()));
        }
    }
}

/// The env files of every release: where the stored secret must be.
fn env_files(state: &Path) -> Vec<Haystack> {
    let mut out = Vec::new();
    for release in fs::read_dir(state.join(RELEASES_DIR)).unwrap() {
        let env = release.unwrap().path().join(ENV_DIR);
        if env.is_dir() {
            for f in fs::read_dir(env).unwrap() {
                let path = f.unwrap().path();
                out.push((path.display().to_string(), fs::read(&path).unwrap()));
            }
        }
    }
    out
}

/// Runs the scenario in a child; its stdout and stderr, the responses and
/// every file it left, as haystacks.
fn scenario_haystacks() -> (tempfile::TempDir, Vec<Haystack>) {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--exact",
            "child_scenario",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{stderr}\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The scenario ran: the logger wrote, and it redacted.
    assert!(stderr.contains("invariant scenario logs"), "{stderr}");
    assert!(stderr.contains("[redacted]"), "{stderr}");
    let mut haystacks = vec![
        ("child stderr".to_owned(), out.stderr),
        ("child stdout".to_owned(), out.stdout),
    ];
    files(dir.path(), &mut haystacks);
    (dir, haystacks)
}

#[test]
fn no_test_secret_appears_anywhere_in_a_full_scenario() {
    let (dir, haystacks) = scenario_haystacks();
    let state = dir.path().join("state");
    let names: Vec<&str> = haystacks.iter().map(|(n, _)| n.as_str()).collect();
    // What was scanned covers each kind of place.
    for wanted in [
        RESPONSES,
        "/fake/log",
        ship_store::DATABASE_FILE,
        "/operations/",
        "/compose.yaml",
        "/labels.yaml",
    ] {
        assert!(
            names.iter().any(|n| n.contains(wanted)),
            "nothing scanned matches {wanted}: {names:?}"
        );
    }
    // The stored secret did reach the deployment, so its absence elsewhere
    // means something.
    assert!(
        !leaks(&env_files(&state), &[STORED]).is_empty(),
        "the stored secret is in no env file"
    );
    let found = leaks(&haystacks, &SECRETS);
    assert!(found.is_empty(), "a test secret leaked into: {found:?}");
}

#[test]
fn the_scan_fails_on_a_deliberate_leak_of_each_kind() {
    let (dir, haystacks) = scenario_haystacks();
    assert!(leaks(&haystacks, &SECRETS).is_empty());

    // Into the log, a response, an event: each found.
    for (name, _) in haystacks
        .iter()
        .filter(|(n, _)| n == "child stderr" || n.ends_with(RESPONSES))
    {
        let mut leaked = haystacks.clone();
        let target = leaked.iter_mut().find(|(n, _)| n == name).unwrap();
        target.1.extend_from_slice(STORED.as_bytes());
        assert_eq!(leaks(&leaked, &SECRETS), std::slice::from_ref(name));
    }

    // Into a generated Compose file on disk: found by the same walk.
    let state = dir.path().join("state");
    let release = fs::read_dir(state.join(RELEASES_DIR))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let compose = release.join("compose.yaml");
    let mut text = fs::read(&compose).unwrap();
    text.extend_from_slice(format!("\n# {REGISTERED}\n").as_bytes());
    fs::write(&compose, text).unwrap();
    let mut again = Vec::new();
    files(dir.path(), &mut again);
    assert_eq!(leaks(&again, &SECRETS), [compose.display().to_string()]);
}
