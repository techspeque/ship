//! `ship projects -o json` over a real sshd returns the same bytes as over
//! the local socket, and both equal the recorded fixture (ADR-0003,
//! ADR-0004 rule 4).
//!
//! Ignored by default: it needs an sshd that accepts this user with a key
//! (BatchMode, host key already known) and `ship` from this build on the
//! login user's PATH. CI's `ssh` job sets that up on the runner and runs:
//!
//! ```sh
//! SHIP_TEST_SSH="$USER@localhost" cargo test -p ship --test ssh_real -- --ignored
//! ```

use std::path::Path;
use std::process::{Command, Output};

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
