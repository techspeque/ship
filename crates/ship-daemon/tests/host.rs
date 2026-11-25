//! `/api/v1/host`: Docker preflight and capacity over both listeners.

mod common;

use common::{Via, get};
use ship_core::{API_VERSION, DockerStatus, HostInfo};
use ship_docker::Runner;

async fn started_with(docker: Runner) -> (tempfile::TempDir, ship_daemon::Running) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::config(dir.path());
    config.docker = docker;
    let running = ship_daemon::start(&config).await.unwrap();
    (dir, running)
}

fn both(running: &ship_daemon::Running) -> [Via; 2] {
    [
        Via::Unix(running.socket_path().to_path_buf()),
        Via::Tcp(running.tcp_addr()),
    ]
}

async fn host(via: &Via) -> HostInfo {
    let r = get(via, "/api/v1/host").await;
    assert_eq!(r.status, 200, "{via:?}: {}", r.body);
    serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.body))
}

#[tokio::test]
async fn ready_reports_versions_and_capacity() {
    let (_dir, running) = started_with(common::fake_docker("28.3.2", "2.39.1")).await;
    for via in both(&running) {
        let info = host(&via).await;
        assert_eq!(info.api_version, API_VERSION);
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.os, std::env::consts::OS);
        assert_eq!(info.docker.status, DockerStatus::Ready, "{info:?}");
        assert_eq!(info.docker.engine_version.as_deref(), Some("28.3.2"));
        assert_eq!(info.docker.compose_version.as_deref(), Some("2.39.1"));
        assert_eq!(info.docker.message, None);
        assert!(info.capacity.cpus.is_some_and(|n| n >= 1), "{info:?}");
        assert!(
            info.capacity.disk_free_bytes.is_some_and(|n| n > 0),
            "{info:?}"
        );
        if cfg!(target_os = "linux") {
            assert!(
                info.capacity.memory_bytes.is_some_and(|n| n > 0),
                "{info:?}"
            );
        }
    }
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn unreachable_docker_is_reported_and_the_daemon_stays_up() {
    // common::config points DOCKER_HOST at a socket that does not exist.
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();
    for via in both(&running) {
        let info = host(&via).await;
        assert_eq!(info.docker.status, DockerStatus::Unreachable, "{info:?}");
        assert_eq!(info.docker.engine_version, None);
        let message = info.docker.message.unwrap();
        assert!(message.contains("not reachable"), "{message}");
        assert!(message.contains("Start Docker"), "{message}");

        // Still serving, on the same connection type, afterwards.
        assert_eq!(get(&via, "/healthz").await.status, 200);
        assert_eq!(get(&via, "/api/v1/projects").await.status, 200);
        assert_eq!(host(&via).await.docker.status, DockerStatus::Unreachable);
    }
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_version_names_the_range() {
    let (_dir, running) = started_with(common::fake_docker("20.10.24", "2.39.1")).await;
    let info = host(&Via::Unix(running.socket_path().to_path_buf())).await;
    assert_eq!(info.docker.status, DockerStatus::UnsupportedVersion);
    assert_eq!(info.docker.engine_version.as_deref(), Some("20.10.24"));
    let message = info.docker.message.unwrap();
    assert!(message.contains(&info.docker.supported.engine), "{message}");
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_endpoint_obeys_the_host_guard() {
    let (_dir, running) = started_with(common::fake_docker("28.3.2", "2.39.1")).await;
    let r = common::send(
        &Via::Tcp(running.tcp_addr()),
        "GET",
        "/api/v1/host",
        &["Host: evil.example"],
    )
    .await;
    assert_eq!(r.status, 403);
    running.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn real_docker_is_ready() {
    let (_dir, running) = started_with(Runner::docker()).await;
    let info = host(&Via::Unix(running.socket_path().to_path_buf())).await;
    eprintln!("host: {info:?}");
    assert_eq!(info.docker.status, DockerStatus::Ready, "{info:?}");
    assert!(info.docker.engine_version.is_some());
    assert!(info.docker.compose_version.is_some());
    running.shutdown().await.unwrap();
}
