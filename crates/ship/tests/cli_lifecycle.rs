//! `ship start|stop|restart|redeploy|remove` and `ship push --recreate`
//! from the built binary (phase-1-ws-1.12), against an in-process daemon
//! whose `docker` is the fake of the push tests (`support/fake-docker.sh`).
//! stdin is not a terminal, so nothing can be confirmed by asking.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ship_docker::Runner;

const SHIP: &str = env!("CARGO_BIN_EXE_ship");
const SCRIPT: &str = include_str!("support/fake-docker.sh");

struct Harness {
    dir: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    running: Option<ship_daemon::Running>,
}

impl Harness {
    fn new() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["fake", "bin", "work"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let docker_path = dir.path().join("bin").join("docker");
        fs::write(&docker_path, SCRIPT).unwrap();
        fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let config = ship_daemon::Config {
            state_dir: dir.path().join("state"),
            socket_path: dir.path().join("ship.sock"),
            port: 0,
            docker: Runner::new(&docker_path).env("FAKE_DIR", dir.path().join("fake")),
            limits: ship_daemon::Limits {
                disk_watermark_bytes: 0,
                ..ship_daemon::Limits::default()
            },
            deploy: ship_daemon::DeploySettings {
                settle: Duration::from_millis(200),
                verify_timeout: Duration::from_secs(5),
                step_timeout: Duration::from_secs(30),
                poll: Duration::from_millis(50),
            },
            tailnet: Arc::new(ship_net::StaticTailnet(None)),
            observe_interval: Duration::from_secs(3600),
            ..ship_daemon::Config::new(ship_daemon::Redactor::new())
        };
        let running = rt.block_on(ship_daemon::start(&config)).unwrap();
        let cfg = json!({
            "current": "t",
            "targets": { "t": { "url": "local", "socket": running.socket_path() } },
        });
        fs::write(dir.path().join("config.json"), cfg.to_string()).unwrap();
        Harness {
            dir,
            rt,
            running: Some(running),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn tree(&self) -> PathBuf {
        let root = self.path().join("work").join("my-app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();
        root
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(SHIP)
            .env("SHIP_CONFIG", self.path().join("config.json"))
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    }

    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("fake").join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn downs(&self) -> usize {
        self.invocations()
            .iter()
            .filter(|l| l.contains("[down]"))
            .count()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = self.rt.block_on(running.shutdown());
        }
    }
}

fn show(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn lifecycle_from_the_command_line() {
    let h = Harness::new();
    let tree = h.tree();
    let tree = tree.to_str().unwrap();
    let out = h.run(&["push", tree]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));

    let out = h.run(&["stop", "my-app/web"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    assert!(
        stdout(&out).ends_with("Stopped my-app/web.\n"),
        "{}",
        show(&out)
    );
    assert!(
        h.invocations()
            .iter()
            .any(|l| l.ends_with("[stop][--][web]")),
        "{:?}",
        h.invocations()
    );
    let out = h.run(&["status", "my-app", "-o", "json"]);
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["services"][0]["desired_state"], "stopped");

    let out = h.run(&["start", "my-app", "-o", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    let op: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(op["kind"], "start");
    assert_eq!(op["status"], "succeeded");

    let out = h.run(&["restart", "my-app"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    assert!(
        stdout(&out).ends_with("Restarted my-app.\n"),
        "{}",
        show(&out)
    );

    // A whole-environment action names no service; nothing is sent.
    let before = h.invocations().len();
    for action in ["redeploy", "remove"] {
        let out = h.run(&[action, "my-app/web"]);
        assert_eq!(out.status.code(), Some(2), "{}", show(&out));
        assert!(stderr(&out).contains("whole environment"), "{}", show(&out));
    }
    // No terminal and no --confirm: exit 3, nothing sent.
    let out = h.run(&["remove", "my-app"]);
    assert_eq!(out.status.code(), Some(3), "{}", show(&out));
    assert!(stderr(&out).contains("--confirm"), "{}", show(&out));
    assert!(stderr(&out).contains("named volume"), "{}", show(&out));
    assert_eq!(h.invocations().len(), before);

    let out = h.run(&["remove", "my-app", "--confirm"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    assert!(
        stdout(&out).ends_with("Removed my-app.\n"),
        "{}",
        show(&out)
    );
    assert_eq!(h.downs(), 1);

    // Removed: the other actions are refused, naming --recreate.
    let out = h.run(&["start", "my-app"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(stderr(&out).contains("--recreate"), "{}", show(&out));

    // A push without --recreate and without a terminal deploys nothing.
    let ups = |h: &Harness| {
        h.invocations()
            .iter()
            .filter(|l| l.contains("[up]"))
            .count()
    };
    let ups_before = ups(&h);
    let out = h.run(&["push", tree]);
    assert_eq!(out.status.code(), Some(3), "{}", show(&out));
    assert!(stderr(&out).contains("--recreate"), "{}", show(&out));
    assert_eq!(ups(&h), ups_before);

    let out = h.run(&["push", tree, "--recreate"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    assert_eq!(ups(&h), ups_before + 1);

    let out = h.run(&["deployments", "my-app", "-o", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows: Vec<(&str, &str)> = list["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["action"].as_str().unwrap(), d["result"].as_str().unwrap()))
        .collect();
    // The push refused for want of --recreate recorded no deployment.
    assert_eq!(
        rows,
        [
            ("deploy", "succeeded"),
            ("remove", "succeeded"),
            ("deploy", "succeeded"),
        ]
    );
    let out = h.run(&["deployments", "my-app"]);
    assert!(stdout(&out).contains("remove"), "{}", show(&out));
    let out = h.run(&["status", "my-app", "-o", "json"]);
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["environment"]["desired_state"], "running");
}

#[test]
fn redeploy_runs_the_serving_revision_again() {
    let h = Harness::new();
    let tree = h.tree();
    let out = h.run(&["push", tree.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    let out = h.run(&["redeploy", "my-app"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    assert!(
        stdout(&out).ends_with("Redeployed my-app.\n"),
        "{}",
        show(&out)
    );
    let out = h.run(&["deployments", "my-app", "-o", "json"]);
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    let deployments = list["deployments"].as_array().unwrap();
    assert_eq!(deployments.len(), 2);
    assert_eq!(deployments[0]["revision_id"], deployments[1]["revision_id"]);
    assert_ne!(deployments[0]["release_id"], deployments[1]["release_id"]);
    // An unknown project is refused by the daemon: exit 1.
    let out = h.run(&["stop", "nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    let out = h.run(&["stop", "My App"]);
    assert_eq!(out.status.code(), Some(2), "{}", show(&out));
}
