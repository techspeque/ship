//! The operation queue against a real SQLite file (ADR-0011): idempotent
//! submission, per-environment serialisation, the build slot and timeout,
//! admission limits, redaction and the bounded log.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ship_core::secret::REDACTED;
use ship_core::{EnvironmentId, ErrorCode, OperationPhase, OperationStatus, SecretValue, Slug};
use ship_daemon::Redactor;
use ship_daemon::queue::{Context, JobFailure, Limits, Queue, QueueError, Submission};
use ship_store::{Operation, Store};
use tokio::sync::{Barrier, Mutex, oneshot};

const SECRET: &str = "hunter2-very-secret-token";
/// A registered value spanning lines (f-022).
const MULTI: &str = "private-first-line\nprivate-second-line";

struct Setup {
    dir: tempfile::TempDir,
    store: Store,
    queue: Queue,
    env: EnvironmentId,
    other: EnvironmentId,
}

async fn setup(limits: Limits) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let redactor = Redactor::new();
    redactor.register(&SecretValue::new(SECRET));
    redactor.register(&SecretValue::new(MULTI));
    let queue = Queue::new(store.clone(), redactor, limits, dir.path()).unwrap();
    let env = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment
        .id;
    let other = store
        .get_or_create_project(&Slug::parse("other").unwrap())
        .await
        .unwrap()
        .environment
        .id;
    Setup {
        dir,
        store,
        queue,
        env,
        other,
    }
}

fn limits() -> Limits {
    Limits {
        disk_watermark_bytes: 0,
        ..Limits::default()
    }
}

fn sub(key: &str, env: EnvironmentId) -> Submission<'_> {
    Submission {
        idempotency_key: key,
        kind: "test",
        environment_id: Some(env),
    }
}

/// Polls until `id` has ended.
async fn finished(store: &Store, id: ship_core::OperationId) -> Operation {
    for _ in 0..1000 {
        let op = store.get_operation(id).await.unwrap();
        if op.phase.is_terminal() {
            return op;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("operation {id} did not finish");
}

#[tokio::test]
async fn replayed_key_returns_the_original_and_runs_once() {
    let s = setup(limits()).await;
    let runs = Arc::new(AtomicUsize::new(0));
    async fn count(runs: Arc<AtomicUsize>) -> Result<(), JobFailure> {
        runs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    let job = |runs: Arc<AtomicUsize>| move |_ctx: Context| count(runs);
    let first = s
        .queue
        .submit(sub("key-1", s.env), job(runs.clone()))
        .await
        .unwrap();
    assert!(first.created);
    let again = s
        .queue
        .submit(sub("key-1", s.env), job(runs.clone()))
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.operation.id, first.operation.id);

    let op = finished(&s.store, first.operation.id).await;
    assert_eq!(op.status, OperationStatus::Succeeded);
    let late = s
        .queue
        .submit(sub("key-1", s.env), job(runs.clone()))
        .await
        .unwrap();
    assert!(!late.created);
    assert_eq!(late.operation.status, OperationStatus::Succeeded);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let events = s.store.events_after(op.id, 0).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.event_type, "finished");
    assert_eq!(last.phase, OperationPhase::Succeeded);
}

#[tokio::test]
async fn a_key_reused_for_another_request_conflicts() {
    let s = setup(limits()).await;
    s.queue
        .submit(sub("key-1", s.env), |_| async { Ok(()) })
        .await
        .unwrap();
    let other_kind = Submission {
        kind: "stop",
        ..sub("key-1", s.env)
    };
    assert!(matches!(
        s.queue.submit(other_kind, |_| async { Ok(()) }).await,
        Err(QueueError::IdempotencyConflict)
    ));
    assert!(matches!(
        s.queue
            .submit(sub("key-1", s.other), |_| async { Ok(()) })
            .await,
        Err(QueueError::IdempotencyConflict)
    ));
    for bad in ["", "a b", &"k".repeat(129)] {
        assert!(matches!(
            s.queue.submit(sub(bad, s.env), |_| async { Ok(()) }).await,
            Err(QueueError::InvalidKey)
        ));
    }
    let unknown: EnvironmentId = "env_01aryz6s41tsv4rrffq69g5fav".parse().unwrap();
    assert!(matches!(
        s.queue
            .submit(sub("key-2", unknown), |_| async { Ok(()) })
            .await,
        Err(QueueError::UnknownEnvironment)
    ));
}

#[tokio::test]
async fn one_environment_runs_one_mutation_at_a_time_in_order() {
    let s = setup(limits()).await;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut ids = Vec::new();
    for i in 0..6 {
        let (active, peak, order) = (active.clone(), peak.clone(), order.clone());
        let submitted = s
            .queue
            .submit(sub(&format!("serial-{i}"), s.env), move |_ctx| async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                order.lock().await.push(i);
                tokio::time::sleep(Duration::from_millis(15)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        ids.push(submitted.operation.id);
    }
    for id in ids {
        assert_eq!(
            finished(&s.store, id).await.status,
            OperationStatus::Succeeded
        );
    }
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    assert_eq!(*order.lock().await, [0, 1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn two_environments_run_at_the_same_time() {
    let s = setup(limits()).await;
    // Each job passes the barrier only if the other runs at the same time.
    let barrier = Arc::new(Barrier::new(2));
    let mut ids = Vec::new();
    for (key, env) in [("a", s.env), ("b", s.other)] {
        let barrier = barrier.clone();
        let submitted = s
            .queue
            .submit(sub(key, env), move |_ctx| async move {
                tokio::time::timeout(Duration::from_secs(5), barrier.wait())
                    .await
                    .map_err(|_| JobFailure::new(ErrorCode::Internal, "ran alone"))?;
                Ok(())
            })
            .await
            .unwrap();
        ids.push(submitted.operation.id);
    }
    for id in ids {
        let op = finished(&s.store, id).await;
        assert_eq!(op.status, OperationStatus::Succeeded, "{op:?}");
    }
}

#[tokio::test]
async fn builds_never_exceed_the_host_build_slots() {
    let s = setup(Limits {
        build_slots: 1,
        ..limits()
    })
    .await;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut ids = Vec::new();
    for (key, env) in [("a", s.env), ("b", s.other)] {
        let (active, peak) = (active.clone(), peak.clone());
        let submitted = s
            .queue
            .submit(sub(key, env), move |ctx| async move {
                ctx.enter(OperationPhase::Building, "building").await?;
                ctx.build(async {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
            })
            .await
            .unwrap();
        ids.push(submitted.operation.id);
    }
    let mut waited = 0;
    for id in ids {
        assert_eq!(
            finished(&s.store, id).await.status,
            OperationStatus::Succeeded
        );
        let events = s.store.events_after(id, 0).await.unwrap();
        waited += events.iter().filter(|e| e.event_type == "waiting").count();
    }
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    assert_eq!(waited, 1, "the second build waited for the slot");
}

#[tokio::test]
async fn a_build_over_the_timeout_fails_build_timeout() {
    let s = setup(Limits {
        build_timeout: Duration::from_millis(50),
        ..limits()
    })
    .await;
    let submitted = s
        .queue
        .submit(sub("slow", s.env), |ctx| async move {
            ctx.enter(OperationPhase::Building, "building").await?;
            ctx.build(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    let op = finished(&s.store, submitted.operation.id).await;
    assert_eq!(op.status, OperationStatus::Failed);
    assert_eq!(op.failure_code.as_deref(), Some("build_timeout"));
    assert_eq!(op.failure_phase, Some(OperationPhase::Building));
}

#[tokio::test]
async fn over_the_queue_length_fails_before_the_job_runs() {
    let s = setup(Limits {
        max_queue: 1,
        ..limits()
    })
    .await;
    let (release, hold) = oneshot::channel::<()>();
    let (started_tx, started) = oneshot::channel::<()>();
    let first = s
        .queue
        .submit(sub("first", s.env), move |_ctx| async move {
            let _ = started_tx.send(());
            let _ = hold.await;
            Ok(())
        })
        .await
        .unwrap();
    started.await.unwrap();

    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    let refused = s
        .queue
        .submit(sub("second", s.other), move |_ctx| async move {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
    assert!(refused.created);
    let op = refused.operation;
    assert_eq!(op.status, OperationStatus::Failed);
    assert_eq!(op.failure_code.as_deref(), Some("queue_full"));
    assert_eq!(op.failure_phase, Some(OperationPhase::Queued));

    release.send(()).unwrap();
    finished(&s.store, first.operation.id).await;
    // The slot is free again.
    let third = s
        .queue
        .submit(sub("third", s.other), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(
        finished(&s.store, third.operation.id).await.status,
        OperationStatus::Succeeded
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn under_the_disk_watermark_fails_before_the_job_runs() {
    let s = setup(Limits {
        disk_watermark_bytes: u64::MAX,
        ..limits()
    })
    .await;
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    let refused = s
        .queue
        .submit(sub("disk", s.env), move |_ctx| async move {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
    let op = refused.operation;
    assert_eq!(op.status, OperationStatus::Failed);
    assert_eq!(op.failure_code.as_deref(), Some("insufficient_storage"));
    assert_eq!(op.failure_phase, Some(OperationPhase::Queued));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert!(!s.queue.log_path(op.id).exists());
}

/// Submits each of several keys a few times at once under `limits`, which
/// refuse them, and checks every answer carries the refusal `code`
/// (f-023).
async fn concurrent_replay_observes_the_refusal(limits: Limits, code: &str) {
    const ROUNDS: usize = 20;
    const COPIES: usize = 4;
    let s = setup(limits).await;
    let ran = Arc::new(AtomicUsize::new(0));
    for round in 0..ROUNDS {
        let start = Arc::new(Barrier::new(COPIES));
        let tasks: Vec<_> = (0..COPIES)
            .map(|_| {
                let (queue, env, ran, start) = (s.queue.clone(), s.env, ran.clone(), start.clone());
                tokio::spawn(async move {
                    let key = format!("same-{round}");
                    start.wait().await;
                    queue
                        .submit(sub(&key, env), move |_ctx: Context| async move {
                            ran.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut answers = Vec::new();
        for task in tasks {
            answers.push(task.await.unwrap());
        }
        assert_eq!(answers.iter().filter(|a| a.created).count(), 1);
        for submitted in &answers {
            let op = &submitted.operation;
            assert_eq!(op.id, answers[0].operation.id);
            assert_eq!(op.status, OperationStatus::Failed, "{submitted:?}");
            assert_eq!(op.failure_code.as_deref(), Some(code), "{submitted:?}");
            assert_eq!(op.failure_phase, Some(OperationPhase::Queued));
            let response = ship_daemon::http::submitted_response(op);
            assert_eq!(
                response.status().as_u16(),
                code.parse::<ErrorCode>().unwrap().http_status(),
                "{submitted:?}"
            );
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_replay_of_a_queue_full_refusal_is_refused() {
    concurrent_replay_observes_the_refusal(
        Limits {
            max_queue: 0,
            ..limits()
        },
        "queue_full",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_replay_of_an_insufficient_storage_refusal_is_refused() {
    concurrent_replay_observes_the_refusal(
        Limits {
            disk_watermark_bytes: u64::MAX,
            ..limits()
        },
        "insufficient_storage",
    )
    .await;
}

#[tokio::test]
async fn a_panicking_job_fails_internal_and_the_worker_goes_on() {
    let s = setup(limits()).await;
    let boom = s
        .queue
        .submit(sub("boom", s.env), |_ctx| async move {
            if std::hint::black_box(true) {
                panic!("job bug");
            }
            Ok(())
        })
        .await
        .unwrap();
    let op = finished(&s.store, boom.operation.id).await;
    assert_eq!(op.status, OperationStatus::Failed);
    assert_eq!(op.failure_code.as_deref(), Some("internal"));
    let next = s
        .queue
        .submit(sub("next", s.env), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(
        finished(&s.store, next.operation.id).await.status,
        OperationStatus::Succeeded
    );
}

#[tokio::test]
async fn a_secret_never_reaches_the_log_events_or_failure() {
    let s = setup(limits()).await;
    let submitted = s
        .queue
        .submit(sub("secret", s.env), |ctx| async move {
            ctx.enter(OperationPhase::Building, &format!("building with {SECRET}"))
                .await?;
            ctx.log(&format!("token={SECRET}\nquoted \"{SECRET}\" end"))
                .await?;
            Err(JobFailure::new(
                ErrorCode::BuildFailed,
                format!("build printed {SECRET}"),
            ))
        })
        .await
        .unwrap();
    let op = finished(&s.store, submitted.operation.id).await;
    assert_eq!(op.failure_code.as_deref(), Some("build_failed"));
    let failure = op.failure_message.unwrap();
    assert!(!failure.contains(SECRET), "{failure}");
    assert!(failure.contains(REDACTED), "{failure}");

    let log = std::fs::read_to_string(s.queue.log_path(op.id)).unwrap();
    assert!(!log.contains(SECRET), "{log}");
    assert!(!log.contains("hunter2"), "{log}");
    assert_eq!(log.lines().count(), 2, "{log}");
    for line in log.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["phase"], "building");
        assert!(v["line"].as_str().unwrap().contains(REDACTED), "{line}");
        assert!(v["ts"].is_string());
    }

    let events = s.store.events_after(op.id, 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>(),
        ["phase_started", "log", "log", "finished"]
    );
    for e in &events {
        assert!(!e.message.contains(SECRET), "{e:?}");
    }
    assert!(events.iter().all(|e| e.message.contains(REDACTED)));
}

/// A registered value spanning lines is removed whole from the log, the
/// events and the failure, and none of its lines survive (f-022).
#[tokio::test]
async fn a_multiline_secret_never_reaches_the_log_events_or_failure() {
    let s = setup(limits()).await;
    let submitted = s
        .queue
        .submit(sub("multi", s.env), |ctx| async move {
            ctx.enter(OperationPhase::Building, &format!("building with {MULTI}"))
                .await?;
            ctx.log(&format!("before\n{MULTI}\nafter")).await?;
            Err(JobFailure::new(
                ErrorCode::BuildFailed,
                format!("build printed {MULTI} and stopped"),
            ))
        })
        .await
        .unwrap();
    let op = finished(&s.store, submitted.operation.id).await;
    let leaked =
        |text: &str| text.contains("private-first-line") || text.contains("private-second-line");

    let failure = op.failure_message.clone().unwrap();
    assert!(!leaked(&failure), "{failure}");
    assert!(failure.contains(REDACTED), "{failure}");

    let log = std::fs::read_to_string(s.queue.log_path(op.id)).unwrap();
    assert!(!leaked(&log), "{log}");
    assert!(log.contains(REDACTED), "{log}");

    let events = s.store.events_after(op.id, 0).await.unwrap();
    for e in &events {
        assert!(!leaked(&e.message), "{e:?}");
    }
    let finished_event = events.last().unwrap();
    assert_eq!(finished_event.event_type, "finished");
    assert!(finished_event.message.contains(REDACTED));

    let view = serde_json::to_string(&ship_daemon::http::operation_view(&op)).unwrap();
    assert!(!leaked(&view), "{view}");
}

#[tokio::test]
async fn the_log_is_bounded_and_its_cursor_is_its_length() {
    let s = setup(Limits {
        max_log_bytes: 600,
        ..limits()
    })
    .await;
    let submitted = s
        .queue
        .submit(sub("chatty", s.env), |ctx| async move {
            ctx.enter(OperationPhase::Building, "building").await?;
            for i in 0..100 {
                ctx.log(&format!("line {i:03}")).await?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let op = finished(&s.store, submitted.operation.id).await;
    assert_eq!(op.status, OperationStatus::Succeeded);

    let path = s.queue.log_path(op.id);
    let log = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(op.log_cursor as usize, log.len());
    let marker = lines.last().unwrap();
    assert!(marker.contains("log truncated"), "{marker}");
    assert_eq!(log.matches("log truncated").count(), 1);
    let kept = &log[..log.len() - marker.len() - 1];
    assert!(kept.len() <= 600, "{}", kept.len());
    assert!(lines.len() < 100);

    let events = s.store.events_after(op.id, 0).await.unwrap();
    let logs = events.iter().filter(|e| e.event_type == "log").count();
    assert_eq!(logs, lines.len());

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert!(path.starts_with(s.dir.path()));
}

#[tokio::test]
async fn a_closed_queue_refuses_new_work() {
    let s = setup(limits()).await;
    s.queue.shutdown().await;
    assert!(matches!(
        s.queue
            .submit(sub("late", s.env), |_| async { Ok(()) })
            .await,
        Err(QueueError::Closed)
    ));
}
