//! `ship projects -o json` over a real sshd returns the same bytes as over
//! the local socket, and both equal the recorded fixture (ADR-0003,
//! ADR-0004 rule 4). `ship open --no-browser` over the same sshd serves the
//! UI on a 127.0.0.1 port, each connection carried by `ssh … ship
//! dial-stdio`, and SIGINT closes it. A snapshot uploaded over the same
//! sshd becomes the same revision as over the local socket, and `ship
//! push` over it deploys.
//!
//! Ignored by default: it needs an sshd that accepts this user with a key
//! (BatchMode, host key already known) and `ship` from this build on the
//! login user's PATH, and a build that embeds the UI (`make web` first).
//! CI's `ssh` job sets that up on the runner and runs:
//!
//! ```sh
//! SHIP_REQUIRE_UI=1 SHIP_TEST_SSH="$USER@localhost" cargo test -p ship --test ssh_real -- --ignored
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

/// A snapshot uploaded over the real sshd (`ssh … ship dial-stdio`) is
/// received like one over the local socket: the same tree is the same
/// revision whichever transport carried it (ADR-0005, ADR-0003).
#[test]
#[ignore = "needs a real sshd; run by CI's ssh job with SHIP_TEST_SSH=user@host"]
fn upload_over_real_ssh_matches_local() {
    use ship_net::client::{self, RequestBody};

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

    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("src")).unwrap();
    std::fs::write(tree.join("Dockerfile"), b"FROM scratch\n").unwrap();
    // Large enough to take many chunks through the ssh pipes.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let noise: Vec<u8> = (0..300_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect();
    std::fs::write(tree.join("src/data.bin"), noise).unwrap();
    let options = ship_source::Options {
        home: Some(dir.path().join("no-home")),
        ..ship_source::Options::default()
    };
    let selection = ship_source::scan(&tree, &options).unwrap();
    let mut stream = Vec::new();
    let packed = ship_source::pack(&selection, &mut stream).unwrap();

    let upload = |target: &ship_net::Target, key: &str| {
        let headers = [
            ("idempotency-key", key),
            ("ship-snapshot-sha256", packed.sha256.as_str()),
        ];
        let r = rt
            .block_on(client::post(
                target,
                "/api/v1/projects/ssh-app/uploads",
                &headers,
                RequestBody::bytes(stream.clone()),
                Duration::from_secs(30),
            ))
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        (r.status, json)
    };

    let local = ship_net::Target::parse("local", Some(&socket)).unwrap();
    let (status, first) = upload(&local, "over-local");
    assert_eq!(status, 201, "{first}");

    let url = format!("ssh://{remote}");
    let over_ssh = ship_net::Target::parse(&url, Some(&socket)).unwrap();
    let (status, second) = upload(&over_ssh, "over-ssh");
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["revision_id"], first["revision_id"]);
    assert_eq!(second["sha256"], packed.sha256.as_str());
    assert_ne!(second["operation_id"], first["operation_id"]);

    rt.block_on(running.shutdown()).unwrap();
}

/// `ship push` over the real sshd deploys a Dockerfile project, against
/// the fake `docker` of `cli_push.rs`, and prints the fixture's lines.
#[test]
#[ignore = "needs a real sshd; run by CI's ssh job with SHIP_TEST_SSH=user@host"]
fn push_over_real_ssh_deploys() {
    use std::os::unix::fs::PermissionsExt;

    let remote = std::env::var("SHIP_TEST_SSH").expect("SHIP_TEST_SSH=user@host");
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    let docker = dir.path().join("docker");
    std::fs::write(&docker, include_str!("support/fake-docker.sh")).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let config = ship_daemon::Config {
        state_dir: dir.path().join("state"),
        socket_path: dir.path().join("ship.sock"),
        port: 0,
        docker: ship_docker::Runner::new(&docker).env("FAKE_DIR", &fake),
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
        tailnet: std::sync::Arc::new(ship_net::StaticTailnet(None)),
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

    let tree = dir.path().join("ssh-push");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("Dockerfile"), b"FROM scratch\n").unwrap();
    let out = ship(&cfg, &["push", tree.to_str().unwrap(), "-o", "json"]);
    check(&out);
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let result = lines.last().unwrap();
    assert_eq!(result["type"], "result", "{lines:#?}");
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["project"]["slug"], "ssh-push");
    for (i, e) in lines[..lines.len() - 1].iter().enumerate() {
        assert_eq!(e["sequence"], serde_json::json!(i + 1), "{e}");
    }

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

    // The UI itself, not the not-built 503: CI's ssh job builds web/dist
    // before compiling ship.
    assert!(
        ship_daemon::ui::is_built(),
        "this build embeds no UI; run `make web` before cargo"
    );
    let (status, body) = http_get(port);
    assert_eq!(status, 200, "{body}");
    let index = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/index.html");
    let index = std::fs::read_to_string(&index)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", index.display()));
    assert_eq!(body, index);

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
