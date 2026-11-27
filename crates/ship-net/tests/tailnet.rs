//! `TailscaleCli` against fake `tailscale` programs: only a clean answer
//! is a tailnet address; everything else is no tailnet, in bounded time.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ship_net::{TailnetAddress, TailscaleCli};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn ask(program: &Path, wait: Duration) -> Option<Ipv4Addr> {
    TailscaleCli::new(program, wait).ipv4().await
}

#[tokio::test]
async fn an_answer_is_the_address() {
    let dir = tempfile::tempdir().unwrap();
    // The arguments must be exactly `ip -4`.
    let ok = script(
        dir.path(),
        "ok",
        r#"[ "$1 $2" = "ip -4" ] || exit 9; echo 100.101.102.103"#,
    );
    assert_eq!(
        ask(&ok, Duration::from_secs(5)).await,
        Some(Ipv4Addr::new(100, 101, 102, 103))
    );
}

#[tokio::test]
async fn failures_are_no_tailnet() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        script(dir.path(), "logged-out", "echo 'Logged out.' >&2; exit 1"),
        script(
            dir.path(),
            "stopped",
            "echo 'failed to connect to local tailscaled' >&2; exit 1",
        ),
        script(dir.path(), "garbage", "echo 'not an address'"),
        script(dir.path(), "empty", "exit 0"),
        script(dir.path(), "v6", "echo fd7a:115c:a1e0::1"),
        dir.path().join("missing"),
    ];
    for program in &cases {
        assert_eq!(
            ask(program, Duration::from_secs(5)).await,
            None,
            "{}",
            program.display()
        );
    }
}

#[tokio::test]
async fn a_slow_answer_is_no_tailnet_within_the_wait() {
    let dir = tempfile::tempdir().unwrap();
    let slow = script(dir.path(), "slow", "sleep 10; echo 100.101.102.103");
    let started = Instant::now();
    assert_eq!(ask(&slow, Duration::from_millis(300)).await, None);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
