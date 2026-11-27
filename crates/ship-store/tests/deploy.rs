//! The store functions a Dockerfile deployment needs (phase-1-ws-1.7):
//! settings, routes, get-or-create of a service and releases recorded
//! under an id allocated before the build.

use ship_core::{DeploymentResult, RevisionKind, RouteProvider, RouteStatus, ServiceKind, Slug};
use ship_store::{
    NewDeployment, NewOperation, NewRelease, NewRevision, NewRoute, NewService, Store, StoreError,
};

async fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
}

fn web() -> NewService {
    NewService {
        name: "web".to_owned(),
        kind: ServiceKind::Web,
        port: None,
    }
}

#[tokio::test]
async fn host_settings_are_stored_and_replaced() {
    let (_dir, store) = open().await;
    assert_eq!(store.host_setting("publish.address").await.unwrap(), None);

    let first = store
        .set_host_setting("publish.address", "127.0.0.1")
        .await
        .unwrap();
    assert_eq!(first.value, "127.0.0.1");
    assert_eq!(
        store.host_setting("publish.address").await.unwrap(),
        Some(first)
    );

    let second = store
        .set_host_setting("publish.address", "auto")
        .await
        .unwrap();
    let read = store
        .host_setting("publish.address")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read, second);
    assert_eq!(read.value, "auto");
    assert_eq!(store.host_setting("compose.confirm").await.unwrap(), None);

    let long = "v".repeat(257);
    for (key, value) in [("", "x"), ("k", ""), ("k", long.as_str())] {
        assert!(
            matches!(
                store.set_host_setting(key, value).await,
                Err(StoreError::Invalid { .. })
            ),
            "{key:?}"
        );
    }
    // A refused write leaves the old value.
    assert_eq!(
        store
            .host_setting("publish.address")
            .await
            .unwrap()
            .unwrap()
            .value,
        "auto"
    );
    store.close().await;
}

#[tokio::test]
async fn routes_are_replaced_as_a_whole() {
    let (_dir, store) = open().await;
    let a = store
        .get_or_create_project(&Slug::parse("a").unwrap())
        .await
        .unwrap();
    let b = store
        .get_or_create_project(&Slug::parse("b").unwrap())
        .await
        .unwrap();
    let env = a.environment.id;
    let svc = store.ensure_service(env, &web()).await.unwrap();
    let other = store
        .ensure_service(b.environment.id, &web())
        .await
        .unwrap();
    assert_eq!(store.list_routes(env).await.unwrap(), []);

    let route = |address: &str| NewRoute {
        service_id: svc.id,
        provider: RouteProvider::Port,
        address: address.to_owned(),
        status: RouteStatus::Confirmed,
    };
    let first = store
        .replace_routes(env, &[route("127.0.0.1:8080")])
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].address, "127.0.0.1:8080");
    assert_eq!(first[0].environment_id, env);
    assert_eq!(store.list_routes(env).await.unwrap(), first);

    let second = store
        .replace_routes(env, &[route("100.64.0.7:9090")])
        .await
        .unwrap();
    let listed = store.list_routes(env).await.unwrap();
    assert_eq!(listed, second);
    assert_eq!(listed[0].address, "100.64.0.7:9090");
    assert_ne!(listed[0].id, first[0].id);

    // A service of another environment is refused, and nothing changes.
    let foreign = NewRoute {
        service_id: other.id,
        ..route("127.0.0.1:1")
    };
    assert!(matches!(
        store.replace_routes(env, &[foreign]).await,
        Err(StoreError::Invalid { .. })
    ));
    assert_eq!(store.list_routes(env).await.unwrap(), second);

    // An empty list clears them.
    assert_eq!(store.replace_routes(env, &[]).await.unwrap(), []);
    assert_eq!(store.list_routes(env).await.unwrap(), []);
    store.close().await;
}

#[tokio::test]
async fn ensure_service_is_one_row_per_name() {
    let (_dir, store) = open().await;
    let p = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap();
    let env = p.environment.id;
    let one = store.ensure_service(env, &web()).await.unwrap();
    let again = NewService {
        port: Some(3000),
        ..web()
    };
    let two = store.ensure_service(env, &again).await.unwrap();
    assert_eq!(one, two);
    assert_eq!(store.list_services(env).await.unwrap(), [one]);
    store.close().await;
}

#[tokio::test]
async fn a_release_is_recorded_under_its_allocated_id() {
    let (_dir, store) = open().await;
    let p = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap();
    let revision = store
        .create_revision(
            p.project.id,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: &"ab".repeat(32),
                manifest_fingerprint: None,
            },
        )
        .await
        .unwrap();
    let id = store.allocate_release_id().unwrap();
    assert_ne!(id, store.allocate_release_id().unwrap());
    let images = vec!["sha256:abc".to_owned()];
    let new = NewRelease {
        images: &images,
        config_fingerprint: "sha256:def",
    };
    let release = store
        .create_release_as(id, revision.id, &new)
        .await
        .unwrap();
    assert_eq!(release.id, id);
    assert_eq!(store.get_release(id).await.unwrap(), release);
    assert!(matches!(
        store.create_release_as(id, revision.id, &new).await,
        Err(StoreError::Conflict { .. })
    ));

    // A deployment takes it as its release and succeeds with it.
    let op = store
        .create_operation(&NewOperation {
            idempotency_key: "k",
            kind: "deploy",
            environment_id: Some(p.environment.id),
        })
        .await
        .unwrap()
        .operation;
    let d = store
        .create_deployment(&NewDeployment {
            environment_id: p.environment.id,
            revision_id: revision.id,
            operation_id: op.id,
            previous_release_id: None,
        })
        .await
        .unwrap();
    store.set_deployment_release(d.id, id).await.unwrap();
    let done = store
        .finish_deployment(d.id, DeploymentResult::Succeeded)
        .await
        .unwrap();
    assert_eq!(done.release_id, Some(id));
    store.close().await;
}
