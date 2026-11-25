//! The daemon's HTTP surface over both listeners.

mod common;

use common::{Via, get, send};
use ship_core::{ProjectList, Slug};

async fn started() -> (tempfile::TempDir, ship_daemon::Running) {
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();
    (dir, running)
}

fn both(running: &ship_daemon::Running) -> [Via; 2] {
    [
        Via::Unix(running.socket_path().to_path_buf()),
        Via::Tcp(running.tcp_addr()),
    ]
}

#[tokio::test]
async fn healthz_reveals_nothing() {
    let (dir, running) = started().await;
    for via in both(&running) {
        let r = get(&via, "/healthz").await;
        assert_eq!(r.status, 200, "{via:?}");
        assert_eq!(r.json(), serde_json::json!({ "status": "ok" }));

        let lower = r.body.to_lowercase();
        let host = std::process::Command::new("hostname")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_lowercase())
            .unwrap_or_default();
        if !host.is_empty() {
            assert!(!lower.contains(&host), "{}", r.body);
        }
        for leak in [
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            "/",
            &dir.path().display().to_string(),
        ] {
            assert!(!r.body.contains(leak), "{leak} in {}", r.body);
        }
        // The headers carry no server identification either.
        assert!(!r.head.to_lowercase().contains("server:"), "{}", r.head);
    }
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn projects_returns_the_registry() {
    let (_dir, running) = started().await;
    for via in both(&running) {
        let r = get(&via, "/api/v1/projects").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json(), serde_json::json!({ "projects": [] }));
    }

    let created = running
        .store()
        .create_project(&Slug::parse("my-app").unwrap())
        .await
        .unwrap();
    for via in both(&running) {
        let r = get(&via, "/api/v1/projects").await;
        assert_eq!(r.status, 200);
        let list: ProjectList = serde_json::from_str(&r.body).unwrap();
        assert_eq!(list.projects.len(), 1);
        assert_eq!(list.projects[0].id, created.id);
        assert_eq!(list.projects[0].slug, created.slug);
        assert_eq!(list.projects[0].created_at, created.created_at);
    }
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_path_is_api_error() {
    let (_dir, running) = started().await;
    let r = get(&Via::Tcp(running.tcp_addr()), "/api/v1/nope").await;
    assert_eq!(r.status, 404);
    let v = r.json();
    assert_eq!(v["code"], "not_found");
    assert!(v["message"].is_string());
    assert_eq!(v["operation_id"], serde_json::Value::Null);
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_origin_post_is_rejected() {
    let (_dir, running) = started().await;
    let tcp = running.tcp_addr();
    let host = format!("Host: 127.0.0.1:{}", tcp.port());
    let same = format!("Origin: http://127.0.0.1:{}", tcp.port());

    for via in both(&running) {
        for extra in [
            "Origin: http://evil.example",
            "Origin: null",
            "Origin: https://127.0.0.1",
            "Sec-Fetch-Site: cross-site",
        ] {
            for method in ["POST", "PUT", "PATCH", "DELETE"] {
                let r = send(&via, method, "/api/v1/projects", &[&host, extra]).await;
                assert_eq!(r.status, 403, "{method} {extra} via {via:?}");
                let v = r.json();
                assert_eq!(v["code"], "forbidden");
            }
        }

        // No Origin (a CLI client) and a same-origin browser request pass
        // the guard and reach routing, which has no POST here yet.
        for headers in [vec![host.as_str()], vec![host.as_str(), same.as_str()]] {
            let r = send(&via, "POST", "/api/v1/projects", &headers).await;
            assert_eq!(r.status, 405, "{headers:?} via {via:?}");
        }
    }
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn tcp_bind_failure_leaves_no_socket() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut config = common::config(dir.path());
    config.port = taken.local_addr().unwrap().port();

    let err = ship_daemon::start(&config).await.unwrap_err();
    assert!(
        matches!(err, ship_daemon::DaemonError::Loopback(..)),
        "{err}"
    );
    assert!(!config.socket_path.exists());
}

#[tokio::test]
async fn second_daemon_on_live_socket_is_refused() {
    let (dir, running) = started().await;
    let mut config = common::config(dir.path());
    config.state_dir = dir.path().join("other-state");

    let err = ship_daemon::start(&config).await.unwrap_err();
    assert!(matches!(err, ship_daemon::DaemonError::Socket(_)), "{err}");
    // The first daemon still answers on its socket.
    let r = get(&Via::Unix(running.socket_path().to_path_buf()), "/healthz").await;
    assert_eq!(r.status, 200);
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn non_loopback_host_is_rejected() {
    let (_dir, running) = started().await;
    for via in both(&running) {
        for host in ["Host: evil.example", "Host: evil.example:7180"] {
            let r = send(&via, "GET", "/api/v1/projects", &[host]).await;
            assert_eq!(r.status, 403, "{host} via {via:?}");
            assert_eq!(r.json()["code"], "forbidden");
        }
        let r = send(&via, "GET", "/healthz", &["Host: rebound.example"]).await;
        assert_eq!(r.status, 403);
    }
    running.shutdown().await.unwrap();
}
