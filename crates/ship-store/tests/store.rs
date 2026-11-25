//! Store behaviour against real SQLite files in temporary state directories.

use std::collections::BTreeMap;
use std::path::Path;

use ship_core::{
    DesiredState, EnvironmentId, OperationId, OperationPhase, OperationStatus, ProjectId,
    RecoveryPolicy, ServiceId, ServiceKind, Slug,
};
use ship_store::{
    DATABASE_FILE, LATEST_VERSION, NewEnvironment, NewEvent, NewOperation, NewService, Store,
    StoreError,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{ConnectOptions, Connection};

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

async fn raw(dir: &Path) -> SqliteConnection {
    SqliteConnectOptions::new()
        .filename(dir.join(DATABASE_FILE))
        .foreign_keys(true)
        .connect()
        .await
        .unwrap()
}

/// Every file in `dir` with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().into_string().unwrap(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn development() -> NewEnvironment {
    NewEnvironment {
        name: slug("development"),
        desired_state: DesiredState::Running,
        recovery_policy: RecoveryPolicy::default(),
    }
}

#[tokio::test]
async fn fresh_state_dir_migrates_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.database_path(), dir.path().join(DATABASE_FILE));
    assert_eq!(store.open_report().from_version, 0);
    assert_eq!(store.open_report().to_version, LATEST_VERSION);
    assert_eq!(store.schema_version().await.unwrap(), LATEST_VERSION);
    store.close().await;

    let mut conn = raw(dir.path()).await;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        tables,
        [
            "audit_event",
            "deployment",
            "environment",
            "operation",
            "operation_event",
            "project",
            "release",
            "revision",
            "service",
        ]
    );
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.open_report().from_version, LATEST_VERSION);
    assert_eq!(store.open_report().to_version, LATEST_VERSION);
    assert_eq!(store.open_report().backup, None);
    assert!(store.list_projects().await.unwrap().is_empty());
    store.close().await;

    let names: Vec<String> = snapshot(dir.path()).into_keys().collect();
    assert!(
        names.iter().all(|n| !n.contains(".backup-")),
        "no backup expected: {names:?}"
    );
}

#[tokio::test]
async fn missing_state_dir_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent");
    assert!(matches!(
        Store::open(&missing).await,
        Err(StoreError::Io(_))
    ));
    assert!(!missing.exists());
}

#[tokio::test]
async fn revision_and_release_reject_update() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let project = store.create_project(&slug("app")).await.unwrap();
    store.close().await;

    let mut conn = raw(dir.path()).await;
    sqlx::query(
        "INSERT INTO revision (id, project_id, kind, source_ref, created_at) \
         VALUES ('rev_1', ?, 'snapshot', 'sha256:aa', 1)",
    )
    .bind(project.id.to_string())
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO release (id, revision_id, images, config_fingerprint, created_at) \
         VALUES ('rel_1', 'rev_1', '[]', 'cfg', 1)",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    for sql in [
        "UPDATE revision SET source_ref = 'sha256:bb' WHERE id = 'rev_1'",
        "UPDATE revision SET created_at = created_at",
        "UPDATE release SET images = '[\"x\"]' WHERE id = 'rel_1'",
        "UPDATE release SET config_fingerprint = config_fingerprint",
    ] {
        let err = sqlx::query(sql).execute(&mut conn).await.unwrap_err();
        assert!(err.to_string().contains("immutable"), "{sql}: {err}");
    }

    let source: String = sqlx::query_scalar("SELECT source_ref FROM revision WHERE id = 'rev_1'")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(source, "sha256:aa");
    let images: String = sqlx::query_scalar("SELECT images FROM release WHERE id = 'rel_1'")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(images, "[]");
    conn.close().await.unwrap();
}

#[tokio::test]
async fn project_environment_service_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();

    let a = store.create_project(&slug("alpha")).await.unwrap();
    let b = store.create_project(&slug("beta")).await.unwrap();
    assert_eq!(store.get_project(a.id).await.unwrap(), a);
    let listed = store.list_projects().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&a) && listed.contains(&b));

    let env = store
        .create_environment(a.id, &development())
        .await
        .unwrap();
    assert_eq!(env.project_id, a.id);
    assert_eq!(env.recovery_policy, RecoveryPolicy::ObserveOnly);
    assert_eq!(store.get_environment(env.id).await.unwrap(), env);
    assert_eq!(
        env.compose_project(),
        ship_core::compose_project_name(&env.id)
    );
    let staging = store
        .create_environment(
            a.id,
            &NewEnvironment {
                name: slug("staging"),
                desired_state: DesiredState::Stopped,
                recovery_policy: RecoveryPolicy::RepairSafeDrift,
            },
        )
        .await
        .unwrap();
    assert_eq!(store.get_environment(staging.id).await.unwrap(), staging);
    assert_eq!(store.list_environments(a.id).await.unwrap().len(), 2);
    assert!(store.list_environments(b.id).await.unwrap().is_empty());

    let web = store
        .create_service(
            env.id,
            &NewService {
                name: "web".into(),
                kind: ServiceKind::Web,
                port: Some(8080),
            },
        )
        .await
        .unwrap();
    let worker = store
        .create_service(
            env.id,
            &NewService {
                name: "queue_worker".into(),
                kind: ServiceKind::Worker,
                port: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(store.get_service(web.id).await.unwrap(), web);
    assert_eq!(store.get_service(worker.id).await.unwrap(), worker);
    let services = store.list_services(env.id).await.unwrap();
    assert_eq!(services.len(), 2);
    assert!(services.contains(&web) && services.contains(&worker));
    assert!(store.list_services(staging.id).await.unwrap().is_empty());

    // Survives a reopen.
    store.close().await;
    let store = Store::open(dir.path()).await.unwrap();
    assert_eq!(store.get_project(a.id).await.unwrap(), a);
    assert_eq!(store.get_environment(env.id).await.unwrap(), env);
    assert_eq!(store.get_service(web.id).await.unwrap(), web);
    store.close().await;
}

#[tokio::test]
async fn duplicate_names_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let p = store.create_project(&slug("app")).await.unwrap();
    assert!(matches!(
        store.create_project(&slug("app")).await,
        Err(StoreError::Conflict { entity: "project" })
    ));
    let env = store
        .create_environment(p.id, &development())
        .await
        .unwrap();
    assert!(matches!(
        store.create_environment(p.id, &development()).await,
        Err(StoreError::Conflict {
            entity: "environment"
        })
    ));
    // The same environment name in another project is fine.
    let q = store.create_project(&slug("other")).await.unwrap();
    store
        .create_environment(q.id, &development())
        .await
        .unwrap();

    let svc = NewService {
        name: "web".into(),
        kind: ServiceKind::Web,
        port: Some(80),
    };
    store.create_service(env.id, &svc).await.unwrap();
    assert!(matches!(
        store.create_service(env.id, &svc).await,
        Err(StoreError::Conflict { entity: "service" })
    ));
    assert_eq!(store.list_projects().await.unwrap().len(), 2);
    store.close().await;
}

#[tokio::test]
async fn invalid_service_values_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let p = store.create_project(&slug("app")).await.unwrap();
    let env = store
        .create_environment(p.id, &development())
        .await
        .unwrap();
    for (name, port) in [("", Some(80)), (&*"x".repeat(64), None), ("web", Some(0))] {
        let res = store
            .create_service(
                env.id,
                &NewService {
                    name: name.to_owned(),
                    kind: ServiceKind::Web,
                    port,
                },
            )
            .await;
        assert!(
            matches!(res, Err(StoreError::Invalid { entity: "service" })),
            "{name:?} {port:?}: {res:?}"
        );
    }
    assert!(store.list_services(env.id).await.unwrap().is_empty());
    store.close().await;
}

#[tokio::test]
async fn unknown_id_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let pid = ProjectId::from_parts(1, 1).unwrap();
    let eid = EnvironmentId::from_parts(1, 1).unwrap();
    let sid = ServiceId::from_parts(1, 1).unwrap();
    let oid = OperationId::from_parts(1, 1).unwrap();
    assert!(matches!(
        store.get_project(pid).await,
        Err(StoreError::NotFound { entity: "project" })
    ));
    assert!(matches!(
        store.get_environment(eid).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.get_service(sid).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.get_operation(oid).await,
        Err(StoreError::NotFound { .. })
    ));
    let ev = NewEvent {
        event_type: "log",
        phase: OperationPhase::Queued,
        message: "m",
    };
    assert!(matches!(
        store.append_event(oid, &ev).await,
        Err(StoreError::NotFound {
            entity: "operation"
        })
    ));
    store.close().await;
}

#[tokio::test]
async fn foreign_key_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let pid = ProjectId::from_parts(1, 1).unwrap();
    let eid = EnvironmentId::from_parts(1, 1).unwrap();
    assert!(matches!(
        store.create_environment(pid, &development()).await,
        Err(StoreError::Conflict {
            entity: "environment"
        })
    ));
    let svc = NewService {
        name: "web".into(),
        kind: ServiceKind::Web,
        port: None,
    };
    assert!(matches!(
        store.create_service(eid, &svc).await,
        Err(StoreError::Conflict { entity: "service" })
    ));
    let op = NewOperation {
        idempotency_key: "k",
        kind: "deploy",
        environment_id: Some(eid),
    };
    assert!(matches!(
        store.create_operation(&op).await,
        Err(StoreError::Conflict {
            entity: "operation"
        })
    ));
    store.close().await;
}

#[tokio::test]
async fn operation_create_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let p = store.create_project(&slug("app")).await.unwrap();
    let env = store
        .create_environment(p.id, &development())
        .await
        .unwrap();

    let new = NewOperation {
        idempotency_key: "key-1",
        kind: "deploy",
        environment_id: Some(env.id),
    };
    let first = store.create_operation(&new).await.unwrap();
    assert!(first.created);
    let op = &first.operation;
    assert_eq!(op.idempotency_key, "key-1");
    assert_eq!(op.kind, "deploy");
    assert_eq!(op.environment_id, Some(env.id));
    assert_eq!(op.phase, OperationPhase::Queued);
    assert_eq!(op.status, OperationStatus::Queued);
    assert_eq!(op.failure_code, None);
    assert_eq!(op.log_cursor, 0);
    assert_eq!(store.get_operation(op.id).await.unwrap(), *op);

    let replay = store.create_operation(&new).await.unwrap();
    assert!(!replay.created);
    assert_eq!(replay.operation, *op);

    // Same key, different request.
    let other_kind = NewOperation {
        kind: "stop",
        ..new
    };
    assert!(matches!(
        store.create_operation(&other_kind).await,
        Err(StoreError::Conflict {
            entity: "operation"
        })
    ));
    let other_env = NewOperation {
        environment_id: None,
        ..new
    };
    assert!(matches!(
        store.create_operation(&other_env).await,
        Err(StoreError::Conflict { .. })
    ));

    // A new key is a new operation; no environment is allowed.
    let second = store
        .create_operation(&NewOperation {
            idempotency_key: "key-2",
            kind: "host_check",
            environment_id: None,
        })
        .await
        .unwrap();
    assert!(second.created);
    assert_ne!(second.operation.id, op.id);

    // An empty key is rejected by the schema.
    assert!(matches!(
        store
            .create_operation(&NewOperation {
                idempotency_key: "",
                ..new
            })
            .await,
        Err(StoreError::Invalid { .. })
    ));
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_are_gapless() {
    const TASKS: i64 = 8;
    const PER_TASK: i64 = 25;
    const OTHER: i64 = 30;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let a = store
        .create_operation(&NewOperation {
            idempotency_key: "a",
            kind: "deploy",
            environment_id: None,
        })
        .await
        .unwrap()
        .operation
        .id;
    let b = store
        .create_operation(&NewOperation {
            idempotency_key: "b",
            kind: "deploy",
            environment_id: None,
        })
        .await
        .unwrap()
        .operation
        .id;

    let mut handles = Vec::new();
    for t in 0..TASKS {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut seqs = Vec::new();
            for i in 0..PER_TASK {
                let msg = format!("task {t} event {i}");
                let ev = store
                    .append_event(
                        a,
                        &NewEvent {
                            event_type: "log",
                            phase: OperationPhase::Building,
                            message: &msg,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(ev.operation_id, a);
                seqs.push(ev.sequence);
            }
            seqs
        }));
    }
    let other = {
        let store = store.clone();
        tokio::spawn(async move {
            for _ in 0..OTHER {
                store
                    .append_event(
                        b,
                        &NewEvent {
                            event_type: "log",
                            phase: OperationPhase::Queued,
                            message: "b",
                        },
                    )
                    .await
                    .unwrap();
            }
        })
    };

    let mut returned = Vec::new();
    for h in handles {
        let seqs = h.await.unwrap();
        // Within one task, appends are ordered.
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        returned.extend(seqs);
    }
    other.await.unwrap();
    returned.sort_unstable();
    let expected: Vec<i64> = (1..=TASKS * PER_TASK).collect();
    assert_eq!(returned, expected);

    let stored: Vec<i64> = store
        .events_after(a, 0)
        .await
        .unwrap()
        .iter()
        .map(|e| e.sequence)
        .collect();
    assert_eq!(stored, expected);
    let stored_b: Vec<i64> = store
        .events_after(b, 0)
        .await
        .unwrap()
        .iter()
        .map(|e| e.sequence)
        .collect();
    assert_eq!(stored_b, (1..=OTHER).collect::<Vec<_>>());

    // Resume from a sequence returns only later events.
    let tail = store.events_after(a, TASKS * PER_TASK - 3).await.unwrap();
    assert_eq!(
        tail.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        (TASKS * PER_TASK - 2..=TASKS * PER_TASK).collect::<Vec<_>>()
    );
    assert!(tail.iter().all(|e| e.phase == OperationPhase::Building));
    store.close().await;
}

#[tokio::test]
async fn newer_schema_is_refused_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    store.create_project(&slug("app")).await.unwrap();
    store.close().await;

    let newer = LATEST_VERSION + 5;
    let mut conn = raw(dir.path()).await;
    sqlx::raw_sql(&format!("PRAGMA user_version = {newer}"))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    let before = snapshot(dir.path());
    assert_eq!(before.keys().collect::<Vec<_>>(), [DATABASE_FILE]);

    match Store::open(dir.path()).await {
        Err(StoreError::SchemaTooNew { found, supported }) => {
            assert_eq!(found, newer);
            assert_eq!(supported, LATEST_VERSION);
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }

    assert_eq!(snapshot(dir.path()), before);
    let mut conn = raw(dir.path()).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(version, i64::from(newer));
    conn.close().await.unwrap();
}
