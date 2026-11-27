//! Config values and write-only secrets (phase-1-ws-1.10) through the
//! daemon's API, against a fake `docker` that records every invocation:
//! the API's refusals, set, list and unset at every scope, the secret
//! store's files, the env files a deployment hands Compose with the
//! precedence of their scopes, stale services, pruning, redaction after a
//! restart, and one scenario that greps everything the daemon produced,
//! its stderr included, for the test secret. The same against the real
//! Engine is in `real_config.rs`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{OperationId, OperationPhase, ReleaseId, RevisionId, Slug};
use ship_daemon::config::{CONFIG_FILE, ENV_DIR, STAMPS_FILE};
use ship_daemon::deploy::release_dir;
use ship_daemon::secrets::SECRETS_DIR;
use ship_daemon::{DeploySettings, Redactor};
use ship_docker::Runner;
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(30);
/// The test secret: holds `"`, `\` and `$`, which the env file escapes.
const SECRET: &str = "hunter2-\"cfg\\$secret-5521";
const CHILD_ENV: &str = "SHIP_CONFIG_SCENARIO_CHILD";

/// A fake `docker`, as in `deploy.rs`: every invocation is appended to
/// `$FAKE_DIR/log`; `compose … config` prints the first `--file`; `build`
/// prints two lines and `$FAKE_DIR/build_echo` if it exists; `image
/// inspect` answers an image with no ports or volumes; `up` records its
/// first two files; `ps` and `container inspect` describe one running
/// container labelled from them.
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
    echo "[{\"Id\":\"sha256:$ID\",\"Config\":{\"ExposedPorts\":null,\"Volumes\":null}}]"
    exit 0;;
  container)
    c=$(sed -n 1p "$d/up_files")
    l=$(sed -n 2p "$d/up_files")
    prj=$(grep -m1 '"dev.ship.project": "prj_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    env=$(grep -m1 '"dev.ship.environment": "env_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    svc=$(grep -m1 '"dev.ship.service": "svc_' "$l" | sed 's/.*: "\([^"]*\)".*/\1/')
    rel=$(grep -m1 '"image"' "$c" | sed 's/.*:\(rel_[a-z0-9]*\)".*/\1/')
    printf '[{"Id":"%s","Name":"/ship-web-1","RestartCount":0,"Config":{"Labels":{"dev.ship.project":"%s","dev.ship.environment":"%s","dev.ship.service":"%s","dev.ship.release":"%s","com.docker.compose.service":"web"}},"State":{"Status":"running","ExitCode":0,"OOMKilled":false}}]\n' "$ID" "$prj" "$env" "$svc" "$rel"
    exit 0;;
  compose) ;;
  *) exit 64;;
esac
case "$cmd" in
  config) cat "$f1"; exit 0;;
  build)
    echo "#1 building web"
    if [ -f "$d/build_echo" ]; then cat "$d/build_echo"; echo; fi
    echo "#2 DONE 0.1s" >&2
    exit 0;;
  up)
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
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("docker");
            fs::write(&path, SCRIPT).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

struct Fixture {
    dir: tempfile::TempDir,
    state: PathBuf,
    fake: PathBuf,
    target: Target,
    running: Option<ship_daemon::Running>,
}

fn daemon_config(dir: &Path, fake: &Path, redactor: Redactor) -> ship_daemon::Config {
    let mut config = common::config(dir);
    config.docker = Runner::new(fake_docker()).env("FAKE_DIR", fake);
    config.deploy = DeploySettings {
        settle: Duration::from_millis(100),
        verify_timeout: Duration::from_secs(5),
        step_timeout: Duration::from_secs(30),
        poll: Duration::from_millis(50),
    };
    config.tailnet = std::sync::Arc::new(StaticTailnet(None));
    config.redactor = redactor;
    config
}

impl Fixture {
    async fn new(redactor: Redactor) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        fs::create_dir(&fake).unwrap();
        let config = daemon_config(dir.path(), &fake, redactor);
        let state = config.state_dir.clone();
        let running = ship_daemon::start(&config).await.unwrap();
        let target = Target::Local {
            socket: running.socket_path().to_path_buf(),
        };
        Fixture {
            dir,
            state,
            fake,
            target,
            running: Some(running),
        }
    }

    /// Stops the daemon and starts another on the same state directory
    /// with `redactor`.
    async fn restart(&mut self, redactor: Redactor) {
        self.running.take().unwrap().shutdown().await.unwrap();
        let config = daemon_config(self.dir.path(), &self.fake, redactor);
        self.running = Some(ship_daemon::start(&config).await.unwrap());
    }

    fn store(&self) -> &Store {
        self.running.as_ref().unwrap().store()
    }

    fn control(&self, name: &str, content: &str) {
        fs::write(self.fake.join(name), content).unwrap();
    }

    fn invocations(&self) -> String {
        fs::read_to_string(self.fake.join("log")).unwrap_or_default()
    }
}

struct Answer {
    status: u16,
    body: String,
    json: Value,
}

async fn post(f: &Fixture, path: &str, key: Option<&str>, body: &[u8]) -> Answer {
    let mut headers = vec![("content-type", "application/json")];
    if let Some(key) = key {
        headers.push(("idempotency-key", key));
    }
    let r = client::post(
        &f.target,
        path,
        &headers,
        RequestBody::bytes(body.to_vec()),
        TIMEOUT,
    )
    .await
    .unwrap();
    let body = String::from_utf8(r.body.to_vec()).unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_str(&body).unwrap_or(Value::Null),
        body,
    }
}

async fn get(f: &Fixture, path: &str) -> Answer {
    let r = client::get(&f.target, path, TIMEOUT).await.unwrap();
    let body = String::from_utf8(r.body.to_vec()).unwrap();
    Answer {
        status: r.status,
        json: serde_json::from_str(&body).unwrap_or(Value::Null),
        body,
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

/// Every answer and event body seen, for the final grep.
#[derive(Default)]
struct Seen(Vec<String>);

impl Seen {
    fn add(&mut self, text: &str) {
        self.0.push(text.to_owned());
    }
}

/// POSTs a change under a new key and waits for its `config` operation.
async fn change(f: &Fixture, seen: &mut Seen, path: &str, body: Value) -> Operation {
    let key = format!("k-{}", seen.0.len());
    let a = post(f, path, Some(&key), body.to_string().as_bytes()).await;
    seen.add(&a.body);
    assert_eq!(a.status, 202, "{}", a.body);
    assert_eq!(a.json["kind"], "config");
    let id: OperationId = a.json["id"].as_str().unwrap().parse().unwrap();
    let op = finished(f.store(), id).await;
    let view = get(f, &format!("/api/v1/operations/{id}")).await;
    seen.add(&view.body);
    op
}

async fn succeeded(f: &Fixture, seen: &mut Seen, path: &str, body: Value) {
    let op = change(f, seen, path, body).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
}

async fn list(f: &Fixture, seen: &mut Seen, path: &str) -> Value {
    let a = get(f, path).await;
    seen.add(&a.body);
    assert_eq!(a.status, 200, "{}", a.body);
    a.json
}

async fn upload(f: &Fixture, project: &str) -> RevisionId {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
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

/// Deploys `revision` and returns the operation and its release.
async fn deploy(
    f: &Fixture,
    seen: &mut Seen,
    revision: RevisionId,
    key: &str,
) -> (Operation, ReleaseId) {
    let body = json!({ "revision_id": revision.to_string() }).to_string();
    let path = "/api/v1/projects/app/deployments";
    let a = post(f, path, Some(key), body.as_bytes()).await;
    seen.add(&a.body);
    assert_eq!(a.status, 202, "{}", a.body);
    let id: OperationId = a.json["id"].as_str().unwrap().parse().unwrap();
    let op = finished(f.store(), id).await;
    assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
    let d = f.store().deployment_for_operation(id).await.unwrap();
    (op, d.unwrap().release_id.unwrap())
}

/// The operation's event messages.
async fn messages(f: &Fixture, op: OperationId) -> Vec<String> {
    let events = f.store().events_after(op, 0).await.unwrap();
    events.into_iter().map(|e| e.message).collect()
}

/// The body setting `name` to `value`.
fn set(name: &str, value: &str) -> Value {
    json!({ "name": name, "value": value })
}

/// The body naming `name` only.
fn name(name: &str) -> Value {
    json!({ "name": name })
}

/// `body` with `key` (`env` or `service`) set to `scope`.
fn at(mut body: Value, key: &str, scope: &str) -> Value {
    body[key] = json!(scope);
    body
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Every file under `dir`, recursively.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for path in files_in(dir) {
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            walk(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_are_checked_before_any_operation() {
    let f = Fixture::new(Redactor::new()).await;
    f.store()
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap();
    let cases: &[(&str, Value, u16)] = &[
        (
            "/api/v1/projects/nope/vars",
            json!({ "name": "A", "value": "v" }),
            404,
        ),
        (
            "/api/v1/projects/app/vars",
            json!({ "name": "1A", "value": "v" }),
            400,
        ),
        (
            "/api/v1/projects/app/vars",
            json!({ "name": "A", "value": "two\nlines" }),
            400,
        ),
        (
            "/api/v1/projects/app/vars",
            json!({ "name": "A", "value": "v", "env": "staging" }),
            404,
        ),
        (
            "/api/v1/projects/app/vars",
            json!({ "name": "A", "value": "v", "service": "web" }),
            404,
        ),
        (
            "/api/v1/projects/app/secrets",
            json!({ "name": "A", "value": "hunter2\nwrapped" }),
            400,
        ),
        (
            "/api/v1/projects/app/secrets",
            json!({ "name": "A", "value": "hun" }),
            400,
        ),
        (
            "/api/v1/projects/app/secrets",
            json!({ "name": "A=hunter2", "value": "hunter2-long" }),
            400,
        ),
        (
            "/api/v1/projects/app/secrets",
            json!({ "name": "A", "value": "hunter2-long\\" }),
            400,
        ),
        (
            "/api/v1/projects/app/vars/unset",
            json!({ "name": "-" }),
            400,
        ),
    ];
    for (path, body, status) in cases {
        let a = post(&f, path, Some("key"), body.to_string().as_bytes()).await;
        assert_eq!(a.status, *status, "{path} {body}: {}", a.body);
        assert!(a.json["operation_id"].is_null(), "{}", a.body);
        if path.contains("secrets") {
            assert!(!a.body.contains("hunter2"), "{}", a.body);
        }
    }
    // A malformed secret body is refused without quoting it.
    for bad in [
        r#"{"name": "A", "value": 7, "x": "hunter2-in-body"}"#,
        r#"{"name": "A", "value": "hunter2-in-body", "env": 3}"#,
        r#"{"name": "A", "value": "hunter2-in-body""#,
    ] {
        let path = "/api/v1/projects/app/secrets";
        let a = post(&f, path, Some("k"), bad.as_bytes()).await;
        assert_eq!(a.status, 400, "{}", a.body);
        assert!(!a.body.contains("hunter2"), "{}", a.body);
    }
    // No idempotency key: refused too.
    let a = post(
        &f,
        "/api/v1/projects/app/vars",
        None,
        json!({ "name": "A", "value": "v" }).to_string().as_bytes(),
    )
    .await;
    assert_eq!(a.status, 400, "{}", a.body);
    // Lists name an unknown scope 404 and a malformed query 400.
    for (path, status) in [
        ("/api/v1/projects/app/vars?env=staging", 404),
        ("/api/v1/projects/app/secrets?x=1", 400),
        ("/api/v1/projects/nope/secrets", 404),
    ] {
        assert_eq!(get(&f, path).await.status, status, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn values_and_secrets_are_set_listed_and_unset_with_files_kept_private() {
    let f = Fixture::new(Redactor::new()).await;
    let mut seen = Seen::default();
    f.store()
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap();
    let vars = "/api/v1/projects/app/vars";
    let secrets = "/api/v1/projects/app/secrets";

    succeeded(&f, &mut seen, vars, set("LEVEL", "p")).await;
    let e = at(set("LEVEL", "e"), "env", "development");
    succeeded(&f, &mut seen, vars, e).await;
    let project = list(&f, &mut seen, vars).await;
    assert_eq!(project["scope"], "project");
    assert_eq!(project["vars"][0]["name"], "LEVEL");
    assert_eq!(project["vars"][0]["value"], "p");
    assert_eq!(project["stale_services"], json!([]));
    let env = list(&f, &mut seen, &format!("{vars}?env=development")).await;
    assert_eq!(env["scope"], "environment");
    assert_eq!(env["environment"], "development");
    assert_eq!(env["vars"][0]["value"], "e");

    succeeded(&f, &mut seen, secrets, set("TOKEN", SECRET)).await;
    succeeded(&f, &mut seen, secrets, set("TOKEN", SECRET)).await;
    let listed = list(&f, &mut seen, secrets).await;
    assert_eq!(listed["secrets"][0]["name"], "TOKEN");
    assert!(listed["secrets"][0]["updated_at"].is_string());
    assert!(listed["secrets"][0].get("value").is_none(), "{listed}");

    // One file per value, 0600 in a 0700 directory; the replaced one gone.
    let store_dir = f.state.join(SECRETS_DIR);
    assert_eq!(mode(&store_dir), 0o700);
    let kept = files_in(&store_dir);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(mode(&kept[0]), 0o600);
    assert_eq!(fs::read_to_string(&kept[0]).unwrap(), SECRET);

    // A config value and a secret cannot share a name in one scope.
    let op = change(&f, &mut seen, vars, set("TOKEN", "v")).await;
    assert_eq!(op.phase, OperationPhase::Failed);
    assert_eq!(op.failure_code.as_deref(), Some("conflict"), "{op:?}");

    // The audit log names the change, never the value.
    let audit = f.store().list_audit_events().await.unwrap();
    let set = audit.iter().filter(|e| e.action == "secrets set").count();
    assert_eq!(set, 2);
    for e in &audit {
        let detail = e.detail.clone().unwrap_or_default();
        assert!(!detail.contains("hunter2"), "{detail}");
    }

    let unset_secret = "/api/v1/projects/app/secrets/unset";
    let unset_var = "/api/v1/projects/app/vars/unset";
    succeeded(&f, &mut seen, unset_secret, name("TOKEN")).await;
    assert!(files_in(&store_dir).is_empty());
    assert_eq!(list(&f, &mut seen, secrets).await["secrets"], json!([]));
    succeeded(&f, &mut seen, unset_var, name("LEVEL")).await;
    // Unsetting what is not set succeeds and says so.
    let op = change(&f, &mut seen, unset_var, name("LEVEL")).await;
    assert_eq!(op.phase, OperationPhase::Succeeded);
    assert_eq!(list(&f, &mut seen, vars).await["vars"], json!([]));

    for body in &seen.0 {
        assert!(!body.contains("hunter2"), "{body}");
    }
}

/// The whole path, run in a child process with the daemon's real stderr
/// logger installed ([`the_secret_appears_nowhere`] reads its stderr).
#[test]
fn scenario_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let redactor = Redactor::new();
    ship_daemon::log::init(redactor.clone()).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario(redactor));
}

#[test]
fn the_secret_appears_nowhere() {
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--exact",
            "scenario_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
    assert!(stderr.contains("daemon listening"), "{stderr}");
    assert!(!stderr.contains("hunter2"), "{stderr}");
    assert!(!stderr.contains("cfg\\$secret"), "{stderr}");
}

async fn scenario(redactor: Redactor) {
    let mut f = Fixture::new(redactor).await;
    let mut seen = Seen::default();
    let vars = "/api/v1/projects/app/vars";
    let secrets = "/api/v1/projects/app/secrets";
    let revision = upload(&f, "app").await;

    // Project and environment values, then a first deployment.
    succeeded(&f, &mut seen, vars, set("LEVEL", "project")).await;
    succeeded(&f, &mut seen, vars, set("ONLY_PROJECT", "1")).await;
    let e = at(set("LEVEL", "environment"), "env", "development");
    succeeded(&f, &mut seen, vars, e).await;
    let (_, r1) = deploy(&f, &mut seen, revision, "d1").await;
    let dir1 = release_dir(&f.state, r1);
    let env1 = fs::read_to_string(dir1.join(ENV_DIR).join("web.env")).unwrap();
    assert_eq!(env1, "LEVEL=\"environment\"\nONLY_PROJECT=\"1\"\n");
    let stale = list(&f, &mut seen, vars).await["stale_services"].clone();
    assert_eq!(stale, json!([]));

    // A secret and a service value: nothing running changes, and web is
    // stale until the next deployment.
    succeeded(&f, &mut seen, secrets, set("TOKEN", SECRET)).await;
    let s = at(set("LEVEL", "service"), "service", "web");
    succeeded(&f, &mut seen, vars, s).await;
    let ups_before = f.invocations().matches("[up]").count();
    let stale = list(&f, &mut seen, vars).await["stale_services"].clone();
    assert_eq!(stale, json!(["web"]));
    let path = format!("{secrets}?service=web");
    let stale = list(&f, &mut seen, &path).await["stale_services"].clone();
    assert_eq!(stale, json!(["web"]));
    assert_eq!(f.invocations().matches("[up]").count(), ups_before);

    // The build prints the secret; the second deployment gets every value.
    f.control("build_echo", &format!("leaked {SECRET} here"));
    let (op2, r2) = deploy(&f, &mut seen, revision, "d2").await;
    let dir2 = release_dir(&f.state, r2);
    let env_dir = dir2.join(ENV_DIR);
    let env_file = env_dir.join("web.env");
    assert_eq!(mode(&env_dir), 0o700);
    assert_eq!(mode(&env_file), 0o600);
    assert_eq!(
        fs::read_to_string(&env_file).unwrap(),
        "LEVEL=\"service\"\nONLY_PROJECT=\"1\"\nTOKEN=\"hunter2-\\\"cfg\\\\\\$secret-5521\"\n"
    );
    let override_text = fs::read_to_string(dir2.join(CONFIG_FILE)).unwrap();
    let doc: Value = serde_json::from_str(&override_text).unwrap();
    assert_eq!(
        doc,
        json!({ "services": { "web": { "env_file": [env_file.to_str().unwrap()] } } })
    );
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(dir2.join("files.json")).unwrap()).unwrap();
    let listed = manifest["files"].as_array().unwrap();
    let last = dir2.join(CONFIG_FILE);
    assert_eq!(listed.last().unwrap(), last.to_str().unwrap());
    // `up` is given the override last; the build is not.
    let calls = f.invocations();
    let up = calls.lines().rfind(|l| l.contains("[up]")).unwrap();
    let named = format!("[{}]", last.display());
    assert!(up.contains(&named), "{up}");
    let build = calls.lines().rfind(|l| l.contains("[build]")).unwrap();
    assert!(!build.contains(CONFIG_FILE), "{build}");
    let said = messages(&f, op2.id).await;
    let gets = "web gets config LEVEL, ONLY_PROJECT; secrets TOKEN";
    assert!(said.iter().any(|m| m == gets), "{said:?}");
    let leaked = "leaked [redacted] here";
    assert!(said.iter().any(|m| m == leaked), "{said:?}");
    let stale = list(&f, &mut seen, vars).await["stale_services"].clone();
    assert_eq!(stale, json!([]));
    let r1_fp = f.store().get_release(r1).await.unwrap().config_fingerprint;
    let r2_fp = f.store().get_release(r2).await.unwrap().config_fingerprint;
    assert_ne!(r1_fp, r2_fp);

    // A third deployment: the first release's values are pruned, the
    // serving and previous ones kept.
    let (_, r3) = deploy(&f, &mut seen, revision, "d3").await;
    assert!(!dir1.join(ENV_DIR).exists());
    assert!(dir2.join(ENV_DIR).join("web.env").exists());
    let dir3 = release_dir(&f.state, r3);
    assert!(dir3.join(ENV_DIR).join("web.env").exists());

    // After a restart with a redactor that knows nothing, the stored
    // secret is still redacted.
    f.restart(Redactor::new()).await;
    let (op4, _) = deploy(&f, &mut seen, revision, "d4").await;
    let said = messages(&f, op4.id).await;
    assert!(said.iter().any(|m| m == leaked), "{said:?}");
    let view = get(&f, &format!("/api/v1/operations/{}/deployment", op4.id)).await;
    seen.add(&view.body);
    let stamps = fs::read_to_string(dir2.join(STAMPS_FILE)).unwrap();
    assert!(stamps.contains("\"web\""), "{stamps}");

    // The grep: every answer, every event, every file the daemon wrote
    // outside the secret store and the env files (the database and its
    // WAL, operation logs, generated Compose files, overrides, stamps,
    // snapshots), and every argument the fake `docker` was given.
    for body in &seen.0 {
        assert!(!body.contains("hunter2"), "{body}");
    }
    let mut files = Vec::new();
    walk(&f.state, &mut files);
    let secret_store = f.state.join(SECRETS_DIR);
    let mut checked = 0;
    for path in &files {
        if path.starts_with(&secret_store)
            || path.parent().and_then(Path::file_name) == Some(ENV_DIR.as_ref())
        {
            continue;
        }
        let bytes = fs::read(path).unwrap();
        for needle in [SECRET, "hunter2"] {
            assert!(!contains(&bytes, needle), "{needle} in {}", path.display());
        }
        checked += 1;
    }
    assert!(checked > 10, "{files:?}");
    assert!(!f.invocations().contains("hunter2"));
    f.running.take().unwrap().shutdown().await.unwrap();
}
