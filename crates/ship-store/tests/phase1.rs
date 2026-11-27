//! Phase 1 store behaviour (plan §1.2) against real SQLite files in
//! temporary state directories: the migration from a Phase 0 database, the
//! new tables, the history repositories, operation and desired-state rules
//! in the API and in the schema, get-or-create and the audit log.

use std::path::Path;

use ship_core::{
    DeploymentResult, DesiredState, EnvironmentId, ErrorCode, OperationId, OperationPhase,
    OperationStatus, ProjectId, ReleaseId, RevisionId, RevisionKind, Slug,
};
use ship_store::{
    DATABASE_FILE, DEVELOPMENT, EnsuredProject, LATEST_VERSION, NewAuditEvent, NewDeployment,
    NewOperation, NewRelease, NewRevision, Operation, OperationFailure, Store, StoreError,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection};

const PHASE0_SQL: &str = include_str!("../migrations/0001_initial.sql");

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

async fn raw(dir: &Path) -> SqliteConnection {
    SqliteConnectOptions::new()
        .filename(dir.join(DATABASE_FILE))
        .foreign_keys(true)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap()
}

/// Runs `sql` on `conn` and returns the error message, panicking if it
/// succeeds.
async fn refused(conn: &mut SqliteConnection, sql: &str) -> String {
    match sqlx::raw_sql(AssertSqlSafe(sql.to_owned()))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => panic!("accepted: {sql}"),
        Err(e) => e.to_string(),
    }
}

async fn ensured(store: &Store, name: &str) -> EnsuredProject {
    store.get_or_create_project(&slug(name)).await.unwrap()
}

async fn operation(store: &Store, key: &str, env: EnvironmentId) -> Operation {
    store
        .create_operation(&NewOperation {
            idempotency_key: key,
            kind: "push",
            environment_id: Some(env),
        })
        .await
        .unwrap()
        .operation
}

async fn revision(store: &Store, project: ProjectId) -> RevisionId {
    store
        .create_revision(
            project,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: "sha256:aa",
                manifest_fingerprint: Some("mf"),
            },
        )
        .await
        .unwrap()
        .id
}

async fn release(store: &Store, revision: RevisionId) -> ReleaseId {
    store
        .create_release(
            revision,
            &NewRelease {
                images: &["sha256:img".to_owned()],
                config_fingerprint: "cfg",
            },
        )
        .await
        .unwrap()
        .id
}

fn failure(code: ErrorCode) -> OperationFailure<'static> {
    OperationFailure {
        code,
        message: "the image build exited 1",
    }
}

fn is_illegal(r: Result<impl std::fmt::Debug, StoreError>, entity: &str) {
    match r {
        Err(StoreError::IllegalTransition { entity: e }) if e == entity => {}
        other => panic!("expected IllegalTransition on {entity}, got {other:?}"),
    }
}

// ---- migration ----------------------------------------------------------

#[tokio::test]
async fn phase0_database_migrates_in_place_with_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let ts = 1_767_225_600_000_u64;
    let prj = ProjectId::from_parts(ts, 1).unwrap().to_string();
    let env = EnvironmentId::from_parts(ts, 2).unwrap().to_string();
    let op = OperationId::from_parts(ts, 3).unwrap().to_string();
    let rev = RevisionId::from_parts(ts, 4).unwrap().to_string();
    let rel = ReleaseId::from_parts(ts, 5).unwrap().to_string();
    let dep = ship_core::DeploymentId::from_parts(ts, 6)
        .unwrap()
        .to_string();

    // A version 1 database as Phase 0 left it, with one row in each table.
    let mut conn = raw(dir.path()).await;
    sqlx::raw_sql(PHASE0_SQL).execute(&mut conn).await.unwrap();
    sqlx::raw_sql("PRAGMA user_version = 1")
        .execute(&mut conn)
        .await
        .unwrap();
    let seed = format!(
        "INSERT INTO project (id, slug, created_at) VALUES ('{prj}', 'app', {ts});
         INSERT INTO environment (id, project_id, name, desired_state, created_at)
             VALUES ('{env}', '{prj}', 'development', 'running', {ts});
         INSERT INTO operation (id, idempotency_key, kind, environment_id, phase, status,
                                created_at, updated_at)
             VALUES ('{op}', 'k', 'push', '{env}', 'succeeded', 'succeeded', {ts}, {ts});
         INSERT INTO revision (id, project_id, kind, source_ref, created_at)
             VALUES ('{rev}', '{prj}', 'snapshot', 'sha256:aa', {ts});
         INSERT INTO release (id, revision_id, images, config_fingerprint, created_at)
             VALUES ('{rel}', '{rev}', '[\"sha256:img\"]', 'cfg', {ts});
         INSERT INTO deployment (id, environment_id, release_id, operation_id, result,
                                 started_at, ended_at)
             VALUES ('{dep}', '{env}', '{rel}', '{op}', 'succeeded', {ts}, {ts});
         INSERT INTO audit_event (actor, action, target, ts) VALUES ('adam', 'push', 'app', {ts});"
    );
    sqlx::raw_sql(AssertSqlSafe(seed))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let report = store.open_report().clone();
    assert_eq!(report.from_version, 1);
    assert_eq!(report.to_version, LATEST_VERSION);
    assert_eq!(LATEST_VERSION, 2);
    let backup = report.backup.expect("a backup was taken");

    // Every row survived; the deployment now names its revision.
    let projects = store.list_projects().await.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id.to_string(), prj);
    let d = store.get_deployment(dep.parse().unwrap()).await.unwrap();
    assert_eq!(d.revision_id.to_string(), rev);
    assert_eq!(d.release_id.map(|r| r.to_string()), Some(rel.clone()));
    assert_eq!(d.operation_id.to_string(), op);
    assert_eq!(d.result, Some(DeploymentResult::Succeeded));
    assert!(d.ended_at.is_some());
    let r = store.get_release(rel.parse().unwrap()).await.unwrap();
    assert_eq!(r.images, ["sha256:img"]);
    let o = store.get_operation(op.parse().unwrap()).await.unwrap();
    assert_eq!(o.failure_phase, None);
    assert_eq!(store.list_audit_events().await.unwrap().len(), 1);
    store.close().await;

    // The backup is the version 1 database.
    let mut conn = SqliteConnectOptions::new()
        .filename(&backup)
        .connect()
        .await
        .unwrap();
    let v: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(v, 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM deployment")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(n, 1);
    conn.close().await.unwrap();

    // Reopening is clean and takes no second backup.
    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.open_report().from_version, LATEST_VERSION);
    assert_eq!(store.open_report().backup, None);
    assert_eq!(store.list_deployments(d.environment_id).await.unwrap(), [d]);
    store.close().await;
}

// ---- new tables ---------------------------------------------------------

#[tokio::test]
async fn secret_table_has_no_column_for_a_value() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).await.unwrap().close().await;
    let mut conn = raw(dir.path()).await;
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('secret')")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(
        columns,
        [
            "scope",
            "project_id",
            "environment_id",
            "service_id",
            "name",
            "backend_ref",
            "updated_at"
        ]
    );
    conn.close().await.unwrap();
}

#[tokio::test]
async fn new_tables_check_their_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let e = ensured(&store, "app").await;
    let svc = store
        .create_service(
            e.environment.id,
            &ship_store::NewService {
                name: "web".into(),
                kind: ship_core::ServiceKind::Web,
                port: Some(8080),
            },
        )
        .await
        .unwrap();
    store.close().await;
    let (prj, env, svc) = (e.project.id, e.environment.id, svc.id);
    let route_id = ship_core::RouteId::from_parts(1, 1).unwrap();

    let mut conn = raw(dir.path()).await;
    let ok = format!(
        "INSERT INTO route (id, environment_id, service_id, provider, address, status, \
                            created_at, updated_at)
             VALUES ('{route_id}', '{env}', '{svc}', 'port', '192.0.2.10:8080', 'pending', 1, 1);
         INSERT INTO config_value (scope, project_id, key, value, updated_at)
             VALUES ('project', '{prj}', 'LOG_LEVEL', 'info', 1);
         INSERT INTO config_value (scope, service_id, key, value, updated_at)
             VALUES ('service', '{svc}', 'LOG_LEVEL', 'debug', 1);
         INSERT INTO secret (scope, environment_id, name, backend_ref, updated_at)
             VALUES ('environment', '{env}', 'DATABASE_URL', 'file:1', 1);
         INSERT INTO observation (service_id, reachable, container_state, exit_code, health,
                                  observed_at)
             VALUES ('{svc}', 1, 'exited', 137, 'none', 1);"
    );
    sqlx::raw_sql(AssertSqlSafe(ok))
        .execute(&mut conn)
        .await
        .unwrap();

    for bad in [
        // Phase 1 routes are published ports only.
        format!(
            "INSERT INTO route (id, environment_id, service_id, provider, address, status, \
             created_at, updated_at) VALUES ('rte_x', '{env}', '{svc}', 'tailnet', 'a', \
             'pending', 1, 1)"
        ),
        // Scope and its column disagree.
        format!(
            "INSERT INTO config_value (scope, environment_id, key, value, updated_at) \
             VALUES ('project', '{env}', 'K', 'v', 1)"
        ),
        // One value per key and scope target.
        format!(
            "INSERT INTO config_value (scope, project_id, key, value, updated_at) \
             VALUES ('project', '{prj}', 'LOG_LEVEL', 'warn', 1)"
        ),
        format!(
            "INSERT INTO secret (scope, environment_id, name, backend_ref, updated_at) \
             VALUES ('environment', '{env}', 'DATABASE_URL', 'file:2', 1)"
        ),
        // Docker unreachable carries no container state.
        "UPDATE observation SET reachable = 0".to_owned(),
        // An exit code only for an exited container.
        "UPDATE observation SET container_state = 'running'".to_owned(),
    ] {
        refused(&mut conn, &bad).await;
    }
    sqlx::raw_sql(
        "UPDATE observation SET reachable = 0, container_state = NULL, \
         exit_code = NULL, health = NULL",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}

// ---- revisions and releases ---------------------------------------------

#[tokio::test]
async fn revisions_and_releases_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let a = ensured(&store, "app").await.project;
    let b = ensured(&store, "other").await.project;

    let r1 = store
        .create_revision(
            a.id,
            &NewRevision {
                kind: RevisionKind::Snapshot,
                source_ref: "sha256:aa",
                manifest_fingerprint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(r1.project_id, a.id);
    assert_eq!(r1.manifest_fingerprint, None);
    assert_eq!(store.get_revision(r1.id).await.unwrap(), r1);
    let r2_id = revision(&store, a.id).await;
    let r2 = store.get_revision(r2_id).await.unwrap();
    assert_eq!(r2.manifest_fingerprint.as_deref(), Some("mf"));
    let rb = revision(&store, b.id).await;
    assert_eq!(store.list_revisions(a.id).await.unwrap(), [r1.clone(), r2]);

    let images = ["sha256:one".to_owned(), "sha256:two".to_owned()];
    let l1 = store
        .create_release(
            r1.id,
            &NewRelease {
                images: &images,
                config_fingerprint: "cfg1",
            },
        )
        .await
        .unwrap();
    assert_eq!(l1.images, images);
    assert_eq!(store.get_release(l1.id).await.unwrap(), l1);
    let l2 = store
        .get_release(release(&store, r1.id).await)
        .await
        .unwrap();
    release(&store, rb).await;
    assert_eq!(store.list_releases(a.id).await.unwrap(), [l1, l2]);

    let unknown_project = ProjectId::from_parts(1, 1).unwrap();
    assert!(matches!(
        store
            .create_revision(
                unknown_project,
                &NewRevision {
                    kind: RevisionKind::Git,
                    source_ref: "abc",
                    manifest_fingerprint: None,
                },
            )
            .await,
        Err(StoreError::Conflict { .. })
    ));
    let unknown_revision = RevisionId::from_parts(1, 1).unwrap();
    assert!(matches!(
        store
            .create_release(
                unknown_revision,
                &NewRelease {
                    images: &[],
                    config_fingerprint: "c",
                },
            )
            .await,
        Err(StoreError::Conflict { .. })
    ));
    assert!(matches!(
        store.get_revision(unknown_revision).await,
        Err(StoreError::NotFound { entity: "revision" })
    ));
    assert!(matches!(
        store
            .get_release(ReleaseId::from_parts(1, 1).unwrap())
            .await,
        Err(StoreError::NotFound { entity: "release" })
    ));
    store.close().await;
}

// ---- deployments --------------------------------------------------------

#[tokio::test]
async fn deployment_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let e = ensured(&store, "app").await;
    let env = e.environment.id;
    let rev = revision(&store, e.project.id).await;
    let rel = release(&store, rev).await;

    // A deployment whose build failed: no release, result failed.
    let op1 = operation(&store, "k1", env).await;
    let d1 = store
        .create_deployment(&NewDeployment {
            environment_id: env,
            revision_id: rev,
            operation_id: op1.id,
            previous_release_id: None,
        })
        .await
        .unwrap();
    assert_eq!(d1.release_id, None);
    assert_eq!(d1.result, None);
    assert_eq!(d1.ended_at, None);
    assert_eq!(store.get_deployment(d1.id).await.unwrap(), d1);
    is_illegal(
        store
            .finish_deployment(d1.id, DeploymentResult::Succeeded)
            .await,
        "deployment",
    );
    let d1 = store
        .finish_deployment(d1.id, DeploymentResult::Failed)
        .await
        .unwrap();
    assert_eq!(d1.result, Some(DeploymentResult::Failed));
    assert!(d1.ended_at.is_some());
    is_illegal(
        store
            .finish_deployment(d1.id, DeploymentResult::Interrupted)
            .await,
        "deployment",
    );
    is_illegal(store.set_deployment_release(d1.id, rel).await, "deployment");

    // A successful one: release set once, then finished.
    let op2 = operation(&store, "k2", env).await;
    let d2 = store
        .create_deployment(&NewDeployment {
            environment_id: env,
            revision_id: rev,
            operation_id: op2.id,
            previous_release_id: None,
        })
        .await
        .unwrap();
    let d2 = store.set_deployment_release(d2.id, rel).await.unwrap();
    assert_eq!(d2.release_id, Some(rel));
    let other_rel = release(&store, rev).await;
    is_illegal(
        store.set_deployment_release(d2.id, other_rel).await,
        "deployment",
    );
    let d2 = store
        .finish_deployment(d2.id, DeploymentResult::Succeeded)
        .await
        .unwrap();
    assert_eq!(d2.result, Some(DeploymentResult::Succeeded));

    // Several deployments may use one release; newest first.
    let op3 = operation(&store, "k3", env).await;
    let d3 = store
        .create_deployment(&NewDeployment {
            environment_id: env,
            revision_id: rev,
            operation_id: op3.id,
            previous_release_id: Some(rel),
        })
        .await
        .unwrap();
    assert_eq!(d3.previous_release_id, Some(rel));
    let listed = store.list_deployments(env).await.unwrap();
    assert_eq!(listed, [d3.clone(), d2, d1]);

    // One deployment per operation.
    assert!(matches!(
        store
            .create_deployment(&NewDeployment {
                environment_id: env,
                revision_id: rev,
                operation_id: op3.id,
                previous_release_id: None,
            })
            .await,
        Err(StoreError::Conflict { .. })
    ));
    // A release of another revision.
    let rev2 = revision(&store, e.project.id).await;
    let rel2 = release(&store, rev2).await;
    assert!(matches!(
        store.set_deployment_release(d3.id, rel2).await,
        Err(StoreError::Invalid {
            entity: "deployment"
        })
    ));
    // An operation of another environment, a revision of another project.
    let f = ensured(&store, "other").await;
    let foreign_op = operation(&store, "k4", f.environment.id).await;
    assert!(matches!(
        store
            .create_deployment(&NewDeployment {
                environment_id: env,
                revision_id: rev,
                operation_id: foreign_op.id,
                previous_release_id: None,
            })
            .await,
        Err(StoreError::Invalid {
            entity: "deployment"
        })
    ));
    let foreign_rev = revision(&store, f.project.id).await;
    let op5 = operation(&store, "k5", env).await;
    assert!(matches!(
        store
            .create_deployment(&NewDeployment {
                environment_id: env,
                revision_id: foreign_rev,
                operation_id: op5.id,
                previous_release_id: None,
            })
            .await,
        Err(StoreError::Invalid {
            entity: "deployment"
        })
    ));
    // Unknown ids.
    let unknown = ship_core::DeploymentId::from_parts(1, 1).unwrap();
    assert!(matches!(
        store.get_deployment(unknown).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.set_deployment_release(unknown, rel).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store
            .finish_deployment(unknown, DeploymentResult::Failed)
            .await,
        Err(StoreError::NotFound { .. })
    ));
    store.close().await;
}

#[tokio::test]
async fn schema_rejects_deployment_without_operation_and_history_edits() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let e = ensured(&store, "app").await;
    let env = e.environment.id;
    let rev = revision(&store, e.project.id).await;
    let rel = release(&store, rev).await;
    let op = operation(&store, "k", env).await;
    let d = store
        .create_deployment(&NewDeployment {
            environment_id: env,
            revision_id: rev,
            operation_id: op.id,
            previous_release_id: None,
        })
        .await
        .unwrap();
    store.close().await;

    let mut conn = raw(dir.path()).await;
    let missing = OperationId::from_parts(1, 1).unwrap();
    for sql in [
        format!(
            "INSERT INTO deployment (id, environment_id, revision_id, started_at) \
             VALUES ('dep_x', '{env}', '{rev}', 1)"
        ),
        format!(
            "INSERT INTO deployment (id, environment_id, revision_id, operation_id, started_at) \
             VALUES ('dep_x', '{env}', '{rev}', NULL, 1)"
        ),
        format!(
            "INSERT INTO deployment (id, environment_id, revision_id, operation_id, started_at) \
             VALUES ('dep_x', '{env}', '{rev}', '{missing}', 1)"
        ),
        format!(
            "UPDATE deployment SET operation_id = NULL WHERE id = '{}'",
            d.id
        ),
        format!("UPDATE deployment SET started_at = 2 WHERE id = '{}'", d.id),
        format!(
            "UPDATE deployment SET result = 'failed' WHERE id = '{}'",
            d.id
        ),
        format!("UPDATE revision SET source_ref = 'x' WHERE id = '{rev}'"),
        format!("UPDATE release SET images = '[]' WHERE id = '{rel}'"),
    ] {
        refused(&mut conn, &sql).await;
    }
    conn.close().await.unwrap();
}

// ---- operations ---------------------------------------------------------

#[tokio::test]
async fn operation_phases_move_forward_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let env = ensured(&store, "app").await.environment.id;
    let op = operation(&store, "k", env).await;
    assert_eq!(op.phase, OperationPhase::Queued);

    let up = store
        .advance_operation(op.id, OperationPhase::Uploading)
        .await
        .unwrap();
    assert_eq!(
        (up.phase, up.status),
        (OperationPhase::Uploading, OperationStatus::Running)
    );
    assert_eq!(store.get_operation(op.id).await.unwrap(), up);
    // Skipping phases is allowed.
    let b = store
        .advance_operation(op.id, OperationPhase::Building)
        .await
        .unwrap();
    assert_eq!(b.phase, OperationPhase::Building);
    // Backwards, the same phase, and `failed` without a failure are not.
    for to in [
        OperationPhase::Queued,
        OperationPhase::Validating,
        OperationPhase::Building,
        OperationPhase::Failed,
        OperationPhase::Interrupted,
    ] {
        is_illegal(store.advance_operation(op.id, to).await, "operation");
    }
    assert_eq!(store.get_operation(op.id).await.unwrap(), b);
    let done = store
        .advance_operation(op.id, OperationPhase::Succeeded)
        .await
        .unwrap();
    assert_eq!(done.status, OperationStatus::Succeeded);
    assert_eq!(done.failure_code, None);
    // Nothing leaves a terminal phase.
    for &to in OperationPhase::ALL {
        is_illegal(store.advance_operation(op.id, to).await, "operation");
    }
    is_illegal(
        store
            .fail_operation(op.id, OperationPhase::Failed, &failure(ErrorCode::Internal))
            .await,
        "operation",
    );
    assert!(matches!(
        store
            .advance_operation(
                OperationId::from_parts(1, 1).unwrap(),
                OperationPhase::Uploading
            )
            .await,
        Err(StoreError::NotFound { .. })
    ));
    store.close().await;
}

#[tokio::test]
async fn operation_failure_records_code_message_and_phase() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let env = ensured(&store, "app").await.environment.id;

    let op = operation(&store, "k1", env).await;
    store
        .advance_operation(op.id, OperationPhase::Building)
        .await
        .unwrap();
    // Only `failed` and `interrupted` take a failure.
    for terminal in [OperationPhase::Succeeded, OperationPhase::Deploying] {
        is_illegal(
            store
                .fail_operation(op.id, terminal, &failure(ErrorCode::BuildFailed))
                .await,
            "operation",
        );
    }
    let failed = store
        .fail_operation(
            op.id,
            OperationPhase::Failed,
            &failure(ErrorCode::BuildFailed),
        )
        .await
        .unwrap();
    assert_eq!(
        (failed.phase, failed.status),
        (OperationPhase::Failed, OperationStatus::Failed)
    );
    assert_eq!(failed.failure_code.as_deref(), Some("build_failed"));
    assert_eq!(
        failed.failure_message.as_deref(),
        Some("the image build exited 1")
    );
    assert_eq!(failed.failure_phase, Some(OperationPhase::Building));
    assert_eq!(store.get_operation(op.id).await.unwrap(), failed);
    is_illegal(
        store
            .fail_operation(
                op.id,
                OperationPhase::Interrupted,
                &failure(ErrorCode::Interrupted),
            )
            .await,
        "operation",
    );

    // Interrupted while still queued.
    let q = operation(&store, "k2", env).await;
    let i = store
        .fail_operation(
            q.id,
            OperationPhase::Interrupted,
            &failure(ErrorCode::Interrupted),
        )
        .await
        .unwrap();
    assert_eq!(i.status, OperationStatus::Interrupted);
    assert_eq!(i.failure_code.as_deref(), Some("interrupted"));
    assert_eq!(i.failure_phase, Some(OperationPhase::Queued));
    store.close().await;
}

#[tokio::test]
async fn schema_rejects_illegal_operation_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let env = ensured(&store, "app").await.environment.id;
    let running = operation(&store, "k1", env).await.id;
    store
        .advance_operation(running, OperationPhase::Building)
        .await
        .unwrap();
    let done = operation(&store, "k2", env).await.id;
    store
        .advance_operation(done, OperationPhase::Succeeded)
        .await
        .unwrap();
    store.close().await;

    let mut conn = raw(dir.path()).await;
    let illegal = [
        // Status disagrees with the phase.
        format!("UPDATE operation SET status = 'queued' WHERE id = '{running}'"),
        format!(
            "UPDATE operation SET phase = 'deploying', status = 'succeeded' WHERE id = '{running}'"
        ),
        // Backwards.
        format!("UPDATE operation SET phase = 'uploading' WHERE id = '{running}'"),
        format!("UPDATE operation SET phase = 'queued', status = 'queued' WHERE id = '{running}'"),
        // A failure on a running operation, or failing without one.
        format!(
            "UPDATE operation SET failure_code = 'x', failure_message = 'm' WHERE id = '{running}'"
        ),
        format!("UPDATE operation SET phase = 'failed', status = 'failed' WHERE id = '{running}'"),
        // A failure naming the wrong phase.
        format!(
            "UPDATE operation SET phase = 'failed', status = 'failed', failure_code = 'x', \
             failure_message = 'm', failure_phase = 'queued' WHERE id = '{running}'"
        ),
        // Leaving a terminal phase.
        format!("UPDATE operation SET phase = 'failed', status = 'failed' WHERE id = '{done}'"),
        // Identity fields.
        format!("UPDATE operation SET kind = 'other' WHERE id = '{running}'"),
        // A new operation that is not queued.
        format!(
            "INSERT INTO operation (id, idempotency_key, kind, environment_id, phase, status, \
             created_at, updated_at) VALUES ('op_x', 'k3', 'push', '{env}', 'building', \
             'running', 1, 1)"
        ),
    ];
    for sql in &illegal {
        let msg = refused(&mut conn, sql).await;
        assert!(msg.contains("illegal"), "{sql}: {msg}");
    }
    // The legal raw change still works.
    sqlx::raw_sql(AssertSqlSafe(format!(
        "UPDATE operation SET phase = 'failed', status = 'failed', failure_code = 'x', \
         failure_message = 'm', failure_phase = 'building' WHERE id = '{running}'"
    )))
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
}

#[tokio::test]
async fn log_cursor_never_moves_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let env = ensured(&store, "app").await.environment.id;
    let op = operation(&store, "k", env).await;
    assert_eq!(op.log_cursor, 0);
    assert_eq!(
        store.set_log_cursor(op.id, 100).await.unwrap().log_cursor,
        100
    );
    assert_eq!(
        store.set_log_cursor(op.id, 100).await.unwrap().log_cursor,
        100
    );
    is_illegal(store.set_log_cursor(op.id, 99).await, "operation");
    assert!(matches!(
        store.set_log_cursor(op.id, u64::MAX).await,
        Err(StoreError::Invalid { .. })
    ));
    // The log may still be flushed after the operation ended.
    store
        .advance_operation(op.id, OperationPhase::Succeeded)
        .await
        .unwrap();
    let after = store.set_log_cursor(op.id, 250).await.unwrap();
    assert_eq!(after.log_cursor, 250);
    assert_eq!(after.phase, OperationPhase::Succeeded);
    assert!(matches!(
        store
            .set_log_cursor(OperationId::from_parts(1, 1).unwrap(), 1)
            .await,
        Err(StoreError::NotFound { .. })
    ));
    store.close().await;

    let mut conn = raw(dir.path()).await;
    let msg = refused(
        &mut conn,
        &format!("UPDATE operation SET log_cursor = 1 WHERE id = '{}'", op.id),
    )
    .await;
    assert!(msg.contains("illegal"), "{msg}");
    conn.close().await.unwrap();
}

#[tokio::test]
async fn unfinished_operations_are_the_queued_and_running_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let env = ensured(&store, "app").await.environment.id;
    assert!(store.unfinished_operations().await.unwrap().is_empty());

    let queued = operation(&store, "k1", env).await;
    let running = operation(&store, "k2", env).await;
    let running = store
        .advance_operation(running.id, OperationPhase::Deploying)
        .await
        .unwrap();
    let done = operation(&store, "k3", env).await;
    store
        .advance_operation(done.id, OperationPhase::Succeeded)
        .await
        .unwrap();
    let failed = operation(&store, "k4", env).await;
    store
        .fail_operation(
            failed.id,
            OperationPhase::Failed,
            &failure(ErrorCode::DeployFailed),
        )
        .await
        .unwrap();
    let interrupted = operation(&store, "k5", env).await;
    store
        .fail_operation(
            interrupted.id,
            OperationPhase::Interrupted,
            &failure(ErrorCode::Interrupted),
        )
        .await
        .unwrap();

    assert_eq!(
        store.unfinished_operations().await.unwrap(),
        [queued, running]
    );
    store.close().await;
}

// ---- desired state ------------------------------------------------------

#[tokio::test]
async fn desired_state_follows_its_state_machine() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let env = ensured(&store, "app").await.environment;
    assert_eq!(env.desired_state, DesiredState::Running);

    let stopped = store
        .set_desired_state(env.id, DesiredState::Stopped)
        .await
        .unwrap();
    assert_eq!(stopped.desired_state, DesiredState::Stopped);
    assert_eq!(store.get_environment(env.id).await.unwrap(), stopped);
    // The same state is a no-op.
    assert_eq!(
        store
            .set_desired_state(env.id, DesiredState::Stopped)
            .await
            .unwrap(),
        stopped
    );
    store
        .set_desired_state(env.id, DesiredState::Running)
        .await
        .unwrap();
    let removed = store
        .set_desired_state(env.id, DesiredState::Removed)
        .await
        .unwrap();
    assert_eq!(removed.desired_state, DesiredState::Removed);
    // `removed` is final.
    for to in [DesiredState::Running, DesiredState::Stopped] {
        is_illegal(store.set_desired_state(env.id, to).await, "environment");
    }
    assert_eq!(store.get_environment(env.id).await.unwrap(), removed);
    assert!(matches!(
        store
            .set_desired_state(
                EnvironmentId::from_parts(1, 1).unwrap(),
                DesiredState::Stopped
            )
            .await,
        Err(StoreError::NotFound { .. })
    ));
    store.close().await;

    let mut conn = raw(dir.path()).await;
    let msg = refused(
        &mut conn,
        &format!(
            "UPDATE environment SET desired_state = 'running' WHERE id = '{}'",
            env.id
        ),
    )
    .await;
    assert!(msg.contains("illegal"), "{msg}");
    refused(
        &mut conn,
        &format!(
            "UPDATE environment SET project_id = 'prj_x' WHERE id = '{}'",
            env.id
        ),
    )
    .await;
    conn.close().await.unwrap();
}

// ---- get-or-create ------------------------------------------------------

#[tokio::test]
async fn get_or_create_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let first = ensured(&store, "app").await;
    assert!(first.created);
    assert_eq!(first.project.slug.as_str(), "app");
    assert_eq!(first.environment.project_id, first.project.id);
    assert_eq!(first.environment.name.as_str(), DEVELOPMENT);
    let again = ensured(&store, "app").await;
    assert!(!again.created);
    assert_eq!(again.project, first.project);
    assert_eq!(again.environment, first.environment);

    // A project created without an environment gains `development`.
    let bare = store.create_project(&slug("bare")).await.unwrap();
    let got = ensured(&store, "bare").await;
    assert!(!got.created);
    assert_eq!(got.project, bare);
    assert_eq!(
        store.list_environments(bare.id).await.unwrap(),
        [got.environment]
    );
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_get_or_create_yields_one_project_and_environment() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.get_or_create_project(&slug("app")).await })
        })
        .collect();
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap().unwrap());
    }
    assert_eq!(results.iter().filter(|r| r.created).count(), 1);
    for r in &results {
        assert_eq!(r.project, results[0].project);
        assert_eq!(r.environment, results[0].environment);
    }
    let projects = store.list_projects().await.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(
        store.list_environments(projects[0].id).await.unwrap().len(),
        1
    );
    store.close().await;
}

// ---- audit --------------------------------------------------------------

#[tokio::test]
async fn audit_log_is_append_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    assert!(store.list_audit_events().await.unwrap().is_empty());
    let a = store
        .append_audit_event(&NewAuditEvent {
            actor: "adam",
            action: "push",
            target: "app",
        })
        .await
        .unwrap();
    let b = store
        .append_audit_event(&NewAuditEvent {
            actor: "policy",
            action: "compose.confirm accepted",
            target: "app",
        })
        .await
        .unwrap();
    assert!(b.id > a.id);
    assert_eq!(a.actor, "adam");
    assert_eq!(b.action, "compose.confirm accepted");
    assert_eq!(store.list_audit_events().await.unwrap(), [a.clone(), b]);
    store.close().await;

    let mut conn = raw(dir.path()).await;
    for sql in [
        format!("UPDATE audit_event SET actor = 'x' WHERE id = {}", a.id),
        format!("DELETE FROM audit_event WHERE id = {}", a.id),
    ] {
        let msg = refused(&mut conn, &sql).await;
        assert!(msg.contains("append-only"), "{msg}");
    }
    conn.close().await.unwrap();
}
