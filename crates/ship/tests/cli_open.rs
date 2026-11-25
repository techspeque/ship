//! `ship open --no-browser` from the built binary (OVERVIEW §3.3, §6).
//!
//! The daemon runs in-process on a free 127.0.0.1 port. Local: the URL is
//! printed only once `/healthz` answers, and with nothing listening the
//! command exits 1 and prints no URL. ssh: a fake `ssh` (the one
//! `cli_client.rs` uses) records its argv and pid and execs the remote
//! `ship dial-stdio …` locally, so every connection to the printed URL is
//! carried by its own dial-stdio child; SIGINT then ends `ship` and leaves
//! no child behind. `ssh_real.rs` does the same against a real sshd.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const SHIP: &str = env!("CARGO_BIN_EXE_ship");

/// What Tailscale SSH says when refusing a streamlocal forward; the fake
/// ssh in `refuse` mode prints it and exits 255, as ssh does.
const REFUSAL: &str = "channel 1: open failed: administratively prohibited: \
                       socket path is not in an allowed directory: ssh: rejected";

/// A fake `ssh`: records its arguments one per line and appends its pid,
/// then runs the remote command's `ship …` words with the real binary. In
/// `refuse` mode it prints [`REFUSAL`] and exits 255.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$FAKE_SSH_ARGV"
echo $$ >> "$FAKE_SSH_PID"
if [ "$FAKE_SSH_MODE" = refuse ]; then
  echo "$FAKE_SSH_REFUSAL" >&2
  exit 255
fi
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift 2
[ "$1" = ship ] || exit 97
shift
exec "$SHIP_BIN" "$@"
"#;

/// A daemon running in-process on its own runtime; shut down on drop.
struct Daemon {
    rt: tokio::runtime::Runtime,
    running: Option<ship_daemon::Running>,
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
            ..ship_daemon::Config::new(ship_daemon::Redactor::new())
        };
        let running = rt.block_on(ship_daemon::start(&config)).unwrap();
        Daemon {
            rt,
            running: Some(running),
        }
    }

    fn running(&self) -> &ship_daemon::Running {
        self.running.as_ref().unwrap()
    }

    fn socket(&self) -> String {
        self.running().socket_path().display().to_string()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = self.rt.block_on(running.shutdown());
        }
    }
}

/// A temporary home: config file and a `bin/` holding the fake `ssh`.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
        };
        let bin = env.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        fs::write(&ssh, FAKE_SSH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        env
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    fn ship(&self) -> Command {
        let mut c = Command::new(SHIP);
        let path = format!(
            "{}:{}",
            self.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.env("PATH", path)
            .env("SHIP_CONFIG", self.file("config.json"))
            .env("SHIP_BIN", SHIP)
            .env("FAKE_SSH_ARGV", self.file("ssh-argv"))
            .env("FAKE_SSH_PID", self.file("ssh-pid"))
            .env("FAKE_SSH_REFUSAL", REFUSAL)
            .env_remove("FAKE_SSH_MODE");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.ship().args(args).output().unwrap()
    }

    fn add(&self, url: &str, socket: &str) {
        let out = self.run(&["target", "add", "lab", url, "--socket", socket]);
        assert!(out.status.success(), "{}", show(&out));
    }

    /// Every pid the fake ssh recorded, in start order.
    fn ssh_pids(&self) -> Vec<i32> {
        fs::read_to_string(self.file("ssh-pid"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().parse().unwrap())
            .collect()
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

/// A port on 127.0.0.1 with nothing listening (free a moment ago).
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Sends `METHOD path` with `Host: 127.0.0.1:port` (unless `headers`
/// names another) on a fresh connection, without reading the answer.
fn send(port: u16, method: &str, path: &str, headers: &[&str]) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    if !headers
        .iter()
        .any(|h| h.to_ascii_lowercase().starts_with("host:"))
    {
        request.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for h in headers {
        request.push_str(h);
        request.push_str("\r\n");
    }
    request.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    s.write_all(request.as_bytes()).unwrap();
    s
}

/// Reads a whole `Connection: close` answer; (status, body).
fn answer(mut s: TcpStream) -> (u16, String) {
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("not HTTP: {raw:?}"));
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
}

fn request(port: u16, method: &str, path: &str, headers: &[&str]) -> (u16, String) {
    answer(send(port, method, path, headers))
}

fn is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("ship did not exit within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `ship open --no-browser` in the background, once it has printed its
/// URL; (child, port).
fn open(env: &Env) -> (Child, u16) {
    let mut child = env
        .ship()
        .args(["open", "--no-browser", "--timeout", "20"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    if line.is_empty() {
        let status = wait_exit(&mut child, Duration::from_secs(5));
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        panic!("ship open printed no URL ({status}): {err}");
    }
    let url = line.trim_end();
    let port = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|r| r.strip_suffix('/'))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("not a 127.0.0.1 URL: {url:?}"));
    (child, port)
}

/// SIGINT to `ship open`; it must exit 0 and leave no ssh child or port.
fn interrupt(env: &Env, mut child: Child, port: u16) {
    // SAFETY: sending a signal to our own child has no memory effects.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(rc, 0);
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert!(status.success(), "{status}");
    for pid in env.ssh_pids() {
        assert!(!is_alive(pid), "ssh {pid} is still running");
    }
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port {port} is still open"
    );
}

#[test]
fn local_prints_the_url_once_healthz_answers() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    let port = daemon.running().tcp_addr().port();

    let out = env.run(&["open", "--no-browser", "--port", &port.to_string()]);
    assert!(out.status.success(), "{}", show(&out));
    let url = format!("http://127.0.0.1:{port}/");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("{url}\n"));

    let (status, body) = request(port, "GET", "/healthz", &[]);
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"status":"ok"}"#);
}

#[test]
fn local_with_nothing_listening_prints_no_url() {
    let env = Env::new();
    let port = free_port().to_string();
    let out = env.run(&["open", "--no-browser", "--port", &port]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(out.stdout.is_empty(), "{}", show(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("127.0.0.1 listener"), "{err}");
    assert!(err.contains(&format!("port {port}")), "{err}");
    assert!(!err.contains("http://"), "{err}");
}

// f-010: a Ship-shaped `/healthz` already answering on the requested port
// is not this `ship open`. The port is bound before ssh runs, so it is
// refused, no URL is printed and no ssh is started.
#[test]
fn ssh_refuses_an_occupied_port() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    let port = daemon.running().tcp_addr().port().to_string();
    env.add("ssh://lab.example", &daemon.socket());

    let out = env.run(&["open", "--no-browser", "--port", &port, "--timeout", "5"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(out.stdout.is_empty(), "{}", show(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("port {port}")), "{err}");
    assert!(!err.contains("http://"), "{err}");
    assert!(!err.contains("forwarding"), "{err}");
    assert!(env.ssh_pids().is_empty(), "ssh was run");
}

// fix-0005: every connection is carried by `ssh … HOST ship dial-stdio`;
// no ssh port forwarding, which Tailscale SSH refuses.
#[test]
fn ssh_serves_the_ui_through_dial_stdio_until_sigint() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    let socket = daemon.socket();
    env.add("ssh://tester@lab.example:2222", &socket);
    let (child, port) = open(&env);

    let (status, body) = request(port, "GET", "/healthz", &[]);
    assert_eq!((status, body.as_str()), (200, r#"{"status":"ok"}"#));
    let (status, body) = request(port, "GET", "/", &[]);
    if ship_daemon::ui::is_built() {
        assert_eq!(status, 200, "{body}");
        assert!(
            body.to_ascii_lowercase().starts_with("<!doctype html>"),
            "{body}"
        );
    } else {
        assert_eq!(status, 503);
        assert_eq!(body, ship_daemon::ui::NOT_BUILT);
    }

    // The exact argv: `ssh_args`, HOST alone after `--`, then the remote
    // command. No -L, no -N.
    let argv: Vec<String> = fs::read_to_string(env.file("ssh-argv"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        argv,
        [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            "-p",
            "2222",
            "-l",
            "tester",
            "--",
            "lab.example",
            "ship",
            "dial-stdio",
            "--socket",
            &socket,
        ]
    );

    // Concurrent requests: all sent before any answer is read, each on its
    // own connection and its own child.
    let before = env.ssh_pids().len();
    let held: Vec<TcpStream> = (0..4).map(|_| send(port, "GET", "/healthz", &[])).collect();
    for s in held {
        assert_eq!(answer(s), (200, r#"{"status":"ok"}"#.to_owned()));
    }
    assert!(env.ssh_pids().len() >= before + 4, "{:?}", env.ssh_pids());

    // An open connection holds a live child; SIGINT must still end it.
    let before = env.ssh_pids().len();
    let idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.ssh_pids().len() == before {
        assert!(
            Instant::now() < deadline,
            "no child for the idle connection"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let live = *env.ssh_pids().last().unwrap();
    assert!(is_alive(live));

    interrupt(&env, child, port);
    drop(idle);
}

// The daemon sees what the browser sent, so its Host and origin guard
// behaves as it did through the old ssh forward.
#[test]
fn ssh_keeps_the_host_and_origin_guard() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    env.add("ssh://lab.example", &daemon.socket());
    let (child, port) = open(&env);

    let (status, body) = request(port, "GET", "/api/v1/host", &[]);
    assert_eq!(status, 200, "{body}");
    let (status, _) = request(
        port,
        "POST",
        "/api/v1/projects",
        &["Origin: http://evil.example"],
    );
    assert_eq!(status, 403);
    let (status, _) = request(port, "GET", "/api/v1/host", &["Host: evil.example"]);
    assert_eq!(status, 403);

    interrupt(&env, child, port);
}

// fix-0005 (3): a refused first connection prints ssh's own reason, not a
// timeout, and no URL.
#[test]
fn ssh_refusal_prints_ssh_reason() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    env.add("ssh://tester@lab.example", &daemon.socket());

    let out = env
        .ship()
        .env("FAKE_SSH_MODE", "refuse")
        .args(["open", "--no-browser", "--timeout", "20"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(out.stdout.is_empty(), "{}", show(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(REFUSAL), "{err}");
    assert!(err.contains("tester@lab.example"), "{err}");
    assert!(!err.contains("http://"), "{err}");
    for pid in env.ssh_pids() {
        assert!(!is_alive(pid), "ssh {pid} is still running");
    }
}
