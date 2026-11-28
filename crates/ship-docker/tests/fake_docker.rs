//! Preflight end to end against a fake `docker` that records its argv, and
//! against the real CLI pointed at a socket that does not exist.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ship_core::DockerStatus;
use ship_docker::preflight::{COMPOSE_VERSION_ARGS, ENGINE_VERSION_ARGS};
use ship_docker::{Runner, preflight};

/// A fake `docker`: appends each invocation's arguments, one `[arg]` each,
/// as one line to `$FAKE_DOCKER_LOG`; answers `version` with
/// `$FAKE_ENGINE` (or fails when it is empty) and `compose` with
/// `$FAKE_COMPOSE` (or fails when it is empty).
const SCRIPT: &str = r#"#!/bin/sh
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$FAKE_DOCKER_LOG"
case "$1" in
  version)
    if [ -z "$FAKE_ENGINE" ]; then
      echo "Cannot connect to the Docker daemon at unix:///fake.sock. Is the docker daemon running?" >&2
      exit 1
    fi
    echo "$FAKE_ENGINE" ;;
  compose)
    if [ -z "$FAKE_COMPOSE" ]; then
      echo "docker: 'compose' is not a docker command." >&2
      exit 1
    fi
    echo "$FAKE_COMPOSE" ;;
  *) exit 64 ;;
esac
"#;

/// Written once per test binary. This process never opens the executed
/// file for writing: a `cp` child creates it from a source file. Otherwise
/// a test spawning a child at that moment (one that does not wait on this
/// lock) hands the child a copy of the write fd until its exec, and running
/// the script in that window fails with "Text file busy" (ETXTBSY).
fn fake_docker() -> &'static Path {
    static PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PATH
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("docker.src");
            std::fs::write(&source, SCRIPT).unwrap();
            let path = dir.path().join("docker");
            let status = std::process::Command::new("cp")
                .arg(&source)
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "cp {status}");
            // chmod changes metadata only; it opens nothing for writing.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

fn runner(log: &Path, engine: &str, compose: &str) -> Runner {
    Runner::new(fake_docker())
        .env("FAKE_DOCKER_LOG", log)
        .env("FAKE_ENGINE", engine)
        .env("FAKE_COMPOSE", compose)
}

fn invocations(log: &Path) -> Vec<String> {
    let mut lines: Vec<String> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn bracketed(args: &[&str]) -> String {
    args.iter().map(|a| format!("[{a}]")).collect()
}

#[tokio::test]
async fn preflight_runs_only_the_two_version_commands() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let info = preflight(&runner(&log, "28.3.2", "2.39.1")).await;
    assert_eq!(info.status, DockerStatus::Ready, "{info:?}");

    let mut expected = vec![
        bracketed(ENGINE_VERSION_ARGS),
        bracketed(COMPOSE_VERSION_ARGS),
    ];
    expected.sort();
    assert_eq!(invocations(&log), expected);
    assert_eq!(
        expected,
        [
            "[compose][version][--short]",
            "[version][--format][{{.Server.Version}}]"
        ]
    );
}

#[tokio::test]
async fn ready_reports_both_versions() {
    let dir = tempfile::tempdir().unwrap();
    let info = preflight(&runner(&dir.path().join("log"), "28.3.2", "v2.39.1")).await;
    assert_eq!(info.status, DockerStatus::Ready);
    assert_eq!(info.engine_version.as_deref(), Some("28.3.2"));
    assert_eq!(info.compose_version.as_deref(), Some("v2.39.1"));
    assert_eq!(info.message, None);
}

#[tokio::test]
async fn unsupported_engine_names_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let info = preflight(&runner(&dir.path().join("log"), "20.10.24", "2.39.1")).await;
    assert_eq!(info.status, DockerStatus::UnsupportedVersion);
    assert_eq!(info.engine_version.as_deref(), Some("20.10.24"));
    let m = info.message.unwrap();
    assert!(m.contains(&info.supported.engine), "{m}");
}

#[tokio::test]
async fn missing_compose_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let info = preflight(&runner(&dir.path().join("log"), "28.3.2", "")).await;
    assert_eq!(info.status, DockerStatus::UnsupportedVersion);
    assert_eq!(info.compose_version, None);
    let m = info.message.unwrap();
    assert!(m.contains(&info.supported.compose), "{m}");
}

#[tokio::test]
async fn failing_engine_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let info = preflight(&runner(&dir.path().join("log"), "", "2.39.1")).await;
    assert_eq!(info.status, DockerStatus::Unreachable);
    assert!(info.message.unwrap().contains("Cannot connect"));
}

#[tokio::test]
async fn missing_cli_is_unreachable() {
    let info = preflight(&Runner::new("/nonexistent/ship-test/docker")).await;
    assert_eq!(info.status, DockerStatus::Unreachable);
    let m = info.message.unwrap();
    assert!(m.contains("not found"), "{m}");
}

/// The real CLI when installed, else none; either way a socket that does
/// not exist must read as `unreachable`, never as anything else.
#[tokio::test]
async fn missing_socket_is_unreachable() {
    let runner = Runner::docker().env("DOCKER_HOST", "unix:///nonexistent/ship-test/docker.sock");
    let info = preflight(&runner).await;
    assert_eq!(info.status, DockerStatus::Unreachable, "{info:?}");
    assert_eq!(info.engine_version, None);
    assert!(info.message.is_some());
}
