//! `ship settings get|set publish.address` from the built binary against a
//! daemon running in-process on a temporary socket.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SHIP: &str = env!("CARGO_BIN_EXE_ship");

struct Daemon {
    rt: tokio::runtime::Runtime,
    running: Option<ship_daemon::Running>,
    socket: PathBuf,
}

impl Daemon {
    fn start(dir: &Path) -> Daemon {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let config = ship_daemon::Config {
            state_dir: dir.join("state"),
            socket_path: dir.join("ship.sock"),
            port: 0,
            docker: ship_docker::Runner::docker()
                .env("DOCKER_HOST", "unix:///nonexistent/ship-test/docker.sock"),
            limits: ship_daemon::Limits {
                disk_watermark_bytes: 0,
                ..ship_daemon::Limits::default()
            },
            tailnet: std::sync::Arc::new(ship_net::StaticTailnet(None)),
            ..ship_daemon::Config::new(ship_daemon::Redactor::new())
        };
        let running = rt.block_on(ship_daemon::start(&config)).unwrap();
        let socket = running.socket_path().to_path_buf();
        Daemon {
            rt,
            running: Some(running),
            socket,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o660));
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(SHIP)
            .env(
                "SHIP_CONFIG",
                self.dir.path().join("cfg").join("config.json"),
            )
            .args(args)
            .output()
            .unwrap()
    }
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

#[test]
fn get_and_set_publish_address() {
    let env = Env::new();
    let daemon = Daemon::start(env.dir.path());
    let socket = daemon.socket.display().to_string();
    let out = env.run(&["target", "add", "here", "local", "--socket", &socket]);
    assert!(out.status.success(), "{}", show(&out));

    let out = env.run(&["settings", "get", "publish.address"]);
    assert!(out.status.success(), "{}", show(&out));
    assert!(
        stdout(&out).starts_with("publish.address: auto (the default;"),
        "{}",
        show(&out)
    );
    let out = env.run(&["settings", "get", "publish.address", "-o", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "key": "publish.address", "value": "auto", "updated_at": null })
    );

    let out = env.run(&["settings", "set", "publish.address", "127.0.0.1"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(stdout(&out), "publish.address: 127.0.0.1\n");
    let out = env.run(&["settings", "get", "publish.address", "-o", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["value"], "127.0.0.1");
    assert!(json["updated_at"].is_string());

    // Not an address at all: a usage error, nothing sent.
    let out = env.run(&["settings", "set", "publish.address", "localhost"]);
    assert_eq!(out.status.code(), Some(2), "{}", show(&out));
    // An address that is not the host's: refused by the daemon.
    let out = env.run(&["settings", "set", "publish.address", "192.0.2.1"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not an address of this host"), "{err}");
    // An unknown key is clap's usage error.
    let out = env.run(&["settings", "get", "compose.nope"]);
    assert_eq!(out.status.code(), Some(2), "{}", show(&out));

    let out = env.run(&["settings", "set", "publish.address", "auto"]);
    assert!(out.status.success(), "{}", show(&out));
    let out = env.run(&["settings", "get", "publish.address"]);
    assert!(
        stdout(&out).starts_with("publish.address: auto (set "),
        "{}",
        show(&out)
    );
}
