//! The store for lifecycle commands (phase-1-ws-1.12), against real SQLite
//! files: a desired state per service, the removal recorded in the
//! deployment history, and the only way out of `removed`.

use ship_core::{
    DeploymentAction, DeploymentId, DeploymentResult, DesiredState, EnvironmentId, OperationId,
    ProjectId, ReleaseId, RevisionId, RevisionKind, ServiceId, ServiceKind, Slug,
};
use ship_store::{
    DATABASE_FILE, Deployment, Environment, LATEST_VERSION, NewDeployment, NewOperation,
    NewRelease, NewRevision, NewService, Store, StoreError,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{AssertSqlSafe, ConnectOptions};

async fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
}

async fn raw(dir: &tempfile::TempDir) -> SqliteConnection {
    SqliteConnectOptions::new()
        .filename(dir.path().join(DATABASE_FILE))
        .foreign_keys(true)
        .connect()
        .await
        .unwrap()
}

fn service(name: &str) -> NewService {
    NewService {
        name: name.to_owned(),
        kind: ServiceKind::Web,
        port: None,
    }
}

async fn environment(store: &Store) -> Environment {
    store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment
}

/// A deployment of a new revision under a new operation, succeeded with a
/// release unless `fail`.
async fn deployed(store: &Store, env: &Environment, key: &str, fail: bool) -> Deployment {
    let revision = store
        .create_revision(
            env.project_id,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: key,
                manifest_fingerprint: None,
            },
        )
        .await
        .unwrap();
    let op = operation(store, env, key).await;
    let d = store
        .create_deployment_with(
            &NewDeployment {
                environment_id: env.id,
                revision_id: revision.id,
                operation_id: op,
                previous_release_id: None,
            },
            Some(r#"{"file":null,"port":"8080"}"#),
        )
        .await
        .unwrap();
    assert_eq!(d.action, DeploymentAction::Deploy);
    if fail {
        return store
            .finish_deployment(d.id, DeploymentResult::Failed)
            .await
            .unwrap();
    }
    let release = store
        .create_release(
            revision.id,
            &NewRelease {
                images: &["sha256:img".to_owned()],
                config_fingerprint: "cfg",
            },
        )
        .await
        .unwrap();
    store
        .set_deployment_release(d.id, release.id)
        .await
        .unwrap();
    store
        .finish_deployment(d.id, DeploymentResult::Succeeded)
        .await
        .unwrap()
}

async fn operation(store: &Store, env: &Environment, key: &str) -> OperationId {
    store
        .create_operation(&NewOperation {
            idempotency_key: key,
            kind: "deploy",
            environment_id: Some(env.id),
        })
        .await
        .unwrap()
        .operation
        .id
}

async fn remove(store: &Store, env: &Environment, serving: &Deployment, key: &str) -> Deployment {
    let op = operation(store, env, key).await;
    let removal = store
        .record_removal(&NewDeployment {
            environment_id: env.id,
            revision_id: serving.revision_id,
            operation_id: op,
            previous_release_id: serving.release_id,
        })
        .await
        .unwrap();
    store
        .set_desired_state(env.id, DesiredState::Removed)
        .await
        .unwrap();
    removal
}

#[tokio::test]
async fn a_service_has_its_own_desired_state() {
    let (_dir, store) = open().await;
    let env = environment(&store).await;
    let web = store.create_service(env.id, &service("web")).await.unwrap();
    let worker = store
        .create_service(env.id, &service("worker"))
        .await
        .unwrap();
    assert_eq!(web.desired_state, DesiredState::Running);
    let stopped = store
        .set_service_desired_state(worker.id, DesiredState::Stopped)
        .await
        .unwrap();
    assert_eq!(stopped.desired_state, DesiredState::Stopped);
    // Asking again changes nothing.
    store
        .set_service_desired_state(worker.id, DesiredState::Stopped)
        .await
        .unwrap();
    let listed = store.list_services(env.id).await.unwrap();
    let states: Vec<DesiredState> = listed.iter().map(|s| s.desired_state).collect();
    assert_eq!(states, [DesiredState::Running, DesiredState::Stopped]);
    assert_eq!(
        store.get_service(worker.id).await.unwrap().desired_state,
        DesiredState::Stopped
    );
    // `ensure_service` returns the stored state.
    let again = store
        .ensure_service(env.id, &service("worker"))
        .await
        .unwrap();
    assert_eq!(again.desired_state, DesiredState::Stopped);

    // `removed` belongs to the environment.
    assert!(matches!(
        store
            .set_service_desired_state(web.id, DesiredState::Removed)
            .await,
        Err(StoreError::IllegalTransition { .. })
    ));
    let unknown = ServiceId::from_parts(1_767_225_600_000, 9).unwrap();
    assert!(matches!(
        store
            .set_service_desired_state(unknown, DesiredState::Stopped)
            .await,
        Err(StoreError::NotFound { .. })
    ));

    assert_eq!(store.set_services_running(env.id).await.unwrap(), 1);
    assert_eq!(store.set_services_running(env.id).await.unwrap(), 0);
    assert!(
        store
            .list_services(env.id)
            .await
            .unwrap()
            .iter()
            .all(|s| s.desired_state == DesiredState::Running)
    );
}

#[tokio::test]
async fn the_schema_refuses_a_service_removed() {
    let (dir, store) = open().await;
    let env = environment(&store).await;
    let web = store.create_service(env.id, &service("web")).await.unwrap();
    let mut conn = raw(&dir).await;
    let sql = format!(
        "UPDATE service SET desired_state = 'removed' WHERE id = '{}'",
        web.id
    );
    assert!(
        sqlx::query(AssertSqlSafe(sql))
            .execute(&mut conn)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_removal_is_a_deployment_with_no_release() {
    let (_dir, store) = open().await;
    let env = environment(&store).await;
    let first = deployed(&store, &env, "one", false).await;
    assert_eq!(
        first.options.as_deref(),
        Some(r#"{"file":null,"port":"8080"}"#)
    );
    let failed = deployed(&store, &env, "two", true).await;
    let removal = remove(&store, &env, &first, "rm").await;
    assert_eq!(removal.action, DeploymentAction::Remove);
    assert_eq!(removal.result, Some(DeploymentResult::Succeeded));
    assert_eq!(removal.release_id, None);
    assert_eq!(removal.options, None);
    assert_eq!(removal.previous_release_id, first.release_id);
    assert!(removal.ended_at.is_some());

    let history = store.list_deployments(env.id).await.unwrap();
    let ids: Vec<DeploymentId> = history.iter().map(|d| d.id).collect();
    assert_eq!(ids, [removal.id, failed.id, first.id]);
    assert_eq!(store.get_deployment(removal.id).await.unwrap(), removal);
    // A removal records no release, ever.
    let release = first.release_id.unwrap();
    assert!(
        store
            .set_deployment_release(removal.id, release)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_schema_keeps_removals_without_a_release_and_actions_fixed() {
    let (dir, store) = open().await;
    let env = environment(&store).await;
    let first = deployed(&store, &env, "one", false).await;
    let mut conn = raw(&dir).await;
    let op = operation(&store, &env, "raw").await;
    let release = first.release_id.unwrap();
    let insert = format!(
        "INSERT INTO deployment (id, environment_id, revision_id, release_id, operation_id, \
         started_at, action) VALUES ('{}', '{}', '{}', '{release}', '{op}', 1, 'remove')",
        DeploymentId::from_parts(1_767_225_600_000, 77).unwrap(),
        env.id,
        first.revision_id,
    );
    assert!(
        sqlx::query(AssertSqlSafe(insert))
            .execute(&mut conn)
            .await
            .is_err()
    );
    let change = format!(
        "UPDATE deployment SET action = 'remove' WHERE id = '{}'",
        first.id
    );
    assert!(
        sqlx::query(AssertSqlSafe(change))
            .execute(&mut conn)
            .await
            .is_err()
    );
    let options = format!(
        "UPDATE deployment SET options = NULL WHERE id = '{}'",
        first.id
    );
    assert!(
        sqlx::query(AssertSqlSafe(options))
            .execute(&mut conn)
            .await
            .is_err()
    );
    // A deploy still needs a release to succeed.
    let op = operation(&store, &env, "no-release").await;
    let open = store
        .create_deployment(&NewDeployment {
            environment_id: env.id,
            revision_id: first.revision_id,
            operation_id: op,
            previous_release_id: None,
        })
        .await
        .unwrap();
    assert!(
        store
            .finish_deployment(open.id, DeploymentResult::Succeeded)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_removed_environment_comes_back_only_through_a_later_deploy() {
    let (dir, store) = open().await;
    let env = environment(&store).await;
    let first = deployed(&store, &env, "one", false).await;
    remove(&store, &env, &first, "rm").await;

    // Not by asking, not through the deploy from before the removal.
    for to in [DesiredState::Running, DesiredState::Stopped] {
        assert!(matches!(
            store.set_desired_state(env.id, to).await,
            Err(StoreError::IllegalTransition { .. })
        ));
    }
    assert!(matches!(
        store.recreate_environment(env.id, first.id).await,
        Err(StoreError::IllegalTransition { .. })
    ));
    let mut conn = raw(&dir).await;
    let sql = format!(
        "UPDATE environment SET desired_state = 'running' WHERE id = '{}'",
        env.id
    );
    assert!(
        sqlx::query(AssertSqlSafe(sql.clone()))
            .execute(&mut conn)
            .await
            .is_err()
    );

    // A failed deploy after it does not count either.
    let failed = deployed(&store, &env, "two", true).await;
    assert!(matches!(
        store.recreate_environment(env.id, failed.id).await,
        Err(StoreError::IllegalTransition { .. })
    ));

    let again = deployed(&store, &env, "three", false).await;
    let back = store.recreate_environment(env.id, again.id).await.unwrap();
    assert_eq!(back.desired_state, DesiredState::Running);
    assert_eq!(
        store.get_environment(env.id).await.unwrap().desired_state,
        DesiredState::Running
    );
    // On an environment that is not removed it changes nothing.
    let same = store.recreate_environment(env.id, first.id).await.unwrap();
    assert_eq!(same.desired_state, DesiredState::Running);

    // A deploy of another environment never brings this one back.
    let other = store
        .get_or_create_project(&Slug::parse("other").unwrap())
        .await
        .unwrap()
        .environment;
    let theirs = deployed(&store, &other, "theirs", false).await;
    let serving = deployed(&store, &env, "four", false).await;
    remove(&store, &env, &serving, "rm-2").await;
    assert!(matches!(
        store.recreate_environment(env.id, theirs.id).await,
        Err(StoreError::IllegalTransition { .. })
    ));
}

#[tokio::test]
async fn an_environment_removed_before_any_deployment_comes_back_through_one() {
    let (_dir, store) = open().await;
    let env = environment(&store).await;
    store
        .set_desired_state(env.id, DesiredState::Removed)
        .await
        .unwrap();
    let d = deployed(&store, &env, "one", false).await;
    let back = store.recreate_environment(env.id, d.id).await.unwrap();
    assert_eq!(back.desired_state, DesiredState::Running);
}

#[tokio::test]
async fn a_version_4_database_migrates_with_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let ts = 1_767_225_600_000_u64;
    let prj = ProjectId::from_parts(ts, 1).unwrap().to_string();
    let env = EnvironmentId::from_parts(ts, 2).unwrap().to_string();
    let op = OperationId::from_parts(ts, 3).unwrap().to_string();
    let rev = RevisionId::from_parts(ts, 4).unwrap().to_string();
    let rel = ReleaseId::from_parts(ts, 5).unwrap().to_string();
    let dep = DeploymentId::from_parts(ts, 6).unwrap().to_string();
    let svc = ServiceId::from_parts(ts, 7).unwrap().to_string();

    let mut conn = SqliteConnectOptions::new()
        .filename(dir.path().join(DATABASE_FILE))
        .foreign_keys(true)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    for sql in [
        include_str!("../migrations/0001_initial.sql"),
        include_str!("../migrations/0002_phase1.sql"),
        include_str!("../migrations/0003_settings.sql"),
        include_str!("../migrations/0004_compose_confirm.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&mut conn).await.unwrap();
    }
    let seed = format!(
        "INSERT INTO project (id, slug, created_at) VALUES ('{prj}', 'app', {ts});
         INSERT INTO environment (id, project_id, name, desired_state, created_at)
             VALUES ('{env}', '{prj}', 'development', 'running', {ts});
         INSERT INTO service (id, environment_id, name, kind, created_at)
             VALUES ('{svc}', '{env}', 'web', 'web', {ts});
         INSERT INTO operation (id, idempotency_key, kind, environment_id, phase, status,
                                created_at, updated_at)
             VALUES ('{op}', 'k', 'push', '{env}', 'queued', 'queued', {ts}, {ts});
         INSERT INTO revision (id, project_id, kind, source_ref, created_at)
             VALUES ('{rev}', '{prj}', 'snapshot', 'sha256:aa', {ts});
         INSERT INTO release (id, revision_id, images, config_fingerprint, created_at)
             VALUES ('{rel}', '{rev}', '[\"sha256:img\"]', 'cfg', {ts});
         INSERT INTO deployment (id, environment_id, revision_id, operation_id, started_at)
             VALUES ('{dep}', '{env}', '{rev}', '{op}', {ts});
         UPDATE deployment SET release_id = '{rel}' WHERE id = '{dep}';
         UPDATE deployment SET result = 'succeeded', ended_at = {ts} WHERE id = '{dep}';
         PRAGMA user_version = 4;"
    );
    sqlx::raw_sql(AssertSqlSafe(seed))
        .execute(&mut conn)
        .await
        .unwrap();
    drop(conn);

    let store = Store::open(dir.path()).await.unwrap();
    let report = store.open_report().clone();
    assert_eq!(report.from_version, 4);
    assert_eq!(report.to_version, LATEST_VERSION);
    assert!(report.backup.is_some());
    let env_id: EnvironmentId = env.parse().unwrap();
    let services = store.list_services(env_id).await.unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].desired_state, DesiredState::Running);
    let d = store.get_deployment(dep.parse().unwrap()).await.unwrap();
    assert_eq!(d.action, DeploymentAction::Deploy);
    assert_eq!(d.options, None);
    assert_eq!(d.result, Some(DeploymentResult::Succeeded));
    store.close().await;

    // Reopening is clean.
    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.open_report().from_version, LATEST_VERSION);
    assert_eq!(store.open_report().backup, None);
}
