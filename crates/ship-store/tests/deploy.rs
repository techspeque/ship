//! The store functions a Dockerfile deployment needs (phase-1-ws-1.7):
//! settings, routes, get-or-create of a service and releases recorded
//! under an id allocated before the build.

use ship_core::{
    DeploymentId, DeploymentResult, EnvironmentId, OperationId, ProjectId, ReleaseId, RevisionId,
    RevisionKind, RouteId, RouteProvider, RouteStatus, ServiceId, ServiceKind, Slug,
};
use ship_store::{
    DATABASE_FILE, LATEST_VERSION, NewDeployment, NewOperation, NewRelease, NewRevision, NewRoute,
    NewService, SettingScope, Store, StoreError,
};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{AssertSqlSafe, ConnectOptions, Connection};

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
async fn a_version_2_database_migrates_to_the_latest_version_with_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let ts = 1_767_225_600_000_u64;
    let prj = ProjectId::from_parts(ts, 1).unwrap().to_string();
    let env = EnvironmentId::from_parts(ts, 2).unwrap().to_string();
    let op = OperationId::from_parts(ts, 3).unwrap().to_string();
    let rev = RevisionId::from_parts(ts, 4).unwrap().to_string();
    let rel = ReleaseId::from_parts(ts, 5).unwrap().to_string();
    let dep = DeploymentId::from_parts(ts, 6).unwrap().to_string();
    let svc = ServiceId::from_parts(ts, 7).unwrap().to_string();
    let rte = RouteId::from_parts(ts, 8).unwrap().to_string();

    // A version 2 database as phase-1-ws-1.2 left it: the Phase 0 schema
    // with rows, migration 0002 run over them, then a route.
    let mut conn = SqliteConnectOptions::new()
        .filename(dir.path().join(DATABASE_FILE))
        .foreign_keys(true)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
        .execute(&mut conn)
        .await
        .unwrap();
    let seed = format!(
        "INSERT INTO project (id, slug, created_at) VALUES ('{prj}', 'app', {ts});
         INSERT INTO environment (id, project_id, name, desired_state, created_at)
             VALUES ('{env}', '{prj}', 'development', 'running', {ts});
         INSERT INTO service (id, environment_id, name, kind, created_at)
             VALUES ('{svc}', '{env}', 'web', 'web', {ts});
         INSERT INTO operation (id, idempotency_key, kind, environment_id, phase, status,
                                created_at, updated_at)
             VALUES ('{op}', 'k', 'push', '{env}', 'succeeded', 'succeeded', {ts}, {ts});
         INSERT INTO revision (id, project_id, kind, source_ref, created_at)
             VALUES ('{rev}', '{prj}', 'snapshot', 'sha256:aa', {ts});
         INSERT INTO release (id, revision_id, images, config_fingerprint, created_at)
             VALUES ('{rel}', '{rev}', '[\"sha256:img\"]', 'cfg', {ts});
         INSERT INTO deployment (id, environment_id, release_id, operation_id, result,
                                 started_at, ended_at)
             VALUES ('{dep}', '{env}', '{rel}', '{op}', 'succeeded', {ts}, {ts});"
    );
    sqlx::raw_sql(AssertSqlSafe(seed))
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0002_phase1.sql"))
        .execute(&mut conn)
        .await
        .unwrap();
    let route = format!(
        "INSERT INTO route (id, environment_id, service_id, provider, address, status,
                            created_at, updated_at)
             VALUES ('{rte}', '{env}', '{svc}', 'port', '127.0.0.1:8080', 'confirmed', {ts}, {ts});
         PRAGMA user_version = 2;"
    );
    sqlx::raw_sql(AssertSqlSafe(route))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let report = store.open_report().clone();
    assert_eq!(report.from_version, 2);
    assert_eq!(report.to_version, 4);
    assert_eq!(LATEST_VERSION, 4);
    let backup = report.backup.expect("a backup was taken");

    // Every row survived.
    let projects = store.list_projects().await.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id.to_string(), prj);
    let env_id: EnvironmentId = env.parse().unwrap();
    let services = store.list_services(env_id).await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].id.to_string(), svc);
    let d = store.get_deployment(dep.parse().unwrap()).await.unwrap();
    assert_eq!(d.revision_id.to_string(), rev);
    assert_eq!(d.release_id.map(|r| r.to_string()), Some(rel.clone()));
    assert_eq!(d.result, Some(DeploymentResult::Succeeded));
    let routes = store.list_routes(env_id).await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id.to_string(), rte);
    assert_eq!(routes[0].address, "127.0.0.1:8080");
    assert_eq!(routes[0].status, RouteStatus::Confirmed);

    // Settings storage is there and works.
    assert_eq!(store.host_setting("publish.address").await.unwrap(), None);
    let set = store
        .set_host_setting("publish.address", "127.0.0.1", "operator")
        .await
        .unwrap();
    assert_eq!(
        store.host_setting("publish.address").await.unwrap(),
        Some(set)
    );
    store.close().await;

    // The backup is the version 2 database, with its rows and no settings.
    let mut conn = SqliteConnectOptions::new()
        .filename(&backup)
        .connect()
        .await
        .unwrap();
    let v: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(v, 2);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM route")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'setting'",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(tables, 0);
    conn.close().await.unwrap();

    // Reopening is clean, takes no second backup and keeps the setting.
    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.open_report().from_version, 4);
    assert_eq!(store.open_report().backup, None);
    assert_eq!(
        store
            .host_setting("publish.address")
            .await
            .unwrap()
            .unwrap()
            .value,
        "127.0.0.1"
    );
    store.close().await;
}

#[tokio::test]
async fn host_settings_are_stored_and_replaced() {
    let (_dir, store) = open().await;
    assert_eq!(store.host_setting("publish.address").await.unwrap(), None);

    let first = store
        .set_host_setting("publish.address", "127.0.0.1", "operator")
        .await
        .unwrap();
    assert_eq!(first.value, "127.0.0.1");
    assert_eq!(
        store.host_setting("publish.address").await.unwrap(),
        Some(first)
    );

    let second = store
        .set_host_setting("publish.address", "auto", "operator")
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
                store.set_host_setting(key, value, "operator").await,
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
async fn project_settings_win_over_the_host() {
    let (_dir, store) = open().await;
    let a = store
        .get_or_create_project(&Slug::parse("a").unwrap())
        .await
        .unwrap()
        .project;
    let b = store
        .get_or_create_project(&Slug::parse("b").unwrap())
        .await
        .unwrap()
        .project;
    let key = "compose.confirm";
    assert_eq!(store.effective_setting(a.id, key).await.unwrap(), None);

    let host = store
        .set_host_setting(key, "ask", "operator")
        .await
        .unwrap();
    assert_eq!(host.scope, SettingScope::Host);
    assert_eq!(host.updated_by, "operator");
    let own = store
        .set_setting(SettingScope::Project(a.id), key, "accept", "adam")
        .await
        .unwrap();
    assert_eq!(own.scope, SettingScope::Project(a.id));
    assert_eq!(
        store.effective_setting(a.id, key).await.unwrap(),
        Some(own.clone())
    );
    assert_eq!(
        store.effective_setting(b.id, key).await.unwrap(),
        Some(host.clone())
    );
    assert_eq!(store.host_setting(key).await.unwrap(), Some(host));
    assert_eq!(
        store
            .setting(SettingScope::Project(b.id), key)
            .await
            .unwrap(),
        None
    );

    // Replacing a project's value leaves one row; the actor is kept.
    let again = store
        .set_setting(SettingScope::Project(a.id), key, "ask", "operator")
        .await
        .unwrap();
    assert_eq!(
        store
            .setting(SettingScope::Project(a.id), key)
            .await
            .unwrap(),
        Some(again)
    );
    // An unknown project and an empty actor are refused.
    let ghost = ProjectId::from_parts(1, 1).unwrap();
    assert!(
        store
            .set_setting(SettingScope::Project(ghost), key, "ask", "operator")
            .await
            .is_err()
    );
    assert!(matches!(
        store.set_host_setting(key, "ask", "").await,
        Err(StoreError::Invalid { .. })
    ));
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
