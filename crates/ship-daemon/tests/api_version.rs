//! The API version handshake (OVERVIEW §8): version headers on `/api/`
//! responses, `426 unsupported_api_version` for a client speaking another
//! version, and nothing on `/healthz`.

mod common;

use common::{Via, get, send};
use ship_core::API_VERSION;

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

#[tokio::test]
async fn api_responses_carry_versions_and_healthz_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();
    let via = Via::Unix(running.socket_path().to_path_buf());

    for path in ["/api/v1/projects", "/api/v1/host", "/api/v1/nope"] {
        let r = get(&via, path).await;
        assert_eq!(
            header(&r.head, "ship-api-version"),
            Some(API_VERSION),
            "{path}: {}",
            r.head
        );
        assert_eq!(
            header(&r.head, "ship-version"),
            Some(env!("CARGO_PKG_VERSION")),
            "{path}: {}",
            r.head
        );
    }

    let r = get(&via, "/healthz").await;
    assert_eq!(r.status, 200);
    assert_eq!(header(&r.head, "ship-api-version"), None, "{}", r.head);
    assert_eq!(header(&r.head, "ship-version"), None, "{}", r.head);

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn other_api_version_is_refused_with_426() {
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();
    let via = Via::Tcp(running.tcp_addr());

    let r = send(
        &via,
        "GET",
        "/api/v1/projects",
        &["Host: localhost", "Ship-Api-Version: v0"],
    )
    .await;
    assert_eq!(r.status, 426, "{}", r.head);
    let v = r.json();
    assert_eq!(v["code"], "unsupported_api_version");
    assert_eq!(v["details"]["client_api_version"], "v0");
    assert_eq!(v["details"]["daemon_api_version"], API_VERSION);
    assert_eq!(v["details"]["daemon_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["operation_id"], serde_json::Value::Null);
    assert_eq!(header(&r.head, "ship-api-version"), Some(API_VERSION));

    // The matching version and no version at all are both served.
    let r = send(
        &via,
        "GET",
        "/api/v1/projects",
        &["Host: localhost", "Ship-Api-Version: v1"],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.json(), serde_json::json!({ "projects": [] }));
    let r = get(&via, "/api/v1/projects").await;
    assert_eq!(r.status, 200, "{}", r.head);

    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn origin_guard_rejections_on_api_carry_versions() {
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();
    let via = Via::Tcp(running.tcp_addr());

    // Rejected for Host, and for a cross-origin mutation: still refused,
    // and still carrying both version headers.
    for (method, headers) in [
        ("GET", &["Host: evil.example"][..]),
        (
            "POST",
            &["Host: localhost", "Origin: http://evil.example"][..],
        ),
    ] {
        let r = send(&via, method, "/api/v1/projects", headers).await;
        assert_eq!(r.status, 403, "{method} {headers:?}: {}", r.head);
        assert_eq!(r.json()["code"], "forbidden", "{}", r.body);
        assert_eq!(header(&r.head, "ship-api-version"), Some(API_VERSION));
        assert_eq!(
            header(&r.head, "ship-version"),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    // A rejected non-API request stays free of version headers.
    let r = send(&via, "GET", "/healthz", &["Host: evil.example"]).await;
    assert_eq!(r.status, 403, "{}", r.head);
    assert_eq!(header(&r.head, "ship-api-version"), None, "{}", r.head);
    assert_eq!(header(&r.head, "ship-version"), None, "{}", r.head);

    running.shutdown().await.unwrap();
}
