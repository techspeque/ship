//! The daemon killed at each phase of a deployment (phase-1-ws-1.14),
//! against a fake `docker` that can hold a command open so the kill lands
//! in the phase wanted. The closest an in-process test gets to `kill -9`:
//! the runtime is dropped under the running deployment, with no shutdown
//! and no cleanup, and a new daemon starts on the same state directory.
//!
//! In `building` and `preparing` nothing in Docker changed: the previous
//! release still serves and `ship status` reports nothing to put right. In
//! `deploying` and `verifying` the new containers have already replaced
//! the old (Human, 2026-11-28): the new daemon takes no Docker action,
//! `ship status` reports what Docker runs and names `ship redeploy`, which
//! brings a good release back. The same against the real Engine is in
//! `real_recovery.rs`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use ship_core::{DeploymentResult, OperationId, OperationPhase, OperationStatus, ReleaseId, Slug};
use ship_daemon::config::ENV_DIR;
use ship_daemon::deploy::{OPERATION_FILE, RELEASES_DIR, release_dir};
use ship_daemon::recovery::TRASH_DIR;
use ship_daemon::{Config, DeploySettings};
use ship_docker::Runner;
use ship_net::client::{self, RequestBody};
use ship_net::{StaticTailnet, Target};
use ship_source::{Options, pack, scan};
use ship_store::{Deployment, Operation, Store};

const TIMEOUT: Duration = Duration::from_secs(30);
const SECRET: &str = "hunter2-kill-secret";

/// A fake `docker`. Every invocation is appended to `$FAKE_DIR/log` as
/// one line of `[arg]`s before anything else. It answers as `deploy.rs`'s
/// fake does, and holds one command open while `$FAKE_DIR/hang_<what>`
/// exists, having touched `$FAKE_DIR/at_<what>`:
/// - `build`: `compose … build`, before building;
/// - `image`: `image inspect`, before answering;
/// - `up`: `compose … up`, after recording the files brought up (the new
///   containers have replaced the old);
/// - `container`: `container inspect`, before answering (verifying).
const SCRIPT: &str = r##"#!/bin/sh
d="$FAKE_DIR"
ID=4f9a1c0d2e3b4a5968778695a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$d/log"
hold() {
  if [ -f "$d/hang_$1" ]; then
    touch "$d/at_$1"
    while [ -f "$d/hang_$1" ]; do sleep 0.05; done
  fi
}
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
    hold image
    echo "[{\"Id\":\"sha256:$ID\",\"Config\":{\"ExposedPorts\":{\"3000/tcp\":{}},\"Volumes\":null}}]"
    exit 0;;
  container)
    hold container
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
  build)
    hold build
    echo "#1 building web"
    exit 0;;
  up)
    printf '%s\n' $all > "$d/up_files"
    echo "Container ship-web-1 Started"
    hold up
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

/// A state directory, the fake's control directory and the daemon config
/// over them, which every daemon of a test shares.
struct Host {
    _dir: tempfile::TempDir,
    fake: PathBuf,
    config: Config,
}

impl Host {
    fn new() -> Host {
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
        Host {
            _dir: dir,
            fake,
            config,
        }
    }

    fn state(&self) -> &Path {
        &self.config.state_dir
    }

    fn control(&self, name: &str) {
        fs::write(self.fake.join(name), "").unwrap();
    }

    fn clear(&self, name: &str) {
        let _ = fs::remove_file(self.fake.join(name));
    }

    fn exists(&self, name: &str) -> bool {
        self.fake.join(name).exists()
    }

    /// The fake's invocations, oldest first.
    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.fake.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The release of the files the fake last brought up: what Docker runs.
    fn running_release(&self) -> ReleaseId {
        let files = fs::read_to_string(self.fake.join("up_files")).unwrap();
        let compose = Path::new(files.lines().next().unwrap());
        let release = compose.parent().unwrap().file_name().unwrap();
        release.to_str().unwrap().parse().unwrap()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn target(running: &ship_daemon::Running) -> Target {
    Target::Local {
        socket: running.socket_path().to_path_buf(),
    }
}

async fn post(t: &Target, path: &str, key: &str, body: Value) -> Value {
    let headers = [
        ("content-type", "application/json"),
        ("idempotency-key", key),
    ];
    let r = client::post(
        t,
        path,
        &headers,
        RequestBody::bytes(body.to_string().into_bytes()),
        TIMEOUT,
    )
    .await
    .unwrap();
    let json: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(r.status, 202, "{json}");
    json
}

async fn get(t: &Target, path: &str) -> Value {
    let r = client::get(t, path, TIMEOUT).await.unwrap();
    assert_eq!(r.status, 200);
    serde_json::from_slice(&r.body).unwrap()
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

fn op_id(accepted: &Value) -> OperationId {
    accepted["id"].as_str().unwrap().parse().unwrap()
}

async fn upload(t: &Target, marker: &str) -> String {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
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
        t,
        "/api/v1/projects/app/uploads",
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

async fn deployments(store: &Store) -> Vec<Deployment> {
    let env = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment;
    store.list_deployments(env.id).await.unwrap()
}

/// The release directories on disk.
fn release_dirs(state: &Path) -> Vec<ReleaseId> {
    let mut out: Vec<ReleaseId> = fs::read_dir(state.join(RELEASES_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_str().unwrap().parse().unwrap())
        .collect();
    out.sort();
    out
}

/// What a test sees after the kill and the new start.
struct Killed {
    /// `None` once shut down, on drop.
    daemon: Option<ship_daemon::Running>,
    runtime: tokio::runtime::Runtime,
    host: Host,
    /// The release serving before the killed deployment.
    good: ReleaseId,
    good_revision: String,
    killed: OperationId,
    /// The fake's invocations before the new daemon started.
    before_restart: usize,
}

/// Deploys v1 (with a secret, so its release has env files), then starts
/// v2 with the fake holding `hang` open, waits for the operation to be in
/// `phase` with the command held, kills the daemon, and starts a new one.
fn kill_in(phase: OperationPhase, hang: &str) -> Killed {
    let host = Host::new();
    let rt = runtime();
    let (running, good, good_revision, killed) = rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        let t = target(&running);
        let store = running.store();
        store
            .get_or_create_project(&Slug::parse("app").unwrap())
            .await
            .unwrap();
        let a = post(
            &t,
            "/api/v1/projects/app/secrets",
            "s1",
            json!({ "name": "TOKEN", "value": SECRET }),
        )
        .await;
        assert_eq!(
            finished(store, op_id(&a)).await.phase,
            OperationPhase::Succeeded
        );
        let v1 = upload(&t, "v1").await;
        let port = free_port();
        let body = json!({ "revision_id": v1, "port": port.to_string() });
        let a = post(&t, "/api/v1/projects/app/deployments", "d1", body).await;
        let op = finished(store, op_id(&a)).await;
        assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
        let good = deployments(store).await[0].release_id.unwrap();
        assert!(
            release_dir(&host.config.state_dir, good)
                .join(ENV_DIR)
                .is_dir()
        );

        let v2 = upload(&t, "v2").await;
        host.control(&format!("hang_{hang}"));
        let body = json!({ "revision_id": v2, "port": port.to_string() });
        let a = post(&t, "/api/v1/projects/app/deployments", "d2", body).await;
        let killed = op_id(&a);
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let op = store.get_operation(killed).await.unwrap();
            if op.phase == phase && host.exists(&format!("at_{hang}")) {
                break;
            }
            assert!(!op.phase.is_terminal(), "{op:?}");
            assert!(tokio::time::Instant::now() < deadline, "{op:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (running, good, v1, killed)
    });
    // Killed: nothing shuts down, nothing is cleaned up.
    rt.shutdown_background();
    std::mem::forget(running);
    let before_restart = host.invocations().len();
    host.clear(&format!("hang_{hang}"));

    let runtime = runtime();
    let running = runtime.block_on(ship_daemon::start(&host.config)).unwrap();
    Killed {
        daemon: Some(running),
        runtime,
        host,
        good,
        good_revision,
        killed,
        before_restart,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Killed {
    fn running(&self) -> &ship_daemon::Running {
        self.daemon.as_ref().expect("running until dropped")
    }

    fn store(&self) -> &Store {
        self.running().store()
    }

    /// `ship status app -o json` from the new daemon.
    fn status(&self) -> Value {
        self.runtime
            .block_on(get(&target(self.running()), "/api/v1/projects/app/status"))
    }

    /// Every Docker command the new daemon ran (or the held one finishing,
    /// which logs nothing more) only read: `ps` and `container inspect`.
    fn assert_only_reads_since_restart(&self) {
        let all = self.host.invocations();
        let since = &all[self.before_restart..];
        assert!(!since.is_empty(), "the new daemon observed nothing");
        for line in since {
            assert!(
                line.starts_with("[ps]") || line.starts_with("[container][inspect]"),
                "a Docker command that is not a read after the restart: {line}\n{since:?}"
            );
        }
    }

    /// The operation and its deployment are `interrupted`, the operation
    /// with the phase it was killed in.
    fn assert_interrupted(&self, phase: OperationPhase) -> Deployment {
        self.runtime.block_on(async {
            let op = self.store().get_operation(self.killed).await.unwrap();
            assert_eq!(op.status, OperationStatus::Interrupted, "{op:?}");
            assert_eq!(op.failure_phase, Some(phase), "{op:?}");
            let d = deployments(self.store())
                .await
                .into_iter()
                .find(|d| d.operation_id == self.killed)
                .unwrap();
            assert_eq!(d.result, Some(DeploymentResult::Interrupted), "{d:?}");
            assert!(d.ended_at.is_some());
            d
        })
    }

    /// Before the new containers started: the previous release is the one
    /// Docker runs and the one reported, with nothing to put right; the
    /// interrupted attempt's release staging is gone, the good one's kept.
    fn assert_previous_serves(&self) {
        assert_eq!(self.host.running_release(), self.good);
        let ups = self
            .host
            .invocations()
            .iter()
            .filter(|l| l.contains("[up]"))
            .count();
        assert_eq!(ups, 1, "only v1 was ever brought up");
        let status = self.status();
        assert_eq!(status["interruption"], Value::Null, "{status}");
        assert_eq!(
            status["last_deployment"]["release_id"],
            self.good.to_string()
        );
        assert_eq!(status["services"][0]["status"], "running_unverified");
        assert_eq!(release_dirs(self.host.state()), [self.good]);
        assert!(
            release_dir(self.host.state(), self.good)
                .join(ENV_DIR)
                .is_dir()
        );
    }

    /// After the new containers started: Docker runs the new release and
    /// status says so, the good release stays the reported one, the
    /// interruption names `ship redeploy app`, and redeploying brings the
    /// good revision back as a new release.
    fn assert_part_way_and_redeploy(&self, phase: &str, interrupted: &Deployment) {
        let new = interrupted.release_id.expect("recorded before deploying");
        assert_eq!(self.host.running_release(), new);
        // The new release keeps its files; its values are gone.
        let dir = release_dir(self.host.state(), new);
        assert!(dir.join(OPERATION_FILE).is_file());
        assert!(dir.join("compose.yaml").is_file());
        assert!(!dir.join(ENV_DIR).exists());
        assert!(
            release_dir(self.host.state(), self.good)
                .join(ENV_DIR)
                .is_dir()
        );

        let status = self.status();
        assert_eq!(
            status["last_deployment"]["release_id"],
            self.good.to_string(),
            "{status}"
        );
        assert_eq!(status["last_deployment"]["result"], "succeeded");
        let i = &status["interruption"];
        assert_eq!(i["deployment_id"], interrupted.id.to_string(), "{status}");
        assert_eq!(i["operation_id"], self.killed.to_string());
        assert_eq!(i["release_id"], new.to_string());
        assert_eq!(i["phase"], phase);
        assert_eq!(i["command"], "ship redeploy app");
        // What Docker runs: the service is running (with no health check).
        assert_eq!(status["services"][0]["status"], "running_unverified");
        self.assert_only_reads_since_restart();

        // `ship redeploy` puts it right.
        let t = target(self.running());
        let op = self.runtime.block_on(async {
            let a = post(&t, "/api/v1/projects/app/redeploy", "r1", json!({})).await;
            finished(self.store(), op_id(&a)).await
        });
        assert_eq!(op.phase, OperationPhase::Succeeded, "{op:?}");
        let status = self.status();
        assert_eq!(status["interruption"], Value::Null, "{status}");
        let latest = self.runtime.block_on(deployments(self.store()))[0].clone();
        assert_eq!(latest.result, Some(DeploymentResult::Succeeded));
        assert_eq!(latest.revision_id.to_string(), self.good_revision);
        let release = latest.release_id.unwrap();
        assert_eq!(self.host.running_release(), release);
        assert_eq!(status["last_deployment"]["release_id"], release.to_string());
    }
}

impl Drop for Killed {
    fn drop(&mut self) {
        // Release anything the fake still holds open.
        for hang in ["build", "image", "up", "container"] {
            self.host.clear(&format!("hang_{hang}"));
        }
        if let Some(running) = self.daemon.take() {
            let _ = self.runtime.block_on(running.shutdown());
        }
    }
}

#[test]
fn killed_while_building_the_previous_release_serves() {
    let k = kill_in(OperationPhase::Building, "build");
    k.assert_interrupted(OperationPhase::Building);
    k.assert_previous_serves();
    k.assert_only_reads_since_restart();
}

#[test]
fn killed_while_preparing_the_previous_release_serves() {
    let k = kill_in(OperationPhase::Preparing, "image");
    let d = k.assert_interrupted(OperationPhase::Preparing);
    assert_eq!(d.release_id, None, "killed before the release was recorded");
    k.assert_previous_serves();
    k.assert_only_reads_since_restart();
}

#[test]
fn killed_while_deploying_status_names_redeploy() {
    let k = kill_in(OperationPhase::Deploying, "up");
    let d = k.assert_interrupted(OperationPhase::Deploying);
    k.assert_part_way_and_redeploy("deploying", &d);
}

#[test]
fn killed_while_verifying_status_names_redeploy() {
    let k = kill_in(OperationPhase::Verifying, "container");
    let d = k.assert_interrupted(OperationPhase::Verifying);
    k.assert_part_way_and_redeploy("verifying", &d);
}

/// A release directory of a deployment that failed (not interrupted) and
/// one created before the operation marker existed are left alone; only
/// the interrupted operation's staging goes.
#[test]
fn only_the_interrupted_operations_staging_is_removed() {
    let host = Host::new();
    let rt = runtime();
    let (running, keep, killed) = rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        let t = target(&running);
        let store = running.store();
        let v1 = upload(&t, "v1").await;
        let a = post(
            &t,
            "/api/v1/projects/app/deployments",
            "d1",
            json!({ "revision_id": v1 }),
        )
        .await;
        assert_eq!(
            finished(store, op_id(&a)).await.phase,
            OperationPhase::Succeeded
        );
        // The unrecorded directory a failed (not interrupted) attempt of
        // another operation left, and one with no marker, as written
        // before the marker existed.
        let releases = host.state().join(RELEASES_DIR);
        let failed = ReleaseId::from_parts(1_767_225_600_000, 7).unwrap();
        fs::create_dir(releases.join(failed.to_string())).unwrap();
        fs::write(
            releases.join(failed.to_string()).join(OPERATION_FILE),
            OperationId::from_parts(1_767_225_600_000, 9)
                .unwrap()
                .to_string(),
        )
        .unwrap();
        let unmarked = ReleaseId::from_parts(1_767_225_600_000, 8).unwrap();
        fs::create_dir(releases.join(unmarked.to_string())).unwrap();

        host.control("hang_build");
        let a = post(
            &t,
            "/api/v1/projects/app/deployments",
            "d2",
            json!({ "revision_id": v1 }),
        )
        .await;
        let killed = op_id(&a);
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !host.exists("at_build") {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let good = deployments(store).await[1].release_id.unwrap();
        (running, vec![good, failed, unmarked], killed)
    });
    rt.shutdown_background();
    std::mem::forget(running);
    host.clear("hang_build");
    let staged: Vec<ReleaseId> = release_dirs(host.state())
        .into_iter()
        .filter(|r| !keep.contains(r))
        .collect();
    assert_eq!(staged.len(), 1, "the killed attempt's own directory");

    let rt = runtime();
    rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        let mut want = keep.clone();
        want.sort();
        assert_eq!(release_dirs(host.state()), want);
        let op = running.store().get_operation(killed).await.unwrap();
        assert_eq!(op.status, OperationStatus::Interrupted);
        running.shutdown().await.unwrap();
    });
}

/// A start that cannot remove the interrupted operation's staging (here
/// the releases directory denies writes) leaves it for the next start,
/// which removes it although the deployment is already `interrupted`
/// (f-051).
#[cfg(unix)]
#[test]
fn staging_a_start_could_not_remove_is_removed_by_the_next() {
    use std::os::unix::fs::PermissionsExt;

    let host = Host::new();
    let rt = runtime();
    let (running, good) = rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        let t = target(&running);
        let store = running.store();
        let v1 = upload(&t, "v1").await;
        let a = post(
            &t,
            "/api/v1/projects/app/deployments",
            "d1",
            json!({ "revision_id": v1 }),
        )
        .await;
        assert_eq!(
            finished(store, op_id(&a)).await.phase,
            OperationPhase::Succeeded
        );
        host.control("hang_build");
        post(
            &t,
            "/api/v1/projects/app/deployments",
            "d2",
            json!({ "revision_id": v1 }),
        )
        .await;
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !host.exists("at_build") {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let good = deployments(store).await[1].release_id.unwrap();
        (running, good)
    });
    rt.shutdown_background();
    std::mem::forget(running);
    host.clear("hang_build");
    let mut left: Vec<ReleaseId> = release_dirs(host.state());
    assert_eq!(left.len(), 2, "the good release and the killed attempt's");
    left.sort();

    let releases = host.state().join(RELEASES_DIR);
    let set_mode = |mode| fs::set_permissions(&releases, fs::Permissions::from_mode(mode)).unwrap();
    set_mode(0o555);
    if fs::create_dir(releases.join("probe")).is_ok() {
        // Permissions do not bind this user (root): nothing to show.
        fs::remove_dir(releases.join("probe")).unwrap();
        set_mode(0o755);
        return;
    }
    let rt = runtime();
    rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        assert!(
            deployments(running.store())
                .await
                .iter()
                .any(|d| d.result == Some(DeploymentResult::Interrupted))
        );
        running.shutdown().await.unwrap();
    });
    set_mode(0o755);
    assert_eq!(
        release_dirs(host.state()),
        left,
        "the failed removal kept it"
    );

    let rt = runtime();
    rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        assert_eq!(release_dirs(host.state()), vec![good]);
        running.shutdown().await.unwrap();
    });
}

/// Deploys v1, then kills the daemon while it builds v2, leaving v2's
/// unrecorded, marked release directory on disk; the good release and
/// that directory.
fn kill_while_building(host: &Host) -> (ReleaseId, PathBuf) {
    let rt = runtime();
    let (running, good) = rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        let t = target(&running);
        let store = running.store();
        let v1 = upload(&t, "v1").await;
        let a = post(
            &t,
            "/api/v1/projects/app/deployments",
            "d1",
            json!({ "revision_id": v1 }),
        )
        .await;
        assert_eq!(
            finished(store, op_id(&a)).await.phase,
            OperationPhase::Succeeded
        );
        host.control("hang_build");
        post(
            &t,
            "/api/v1/projects/app/deployments",
            "d2",
            json!({ "revision_id": v1 }),
        )
        .await;
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !host.exists("at_build") {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let good = deployments(store).await[1].release_id.unwrap();
        (running, good)
    });
    rt.shutdown_background();
    std::mem::forget(running);
    host.clear("hang_build");
    let staged: Vec<ReleaseId> = release_dirs(host.state())
        .into_iter()
        .filter(|r| *r != good)
        .collect();
    assert_eq!(staged.len(), 1, "the killed attempt's own directory");
    (good, release_dir(host.state(), staged[0]))
}

/// The entries of the trash directory.
fn trash_entries(state: &Path) -> Vec<PathBuf> {
    match fs::read_dir(state.join(TRASH_DIR)) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Starts a daemon on `host` and shuts it down.
fn start_once(host: &Host) {
    let rt = runtime();
    rt.block_on(async {
        let running = ship_daemon::start(&host.config).await.unwrap();
        running.shutdown().await.unwrap();
    });
}

/// A start whose removal of the renamed staging fails part-way (a
/// directory in it denies writes) still starts and leaves the rest in the
/// trash; the next start empties it (f-051).
#[cfg(unix)]
#[test]
fn staging_whose_removal_failed_in_the_trash_is_removed_by_the_next_start() {
    use std::os::unix::fs::PermissionsExt;

    let host = Host::new();
    let (good, staged) = kill_while_building(&host);
    let locked = staged.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("file"), "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(locked.join("probe"), "x").is_ok() {
        // Permissions do not bind this user (root): nothing to show.
        return;
    }

    start_once(&host);
    assert_eq!(release_dirs(host.state()), vec![good]);
    let left = trash_entries(host.state());
    assert_eq!(
        left.len(),
        1,
        "what could not be removed stays in the trash"
    );
    fs::set_permissions(left[0].join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    start_once(&host);
    assert_eq!(release_dirs(host.state()), vec![good]);
    assert!(trash_entries(host.state()).is_empty());
}
