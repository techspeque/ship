//! `ship open --no-browser` from the built binary (OVERVIEW §3.3, §6).
//!
//! The daemon runs in-process on a free 127.0.0.1 port. Local: the URL is
//! printed only once `/healthz` answers, and with nothing listening the
//! command exits 1 and prints no URL. ssh: a fake `ssh` (Python 3) records
//! its argv and runs the `-L 127.0.0.1:P:SOCKET` forward itself, so the
//! printed URL serves the UI through it; SIGINT then ends `ship` and leaves
//! no `ssh` behind. `ssh_real.rs` does the same against a real sshd.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const SHIP: &str = env!("CARGO_BIN_EXE_ship");

/// A fake `ssh`: records argv one per line and its pid, then serves the
/// `-L 127.0.0.1:PORT:SOCKET` forward until killed.
const FAKE_SSH: &str = r#"#!/usr/bin/env python3
import os, socket, sys, threading

with open(os.environ["FAKE_SSH_ARGV"], "w") as f:
    f.write("".join(a + "\n" for a in sys.argv[1:]))
with open(os.environ["FAKE_SSH_PID"], "w") as f:
    f.write(str(os.getpid()))

args = sys.argv[1:]
spec = args[args.index("-L") + 1]
host, port, path = spec.split(":", 2)
assert host == "127.0.0.1", spec

def pump(a, b):
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind((host, int(port)))
server.listen(16)
while True:
    conn, _ = server.accept()
    upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    upstream.connect(path)
    threading.Thread(target=pump, args=(conn, upstream), daemon=True).start()
    threading.Thread(target=pump, args=(upstream, conn), daemon=True).start()
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
            .env("FAKE_SSH_ARGV", self.file("ssh-argv"))
            .env("FAKE_SSH_PID", self.file("ssh-pid"));
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.ship().args(args).output().unwrap()
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

/// `GET path` over TCP; (status, body).
fn http_get(url: &str, path: &str) -> (u16, String) {
    let authority = url
        .strip_prefix("http://")
        .and_then(|r| r.strip_suffix('/'))
        .unwrap();
    let mut s = TcpStream::connect(authority).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
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

#[test]
fn local_prints_the_url_once_healthz_answers() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    let port = daemon.running().tcp_addr().port().to_string();

    let out = env.run(&["open", "--no-browser", "--port", &port]);
    assert!(out.status.success(), "{}", show(&out));
    let url = format!("http://127.0.0.1:{port}/");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("{url}\n"));

    let (status, body) = http_get(&url, "/healthz");
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

#[test]
fn ssh_forwards_the_ui_until_sigint() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: python3 is needed for the fake ssh");
        return;
    }
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    let socket = daemon.running().socket_path().display().to_string();
    let add = env.run(&[
        "target",
        "add",
        "lab",
        "ssh://tester@lab.example:2222",
        "--socket",
        &socket,
    ]);
    assert!(add.status.success(), "{}", show(&add));

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
        let mut stderr = child.stderr.take().unwrap();
        stderr.read_to_string(&mut err).unwrap();
        panic!("ship open printed no URL ({status}): {err}");
    }
    let url = line.trim_end().to_owned();
    let port: u16 = url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|r| r.strip_suffix('/'))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("not a 127.0.0.1 URL: {url:?}"));

    // The URL serves the UI through the forward.
    let (status, body) = http_get(&url, "/");
    if ship_daemon::ui::is_built() {
        assert_eq!(status, 200, "{body}");
        let lower = body.to_ascii_lowercase();
        assert!(lower.starts_with("<!doctype html>"), "{body}");
    } else {
        assert_eq!(status, 503);
        assert_eq!(body, ship_daemon::ui::NOT_BUILT);
    }

    // The exact argv, with HOST alone after `--`.
    let argv: Vec<String> = fs::read_to_string(env.file("ssh-argv"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let forward = format!("127.0.0.1:{port}:{socket}");
    assert_eq!(
        argv,
        [
            "-N",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ConnectTimeout=20",
            "-p",
            "2222",
            "-l",
            "tester",
            "-L",
            &forward,
            "--",
            "lab.example",
        ]
    );
    let ssh_pid: i32 = fs::read_to_string(env.file("ssh-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(is_alive(ssh_pid));

    // SIGINT ends ship, which kills and reaps ssh.
    // SAFETY: sending a signal to our own child has no memory effects.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(rc, 0);
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert!(status.success(), "{status}");
    assert!(!is_alive(ssh_pid), "ssh {ssh_pid} is still running");
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the forward on {port} is still open"
    );
}
