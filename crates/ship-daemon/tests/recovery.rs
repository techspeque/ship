//! Daemon start marks operations left unfinished as `interrupted` and
//! never resumes them (ADR-0011 rule 8).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ship_core::{ErrorCode, OperationPhase, OperationStatus, Slug};
use ship_daemon::queue::{INTERRUPTED_MESSAGE, LOG_DIR, Submission};
use ship_store::{NewEvent, NewOperation, Store};

#[tokio::test]
async fn rows_left_queued_or_running_are_interrupted_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::config(dir.path());
    std::fs::create_dir_all(&config.state_dir).unwrap();

    // A previous daemon's leftovers, written straight to the store.
    let (queued, building, done, log) = {
        let store = Store::open(&config.state_dir).await.unwrap();
        let env = store
            .get_or_create_project(&Slug::parse("app").unwrap())
            .await
            .unwrap()
            .environment
            .id;
        let new = |key| NewOperation {
            idempotency_key: key,
            kind: "push",
            environment_id: Some(env),
        };
        let queued = store
            .create_operation(&new("q"))
            .await
            .unwrap()
            .operation
            .id;
        let building = store
            .create_operation(&new("b"))
            .await
            .unwrap()
            .operation
            .id;
        store
            .advance_operation(building, OperationPhase::Building)
            .await
            .unwrap();
        store
            .append_event(
                building,
                &NewEvent {
                    event_type: "phase_started",
                    phase: OperationPhase::Building,
                    message: "building web",
                },
            )
            .await
            .unwrap();
        let done = store
            .create_operation(&new("d"))
            .await
            .unwrap()
            .operation
            .id;
        store
            .advance_operation(done, OperationPhase::Succeeded)
            .await
            .unwrap();
        let log_dir = config.state_dir.join(LOG_DIR);
        std::fs::create_dir_all(&log_dir).unwrap();
        let log = log_dir.join(format!("{building}.log"));
        std::fs::write(&log, "{\"line\":\"step 1\"}\n").unwrap();
        store.close().await;
        (queued, building, done, log)
    };

    let running = ship_daemon::start(&config).await.unwrap();
    let store = running.store();
    for (id, phase) in [
        (queued, OperationPhase::Queued),
        (building, OperationPhase::Building),
    ] {
        let op = store.get_operation(id).await.unwrap();
        assert_eq!(op.phase, OperationPhase::Interrupted);
        assert_eq!(op.status, OperationStatus::Interrupted);
        assert_eq!(
            op.failure_code.as_deref(),
            Some(ErrorCode::Interrupted.as_str())
        );
        assert_eq!(op.failure_message.as_deref(), Some(INTERRUPTED_MESSAGE));
        assert_eq!(op.failure_phase, Some(phase));
        let last = store.events_after(id, 0).await.unwrap().pop().unwrap();
        assert_eq!(last.event_type, "finished");
        assert_eq!(last.phase, OperationPhase::Interrupted);
    }
    let events = store.events_after(building, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].message, "building web");
    assert_eq!(
        store.get_operation(done).await.unwrap().status,
        OperationStatus::Succeeded
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "{\"line\":\"step 1\"}\n"
    );
    running.shutdown().await.unwrap();

    // A second start finds nothing left to interrupt.
    let running = ship_daemon::start(&config).await.unwrap();
    assert_eq!(
        running
            .store()
            .events_after(building, 0)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        running
            .store()
            .unfinished_operations()
            .await
            .unwrap()
            .is_empty()
    );
    running.shutdown().await.unwrap();
}

/// A start records an observation of every service at once, with no
/// request and long before the observer's interval: here Docker cannot be
/// reached, so each is `unknown` and nothing else happens (ADR-0006 rules
/// 4 and 11).
#[tokio::test]
async fn observed_state_is_rebuilt_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = common::config(dir.path());
    config.observe_interval = Duration::from_secs(3600);
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let env = {
        let store = Store::open(&config.state_dir).await.unwrap();
        let env = store
            .get_or_create_project(&Slug::parse("app").unwrap())
            .await
            .unwrap()
            .environment;
        store
            .create_service(
                env.id,
                &ship_store::NewService {
                    name: "web".to_owned(),
                    kind: ship_core::ServiceKind::Web,
                    port: None,
                },
            )
            .await
            .unwrap();
        assert!(store.list_observations(env.id).await.unwrap().is_empty());
        store.close().await;
        env
    };

    let running = ship_daemon::start(&config).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let observed = loop {
        let list = running.store().list_observations(env.id).await.unwrap();
        if !list.is_empty() {
            break list;
        }
        assert!(tokio::time::Instant::now() < deadline, "nothing observed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(observed.len(), 1);
    assert_eq!(
        observed[0].observation,
        ship_core::Observation::DockerUnreachable
    );
    assert_eq!(
        running
            .store()
            .get_environment(env.id)
            .await
            .unwrap()
            .desired_state,
        ship_core::DesiredState::Running
    );
    running.shutdown().await.unwrap();
}

static RUNS: AtomicUsize = AtomicUsize::new(0);

/// The closest an in-process test gets to `kill -9`: the runtime is
/// dropped under a running job, with no shutdown and no cleanup.
#[test]
fn a_daemon_killed_mid_operation_leaves_it_interrupted_with_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::config(dir.path());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (running, id) = runtime.block_on(async {
        let running = ship_daemon::start(&config).await.unwrap();
        let env = running
            .store()
            .get_or_create_project(&Slug::parse("app").unwrap())
            .await
            .unwrap()
            .environment
            .id;
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let submitted = running
            .queue()
            .submit(
                Submission {
                    idempotency_key: "kill-me",
                    kind: "push",
                    environment_id: Some(env),
                },
                move |ctx| async move {
                    RUNS.fetch_add(1, Ordering::SeqCst);
                    ctx.enter(OperationPhase::Building, "building web").await?;
                    ctx.log("first line\nsecond line").await?;
                    let _ = started_tx.send(());
                    std::future::pending::<()>().await;
                    Ok(())
                },
            )
            .await
            .unwrap();
        started.await.unwrap();
        (running, submitted.operation.id)
    });
    runtime.shutdown_background();
    // Never shut down: the socket file, the database connections and the
    // job are abandoned as a killed process would abandon them.
    std::mem::forget(running);

    let log_path = config.state_dir.join(LOG_DIR).join(format!("{id}.log"));
    let log_before = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(log_before.lines().count(), 2);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let running = ship_daemon::start(&config).await.unwrap();
        let op = running.store().get_operation(id).await.unwrap();
        assert_eq!(op.status, OperationStatus::Interrupted);
        assert_eq!(op.failure_phase, Some(OperationPhase::Building));
        assert_eq!(op.failure_code.as_deref(), Some("interrupted"));
        assert_eq!(op.log_cursor as usize, log_before.len());
        let events = running.store().events_after(id, 0).await.unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(kinds, ["phase_started", "log", "log", "finished"]);
        assert_eq!(events[1].message, "first line");
        assert_eq!(events[2].message, "second line");
        assert_eq!(events[3].phase, OperationPhase::Interrupted);
        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), log_before);

        // Not resumed: the job ran once, and a replay of its key returns
        // the interrupted operation without running anything.
        let replay = running
            .queue()
            .submit(
                Submission {
                    idempotency_key: "kill-me",
                    kind: "push",
                    environment_id: op.environment_id,
                },
                |_| async {
                    RUNS.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(!replay.created);
        assert_eq!(replay.operation.status, OperationStatus::Interrupted);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);
        running.shutdown().await.unwrap();
    });
}
