//! `ship push` and `ship settings … compose.confirm` from the built binary
//! (phase-1-ws-1.9), against an in-process daemon (`ship_daemon::start`)
//! whose `docker` is a fake playing the Engine's part, so these run on
//! macOS too. Every exit code of `docs/contracts/cli-json.md` is produced
//! here by creating its condition; the `-o json` lines are checked
//! against the push fixtures; a proxy cuts the event stream once to show
//! that a resumed stream loses and repeats nothing; the ssh transport goes
//! through a fake `ssh` running `ship dial-stdio` locally (the real sshd
//! is `ssh_real.rs`).

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ship_docker::Runner;

const SHIP: &str = env!("CARGO_BIN_EXE_ship");
const SUCCEEDED_FIXTURE: &str = include_str!("fixtures/push-wait-succeeded.jsonl");
const FAILED_FIXTURE: &str = include_str!("fixtures/push-wait-failed.jsonl");
const WAIT: Duration = Duration::from_secs(30);

/// The fake `docker`; what it answers is described at its top.
const SCRIPT: &str = include_str!("support/fake-docker.sh");

/// A fake `ssh`: skips its options and the host, then runs the remote
/// `ship …` words with the real binary.
const FAKE_SSH: &str = r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift 2
[ "$1" = ship ] || exit 97
shift
exec "$SHIP_BIN" "$@"
"#;

fn write_script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A daemon in this process, its fake `docker` and a client config.
struct Harness {
    dir: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    running: Option<ship_daemon::Running>,
    docker: Runner,
}

impl Harness {
    fn new() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["fake", "bin", "work"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let docker_path = dir.path().join("bin").join("docker");
        write_script(&docker_path, SCRIPT);
        write_script(&dir.path().join("bin").join("ssh"), FAKE_SSH);
        let docker = Runner::new(&docker_path).env("FAKE_DIR", dir.path().join("fake"));
        Harness::with_docker(dir, docker)
    }

    fn with_docker(dir: tempfile::TempDir, docker: Runner) -> Harness {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut h = Harness {
            dir,
            rt,
            running: None,
            docker,
        };
        h.start();
        h.target("local", None);
        h
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn fake(&self, name: &str) -> PathBuf {
        self.path().join("fake").join(name)
    }

    fn control(&self, name: &str, content: &str) {
        fs::write(self.fake(name), content).unwrap();
    }

    fn socket(&self) -> PathBuf {
        self.path().join("ship.sock")
    }

    fn start(&mut self) {
        let config = ship_daemon::Config {
            state_dir: self.path().join("state"),
            socket_path: self.socket(),
            port: 0,
            docker: self.docker.clone(),
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
            ..ship_daemon::Config::new(ship_daemon::Redactor::new())
        };
        self.running = Some(self.rt.block_on(ship_daemon::start(&config)).unwrap());
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            self.rt.block_on(running.shutdown()).unwrap();
        }
    }

    /// Makes the current target reach `socket` (default: the daemon's)
    /// over `url` (`local` or `ssh://…`).
    fn target(&self, url: &str, socket: Option<&Path>) {
        let socket = socket.map_or_else(|| self.socket(), Path::to_path_buf);
        let cfg = json!({
            "current": "t",
            "targets": { "t": { "url": url, "socket": socket } },
        });
        fs::write(self.config(), cfg.to_string()).unwrap();
    }

    fn config(&self) -> PathBuf {
        self.path().join("config.json")
    }

    /// A working tree `work/<name>` holding `files`.
    fn tree(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = self.path().join("work").join(name);
        for (file, content) in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn ship(&self) -> Command {
        let mut c = Command::new(SHIP);
        let path = format!(
            "{}:{}",
            self.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.env("PATH", path)
            .env("SHIP_CONFIG", self.config())
            .env("SHIP_BIN", SHIP)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.ship().args(args).output().unwrap()
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.ship()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Waits for the fake's file `name`.
    fn wait_for(&self, name: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.fake(name).exists() {
            assert!(Instant::now() < deadline, "{name} never appeared");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Lets a waiting fake build end.
        let _ = fs::write(self.fake("build_go"), "");
        self.stop();
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

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn json_lines(out: &Output) -> Vec<Value> {
    stdout(out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}\n{}", show(out))))
        .collect()
}

/// A value's shape: every string, number and bool replaced by its type,
/// keys kept. Ids, timestamps and messages differ between runs; shapes do
/// not.
fn shape(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
        Value::String(_) => json!("string"),
        Value::Number(_) => json!("number"),
        Value::Bool(_) => json!("bool"),
        Value::Null => Value::Null,
    }
}

fn fixture_lines(text: &str) -> Vec<Value> {
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Event lines have the fixtures' shape and gapless sequences from 1; one
/// result line ends the output, with the fixture's shape.
fn assert_matches_fixture(lines: &[Value], fixture: &str) {
    let expected = fixture_lines(fixture);
    let event_shape = shape(&expected[0]);
    let (result, events) = lines.split_last().expect("some output");
    assert_eq!(result["type"], "result", "{lines:#?}");
    assert!(!events.is_empty());
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e["type"], "event", "{e}");
        assert_eq!(shape(e), event_shape, "{e}");
        assert_eq!(e["sequence"], json!(i + 1), "{lines:#?}");
        assert_eq!(e["operation_id"], result["operation_id"]);
    }
    let mut want = expected.last().unwrap().clone();
    // A push that publishes no port has no endpoint, which `[]` says; the
    // endpoint's own shape is checked by the push that publishes one.
    if let (Some(ours), Some(theirs)) = (
        result["services"].as_array(),
        want["services"].as_array_mut(),
    ) {
        for (o, t) in ours.iter().zip(theirs.iter_mut()) {
            if o["endpoints"] == json!([]) {
                t["endpoints"] = json!([]);
            }
        }
    }
    assert_eq!(shape(result), shape(&want), "{result}");
    assert_eq!(result["status"], want["status"]);
    assert_eq!(result["environment"]["name"], "development");
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Binds `port` on 127.0.0.1 once the fake brought a release up (after
/// the daemon's own conflict probe), and holds it until dropped.
fn listen_after_up(h: &Harness, port: u16) -> thread::JoinHandle<TcpListener> {
    let up = h.fake("up_files");
    thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        while !up.exists() {
            assert!(Instant::now() < deadline, "nothing was brought up");
            thread::sleep(Duration::from_millis(10));
        }
        TcpListener::bind(("127.0.0.1", port)).unwrap()
    })
}

const DOCKERFILE: &[(&str, &str)] = &[("Dockerfile", "FROM scratch\n")];

#[test]
fn a_dockerfile_push_prints_the_fixture_lines_and_the_endpoint() {
    let h = Harness::new();
    let root = h.tree("my-app", DOCKERFILE);
    let port = free_port();
    let listener = listen_after_up(&h, port);
    let out = h.run(&[
        "push",
        root.to_str().unwrap(),
        "--port",
        &port.to_string(),
        "-o",
        "json",
    ]);
    let _listener = listener.join().unwrap();
    assert_eq!(code(&out), 0, "{}", show(&out));
    let lines = json_lines(&out);
    assert_matches_fixture(&lines, SUCCEEDED_FIXTURE);
    let result = lines.last().unwrap();
    assert_eq!(result["project"]["slug"], "my-app");
    assert_eq!(
        result["services"],
        json!([{
            "name": "web",
            "status": "running_unverified",
            "endpoints": [{ "provider": "port", "address": format!("127.0.0.1:{port}") }],
        }])
    );
    let phases: Vec<&str> = lines
        .iter()
        .filter(|l| l["event"] == "phase_started")
        .map(|l| l["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        phases,
        [
            "uploading",
            "validating",
            "building",
            "preparing",
            "deploying",
            "verifying"
        ]
    );

    // The same project again, as text: the address that answers is printed.
    let out = h.run(&[
        "push",
        root.to_str().unwrap(),
        "--port",
        &port.to_string(),
        "--wait",
    ]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    let text = stdout(&out);
    assert!(text.contains("Deployed my-app: release rel_"), "{text}");
    assert!(text.contains("  web: running_unverified\n"), "{text}");
    assert!(
        text.contains(&format!("Endpoint: 127.0.0.1:{port}\n")),
        "{text}"
    );
}

#[test]
fn without_a_route_no_address_is_printed() {
    let h = Harness::new();
    let root = h.tree("quiet", DOCKERFILE);
    let out = h.run(&["push", root.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    assert!(
        stdout(&out).ends_with("Endpoint: not configured\n"),
        "{}",
        show(&out)
    );

    // A route whose address does not answer is not printed either.
    let port = free_port();
    let out = h.run(&[
        "push",
        root.to_str().unwrap(),
        "--port",
        &port.to_string(),
        "-o",
        "json",
    ]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    let lines = json_lines(&out);
    assert_eq!(lines.last().unwrap()["services"][0]["endpoints"], json!([]));
}

#[test]
fn a_failed_build_exits_1_with_the_failed_fixture_shape() {
    let h = Harness::new();
    h.control("build_exit", "1");
    let root = h.tree("my-app", DOCKERFILE);
    let out = h.run(&["push", root.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&out), 1, "{}", show(&out));
    let lines = json_lines(&out);
    assert_matches_fixture(&lines, FAILED_FIXTURE);
    let result = lines.last().unwrap();
    assert_eq!(result["failure"]["code"], "build_failed");
    assert_eq!(result["failure"]["phase"], "building");
    assert_eq!(result["release_id"], Value::Null);

    let out = h.run(&["push", root.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", show(&out));
    assert!(
        stderr(&out).contains("build_failed in building"),
        "{}",
        show(&out)
    );
}

#[test]
fn dry_run_sends_nothing_and_lists_the_sensitive_files() {
    let h = Harness::new();
    // No daemon at all: a dry run must not need one.
    h.target("local", Some(&h.path().join("nothing.sock")));
    let root = h.tree(
        "app",
        &[
            ("Dockerfile", "FROM scratch\n"),
            (".env", "TOKEN=x\n"),
            (".env.example", "TOKEN=\n"),
            ("id_rsa", "-----BEGIN OPENSSH PRIVATE KEY-----\n"),
        ],
    );
    let out = h.run(&["push", root.to_str().unwrap(), "--dry-run"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    let text = stdout(&out);
    assert!(text.contains("Excluded by default:"), "{text}");
    assert!(text.contains("    .env  ("), "{text}");
    assert!(text.contains("    id_rsa  ("), "{text}");
    assert!(!text.contains(".env.example"), "{text}");

    let out = h.run(&["push", root.to_str().unwrap(), "--dry-run", "-o", "json"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let excluded: Vec<&str> = report["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(excluded.contains(&".env"), "{report}");
    assert!(excluded.contains(&"id_rsa"), "{report}");
    assert!(!h.fake("log").exists(), "docker was run");
}

#[test]
fn detach_returns_the_operation_before_the_build_ends() {
    let h = Harness::new();
    h.control("build_wait", "");
    let root = h.tree("app", DOCKERFILE);
    let out = h.run(&["push", root.to_str().unwrap(), "--detach"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    assert!(stdout(&out).contains("operation op_"), "{}", show(&out));
    assert!(!h.fake("build_go").exists());
}

#[test]
fn usage_errors_exit_2() {
    let h = Harness::new();
    let root = h.tree("app", DOCKERFILE);
    let bad = h.tree("Not A Slug", DOCKERFILE);
    let r = root.to_str().unwrap();
    for args in [
        vec!["push", r, "--wait", "--detach"],
        vec!["push", r, "--port", "0"],
        vec!["push", r, "--project", "Bad Name"],
        vec!["push", r, "--file", "../Dockerfile"],
        vec!["push", r, "--to", "nowhere"],
        vec!["push", bad.to_str().unwrap()],
        vec!["push", "/nonexistent/ship-test-tree"],
    ] {
        let out = h.run(&args);
        assert_eq!(code(&out), 2, "{args:?}\n{}", show(&out));
    }
}

#[test]
fn no_daemon_exits_90() {
    let h = Harness::new();
    h.target("local", Some(&h.path().join("nothing.sock")));
    let root = h.tree("app", DOCKERFILE);
    let out = h.run(&["push", root.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&out), 90, "{}", show(&out));
    let result = json_lines(&out).pop().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["failure"]["code"], "daemon_unreachable");
}

/// A fake daemon on `socket` answering every request, once its body has
/// been read, with `status`, the API version `api` and `body`.
fn fake_daemon(socket: &Path, status: u16, api: &'static str, body: &'static str) {
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            thread::spawn(move || {
                let mut seen = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let Ok(n) = conn.read(&mut buf) else { return };
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&seen);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let head = text[..end].to_ascii_lowercase();
                        let chunked = head.contains("transfer-encoding: chunked");
                        if !chunked || text.ends_with("0\r\n\r\n") {
                            break;
                        }
                    }
                }
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     ship-api-version: {api}\r\nship-version: 9.9.9\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            });
        }
    });
}

#[test]
fn another_api_version_exits_91_and_an_internal_failure_99() {
    let h = Harness::new();
    let root = h.tree("app", DOCKERFILE);
    let other = h.path().join("v9.sock");
    fake_daemon(&other, 200, "v9", "{}");
    h.target("local", Some(&other));
    let out = h.run(&["push", root.to_str().unwrap()]);
    assert_eq!(code(&out), 91, "{}", show(&out));
    assert!(stderr(&out).contains("Upgrade"), "{}", show(&out));

    let broken = h.path().join("500.sock");
    fake_daemon(
        &broken,
        500,
        "v1",
        r#"{"code":"internal","message":"the daemon could not read its database","details":{},"operation_id":null}"#,
    );
    h.target("local", Some(&broken));
    let out = h.run(&["push", root.to_str().unwrap()]);
    assert_eq!(code(&out), 99, "{}", show(&out));
}

#[test]
fn docker_unavailable_exits_92() {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["fake", "bin", "work"] {
        fs::create_dir(dir.path().join(sub)).unwrap();
    }
    let docker = Runner::new("/nonexistent/ship-test/docker");
    let h = Harness::with_docker(dir, docker);
    let root = h.tree("app", DOCKERFILE);
    let out = h.run(&["push", root.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&out), 92, "{}", show(&out));
    let result = json_lines(&out).pop().unwrap();
    assert_eq!(result["failure"]["code"], "docker_unavailable");
}

#[test]
fn a_daemon_restart_mid_build_exits_93() {
    let mut h = Harness::new();
    h.control("build_wait", "");
    let root = h.tree("app", DOCKERFILE);
    let child = h.spawn(&["push", root.to_str().unwrap(), "-o", "json"]);
    h.wait_for("build_started");
    h.stop();
    h.control("build_go", "");
    h.start();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 93, "{}", show(&out));
    let result = json_lines(&out).pop().unwrap();
    assert_eq!(result["status"], "interrupted");
}

#[test]
fn losing_the_daemon_for_good_exits_94() {
    let mut h = Harness::new();
    h.control("build_wait", "");
    let root = h.tree("app", DOCKERFILE);
    let child = h.spawn(&[
        "push",
        root.to_str().unwrap(),
        "-o",
        "json",
        "--reconnect",
        "1",
    ]);
    h.wait_for("build_started");
    h.stop();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 94, "{}", show(&out));
    let result = json_lines(&out).pop().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["failure"]["code"], "connection_lost");
    assert!(
        result["operation_id"].as_str().unwrap().starts_with("op_"),
        "{result}"
    );
}

/// A proxy from `listen` to `upstream` that cuts the first event stream
/// once the fake build is waiting and some event went through. Returns the
/// number of event streams opened and whether the cut happened.
fn cutting_proxy(
    listen: &Path,
    upstream: PathBuf,
    started: PathBuf,
) -> (Arc<AtomicUsize>, Arc<AtomicBool>) {
    let listener = UnixListener::bind(listen).unwrap();
    let streams = Arc::new(AtomicUsize::new(0));
    let cut = Arc::new(AtomicBool::new(false));
    let (s, c) = (Arc::clone(&streams), Arc::clone(&cut));
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut client) = conn else { return };
            let (upstream, started, streams, cut) = (
                upstream.clone(),
                started.clone(),
                Arc::clone(&s),
                Arc::clone(&c),
            );
            thread::spawn(move || {
                let Ok(mut daemon) = UnixStream::connect(&upstream) else {
                    return;
                };
                let mut first = vec![0u8; 16 * 1024];
                let Ok(n) = client.read(&mut first) else {
                    return;
                };
                let is_events = String::from_utf8_lossy(&first[..n]).contains("/events");
                if daemon.write_all(&first[..n]).is_err() {
                    return;
                }
                let cut_this = is_events && {
                    streams.fetch_add(1, Ordering::SeqCst);
                    !cut.load(Ordering::SeqCst)
                };
                let (mut c2, mut d2) = (client.try_clone().unwrap(), daemon.try_clone().unwrap());
                thread::spawn(move || {
                    let _ = std::io::copy(&mut c2, &mut d2);
                    let _ = d2.shutdown(std::net::Shutdown::Write);
                });
                daemon
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let mut buf = [0u8; 8192];
                let mut saw_data = false;
                loop {
                    match daemon.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            saw_data |= buf[..n].windows(5).any(|w| w == b"data:");
                            if client.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                    if cut_this && saw_data && started.exists() {
                        cut.store(true, Ordering::SeqCst);
                        let _ = client.shutdown(std::net::Shutdown::Both);
                        let _ = daemon.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                }
            });
        }
    });
    (streams, cut)
}

#[test]
fn a_dropped_stream_is_resumed_with_nothing_lost_or_repeated() {
    let h = Harness::new();
    h.control("build_wait", "");
    let proxy = h.path().join("proxy.sock");
    let (streams, cut) = cutting_proxy(&proxy, h.socket(), h.fake("build_started"));
    h.target("local", Some(&proxy));
    let root = h.tree("app", DOCKERFILE);
    let child = h.spawn(&["push", root.to_str().unwrap(), "-o", "json"]);
    // Released once the client has opened a second stream.
    let deadline = Instant::now() + WAIT;
    while !(cut.load(Ordering::SeqCst) && streams.load(Ordering::SeqCst) >= 2) {
        assert!(
            Instant::now() < deadline,
            "the stream was never cut and resumed"
        );
        thread::sleep(Duration::from_millis(20));
    }
    h.control("build_go", "");
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "{}", show(&out));
    let lines = json_lines(&out);
    assert_matches_fixture(&lines, SUCCEEDED_FIXTURE);
    let messages: Vec<&str> = lines.iter().filter_map(|l| l["message"].as_str()).collect();
    assert!(messages.contains(&"#1 building"), "{messages:?}");
    assert!(messages.contains(&"#3 resumed"), "{messages:?}");
}

/// How `faulty_proxy` answers event streams once faulted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    /// `200` with an empty body.
    Empty,
    /// `200` with a body that ends inside a frame.
    Truncated,
    /// Accepts the connection and never answers.
    Stalled,
}

/// A proxy from `listen` to `upstream` that, once `faulted` is set, cuts
/// the open event stream and answers every new one with `fault`. Returns
/// the number of event streams opened.
fn faulty_proxy(
    listen: &Path,
    upstream: PathBuf,
    fault: Fault,
    faulted: Arc<AtomicBool>,
) -> Arc<AtomicUsize> {
    let listener = UnixListener::bind(listen).unwrap();
    let streams = Arc::new(AtomicUsize::new(0));
    let s = Arc::clone(&streams);
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut client) = conn else { return };
            let (upstream, streams, faulted) =
                (upstream.clone(), Arc::clone(&s), Arc::clone(&faulted));
            thread::spawn(move || {
                let mut first = vec![0u8; 16 * 1024];
                let Ok(n) = client.read(&mut first) else {
                    return;
                };
                let is_events = String::from_utf8_lossy(&first[..n]).contains("/events");
                if is_events {
                    streams.fetch_add(1, Ordering::SeqCst);
                }
                if is_events && faulted.load(Ordering::SeqCst) {
                    let body: &str = match fault {
                        Fault::Empty => "",
                        Fault::Truncated => "id: 99\ndata: {\"operation_id\":",
                        Fault::Stalled => {
                            thread::sleep(Duration::from_secs(120));
                            return;
                        }
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         ship-api-version: v1\r\nship-version: 9.9.9\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = client.write_all(reply.as_bytes());
                    return;
                }
                let Ok(mut daemon) = UnixStream::connect(&upstream) else {
                    return;
                };
                if daemon.write_all(&first[..n]).is_err() {
                    return;
                }
                let (mut c2, mut d2) = (client.try_clone().unwrap(), daemon.try_clone().unwrap());
                thread::spawn(move || {
                    let _ = std::io::copy(&mut c2, &mut d2);
                    let _ = d2.shutdown(std::net::Shutdown::Write);
                });
                daemon
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let mut buf = [0u8; 8192];
                loop {
                    match daemon.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if client.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                    if is_events && faulted.load(Ordering::SeqCst) {
                        let _ = client.shutdown(std::net::Shutdown::Both);
                        let _ = daemon.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                }
            });
        }
    });
    streams
}

/// A reopened stream that answers but delivers no frame, or never answers,
/// does not restore the connection: the reconnect window still ends the
/// wait with 94, on time.
#[test]
fn empty_truncated_or_stalled_reconnects_still_exit_94_in_time() {
    for fault in [Fault::Empty, Fault::Truncated, Fault::Stalled] {
        let h = Harness::new();
        h.control("build_wait", "");
        let proxy = h.path().join("proxy.sock");
        let faulted = Arc::new(AtomicBool::new(false));
        let streams = faulty_proxy(&proxy, h.socket(), fault, Arc::clone(&faulted));
        h.target("local", Some(&proxy));
        let root = h.tree("app", DOCKERFILE);
        let child = h.spawn(&[
            "push",
            root.to_str().unwrap(),
            "-o",
            "json",
            "--reconnect",
            "1",
        ]);
        h.wait_for("build_started");
        let cut_at = Instant::now();
        faulted.store(true, Ordering::SeqCst);
        let out = child.wait_with_output().unwrap();
        let took = cut_at.elapsed();
        assert_eq!(code(&out), 94, "{fault:?}\n{}", show(&out));
        assert!(took < Duration::from_secs(10), "{fault:?} took {took:?}");
        assert!(streams.load(Ordering::SeqCst) >= 2, "{fault:?}: never reopened");
        let result = json_lines(&out).pop().unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["failure"]["code"], "connection_lost");
    }
}

#[test]
fn a_push_over_ssh_deploys() {
    let h = Harness::new();
    h.target("ssh://deploy@lab", None);
    let root = h.tree("remote", DOCKERFILE);
    let out = h.run(&["push", root.to_str().unwrap(), "-o", "json"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    assert_matches_fixture(&json_lines(&out), SUCCEEDED_FIXTURE);

    // `--to` picks a target that is not current.
    let cfg = json!({
        "current": "t",
        "targets": {
            "t": { "url": "local", "socket": h.path().join("nothing.sock") },
            "lab": { "url": "ssh://deploy@lab", "socket": h.socket() },
        },
    });
    fs::write(h.config(), cfg.to_string()).unwrap();
    let out = h.run(&["push", root.to_str().unwrap(), "--to", "lab"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
}

/// The canonical model of a Compose file publishing one port, as
/// `docker compose config` prints it.
fn compose_model(port: u16) -> String {
    json!({
        "name": "x",
        "services": {
            "web": {
                "image": "nginx",
                "ports": [{
                    "mode": "ingress",
                    "host_ip": "127.0.0.1",
                    "target": 80,
                    "published": port.to_string(),
                    "protocol": "tcp",
                }],
            },
        },
    })
    .to_string()
}

#[test]
fn a_compose_file_needing_confirmation_exits_3_until_confirmed() {
    let h = Harness::new();
    let port = free_port();
    h.control("config", &compose_model(port));
    let root = h.tree(
        "stack",
        &[(
            "compose.yaml",
            "services:\n  web:\n    image: nginx\n    ports:\n      - \"127.0.0.1:8080:80\"\n",
        )],
    );
    let r = root.to_str().unwrap();

    let out = h.run(&["push", r]);
    assert_eq!(code(&out), 3, "{}", show(&out));
    assert!(
        stderr(&out).contains("confirmation_required"),
        "{}",
        show(&out)
    );
    assert!(stderr(&out).contains("ports"), "{}", show(&out));
    assert!(!h.fake("up_files").exists(), "something was started");

    let out = h.run(&["push", r, "-o", "json"]);
    assert_eq!(code(&out), 3, "{}", show(&out));
    let result = json_lines(&out).pop().unwrap();
    assert_eq!(result["failure"]["code"], "confirmation_required");

    let out = h.run(&["push", r, "--confirm"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    assert!(h.fake("up_files").exists());

    // Waiving confirmation for the project needs a confirmation itself.
    let out = h.run(&[
        "settings",
        "set",
        "compose.confirm",
        "accept",
        "--project",
        "stack",
    ]);
    assert_eq!(code(&out), 3, "{}", show(&out));
    assert!(stderr(&out).contains("waives"), "{}", show(&out));
    let out = h.run(&["settings", "get", "compose.confirm", "--project", "stack"]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    assert!(
        stdout(&out).starts_with("compose.confirm: ask"),
        "{}",
        show(&out)
    );

    let out = h.run(&[
        "settings",
        "set",
        "compose.confirm",
        "accept",
        "--project",
        "stack",
        "--confirm",
    ]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    let out = h.run(&[
        "settings",
        "get",
        "compose.confirm",
        "--project",
        "stack",
        "-o",
        "json",
    ]);
    assert_eq!(code(&out), 0, "{}", show(&out));
    let view: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(view["value"], "accept");
    // The host's own value is unchanged.
    let out = h.run(&["settings", "get", "compose.confirm", "-o", "json"]);
    let view: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(view["value"], "ask");

    let out = h.run(&["settings", "set", "compose.confirm", "maybe"]);
    assert_eq!(code(&out), 2, "{}", show(&out));
    let out = h.run(&[
        "settings",
        "set",
        "publish.address",
        "auto",
        "--project",
        "stack",
    ]);
    assert_eq!(code(&out), 2, "{}", show(&out));
}
