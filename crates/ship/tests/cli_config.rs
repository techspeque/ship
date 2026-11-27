//! `ship vars` and `ship secrets` from the built binary against a daemon
//! running in-process on a temporary socket, and the refusals of a secret
//! given as an argument before anything is sent.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SHIP: &str = env!("CARGO_BIN_EXE_ship");
const SECRET: &str = "hunter2-cli-secret";

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
        let slug = ship_core::Slug::parse("app").unwrap();
        let store = running.store();
        rt.block_on(store.get_or_create_project(&slug)).unwrap();
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

    /// `ship ARGS --project app`.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(SHIP);
        let config = self.dir.path().join("cfg").join("config.json");
        c.env("SHIP_CONFIG", config).args(args);
        if args[0] != "target" {
            c.args(["--project", "app"]);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// [`Env::run`] with `input` on stdin.
    fn run_with(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
        drop(stdin);
        child.wait_with_output().unwrap()
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

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", show(out)))
}

/// Asserts `out` succeeded and its stdout starts with `prefix`.
fn starts(out: &Output, prefix: &str) {
    let text = show(out);
    assert!(out.status.success(), "{text}");
    assert!(stdout(out).starts_with(prefix), "{text}");
}

/// Asserts `out` exited `code`.
fn exits(out: &Output, code: i32) {
    assert_eq!(out.status.code(), Some(code), "{}", show(out));
}

#[test]
fn a_secret_given_as_an_argument_is_refused_before_anything_is_sent() {
    // No target is configured: a command that tried to connect would fail
    // with exit 1 for that, not 2.
    let env = Env::new();
    let assignment = format!("TOKEN={SECRET}");
    let cases: [Vec<&str>; 4] = [
        vec!["secrets", "set", &assignment, "--stdin"],
        vec!["secrets", "set", "TOKEN", SECRET],
        vec!["secrets", "set", "TOKEN", SECRET, "--stdin"],
        vec!["secrets", "set", &assignment],
    ];
    for args in &cases {
        let out = env.run_with(args, "from-stdin-value\n");
        exits(&out, 2);
        let all = show(&out);
        assert!(!all.contains("hunter2"), "{args:?}: {all}");
        assert!(all.contains("never taken as an argument"), "{all}");
    }
    // Neither --prompt nor --stdin.
    exits(&env.run(&["secrets", "set", "TOKEN"]), 2);
    // A value the rules refuse is refused here too.
    let args = ["secrets", "set", "TOKEN", "--stdin"];
    exits(&env.run_with(&args, "hun\n"), 2);
    exits(&env.run(&["vars", "set", "NOEQUALS"]), 2);
    exits(&env.run(&["vars", "set", "1A=v"]), 2);
}

#[test]
fn vars_and_secrets_are_set_listed_and_unset() {
    let env = Env::new();
    let daemon = Daemon::start(env.dir.path());
    let socket = daemon.socket.display().to_string();
    let out = env.run(&["target", "add", "here", "local", "--socket", &socket]);
    assert!(out.status.success(), "{}", show(&out));

    let out = env.run(&["vars", "set", "LEVEL=a=b c"]);
    starts(&out, "LEVEL is set for the project app.");
    let args = ["vars", "set", "LEVEL=e", "--env", "development"];
    starts(&env.run(&args), "LEVEL is set for the environment");

    starts(&env.run(&["vars", "list"]), "LEVEL=a=b c  (set ");
    let list = json(&env.run(&["vars", "list", "-o", "json"]));
    assert_eq!(list["scope"], "project");
    assert_eq!(list["vars"][0]["value"], "a=b c");
    assert_eq!(list["stale_services"], serde_json::json!([]));
    let args = ["vars", "list", "--env", "development"];
    starts(&env.run(&args), "LEVEL=e  (set ");

    // A service is known only once deployed.
    let args = ["vars", "set", "A=1", "--service", "web"];
    exits(&env.run(&args), 1);

    let args = ["secrets", "set", "TOKEN", "--stdin"];
    let out = env.run_with(&args, &format!("{SECRET}\n"));
    starts(&out, "secret TOKEN is set for the project app.");
    assert!(!show(&out).contains("hunter2"), "{}", show(&out));
    let out = env.run(&["secrets", "list"]);
    starts(&out, "TOKEN  (set ");
    assert!(!show(&out).contains("hunter2"), "{}", show(&out));
    let list = json(&env.run(&["secrets", "list", "-o", "json"]));
    assert_eq!(list["secrets"][0]["name"], "TOKEN");
    assert!(!list.to_string().contains("hunter2"), "{list}");
    // The value the daemon stored is the one sent, less its newline.
    let store = env.dir.path().join("state").join("secrets");
    let files: Vec<_> = fs::read_dir(&store).unwrap().collect();
    assert_eq!(files.len(), 1);
    let path = files[0].as_ref().unwrap().path();
    assert_eq!(fs::read_to_string(path).unwrap(), SECRET);

    // The same name as a config value in the same scope fails.
    let out = env.run(&["vars", "set", "TOKEN=v"]);
    exits(&out, 1);
    assert!(show(&out).contains("conflict"), "{}", show(&out));

    let out = env.run(&["secrets", "unset", "TOKEN"]);
    assert!(out.status.success(), "{}", show(&out));
    let out = env.run(&["secrets", "list"]);
    assert_eq!(stdout(&out), "No secrets set for the project app.\n");
    let out = env.run(&["vars", "unset", "LEVEL"]);
    assert!(out.status.success(), "{}", show(&out));
    let out = env.run(&["vars", "list"]);
    let none = "No config values set for the project app.\n";
    assert_eq!(stdout(&out), none);
    drop(daemon);
}
