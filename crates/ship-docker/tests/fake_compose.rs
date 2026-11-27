//! The Compose adapter end to end against a fake `docker` that records its
//! argv, and against the real CLI pointed at a socket that does not exist.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use ship_core::{EnvironmentId, ProjectId, ReleaseId, ServiceId};
use ship_docker::argv::{
    DENIED, compose_build, compose_down, compose_stop, container_logs, project_container_ids,
    project_network_ids,
};
use ship_docker::compose;
use ship_docker::labels::ShipLabels;
use ship_docker::{
    ComposeFiles, ComposeProject, DockerError, Line, LogOptions, Runner, Selector, Stream,
    containers,
};

/// A fake `docker`: appends each invocation's arguments, one `[arg]` each,
/// as one line to `$FAKE_DOCKER_LOG`. With `$FAKE_STDERR` set it prints it
/// to stderr and exits 1. For `logs` it prints one line to each stream.
/// A container listing (`ps`, not `network ls`) prints `$FAKE_IDS` and
/// `container inspect` prints `$FAKE_INSPECT`. Anything else succeeds with
/// no output (an empty project).
const SCRIPT: &str = r#"#!/bin/sh
line=""
for a in "$@"; do line="$line[$a]"; done
printf '%s\n' "$line" >> "$FAKE_DOCKER_LOG"
if [ -n "$FAKE_STDERR" ]; then
  echo "$FAKE_STDERR" >&2
  exit 1
fi
first="$1"
for a in "$@"; do
  case "$a" in
    logs)
      echo "out-line"
      echo "err-line" >&2
      exit 0;;
    ps)
      [ -n "$FAKE_IDS" ] && printf '%s\n' "$FAKE_IDS"
      exit 0;;
    inspect)
      [ "$first" = container ] && printf '%s\n' "${FAKE_INSPECT:-[]}"
      exit 0;;
  esac
done
exit 0
"#;

fn fake_docker() -> &'static Path {
    static PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PATH
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("docker");
            std::fs::write(&path, SCRIPT).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

fn runner(log: &Path, stderr: &str) -> Runner {
    Runner::new(fake_docker())
        .env("FAKE_DOCKER_LOG", log)
        .env("FAKE_STDERR", stderr)
}

fn invocations(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn bracketed(argv: &ship_docker::Argv) -> String {
    argv.args()
        .iter()
        .map(|a| format!("[{}]", a.to_string_lossy()))
        .collect()
}

fn project() -> ComposeProject {
    ComposeProject::new(EnvironmentId::from_parts(1_700_000_000_000, 42).unwrap())
}

const T: Duration = Duration::from_secs(10);
const NO: &[&str] = &[];

const OURS: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FOREIGN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// One running container of `service`; with Ship's labels for the
/// project's environment when `ours`, with Compose's labels only otherwise.
fn container_json(id: &str, service: &str, ours: bool) -> serde_json::Value {
    let p = project();
    let mut labels = serde_json::json!({
        "com.docker.compose.project": p.name(),
        "com.docker.compose.service": service,
    });
    if ours {
        let ship = ShipLabels {
            project: ProjectId::from_parts(1_700_000_000_000, 1).unwrap(),
            environment: p.environment(),
            service: Some(ServiceId::from_parts(1_700_000_000_000, 3).unwrap()),
            release: ReleaseId::from_parts(1_700_000_000_000, 4).unwrap(),
        };
        for (k, v) in ship.pairs() {
            labels[k] = v.into();
        }
    }
    serde_json::json!({
        "Id": id,
        "Name": format!("/{}-{service}-1", p.name()),
        "State": { "Status": "running", "ExitCode": 0 },
        "Config": { "Labels": labels },
    })
}

/// A fake whose project holds one container of Ship's (`web`) and, when
/// `foreign`, one without Ship's labels (`intruder`).
fn mixed_runner(log: &Path, foreign: bool) -> Runner {
    let mut ids = vec![OURS];
    let mut items = vec![container_json(OURS, "web", true)];
    if foreign {
        ids.push(FOREIGN);
        items.push(container_json(FOREIGN, "intruder", false));
    }
    runner(log, "")
        .env("FAKE_IDS", ids.join("\n"))
        .env("FAKE_INSPECT", serde_json::Value::Array(items).to_string())
}

fn files() -> ComposeFiles {
    ComposeFiles::new(
        "/srv/ship/snap",
        [PathBuf::from("/srv/ship/snap/compose.yaml")],
    )
    .unwrap()
}

#[tokio::test]
async fn a_foreign_container_prevents_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let p = project();
    let err = compose::build(&mixed_runner(&log, true), &p, &files(), NO, T, &mut |_| {})
        .await
        .unwrap_err();
    match err {
        DockerError::Foreign { kind, id } => {
            assert_eq!(kind, "container");
            assert_eq!(id, FOREIGN);
        }
        other => panic!("{other:?}"),
    }
    let build = bracketed(&compose_build(&p, &files(), NO).unwrap());
    let calls = invocations(&log);
    assert!(!calls.contains(&build), "{calls:?}");

    // With only Ship's containers the build runs.
    let log = dir.path().join("argv-ours.log");
    compose::build(&mixed_runner(&log, false), &p, &files(), NO, T, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(invocations(&log).last(), Some(&build));
}

#[tokio::test]
async fn stop_checks_labels_then_stops() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let p = project();
    compose::stop(&runner(&log, ""), &p, &["web"], T, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        invocations(&log),
        [
            bracketed(&project_container_ids(&p).unwrap()),
            bracketed(&project_network_ids(&p).unwrap()),
            bracketed(&compose_stop(&p, &["web"]).unwrap()),
        ]
    );
}

#[tokio::test]
async fn down_never_passes_a_volume_flag() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let p = project();
    compose::down(&runner(&log, ""), &p, T, &mut |_| {})
        .await
        .unwrap();
    let calls = invocations(&log);
    assert_eq!(
        calls.last().unwrap(),
        &bracketed(&compose_down(&p).unwrap())
    );
    for call in &calls {
        for bad in DENIED {
            assert!(!call.contains(&format!("[{bad}]")), "{call}");
        }
    }
}

#[tokio::test]
async fn logs_keep_stdout_and_stderr_apart() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines: Vec<Line> = Vec::new();
    let log = dir.path().join("log");
    compose::logs(
        &mixed_runner(&log, false),
        &project(),
        &["web"],
        LogOptions::default(),
        Some(T),
        &mut |l| lines.push(l),
    )
    .await
    .unwrap();
    let of = |s| {
        lines
            .iter()
            .filter(|l| l.stream == s)
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(of(Stream::Stdout), ["web-1  | out-line"]);
    assert_eq!(of(Stream::Stderr), ["web-1  | err-line"]);
    assert_eq!(
        invocations(&log).last(),
        Some(&bracketed(
            &container_logs(OURS, LogOptions::default()).unwrap()
        ))
    );
}

#[tokio::test]
async fn reads_of_a_mixed_project_return_only_ships_containers() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let r = mixed_runner(&log, true);
    let p = project();
    assert_eq!(compose::ps(&r, &p).await.unwrap(), [OURS]);

    let mut lines: Vec<Line> = Vec::new();
    compose::logs(&r, &p, NO, LogOptions::default(), Some(T), &mut |l| {
        lines.push(l)
    })
    .await
    .unwrap();
    assert!(
        lines.iter().all(|l| l.text.starts_with("web-1  | ")),
        "{lines:?}"
    );
    let calls = invocations(&log);
    assert!(
        calls.contains(&bracketed(
            &container_logs(OURS, LogOptions::default()).unwrap()
        )),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("[logs]") && c.contains(FOREIGN)),
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_daemon_error_is_failed_not_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let r = runner(
        &dir.path().join("log"),
        "Error response from daemon: conflict: unable to remove",
    );
    let err = compose::stop(&r, &project(), NO, T, &mut |_| {})
        .await
        .unwrap_err();
    match err {
        DockerError::Failed { detail, .. } => assert!(detail.contains("conflict"), "{detail}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_refused_connection_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let r = runner(
        &dir.path().join("log"),
        "Cannot connect to the Docker daemon at unix:///fake.sock. Is the docker daemon running?",
    );
    let sel = Selector::environment(project().environment());
    assert!(containers(&r, &sel).await.unwrap_err().is_unreachable());
    assert!(
        compose::ps(&r, &project())
            .await
            .unwrap_err()
            .is_unreachable()
    );
    let err = compose::down(&r, &project(), T, &mut |_| {})
        .await
        .unwrap_err();
    assert!(err.is_unreachable(), "{err:?}");
}

#[tokio::test]
async fn a_missing_cli_is_unreachable() {
    let r = Runner::new("/nonexistent/ship-test/docker");
    let sel = Selector::environment(project().environment());
    assert!(containers(&r, &sel).await.unwrap_err().is_unreachable());
    let err = compose::stop(&r, &project(), NO, T, &mut |_| {})
        .await
        .unwrap_err();
    assert!(err.is_unreachable(), "{err:?}");
}

/// The real CLI when installed, else none; either way a socket that does
/// not exist must read as unreachable, never as a failed command.
#[tokio::test]
async fn a_missing_socket_is_unreachable() {
    let r = Runner::docker().env("DOCKER_HOST", "unix:///nonexistent/ship-test/docker.sock");
    let sel = Selector::environment(project().environment());
    let err = containers(&r, &sel).await.unwrap_err();
    assert!(err.is_unreachable(), "{err:?}");
    let err = compose::ps(&r, &project()).await.unwrap_err();
    assert!(err.is_unreachable(), "{err:?}");
}
