//! `ship projects -o json` over a real sshd returns the same bytes as over
//! the local socket, and both equal the recorded fixture (ADR-0003,
//! ADR-0004 rule 4). `ship open --no-browser` over the same sshd forwards a
//! 127.0.0.1 port that serves the UI, and SIGINT closes it.
//!
//! Ignored by default: it needs an sshd that accepts this user with a key
//! (BatchMode, host key already known) and `ship` from this build on the
//! login user's PATH. CI's `ssh` job sets that up on the runner and runs:
//!
//! ```sh
//! SHIP_TEST_SSH="$USER@localhost" cargo test -p ship --test ssh_real -- --ignored
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SHIP: &str = env!("CARGO_BIN_EXE_ship");
const FIXTURE: &str = include_str!("fixtures/projects-empty.json");

fn ship(config: &Path, args: &[&str]) -> Output {
    Command::new(SHIP)
        .env("SHIP_CONFIG", config)
        .args(args)
        .output()
        .unwrap()
}

fn check(out: &Output) {
    assert!(
        out.status.success(),
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "needs a real sshd; run by CI's ssh job with SHIP_TEST_SSH=user@host"]
fn projects_over_real_ssh_match_local_and_fixture() {
    let remote = std::env::var("SHIP_TEST_SSH").expect("SHIP_TEST_SSH=user@host");
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let config = ship_daemon::Config {
        state_dir: dir.path().join("state"),
        socket_path: dir.path().join("ship.sock"),
        port: 0,
        ..ship_daemon::Config::new(ship_daemon::Redactor::new())
    };
    let running = rt.block_on(ship_daemon::start(&config)).unwrap();
    let socket = running.socket_path().display().to_string();
    let cfg = dir.path().join("config.json");

    check(&ship(
        &cfg,
        &["target", "add", "here", "local", "--socket", &socket],
    ));
    let url = format!("ssh://{remote}");
    check(&ship(
        &cfg,
        &["target", "add", "lab", &url, "--socket", &socket],
    ));

    let local = ship(&cfg, &["projects", "-o", "json"]);
    check(&local);
    check(&ship(&cfg, &["target", "use", "lab"]));
    let over_ssh = ship(&cfg, &["projects", "-o", "json", "--timeout", "20"]);
    check(&over_ssh);

    assert_eq!(over_ssh.stdout, local.stdout);
    assert_eq!(String::from_utf8(over_ssh.stdout).unwrap(), FIXTURE);

    rt.block_on(running.shutdown()).unwrap();
}

/// `GET /` on `http://127.0.0.1:PORT/`; (status, body).
fn http_get(port: u16) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
}

#[test]
#[ignore = "needs a real sshd; run by CI's ssh job with SHIP_TEST_SSH=user@host"]
fn open_over_real_ssh_forwards_the_ui_until_sigint() {
    let remote = std::env::var("SHIP_TEST_SSH").expect("SHIP_TEST_SSH=user@host");
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let config = ship_daemon::Config {
        state_dir: dir.path().join("state"),
        socket_path: dir.path().join("ship.sock"),
        port: 0,
        ..ship_daemon::Config::new(ship_daemon::Redactor::new())
    };
    let running = rt.block_on(ship_daemon::start(&config)).unwrap();
    let socket = running.socket_path().display().to_string();
    let cfg = dir.path().join("config.json");
    let url = format!("ssh://{remote}");
    check(&ship(
        &cfg,
        &["target", "add", "lab", &url, "--socket", &socket],
    ));

    let mut child = Command::new(SHIP)
        .env("SHIP_CONFIG", &cfg)
        .args(["open", "--no-browser", "--timeout", "20"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut line).unwrap();
    let port: u16 = line
        .trim_end()
        .strip_prefix("http://127.0.0.1:")
        .and_then(|r| r.strip_suffix('/'))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("not a 127.0.0.1 URL: {line:?}"));

    let (status, body) = http_get(port);
    if ship_daemon::ui::is_built() {
        assert_eq!(status, 200, "{body}");
    } else {
        assert_eq!(status, 503, "{body}");
    }

    // SAFETY: sending a signal to our own child has no memory effects.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "ship open did not exit");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}");
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the forward on {port} is still open"
    );

    rt.block_on(running.shutdown()).unwrap();
}
